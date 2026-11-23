//! The lint oracle's executions: what a lint finding claims, run over the
//! nullability oracle's adversarial scenarios.
//!
//! `typedpg_lint` (under its own `pg_sanity` feature) turns each finding
//! into [`LintProbe`]s — its fix, or its claim as an equivalent query, as
//! assertions, or as a row-count bound — and hands them here with the query
//! as analyzed. Each probe is prepared next to the original statement, and
//! both run, in one transaction rolled back at the end, over every
//! scenario of [`super::SCENARIOS`] and two parameter samplings: NULL
//! where the analyzer lets a parameter be NULL (a sample otherwise), and a
//! sample wherever the type has one.
//!
//! An assertion probe calls the `pg_temp.typedpg_lint_*` functions below:
//! each raises an error carrying [`CLAIM_VIOLATED`] when the value it
//! wraps breaks the claim, and returns the value otherwise, so the probe is
//! the original query with a check where the claim is made.

use postgres::types::ToSql;
use postgres::{Client, Statement, Transaction};

use super::{
    Difference, Outcome, Param, Rows, SCENARIOS, SeedPlan, Soundness, nondeterministic, repro,
    restore_sequences, result_difference, row_comparison, run, sequence_state,
};
use crate::resolve::AnalyzedQuery;

/// The text every assertion's error starts with.
pub const CLAIM_VIOLATED: &str = "typedpg-lint claim violated";

/// The assertion functions, created in the query session's `pg_temp`.
///
/// `num_nulls` tests the datum itself: a row of NULL fields `IS NULL`, but
/// `COALESCE` and `count` take it as a value. The `text` overloads take an
/// untyped literal, which a polymorphic parameter can't.
const LINT_SQL: &str = r#"
CREATE FUNCTION pg_temp.typedpg_lint_true(v boolean, claim text) RETURNS boolean
LANGUAGE plpgsql VOLATILE AS $f$
BEGIN
    IF v IS NOT TRUE THEN
        RAISE EXCEPTION 'typedpg-lint claim violated: % (it is %)', claim, coalesce(v::text, 'NULL');
    END IF;
    RETURN v;
END $f$;
CREATE FUNCTION pg_temp.typedpg_lint_not_true(v boolean, claim text) RETURNS boolean
LANGUAGE plpgsql VOLATILE AS $f$
BEGIN
    IF v IS TRUE THEN
        RAISE EXCEPTION 'typedpg-lint claim violated: % (it is true)', claim;
    END IF;
    RETURN v;
END $f$;
CREATE FUNCTION pg_temp.typedpg_lint_nonnull(v anyelement, claim text) RETURNS anyelement
LANGUAGE plpgsql VOLATILE AS $f$
BEGIN
    IF num_nulls(v) > 0 THEN
        RAISE EXCEPTION 'typedpg-lint claim violated: % (it is NULL)', claim;
    END IF;
    RETURN v;
END $f$;
CREATE FUNCTION pg_temp.typedpg_lint_nonnull(v text, claim text) RETURNS text
LANGUAGE plpgsql VOLATILE AS $f$
BEGIN
    IF v IS NULL THEN
        RAISE EXCEPTION 'typedpg-lint claim violated: % (it is NULL)', claim;
    END IF;
    RETURN v;
END $f$;
CREATE FUNCTION pg_temp.typedpg_lint_unreached(v anyelement, claim text) RETURNS anyelement
LANGUAGE plpgsql VOLATILE AS $f$
BEGIN
    RAISE EXCEPTION 'typedpg-lint claim violated: % (it is evaluated)', claim;
END $f$;
CREATE FUNCTION pg_temp.typedpg_lint_unreached(v text, claim text) RETURNS text
LANGUAGE plpgsql VOLATILE AS $f$
BEGIN
    RAISE EXCEPTION 'typedpg-lint claim violated: % (it is evaluated)', claim;
END $f$;
"#;

/// What a lint finding claims, as something the mirror runs. Every `sql`
/// is the analyzed text (`$n` parameters, the original's numbering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintProbe {
    /// A fix: `sql` must prepare to the original's columns and give the
    /// same result in every scenario — the same rows (as a multiset; in
    /// the order of the original's `ORDER BY` output columns too), row
    /// count and table contents, or an error with the same SQLSTATE.
    Fix { sql: String },
    /// A fix dropping a `RETURNING`: `sql` returns no columns, affects as
    /// many rows as the original returns and leaves the same tables.
    FixDropsReturning { sql: String },
    /// A claim as an equivalent query: wherever both run, the same result
    /// as the original. The probe failing where the original runs proves
    /// nothing (the rewrite may evaluate something the original doesn't).
    Equivalent { sql: String },
    /// A claim as assertions (the `pg_temp.typedpg_lint_*` functions): it
    /// is broken when the probe fails with [`CLAIM_VIOLATED`] where the
    /// original runs.
    Assert { sql: String },
    /// `sql` returns at most one row wherever the original runs.
    AtMostOneRow { sql: String },
    /// The original statement fails wherever it would write a row.
    NeverWrites,
}

impl LintProbe {
    fn sql(&self) -> Option<&str> {
        match self {
            LintProbe::Fix { sql }
            | LintProbe::FixDropsReturning { sql }
            | LintProbe::Equivalent { sql }
            | LintProbe::Assert { sql }
            | LintProbe::AtMostOneRow { sql } => Some(sql),
            LintProbe::NeverWrites => None,
        }
    }

    fn is_fix(&self) -> bool {
        matches!(
            self,
            LintProbe::Fix { .. } | LintProbe::FixDropsReturning { .. }
        )
    }
}

/// What running a [`LintProbe`] showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintProbeResult {
    /// The claim held in every scenario compared (how many).
    Held { compared: usize },
    /// The claim is broken: how, with the scenario and the data.
    Broken(String),
    /// Nothing could be compared: why.
    Inconclusive(String),
}

/// A probe prepared next to the original, and what it showed so far.
struct Prepared<'a> {
    probe: &'a LintProbe,
    stmt: Option<Statement>,
    result: Option<LintProbeResult>,
    compared: usize,
    /// Why scenarios couldn't be compared, the first one.
    skipped: Option<String>,
}

impl Soundness {
    /// Create the assertion functions in the query session (once).
    fn install_lint(&mut self, client: &mut Client) {
        if !self.lint_installed {
            client
                .batch_execute(LINT_SQL)
                .unwrap_or_else(|e| panic!("pg_sanity: installing the lint checks failed: {e:?}"));
            self.lint_installed = true;
        }
    }

    /// Run `probes` next to `original` (the analyzed text of a query the
    /// analyzer accepted as `ours`). One result per probe.
    pub(crate) fn lint_probes(
        &mut self,
        client: &mut Client,
        catalog: &crate::PgCatalog,
        original_sql: &str,
        ours: &AnalyzedQuery,
        probes: &[LintProbe],
        finite_params: &std::collections::HashSet<u32>,
    ) -> Vec<LintProbeResult> {
        let inconclusive =
            |why: &str| vec![LintProbeResult::Inconclusive(why.to_owned()); probes.len()];
        if !self.enabled {
            return inconclusive("the oracle is off");
        }
        if !super::is_executable(original_sql) {
            return inconclusive("not a SELECT / INSERT / UPDATE / DELETE / MERGE");
        }
        let lower = original_sql.to_ascii_lowercase();
        if super::UNSAFE_CALLS.iter().any(|f| lower.contains(f)) {
            return inconclusive("calls an unsafe function");
        }
        if !ours.spreads.is_empty() {
            return inconclusive("has a spread");
        }
        self.install(client);
        self.install_lint(client);
        let original = match client.prepare(original_sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                return inconclusive(&format!(
                    "the original doesn't prepare: {}",
                    super::super::render_pg_error(&e)
                ));
            }
        };
        let shape = |s: &Statement| {
            s.columns()
                .iter()
                .map(|c| (c.name().to_owned(), c.type_().oid()))
                .collect::<Vec<_>>()
        };
        let mut prepared: Vec<Prepared<'_>> = probes
            .iter()
            .map(|probe| {
                let mut p = Prepared {
                    probe,
                    stmt: None,
                    result: None,
                    compared: 0,
                    skipped: None,
                };
                let Some(sql) = probe.sql() else {
                    return p;
                };
                let compares_rows = !matches!(
                    probe,
                    LintProbe::Assert { .. } | LintProbe::AtMostOneRow { .. }
                );
                if compares_rows && (nondeterministic(original_sql) || nondeterministic(sql)) {
                    p.result = Some(LintProbeResult::Inconclusive(
                        "a volatile call makes the results differ between runs".to_owned(),
                    ));
                    return p;
                }
                match client.prepare_typed(sql, original.params()) {
                    Ok(stmt) => {
                        let broken = match probe {
                            LintProbe::Fix { .. } if shape(&stmt) != shape(&original) => {
                                Some(format!(
                                    "the fixed query prepares to other columns: {:?}, the \
                                     original to {:?}",
                                    shape(&stmt),
                                    shape(&original)
                                ))
                            }
                            LintProbe::FixDropsReturning { .. } if !stmt.columns().is_empty() => {
                                Some("the fixed query still returns columns".to_owned())
                            }
                            _ => None,
                        };
                        if let Some(broken) = broken {
                            p.result = Some(LintProbeResult::Broken(format!(
                                "{broken}\nfixed:\n---\n{sql}\n---"
                            )));
                        }
                        p.stmt = Some(stmt);
                    }
                    Err(e) => {
                        let e = super::super::render_pg_error(&e);
                        p.result = Some(if probe.is_fix() {
                            LintProbeResult::Broken(format!(
                                "PG rejects the fixed query: {e}\nfixed:\n---\n{sql}\n---"
                            ))
                        } else {
                            LintProbeResult::Inconclusive(format!("the probe doesn't prepare: {e}"))
                        });
                    }
                }
                p
            })
            .collect();
        if prepared.iter().all(|p| p.result.is_some()) {
            return prepared.into_iter().filter_map(|p| p.result).collect();
        }

        // NULL where the analyzer allows it, then a sample wherever there
        // is one (a parameter of a type without a sample stays NULL).
        let mut samplings: Vec<Vec<Option<String>>> = Vec::new();
        if let Some(values) = self.param_values(client, ours, &original, finite_params) {
            samplings.push(values);
        }
        let sampled: Vec<Option<String>> = original
            .params()
            .iter()
            .map(|ty| {
                if ty.oid() == 0 {
                    None
                } else {
                    self.param_text(client, ty.oid(), 1)
                }
            })
            .collect();
        if !samplings.contains(&sampled) {
            samplings.push(sampled);
        }
        let rows = row_comparison(catalog, original_sql, ours);
        let mut targets = super::written_tables(original_sql);
        for p in &prepared {
            if let Some(sql) = p.probe.sql() {
                targets.extend(super::written_tables(sql));
            }
        }
        targets.sort();
        targets.dedup();
        let literals = super::query_literals(original_sql);
        for values in &samplings {
            if prepared.iter().all(|p| p.result.is_some()) {
                break;
            }
            let params: Vec<Param> = values.iter().map(|v| Param(v.clone())).collect();
            let param_refs: Vec<&(dyn ToSql + Sync)> =
                params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
            let mut tx = client
                .transaction()
                .unwrap_or_else(|e| panic!("pg_sanity: BEGIN failed in the lint oracle: {e}"));
            tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '1s'")
                .unwrap_or_else(|e| panic!("pg_sanity: SET LOCAL failed: {e}"));
            self.lint_scenarios(
                &mut tx,
                &original,
                &mut prepared,
                &param_refs,
                values,
                &targets,
                &literals,
                &rows,
            );
        }
        prepared
            .into_iter()
            .map(|p| {
                p.result.unwrap_or_else(|| {
                    if p.compared > 0 {
                        LintProbeResult::Held {
                            compared: p.compared,
                        }
                    } else {
                        LintProbeResult::Inconclusive(
                            p.skipped.unwrap_or_else(|| {
                                "the original failed in every scenario".to_owned()
                            }),
                        )
                    }
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn lint_scenarios(
        &mut self,
        tx: &mut Transaction<'_>,
        original: &Statement,
        prepared: &mut [Prepared<'_>],
        param_refs: &[&(dyn ToSql + Sync)],
        values: &[Option<String>],
        targets: &[String],
        literals: &[String],
        rows: &Rows,
    ) {
        let mut seeded: Vec<(&str, SeedPlan)> = Vec::new();
        for (scenario, mode) in SCENARIOS {
            if prepared.iter().all(|p| p.result.is_some()) {
                return;
            }
            if mode > 0 {
                let plan = self.seed(tx, mode, literals);
                let unchanged = mode >= 3 && plan.inserts.is_empty();
                seeded.push((scenario, plan));
                if unchanged {
                    continue;
                }
            }
            // Sequences are not transactional: each statement starts from
            // the values the seeding left, or an identity column would
            // differ between the runs.
            let restore = sequence_state(tx);
            let (written, written_tables) = run(tx, original, param_refs, targets);
            for p in prepared.iter_mut().filter(|p| p.result.is_none()) {
                let verdict = match p.probe {
                    LintProbe::NeverWrites => never_writes(&written),
                    _ => {
                        let stmt = p.stmt.as_ref().expect("a probe with SQL is prepared");
                        restore_sequences(tx, restore.as_deref());
                        let (sent, sent_tables) = run(tx, stmt, param_refs, targets);
                        compare(
                            p.probe,
                            (&written, &written_tables),
                            (&sent, &sent_tables),
                            rows,
                        )
                    }
                };
                match verdict {
                    Verdict::Held => p.compared += 1,
                    Verdict::Skip(why) => {
                        p.skipped.get_or_insert(why);
                    }
                    Verdict::Broken(problem) => {
                        let (params, seeding) = repro(values, &seeded);
                        let probe = p
                            .probe
                            .sql()
                            .map(|sql| format!("probe:\n---\n{sql}\n---\n"))
                            .unwrap_or_default();
                        p.result = Some(LintProbeResult::Broken(format!(
                            "{problem}\n{probe}scenario: {scenario}\nparams: {params}\n\
                             seeding:\n{seeding}"
                        )));
                    }
                }
            }
        }
    }
}

enum Verdict {
    Held,
    Skip(String),
    Broken(String),
}

/// The SQLSTATE at the end of a rendered error.
fn sqlstate(rendered: &str) -> &str {
    rendered
        .rsplit_once("(SQLSTATE ")
        .map_or(rendered, |(_, code)| code.trim_end_matches(')'))
}

fn never_writes(written: &Outcome) -> Verdict {
    match written {
        Outcome::Failed(_) => Verdict::Held,
        Outcome::Count(0) => Verdict::Held,
        Outcome::Rows(rows) if rows.is_empty() => Verdict::Held,
        Outcome::Truncated => Verdict::Skip("too many rows".to_owned()),
        Outcome::Count(n) => Verdict::Broken(format!("the statement writes {n} rows")),
        Outcome::Rows(rows) => Verdict::Broken(format!(
            "the statement writes {} rows: {rows:?}",
            rows.len()
        )),
    }
}

type Tables = Vec<super::TableContents>;

fn compare(
    probe: &LintProbe,
    (written, written_tables): (&Outcome, &Tables),
    (sent, sent_tables): (&Outcome, &Tables),
    rows: &Rows,
) -> Verdict {
    let fix = probe.is_fix();
    match (written, sent) {
        (Outcome::Truncated, _) | (_, Outcome::Truncated) => {
            return Verdict::Skip("too many rows".to_owned());
        }
        (Outcome::Failed(a), Outcome::Failed(b)) => {
            return if !fix || sqlstate(a) == sqlstate(b) {
                if fix {
                    Verdict::Held
                } else {
                    Verdict::Skip(format!("the original fails: {a}"))
                }
            } else {
                Verdict::Broken(format!(
                    "the fixed query fails with another error:\n  original: {a}\n  fixed:    {b}"
                ))
            };
        }
        (Outcome::Failed(a), _) => {
            return if fix {
                Verdict::Broken(format!("the original fails, the fixed query doesn't: {a}"))
            } else {
                Verdict::Skip(format!("the original fails: {a}"))
            };
        }
        (_, Outcome::Failed(b)) => {
            return match probe {
                LintProbe::Fix { .. } | LintProbe::FixDropsReturning { .. } => {
                    Verdict::Broken(format!("the fixed query fails: {b}"))
                }
                LintProbe::Assert { .. } if b.contains(CLAIM_VIOLATED) => {
                    Verdict::Broken(format!("the claim is broken: {b}"))
                }
                _ => Verdict::Skip(format!("the probe fails: {b}")),
            };
        }
        _ => {}
    }
    match probe {
        LintProbe::Assert { .. } => return Verdict::Held,
        LintProbe::AtMostOneRow { .. } => {
            return match sent {
                Outcome::Rows(rows) if rows.len() > 1 => {
                    Verdict::Broken(format!("the query returns {} rows: {rows:?}", rows.len()))
                }
                _ => Verdict::Held,
            };
        }
        _ => {}
    }
    if let (LintProbe::FixDropsReturning { .. }, Outcome::Rows(a), Outcome::Count(_)) =
        (probe, written, sent)
    {
        // The rows it no longer returns are the rows it affects.
        if let Difference::Differs(problem) = result_difference(
            (&Outcome::Count(a.len() as u64), written_tables),
            (sent, sent_tables),
            rows,
            NAMES,
        ) {
            return Verdict::Broken(problem);
        }
        return Verdict::Held;
    }
    match result_difference((written, written_tables), (sent, sent_tables), rows, NAMES) {
        Difference::Same => Verdict::Held,
        Difference::Differs(problem) => Verdict::Broken(problem),
        Difference::Uncompared => Verdict::Skip("too many rows".to_owned()),
    }
}

/// How the lint oracle names the two statements in a difference.
const NAMES: (&str, &str) = ("original", "probe");
