//! Nullability soundness oracle.
//!
//! PG's Describe says nothing about nullability, so the type comparison in
//! [`super::PgSanityServer::compare_analyze_matches`] can't check typedpg's
//! core promise: a column the analyzer infers NOT NULL (which `sql!` maps
//! to a non-`Option` Rust type) never comes back NULL. This module checks
//! it by running every accepted query over adversarial data and looking at
//! what comes back.
//!
//! Each check runs in one transaction on the query session, rolled back at
//! the end, over four cumulative scenarios:
//!
//! 1. `as-is` — the tables as the migrations left them (usually empty):
//!    aggregates over empty sets, scalar subqueries returning no row, outer
//!    joins against an empty side.
//! 2. `nulls` — plus one row per table with every nullable column NULL
//!    (NOT NULL composite columns get a value whose fields are all NULL,
//!    NOT NULL arrays one holding a NULL element, NOT NULL dates,
//!    timestamps and intervals infinity).
//! 3. `full` — plus one row per table with every column filled. Foreign
//!    keys of both rows point at the referenced table's first row, so the
//!    second parent row has no children: outer joins see matched and
//!    unmatched rows at once.
//! 4. `mixed` — plus, in tables with CHECK constraints, up to 8 rows
//!    mixing NULL and filled nullable columns (one NULL, or one filled,
//!    under either variant), so rows on each side of a constraint tying
//!    columns together (`kind <> 'a' OR a_id IS NOT NULL`) exist.
//!
//! The rows come from `seed.sql`'s `typedpg_seed`, which introspects the
//! scratch database and builds INSERTs that respect NOT NULL, defaults,
//! identity / generated columns, CHECKs (by retrying with filled values)
//! and foreign keys (by retrying in passes); a table it can't seed is
//! skipped and reported. Seeding plans are cached per schema generation
//! (every DDL batch bumps it) and replayed.
//!
//! Parameters bind NULL when the analyzer lets them be NULL, and a sample
//! value of their type (in text format) otherwise; a query with a NOT NULL
//! parameter of a type with no sample isn't executed (NULL there would
//! break the analyzer's premise, not its inference). A column whose
//! PG-side name carries the `!` annotation is the user's own NOT NULL
//! claim, not an inference, and isn't checked.
//!
//! A column's [`crate::Refinement`] is a promise too: one refined finite
//! never comes back an infinite date, timestamp or interval, and one
//! refined to a set of values never comes back another.
//!
//! Only SELECT / INSERT / UPDATE / DELETE / MERGE statements run; queries
//! calling server-administration functions with effects that outlive a
//! rollback (`pg_terminate_backend`, advisory locks, …) don't. Runtime
//! errors (a CHECK the seeded data violates, a cast the sample value
//! fails) only skip that scenario: the oracle reports what it saw, not
//! what it couldn't run. `PG_SANITY_NULLABILITY=0` turns the check off;
//! `TYPEDPG_SOUNDNESS_STATS=<file>` appends per-catalog counters to a file
//! (`scripts/run-pg-sanity.sh` prints their totals).

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;

use bytes::BytesMut;
use postgres::types::{Format, FromSql, IsNull, ToSql, Type as PgType};
use postgres::{Client, Statement, Transaction};

use super::{Divergence, DivergenceKind, NullParam};
use crate::resolve::AnalyzedQuery;
use crate::types::Type;

/// The PL/pgSQL helpers, created in the query session's `pg_temp`.
const SEED_SQL: &str = include_str!("seed.sql");

/// Rows read per scenario: enough to see every seeded combination, bounded
/// so a `generate_series` over millions stays cheap.
const MAX_ROWS: i32 = 1000;

/// Substrings of calls whose effects outlive the rolled-back transaction or
/// reach other sessions; a query mentioning one isn't executed.
const UNSAFE_CALLS: &[&str] = &[
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_switch_wal",
    "pg_promote",
    "pg_sleep",
    "advisory",
    "replication_slot",
    "replication_origin",
    "pg_log_backend",
    "pg_log_standby",
    "pg_stat_reset",
    "pg_stat_clear",
    "lo_import",
    "lo_export",
    "pg_file_",
    "pg_read_",
    "pg_ls_",
    "pg_backup_",
    "pg_wal_replay",
    "pg_import_system_collations",
    "pg_create_restore_point",
    "dblink",
];

/// The cumulative scenarios, in order: name and seeding mode (0 = none).
const SCENARIOS: [(&str, i32); 4] = [("as-is", 0), ("nulls", 1), ("full", 2), ("mixed", 3)];

/// What `typedpg_seed(mode)` did: the INSERTs that succeeded, in order,
/// and the tables it skipped (`table: reason`).
#[derive(Debug, Clone, Default)]
struct SeedPlan {
    inserts: Vec<String>,
    skips: Vec<String>,
}

#[derive(Debug, Default)]
struct Stats {
    /// Queries with a NOT NULL promise that were executed.
    checked: u64,
    /// Scenario executions that returned rows (or ran to completion).
    executions: u64,
    /// Scenario executions that failed at runtime (skipped).
    exec_errors: u64,
    /// Accepted queries with nothing to check (every column nullable).
    no_promise: u64,
    /// Accepted queries of a kind that isn't executed (utility, CALL, …).
    not_executable: u64,
    /// Queries skipped for calling an unsafe function.
    unsafe_calls: u64,
    /// Queries skipped for a NOT NULL parameter of a type with no sample.
    no_param_sample: u64,
    /// Every distinct `table: reason` the seeder skipped.
    seed_skips: BTreeMap<String, u64>,
    /// The runtime errors of `exec_errors`, by SQLSTATE.
    exec_error_codes: BTreeMap<String, u64>,
}

/// Per-server oracle state.
pub(super) struct Soundness {
    enabled: bool,
    installed: bool,
    /// Bumped on every DDL batch: seeding plans and parameter samples are
    /// valid for one schema generation only.
    schema_gen: u64,
    plans: HashMap<(u64, i32), SeedPlan>,
    param_texts: HashMap<(u64, u32), Option<String>>,
    stats: Stats,
}

impl Soundness {
    pub(super) fn new() -> Self {
        Self {
            enabled: std::env::var("PG_SANITY_NULLABILITY").map_or(true, |v| v != "0"),
            installed: false,
            schema_gen: 0,
            plans: HashMap::new(),
            param_texts: HashMap::new(),
            stats: Stats::default(),
        }
    }

    /// The schema (or the migrations' data) may have changed.
    pub(super) fn schema_changed(&mut self) {
        self.schema_gen += 1;
        self.plans.clear();
        self.param_texts.clear();
    }

    /// Execute `stmt` (prepared from `sql`, which the analyzer accepted as
    /// `ours`) over the adversarial scenarios and return a divergence if a
    /// value the analyzer promised NOT NULL comes back NULL.
    pub(super) fn check(
        &mut self,
        client: &mut Client,
        sql: &str,
        ours: &AnalyzedQuery,
        stmt: &Statement,
    ) -> Option<Divergence> {
        if !self.enabled {
            return None;
        }
        // Columns whose PG name ends in `!` carry the user's annotation.
        let checked: Vec<bool> = stmt
            .columns()
            .iter()
            .map(|c| !c.name().ends_with('!'))
            .collect();
        let has_promise = ours.columns.iter().zip(&checked).any(|(c, &on)| {
            on && (!c.nullable
                || has_inner_promise(&c.pg_type)
                || c.refinement != crate::refine::Refinement::NONE)
        });
        if !has_promise {
            self.stats.no_promise += 1;
            return None;
        }
        if !is_executable(sql) {
            self.stats.not_executable += 1;
            return None;
        }
        let lower = sql.to_ascii_lowercase();
        if UNSAFE_CALLS.iter().any(|f| lower.contains(f)) {
            self.stats.unsafe_calls += 1;
            return None;
        }
        self.install(client);

        // Parameter values: NULL where the analyzer allows it, a sample of
        // the type otherwise. Without a sample for a NOT NULL parameter
        // there is nothing the caller could pass to check against.
        let nullable_params: Vec<bool> = ours
            .params
            .iter()
            .map(|p| p.nullable)
            .chain(
                ours.spreads
                    .iter()
                    .flat_map(|s| s.fields.iter().map(|f| f.nullable)),
            )
            .collect();
        let mut values: Vec<Option<String>> = Vec::new();
        for (i, ty) in stmt.params().iter().enumerate() {
            let nullable = nullable_params.get(i).copied().unwrap_or(true);
            if nullable || ty.oid() == 0 {
                values.push(None);
                continue;
            }
            let Some(text) = self.param_text(client, ty.oid()) else {
                self.stats.no_param_sample += 1;
                return None;
            };
            values.push(Some(text));
        }
        self.stats.checked += 1;
        let params: Vec<Param> = values.iter().map(|v| Param(v.clone())).collect();
        let param_refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();

        let mut tx = client
            .transaction()
            .unwrap_or_else(|e| panic!("pg_sanity: BEGIN failed in the nullability check: {e}"));
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '1s'")
            .unwrap_or_else(|e| panic!("pg_sanity: SET LOCAL failed: {e}"));

        let mut seeded: Vec<(&str, SeedPlan)> = Vec::new();
        for (scenario, mode) in SCENARIOS {
            if mode > 0 {
                let plan = self.seed(&mut tx, mode);
                for skip in &plan.skips {
                    *self.stats.seed_skips.entry(skip.clone()).or_default() += 1;
                }
                // Nothing added (no table has mixed rows): the data is the
                // previous scenario's, already checked.
                let unchanged = mode == 3 && plan.inserts.is_empty();
                seeded.push((scenario, plan));
                if unchanged {
                    continue;
                }
            }
            let rows = {
                let mut sp = tx
                    .transaction()
                    .unwrap_or_else(|e| panic!("pg_sanity: SAVEPOINT failed: {e}"));
                let rows = sp
                    .bind(stmt, &param_refs)
                    .and_then(|portal| sp.query_portal(&portal, MAX_ROWS));
                // Dropping `sp` rolls the statement's own effects back, so
                // the next scenario starts from the seeded rows alone.
                rows
            };
            let rows = match rows {
                Ok(rows) => rows,
                Err(e) => {
                    self.stats.exec_errors += 1;
                    let code = e.code().map_or("(no SQLSTATE)", |c| c.code());
                    *self
                        .stats
                        .exec_error_codes
                        .entry(code.to_owned())
                        .or_default() += 1;
                    continue;
                }
            };
            self.stats.executions += 1;
            for (r, row) in rows.iter().enumerate() {
                for (i, col) in ours.columns.iter().enumerate() {
                    if !checked.get(i).copied().unwrap_or(false) {
                        continue;
                    }
                    let Ok(Raw(raw)) = row.try_get::<_, Raw>(i) else {
                        continue;
                    };
                    let problem = match raw {
                        None if !col.nullable => Some("the value is NULL".to_string()),
                        None => None,
                        Some(bytes) => inner_violation(&col.pg_type, bytes)
                            .or_else(|| refinement_violation(col, bytes)),
                    };
                    if let Some(problem) = problem {
                        let message = render(sql, ours, i, r, &problem, scenario, &values, &seeded);
                        drop(tx);
                        return Some(Divergence {
                            kind: DivergenceKind::Nullability,
                            message,
                        });
                    }
                }
            }
        }
        drop(tx);
        None
    }

    /// Create the seeding helpers in the query session (once).
    fn install(&mut self, client: &mut Client) {
        if !self.installed {
            client
                .batch_execute(SEED_SQL)
                .unwrap_or_else(|e| panic!("pg_sanity: installing seed.sql failed: {e:?}"));
            self.installed = true;
        }
    }

    /// Seed one row per table for `mode`, replaying this generation's plan
    /// when there is one.
    fn seed(&mut self, tx: &mut Transaction<'_>, mode: i32) -> SeedPlan {
        let key = (self.schema_gen, mode);
        if let Some(plan) = self.plans.get(&key) {
            if plan.inserts.is_empty() {
                return plan.clone();
            }
            let mut sp = tx
                .transaction()
                .unwrap_or_else(|e| panic!("pg_sanity: SAVEPOINT failed: {e}"));
            if sp.batch_execute(&plan.inserts.join(";\n")).is_ok() {
                sp.commit()
                    .unwrap_or_else(|e| panic!("pg_sanity: RELEASE SAVEPOINT failed: {e}"));
                return plan.clone();
            }
            // The replay hit something new (a sequence past a CHECK's
            // range…): rebuild the plan.
        }
        let mut sp = tx
            .transaction()
            .unwrap_or_else(|e| panic!("pg_sanity: SAVEPOINT failed: {e}"));
        let plan = match sp.query(
            "SELECT kind, detail FROM pg_temp.typedpg_seed($1)",
            &[&mode],
        ) {
            Ok(rows) => {
                let mut plan = SeedPlan::default();
                for row in rows {
                    let kind: String = row.get(0);
                    let detail: String = row.get(1);
                    if kind == "insert" {
                        plan.inserts.push(detail);
                    } else {
                        plan.skips.push(detail);
                    }
                }
                sp.commit()
                    .unwrap_or_else(|e| panic!("pg_sanity: RELEASE SAVEPOINT failed: {e}"));
                plan
            }
            Err(e) => SeedPlan {
                inserts: Vec::new(),
                skips: vec![format!(
                    "(every table): typedpg_seed failed: {}",
                    super::render_pg_error(&e)
                )],
            },
        };
        self.plans.insert(key, plan.clone());
        plan
    }

    /// The text form of a sample value of type `oid`, if it has one.
    fn param_text(&mut self, client: &mut Client, oid: u32) -> Option<String> {
        let key = (self.schema_gen, oid);
        if let Some(v) = self.param_texts.get(&key) {
            return v.clone();
        }
        let v = client
            .query_one(
                &format!("SELECT pg_temp.typedpg_param_text({oid}::pg_catalog.oid)"),
                &[],
            )
            .ok()
            .and_then(|row| row.get::<_, Option<String>>(0));
        self.param_texts.insert(key, v.clone());
        v
    }

    /// Append this server's counters to `$TYPEDPG_SOUNDNESS_STATS`, one
    /// tab-separated line per counter, for `scripts/run-pg-sanity.sh`.
    pub(super) fn write_stats(&self) {
        let Ok(path) = std::env::var("TYPEDPG_SOUNDNESS_STATS") else {
            return;
        };
        let s = &self.stats;
        let mut out = String::new();
        for (name, n) in [
            ("checked", s.checked),
            ("executions", s.executions),
            ("exec_errors", s.exec_errors),
            ("no_promise", s.no_promise),
            ("not_executable", s.not_executable),
            ("unsafe_calls", s.unsafe_calls),
            ("no_param_sample", s.no_param_sample),
        ] {
            out.push_str(&format!("count\t{name}\t{n}\n"));
        }
        for (code, n) in &s.exec_error_codes {
            out.push_str(&format!("error\t{code}\t{n}\n"));
        }
        for skip in s.seed_skips.keys() {
            out.push_str("skip\t");
            out.push_str(&skip.replace(['\n', '\t'], " "));
            out.push('\n');
        }
        // One `write` on an O_APPEND file: lines from concurrent test
        // processes don't interleave.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(out.as_bytes());
        }
    }
}

/// Whether `ty` promises something about the inside of a non-NULL value:
/// a non-NULL array element or record field.
fn has_inner_promise(ty: &Type) -> bool {
    match ty {
        Type::Domain { base, .. } => has_inner_promise(base),
        Type::Array {
            element,
            element_nullable,
        } => *element_nullable == Some(false) || has_inner_promise(element),
        Type::Composite { fields, .. } | Type::AnonymousRecord { fields } => fields
            .iter()
            .any(|f| !f.nullable || has_inner_promise(&f.ty)),
        Type::Basic { .. } | Type::Enum { .. } | Type::Range { .. } => false,
    }
}

/// Whether `sql` is one plannable statement the oracle may run:
/// SELECT / VALUES / INSERT / UPDATE / DELETE / MERGE (with or without
/// WITH).
fn is_executable(sql: &str) -> bool {
    use typedpg_pg_query::protobuf::node::Node;
    let Ok(parsed) = typedpg_pg_query::parse(sql) else {
        return false;
    };
    let [raw] = parsed.protobuf.stmts.as_slice() else {
        return false;
    };
    matches!(
        raw.stmt.as_ref().and_then(|n| n.node.as_ref()),
        Some(
            Node::SelectStmt(_)
                | Node::InsertStmt(_)
                | Node::UpdateStmt(_)
                | Node::DeleteStmt(_)
                | Node::MergeStmt(_)
        )
    )
}

/// What the binary value `bytes` of column `col` isn't of what its
/// refinement says it is.
fn refinement_violation(col: &crate::AnalyzedColumn, bytes: &[u8]) -> Option<String> {
    let mut ty = &col.pg_type;
    while let Type::Domain { base, .. } = ty {
        ty = base;
    }
    if let Some(values) = &col.refinement.values {
        // `int2send` / … big-endian, `boolsend` a byte, `textsend` /
        // `enum_send` the text.
        let printed = match ty {
            Type::Enum { .. } => std::str::from_utf8(bytes).ok().map(str::to_owned),
            Type::Basic { schema, name, .. } if schema == "pg_catalog" => match name.as_str() {
                "int2" => Some(i16::from_be_bytes(bytes.try_into().ok()?).to_string()),
                "int4" => Some(i32::from_be_bytes(bytes.try_into().ok()?).to_string()),
                "int8" => Some(i64::from_be_bytes(bytes.try_into().ok()?).to_string()),
                "bool" => Some((bytes.first()? != &0).to_string()),
                "text" | "varchar" => std::str::from_utf8(bytes).ok().map(str::to_owned),
                _ => None,
            },
            _ => None,
        };
        if let Some(v) = printed
            && !values.contains(&v)
        {
            return Some(format!(
                "the value is {v:?}, not one of {values:?} (refinement: values)"
            ));
        }
    }
    let Type::Basic { schema, name, .. } = ty else {
        return None;
    };
    if !col.refinement.finite || schema != "pg_catalog" {
        return None;
    }
    // `date_send` / `timestamp_send` / `interval_send`: the infinities are
    // the extreme values (DATEVAL_NOBEGIN / DT_NOEND, every field of an
    // interval at once).
    let infinite = match name.as_str() {
        "date" => {
            let v = read_i32(bytes, 0)?;
            v == i32::MIN || v == i32::MAX
        }
        "timestamp" | "timestamptz" => {
            let v = i64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
            v == i64::MIN || v == i64::MAX
        }
        "interval" => {
            let time = i64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
            let (day, month) = (read_i32(bytes, 8)?, read_i32(bytes, 12)?);
            (time == i64::MIN && day == i32::MIN && month == i32::MIN)
                || (time == i64::MAX && day == i32::MAX && month == i32::MAX)
        }
        _ => false,
    };
    infinite.then(|| "the value is infinite (refinement: finite)".to_string())
}

/// Find a NULL inside the binary value `bytes` of type `ty` where `ty`
/// promises none, as a path from the value (`element [2]`, `field "a"`).
fn inner_violation(ty: &Type, bytes: &[u8]) -> Option<String> {
    match ty {
        Type::Domain { base, .. } => inner_violation(base, bytes),
        Type::Array {
            element,
            element_nullable,
        } => {
            for (k, elem) in array_elements(bytes)?.into_iter().enumerate() {
                let found = match elem {
                    None if *element_nullable == Some(false) => {
                        Some("is NULL (element_nullable = Some(false))".to_string())
                    }
                    None => None,
                    Some(b) => inner_violation(element, b),
                };
                if let Some(found) = found {
                    return Some(format!("element [{}] {found}", k + 1));
                }
            }
            None
        }
        Type::Composite { fields, .. } | Type::AnonymousRecord { fields } => {
            let values = record_fields(bytes)?;
            if values.len() != fields.len() {
                return None;
            }
            for (field, value) in fields.iter().zip(values) {
                let found = match value {
                    None if !field.nullable => Some("is NULL (nullable = false)".to_string()),
                    None => None,
                    Some(b) => inner_violation(&field.ty, b),
                };
                if let Some(found) = found {
                    return Some(format!("field \"{}\" {found}", field.name));
                }
            }
            None
        }
        Type::Basic { .. } | Type::Enum { .. } | Type::Range { .. } => None,
    }
}

fn read_i32(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Read a length-prefixed datum at `*at` (length -1 = NULL).
fn read_datum<'a>(bytes: &'a [u8], at: &mut usize) -> Option<Option<&'a [u8]>> {
    let len = read_i32(bytes, *at)?;
    *at += 4;
    if len < 0 {
        return Some(None);
    }
    let len = usize::try_from(len).ok()?;
    let value = bytes.get(*at..*at + len)?;
    *at += len;
    Some(Some(value))
}

/// The elements of a binary array (`array_send`): ndim, has-null flag,
/// element type, a (length, lower bound) pair per dimension, then the
/// elements in row-major order.
fn array_elements(bytes: &[u8]) -> Option<Vec<Option<&[u8]>>> {
    let ndim = usize::try_from(read_i32(bytes, 0)?).ok()?;
    let mut count: usize = if ndim == 0 { 0 } else { 1 };
    for d in 0..ndim {
        count = count.checked_mul(usize::try_from(read_i32(bytes, 12 + d * 8)?).ok()?)?;
    }
    let mut at = 12 + ndim * 8;
    (0..count).map(|_| read_datum(bytes, &mut at)).collect()
}

/// The fields of a binary record (`record_send`): a field count, then each
/// field's type OID and datum.
fn record_fields(bytes: &[u8]) -> Option<Vec<Option<&[u8]>>> {
    let n = usize::try_from(read_i32(bytes, 0)?).ok()?;
    let mut at = 4;
    (0..n)
        .map(|_| {
            at += 4; // field type OID
            read_datum(bytes, &mut at)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render(
    sql: &str,
    ours: &AnalyzedQuery,
    col: usize,
    row: usize,
    problem: &str,
    scenario: &str,
    values: &[Option<String>],
    seeded: &[(&str, SeedPlan)],
) -> String {
    let c = &ours.columns[col];
    let params = if values.is_empty() {
        "(none)".to_string()
    } else {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| match v {
                Some(v) => format!("${} = '{v}'", i + 1),
                None => format!("${} = NULL", i + 1),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut seeding = String::new();
    if seeded.is_empty() {
        seeding.push_str("  (none: the tables as the migrations left them)\n");
    }
    for (name, plan) in seeded {
        seeding.push_str(&format!("  [{name}]\n"));
        for insert in &plan.inserts {
            seeding.push_str(&format!("    {insert};\n"));
        }
        for skip in &plan.skips {
            seeding.push_str(&format!("    skipped {skip}\n"));
        }
    }
    format!(
        "pg_sanity: nullability unsound: column {col} '{}' ({}) inferred {}, but in a result row \
         {problem}.\n\
         SQL:\n---\n{sql}\n---\n\
         scenario: {scenario} (row {})\nparams: {params}\nseeding:\n{seeding}",
        c.name,
        super::qualified_type_name_for_compare(&c.pg_type),
        if c.nullable { "nullable" } else { "NOT NULL" },
        row + 1,
    )
}

/// A result value as raw binary bytes (`None` for NULL), whatever its type.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(
        _ty: &PgType,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_ty: &PgType) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_ty: &PgType) -> bool {
        true
    }
}

/// A parameter bound in text format (`Some`), or NULL.
#[derive(Debug)]
struct Param(Option<String>);

impl ToSql for Param {
    fn to_sql(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
            None => NullParam.to_sql(ty, out),
        }
    }

    fn accepts(_ty: &PgType) -> bool {
        true
    }

    fn encode_format(&self, _ty: &PgType) -> Format {
        Format::Text
    }

    fn to_sql_checked(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.to_sql(ty, out)
    }
}
