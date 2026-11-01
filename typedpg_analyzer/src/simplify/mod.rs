//! Static query simplifications: rewrites of a query the analysis proved
//! return the same rows, in every database state the declared schema
//! allows, that PostgreSQL plans better.
//!
//! A rule ([`RuleFn`]) looks at the analyzed query and proposes
//! [`Candidate`]s, each a set of text [`Edit`]s on the post-lex SQL. The
//! driver ([`simplify`]) applies each candidate, analyzes the result again
//! and keeps it only when the output columns and the parameters come out
//! identical; it repeats until no rule proposes anything new. The rules
//! own the proof that a rewrite returns the same rows; the driver only
//! guards against a rewrite that changes the query's shape.

use typedpg_pg_query::protobuf::ParseResult;

use crate::pg_catalog::PgCatalog;
use crate::resolve::AnalyzedQuery;

mod not_in;
#[allow(dead_code)] // The rules use them as they land.
pub(crate) mod proof;

/// A text edit on the post-lex SQL ([`AnalyzedQuery::sql`], with `$n`
/// placeholders): `range` replaced by `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    pub range: std::ops::Range<usize>,
    pub text: String,
}

/// One rule's proposed rewrite of one site. Edits must not overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub rule: Rule,
    pub edits: Vec<Edit>,
}

/// A simplification rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rule {
    /// `x NOT IN (SELECT y …)` → `NOT EXISTS (… WHERE x = y)` when neither
    /// side can be NULL.
    NotIn,
    /// A count compared with zero → `EXISTS`.
    CountExists,
    /// A `DISTINCT` that can't remove a row.
    Distinct,
    /// A `UNION` whose branches can't share a row → `UNION ALL`.
    UnionAll,
    /// A join a foreign key proves matches exactly one row, none of whose
    /// columns are read.
    FkJoin,
}

impl Rule {
    /// Every rule.
    pub const ALL: [Rule; 5] = [
        Rule::NotIn,
        Rule::CountExists,
        Rule::Distinct,
        Rule::UnionAll,
        Rule::FkJoin,
    ];

    /// The rule's configuration key (`[package.metadata.typedpg.simplify]`).
    pub fn name(self) -> &'static str {
        match self {
            Rule::NotIn => "not_in",
            Rule::CountExists => "count_exists",
            Rule::Distinct => "distinct",
            Rule::UnionAll => "union_all",
            Rule::FkJoin => "fk_join",
        }
    }

    /// The rule named `name` (see [`Rule::name`]).
    pub fn from_name(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|r| r.name() == name)
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A simplification applied to a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedSimplification {
    pub rule: Rule,
    /// The SQL before this rewrite (post-lex, `$n` placeholders).
    pub original_sql: String,
}

/// What a rule sees.
#[allow(dead_code)] // Read by the rules.
pub(crate) struct Ctx<'a> {
    pub sql: &'a str,
    /// The parse of `sql`.
    pub tree: &'a ParseResult,
    pub catalog: &'a PgCatalog,
    /// The original query's columns and parameters.
    pub analyzed: &'a AnalyzedQuery,
}

/// A rule: finds candidate rewrites. Pure; no side effects.
pub(crate) type RuleFn = fn(&Ctx<'_>) -> Vec<Candidate>;

/// The rules, in the order they run.
pub(crate) const RULES: &[(Rule, RuleFn)] = &[(Rule::NotIn, not_in::candidates)];

/// Which rules run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimplifyOptions {
    /// The whole feature.
    pub enabled: bool,
    /// Rules turned off one by one.
    pub disabled: std::collections::BTreeSet<Rule>,
}

impl Default for SimplifyOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: Default::default(),
        }
    }
}

impl SimplifyOptions {
    /// The options a project's configuration asks for (its names were
    /// checked when it was read).
    pub fn from_config(config: &typedpg_core::config::SimplifyConfig) -> Self {
        Self {
            enabled: config.enabled,
            disabled: config
                .disabled
                .iter()
                .filter_map(|name| Rule::from_name(name))
                .collect(),
        }
    }

    /// Whether `rule` runs.
    pub fn runs(&self, rule: Rule) -> bool {
        self.enabled && !self.disabled.contains(&rule)
    }
}

/// How many times the rules run over one query at most: each pass that
/// applies something runs them again on the result.
const MAX_PASSES: usize = 16;

/// How many re-analyses one query's simplification may cost at most, a
/// batch's or a single candidate's: each is a full analysis.
const MAX_ANALYSES: usize = 32;

/// Simplify `analyzed` in place: its SQL becomes the simplified text and
/// [`AnalyzedQuery::simplifications`] records each rewrite applied.
///
/// Each rule runs, in the order of [`RULES`], over the current SQL. Its
/// candidates that don't overlap are applied together and the result
/// analyzed once; when that analysis gives the same columns (name, type,
/// nullability) and parameters (count, types) as the original, the batch
/// is kept. Otherwise the candidates are tried one at a time, the first
/// that passes kept. Either way the rules then run again on the result,
/// until none proposes anything, or after [`MAX_PASSES`] passes or
/// [`MAX_ANALYSES`] analyses. `param_nullability` is the parameters'
/// explicit nullability, as the original analysis had it.
pub(crate) fn simplify(
    catalog: &PgCatalog,
    analyzed: &mut AnalyzedQuery,
    param_nullability: &[Option<bool>],
) {
    let options = &catalog.simplify_options;
    let rules: Vec<(Rule, RuleFn)> = RULES
        .iter()
        .copied()
        .filter(|(rule, _)| options.runs(*rule))
        .collect();
    run_rules(catalog, analyzed, param_nullability, &rules);
}

/// [`simplify`] with `rules`.
fn run_rules(
    catalog: &PgCatalog,
    analyzed: &mut AnalyzedQuery,
    param_nullability: &[Option<bool>],
    rules: &[(Rule, RuleFn)],
) {
    if rules.is_empty() || !analyzed.spreads.is_empty() {
        return;
    }
    let mut sql = analyzed.sql.clone();
    let mut applied = Vec::new();
    // Rewritten texts that failed the check, not to analyze them twice.
    let mut rejected = std::collections::HashSet::new();
    let mut analyses = 0usize;
    for _ in 0..MAX_PASSES {
        let Ok(tree) = typedpg_pg_query::parse(&sql) else {
            break;
        };
        let ctx = Ctx {
            sql: &sql,
            tree: &tree.protobuf,
            catalog,
            analyzed,
        };
        let mut check = |edited: &str| {
            if analyses >= MAX_ANALYSES || rejected.contains(edited) {
                return false;
            }
            analyses += 1;
            let same = same_shape(catalog, edited, param_nullability, analyzed);
            if !same {
                rejected.insert(edited.to_owned());
            }
            same
        };
        let mut accepted = None;
        for (rule, find) in rules {
            let candidates: Vec<(Candidate, String)> = find(&ctx)
                .into_iter()
                .filter_map(|c| {
                    debug_assert_eq!(c.rule, *rule);
                    let edited = apply_edits(&sql, &c.edits)?;
                    (edited != sql).then_some((c, edited))
                })
                .collect();
            if candidates.is_empty() {
                continue;
            }
            // The candidates that don't overlap an earlier one, together.
            let mut batch: Vec<&Candidate> = Vec::new();
            for (c, _) in &candidates {
                if batch.iter().all(|b| !overlap(b, c)) {
                    batch.push(c);
                }
            }
            if batch.len() > 1 {
                let edits: Vec<Edit> = batch.iter().flat_map(|c| c.edits.clone()).collect();
                if let Some(edited) = apply_edits(&sql, &edits)
                    && check(&edited)
                {
                    accepted = Some((*rule, edited, batch.len()));
                    break;
                }
            }
            if let Some((_, edited)) = candidates.into_iter().find(|(_, e)| check(e)) {
                accepted = Some((*rule, edited, 1));
                break;
            }
        }
        let Some((rule, edited, count)) = accepted else {
            break;
        };
        for _ in 0..count {
            applied.push(AppliedSimplification {
                rule,
                original_sql: sql.clone(),
            });
        }
        sql = edited;
    }
    #[cfg(test)]
    ANALYSES.with(|a| a.set(a.get() + analyses));
    if applied.is_empty() {
        return;
    }
    let Some(offsets) = placeholder_offsets(&sql, analyzed.params.len()) else {
        return;
    };
    for (param, offsets) in analyzed.params.iter_mut().zip(offsets) {
        param.sql_offsets = offsets;
    }
    analyzed.sql = sql;
    analyzed.simplifications = applied;
}

#[cfg(test)]
thread_local! {
    /// The re-analyses [`simplify`] ran on this thread, for the tests.
    static ANALYSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether two candidates' edits touch the same text (or insert at the
/// same place, where their order would matter).
fn overlap(a: &Candidate, b: &Candidate) -> bool {
    a.edits.iter().any(|x| {
        b.edits.iter().any(|y| {
            x.range.start == y.range.start
                || (x.range.start < y.range.end && y.range.start < x.range.end)
        })
    })
}

/// `sql` with `edits` applied, or `None` when they overlap or don't fall
/// on character boundaries within it.
fn apply_edits(sql: &str, edits: &[Edit]) -> Option<String> {
    let mut edits: Vec<&Edit> = edits.iter().collect();
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    let mut out = String::with_capacity(sql.len() + 64);
    let mut last = 0;
    for edit in edits {
        let range = edit.range.clone();
        if range.start < last
            || range.start > range.end
            || !sql.is_char_boundary(range.start)
            || !sql.is_char_boundary(range.end)
        {
            return None;
        }
        out.push_str(&sql[last..range.start]);
        out.push_str(&edit.text);
        last = range.end;
    }
    out.push_str(sql.get(last..)?);
    Some(out)
}

/// Whether `sql` analyzes to the columns and parameters of `original`.
fn same_shape(
    catalog: &PgCatalog,
    sql: &str,
    param_nullability: &[Option<bool>],
    original: &AnalyzedQuery,
) -> bool {
    let Ok((columns, params, can_run_as_subquery)) =
        crate::resolve::analyze_static(catalog, sql, param_nullability)
    else {
        return false;
    };
    columns.len() == original.columns.len()
        && columns
            .iter()
            .zip(&original.columns)
            .all(|(a, b)| a.name == b.name && a.pg_type == b.pg_type && a.nullable == b.nullable)
        && params.len() == original.params.len()
        && params
            .iter()
            .zip(&original.params)
            .all(|(a, b)| a.pg_type == b.pg_type)
        && can_run_as_subquery == original.can_run_as_subquery
}

/// Each parameter's [`crate::AnalyzedParam::sql_offsets`] in `sql`: just
/// past each of its `$n` placeholders, read from PG's own tokens. `None`
/// when a placeholder is out of range or the SQL doesn't scan.
fn placeholder_offsets(sql: &str, count: usize) -> Option<Vec<Vec<usize>>> {
    use typedpg_pg_query::protobuf::Token;
    let scanned = typedpg_pg_query::scan(sql).ok()?;
    let mut offsets = vec![Vec::new(); count];
    for token in &scanned.tokens {
        if token.token() != Token::Param {
            continue;
        }
        let (start, end) = (
            usize::try_from(token.start).ok()?,
            usize::try_from(token.end).ok()?,
        );
        let n: usize = sql.get(start + 1..end)?.parse().ok()?;
        offsets.get_mut(n.checked_sub(1)?)?.push(end);
    }
    Some(offsets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_names_are_the_configuration_keys() {
        let names: Vec<&str> = Rule::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(names, typedpg_core::config::SIMPLIFY_RULES);
    }

    #[test]
    fn many_sites_cost_one_analysis() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql("CREATE TABLE t (a int NOT NULL); CREATE TABLE u (b int NOT NULL);")
            .unwrap();
        let sites: Vec<String> = (0..200)
            .map(|i| format!("a NOT IN (SELECT b FROM u WHERE b > {i})"))
            .collect();
        let sql = format!("SELECT a FROM t WHERE {}", sites.join(" AND "));
        ANALYSES.with(|a| a.set(0));
        let analyzed = db.analyze(&sql).unwrap();
        assert_eq!(analyzed.simplifications.len(), 200);
        assert!(!analyzed.sql.contains("NOT IN"));
        // One batch, verified by one analysis.
        assert_eq!(ANALYSES.with(|a| a.get()), 1);
    }

    /// A rule renaming the column at each `x` of the query: every candidate
    /// fails the check (the column's name changes).
    fn renaming(ctx: &Ctx<'_>) -> Vec<Candidate> {
        ctx.sql
            .match_indices(" x")
            .map(|(at, _)| Candidate {
                rule: Rule::Distinct,
                edits: vec![Edit {
                    range: at + 1..at + 2,
                    text: "y".into(),
                }],
            })
            .collect()
    }

    #[test]
    fn analyses_are_capped() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql("CREATE TABLE t (a int NOT NULL)").unwrap();
        let columns: Vec<String> = (0..100).map(|i| format!("a AS x{i}")).collect();
        let mut analyzed = db
            .analyze(&format!("SELECT {} FROM t", columns.join(", ")))
            .unwrap();
        let written = analyzed.sql.clone();
        ANALYSES.with(|a| a.set(0));
        run_rules(&db, &mut analyzed, &[], &[(Rule::Distinct, renaming)]);
        assert!(analyzed.simplifications.is_empty());
        assert_eq!(analyzed.sql, written);
        // The batch, then single candidates up to the cap.
        assert_eq!(ANALYSES.with(|a| a.get()), MAX_ANALYSES);
    }

    #[test]
    fn apply_edits_rejects_overlaps() {
        let edit = |range: std::ops::Range<usize>, text: &str| Edit {
            range,
            text: text.into(),
        };
        assert_eq!(
            apply_edits("abcdef", &[edit(4..5, "X"), edit(0..2, "YZ")]).as_deref(),
            Some("YZcdXf")
        );
        assert_eq!(
            apply_edits("abcdef", &[edit(0..3, ""), edit(2..4, "")]),
            None
        );
        assert_eq!(apply_edits("abc", &[edit(2..9, "")]), None);
    }
}
