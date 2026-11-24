//! The `deployed_query_breakage` oracle (the `pg_sanity` feature): what the
//! rule says of the deployed queries, checked by running them on a real
//! PostgreSQL before and after the new migrations.
//!
//! For one case — the deployed schema's migrations, the new ones, the
//! deployed queries and the rule's findings — on a scratch database:
//!
//! 1. the deployed migrations are applied and every table seeded with the
//!    nullability oracle's adversarial rows (`seed.sql`, modes 1 to 5:
//!    NULLs, filled rows, mixed rows, each CHECK literal and its
//!    neighbours, each enum label, each type's extremes);
//! 2. each deployed query is called, as each role it runs as, with sampled
//!    parameter values: a value already stored (so a key matches a row),
//!    NULL where the deployed analysis lets the code send it, each enum
//!    label, the literals of the CHECKs of the columns of the type and
//!    their neighbours, a `varchar(n)` / `char(n)` column's longest value,
//!    a `numeric(p, s)` column's largest, a `bit(n)` column's value, the
//!    type's samples and extremes — one parameter varied at a time from a
//!    baseline, plus a few random combinations. A `copy_in!` target copies
//!    one row (in text format) of the same values;
//! 3. the new migrations are applied over those rows and every call runs
//!    again (`migrated`), then the tables are emptied, seeded again under
//!    the new schema (rows only the new schema allows: a new enum label, a
//!    NULL a dropped NOT NULL admits) and every call runs a third time
//!    (`reseeded`). When the new migrations fail over the seeded rows, the
//!    database is rebuilt with the new schema on empty tables and only
//!    `reseeded` runs.
//!
//! A deployed query breaks when a call that ran before fails after on the
//! migrated rows — or, where those couldn't be kept, on the reseeded ones
//! when it reached a row before (others than the deployed schema's rows
//! may fail it for their own reasons) — other than by data (a unique or
//! exclusion violation, a serialization failure, a timeout, a subquery
//! returning more rows; a foreign key violation too, except on the
//! migrated rows, the same rows as before: there a new foreign key made
//! it), inside a function's or trigger's body included (a body a migration
//! broke, a new trigger raising), when its parameter or result types or
//! result column names change (the deployed code binds and decodes them),
//! or when a result value is one its deployed analysis said it couldn't
//! be: NULL in a column analyzed NOT NULL, a value outside the column's
//! refinement (a value set only when the code's type narrows to it, see
//! [`Decoding`]), a label the old enum doesn't have. A query that breaks
//! with no finding is **missed** (the rule is about safety: that's a bug).
//!
//! A finding claiming the query "no longer runs" (as a role, or for
//! anyone), that an INSERT or COPY leaving out a column "fails" for any
//! row, that an INSERT "fails as role", or that a trigger it fires "fails
//! for every row it writes", is **false** when a call it covers runs after
//! the migrations (and, for a write, writes a row); one claiming a trigger
//! "fails every time it runs", when a call runs at all. Any
//! other finding ("may now be NULL", "a longer value fails", "may reject")
//! says a value may break the query; when no call broke it (nor, for row
//! security, saw or changed rows before and none after), it is
//! **unjustified** — reported, not failed on: the sampled values may just
//! not reach the value that breaks it. A query no call ran before bears
//! nothing out.
//!
//! [`assert_agrees`] runs from `deployed_query_breakage`' own tests (with
//! the feature on, as `scripts/run-lint-oracle.sh` runs them) and panics on
//! a missed break or a false claim; `tests/deployed_oracle.rs` runs a
//! generated corpus. `TYPEDPG_DEPLOYED_ORACLE_VERBOSE=1` prints every
//! case's calls and outcomes.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;

use typedpg_analyzer::pg_sanity_support::postgres::types::{Kind, ToSql, Type as PgType};
use typedpg_analyzer::pg_sanity_support::postgres::{Client, NoTls};
use typedpg_analyzer::pg_sanity_support::{self as support, Param, Raw};
use typedpg_analyzer::{AnalyzedColumn, PgCatalog, Type};
use typedpg_core::quote_identifier;
use typedpg_pg_query::ast::Node as N;

use crate::deployed::{Decoding, DeployedKind, DeployedQuery};
use crate::migrations::MigrationFinding;

/// One case: the deployed schema, the new migrations, the deployed queries
/// and what the rule found.
pub struct Case<'a> {
    /// The deployed schema's migrations, in order.
    pub old: &'a [String],
    /// The new migrations, in order.
    pub new: &'a [String],
    pub queries: &'a [DeployedQuery],
    pub findings: &'a [MigrationFinding],
    /// The deployed schema (`old` applied).
    pub old_catalog: &'a PgCatalog,
    pub decoding: Decoding<'a>,
}

/// What the oracle concluded of a case.
#[derive(Debug, Default)]
pub struct Report {
    /// A deployed query that breaks with no finding (a bug).
    pub missed: Vec<String>,
    /// A finding claiming every call fails, contradicted by a call that ran
    /// (a bug).
    pub false_claims: Vec<String>,
    /// A finding no call bore out (the values sampled may not reach it).
    pub unjustified: Vec<String>,
    /// Findings a broken call bore out (or, for row-level security hiding
    /// rows, a call that saw or changed rows before and none after).
    pub justified: usize,
    /// Findings of a query no call ran before: nothing to bear them out.
    pub unobservable: usize,
    /// Deployed queries checked, and calls made (each phase counted).
    pub queries: usize,
    pub calls: usize,
    /// Why the case couldn't run (the migrations fail on PostgreSQL, no
    /// server), if it couldn't.
    pub inconclusive: Option<String>,
}

/// SQLSTATEs that depend on the rows, not the schema: a unique, exclusion
/// or foreign key violation, a serialization failure or deadlock, a
/// timeout, a lock not available. A new UNIQUE is no break the rule
/// reports (whether a row breaks it depends on the data); a foreign key
/// violation on the migrated rows is ([`FOREIGN_KEY_VIOLATION`]).
const DATA_DEPENDENT: &[&str] = &[
    "23505", "23P01", "23503", "40001", "40P01", "57014", "55P03",
];

/// A foreign key violation: on the rows a call ran over before, only a
/// foreign key the migrations added (or enforced, or whose action they
/// changed) raises it, which the rule reports as "may fail".
const FOREIGN_KEY_VIOLATION: &str = "23503";

/// Marks a failure raised inside a function's or trigger's body (its
/// context names the function), for the report.
const IN_FUNCTION: &str = "[in a function: ";

/// Marks a COPY that failed before its data was sent.
const COPY_UNSTARTED: &str = "[before the COPY's data]";

/// The sampled values of a type, as text: see the module's documentation.
const CANDIDATES_SQL: &str = r#"
CREATE FUNCTION pg_temp.typedpg_base_type(typ oid)
RETURNS oid
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $fn$
    WITH RECURSIVE d(oid, b) AS (
        SELECT oid, typbasetype FROM pg_type WHERE oid = typ
        UNION ALL
        SELECT t.oid, t.typbasetype FROM pg_type t JOIN d ON t.oid = d.b
    )
    SELECT oid FROM d WHERE b = 0
$fn$;

CREATE FUNCTION pg_temp.typedpg_deployed_candidates(typ oid)
RETURNS text[]
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
    base oid := pg_temp.typedpg_base_type(typ);
    tname text := format_type(typ, NULL);
    stored text[] := '{}';
    exprs text[] := '{}';
    out text[] := '{}';
    vals text[];
    e text;
    v text;
    l text;
    a record;
    p int;
    s int;
BEGIN
    IF base IS NULL THEN
        RETURN out;
    END IF;
    FOR a IN SELECT c.oid AS rel, att.attnum, att.attname, att.atttypid, att.atttypmod
             FROM pg_attribute att
             JOIN pg_class c ON c.oid = att.attrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p') AND att.attnum > 0 AND NOT att.attisdropped
               AND n.nspname NOT IN ('pg_catalog', 'information_schema')
               AND n.nspname NOT LIKE 'pg\_%'
               AND pg_temp.typedpg_base_type(att.atttypid) = base
             ORDER BY c.oid, att.attnum
    LOOP
        BEGIN
            EXECUTE format(
                'SELECT coalesce(array_agg(quote_literal(v)), ''{}'') FROM '
                '(SELECT DISTINCT %1$I::text AS v FROM %2$s WHERE %1$I IS NOT NULL ORDER BY 1 LIMIT 3) s',
                a.attname, a.rel::regclass) INTO vals;
            stored := stored || vals;
        EXCEPTION WHEN others THEN
            NULL;
        END;
        FOREACH l IN ARRAY pg_temp.typedpg_check_literals(a.rel, a.attnum, a.atttypid) LOOP
            exprs := exprs || pg_temp.typedpg_near(l, base);
        END LOOP;
        IF base IN ('varchar'::regtype, 'bpchar'::regtype) AND a.atttypmod > 4 THEN
            exprs := exprs || quote_literal(repeat('x', a.atttypmod - 4));
        ELSIF base = 'numeric'::regtype AND a.atttypmod > 4 THEN
            p := ((a.atttypmod - 4) >> 16) & 65535;
            s := (a.atttypmod - 4) & 65535;
            l := CASE WHEN p > s THEN repeat('9', p - s) ELSE '0' END
                 || CASE WHEN s > 0 THEN '.' || repeat('9', s) ELSE '' END;
            exprs := exprs || quote_literal(l) || quote_literal('-' || l)
                     || quote_literal('0.' || repeat('5', s + 1));
        ELSIF base IN ('bit'::regtype, 'varbit'::regtype) AND a.atttypmod > 0 THEN
            exprs := exprs || quote_literal(repeat('1', a.atttypmod));
        END IF;
    END LOOP;
    SELECT exprs || coalesce(array_agg(quote_literal(enumlabel) ORDER BY enumsortorder), '{}')
    INTO exprs FROM pg_enum WHERE enumtypid = base;
    exprs := stored || exprs
             || quote_nullable(pg_temp.typedpg_param_text(typ, 1))
             || quote_nullable(pg_temp.typedpg_param_text(typ, 2))
             || pg_temp.typedpg_extremes(base);
    FOREACH e IN ARRAY exprs LOOP
        CONTINUE WHEN e IS NULL;
        BEGIN
            EXECUTE format('SELECT ((%s)::%s)::text', e, tname) INTO v;
        EXCEPTION WHEN others THEN
            CONTINUE;
        END;
        CONTINUE WHEN v IS NULL OR v = ANY (out);
        out := out || v;
        EXIT WHEN cardinality(out) >= 24;
    END LOOP;
    RETURN out;
END
$fn$;
"#;

/// The most calls one query gets per role and phase.
const MAX_CALLS: usize = 300;

/// Run `case` and panic on a missed break or a false claim.
#[cfg(test)]
pub(crate) fn assert_agrees(case: &Case<'_>) {
    let report = run(case);
    if let Some(why) = &report.inconclusive {
        if verbose() {
            eprintln!("deployed oracle: inconclusive: {why}");
        }
        return;
    }
    if verbose() {
        for text in &report.unjustified {
            eprintln!("deployed oracle: unjustified: {text}");
        }
    }
    let broken: Vec<String> = report
        .missed
        .iter()
        .chain(&report.false_claims)
        .cloned()
        .collect();
    if !broken.is_empty() {
        panic!("{}", broken.join("\n\n"));
    }
}

fn verbose() -> bool {
    std::env::var_os("TYPEDPG_DEPLOYED_ORACLE_VERBOSE").is_some()
}

/// A deployed query as the oracle calls it.
struct Target<'a> {
    query: &'a DeployedQuery,
    /// Where the deployed code has it, as findings name it.
    location: Option<&'a str>,
    /// The text sent: the query with `$n` parameters, or `COPY … FROM
    /// STDIN` in text format.
    sql: String,
    /// What the deployed analysis says of each result column.
    columns: Vec<AnalyzedColumn>,
    /// Whether the deployed code may send NULL, by parameter (a COPY's by
    /// column).
    nullable: Vec<bool>,
    /// For a COPY: `SELECT NULL::<type>, …` over its columns' types.
    describe: Option<String>,
}

/// What preparing a query told: its parameter types and result columns.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shape {
    params: Vec<String>,
    columns: Vec<(String, String)>,
}

/// A call's outcome.
#[derive(Debug, Clone)]
enum Outcome {
    /// It ran: the rows it returned (raw binary values) and the rows it
    /// returned or affected.
    Ran {
        rows: Vec<Vec<Option<Vec<u8>>>>,
        types: Vec<PgType>,
        count: u64,
    },
    /// It failed, with this SQLSTATE and message.
    Failed { code: String, message: String },
}

/// One call: a target, a role and parameter values (text, `None` NULL).
#[derive(Debug, Clone)]
struct Call {
    target: usize,
    role: Option<String>,
    values: Vec<Option<String>>,
}

/// A scratch database, dropped with the value.
struct Scratch {
    admin: String,
    name: String,
    url: String,
    client: Option<Client>,
}

impl Scratch {
    fn create(admin: &str) -> Result<Scratch, String> {
        let name = support::scratch_db_name().replace("pg_sanity", "pg_deployed");
        let mut root = Client::connect(admin, NoTls).map_err(|e| e.to_string())?;
        root.batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .map_err(|e| e.to_string())?;
        let url = support::with_dbname(admin, &name)?;
        let mut scratch = Scratch {
            admin: admin.to_owned(),
            name,
            url,
            client: None,
        };
        scratch.connect()?;
        Ok(scratch)
    }

    /// Connect (again) and install the helpers in the session.
    fn connect(&mut self) -> Result<(), String> {
        let mut client = Client::connect(&self.url, NoTls).map_err(|e| e.to_string())?;
        // Roles are the cluster's: a migration another oracle replays
        // (`ALTER ROLE postgres SET session_replication_role = replica`)
        // reaches this session too, and would keep every trigger from
        // firing.
        client
            .batch_execute("SET session_replication_role = origin")
            .and_then(|()| client.batch_execute(support::SEED_SQL))
            .and_then(|()| client.batch_execute(CANDIDATES_SQL))
            .map_err(|e| format!("installing the helpers: {e}"))?;
        self.client = Some(client);
        Ok(())
    }

    /// The session, connected again if the last call closed it.
    fn client(&mut self) -> &mut Client {
        if self.client.as_ref().is_none_or(Client::is_closed) {
            let _ = self.connect();
        }
        self.client.as_mut().expect("connected")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(self.client.take());
        if let Ok(mut root) = Client::connect(&self.admin, NoTls) {
            let _ = root.batch_execute(&format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            ));
        }
    }
}

fn pg_error(e: &typedpg_analyzer::pg_sanity_support::postgres::Error) -> (String, String) {
    match e.as_db_error() {
        Some(db) => (db.code().code().to_owned(), db.message().to_owned()),
        None => ("(client)".to_owned(), e.to_string()),
    }
}

/// Apply one migration as the runner does: in a transaction unless it
/// opts out. A role that already exists (roles are the cluster's, shared
/// with every other scratch database) is taken as created; dropping one
/// is left out.
fn apply(client: &mut Client, sql: &str) -> Result<(), String> {
    let parsed = typedpg_pg_query::parse(sql).map_err(|e| e.to_string())?;
    let transaction = !sql
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("-- no-transaction");
    if transaction {
        client.batch_execute("BEGIN").map_err(|e| e.to_string())?;
    }
    let fail = |client: &mut Client, e: String| {
        if transaction {
            let _ = client.batch_execute("ROLLBACK");
        }
        Err(e)
    };
    for raw in &parsed.tree.stmts {
        let start = usize::try_from(raw.stmt_location).unwrap_or(0);
        let end = if raw.stmt_len == 0 {
            sql.len()
        } else {
            start + usize::try_from(raw.stmt_len).unwrap_or(0)
        };
        let text = &sql[start..end];
        match raw.stmt.as_ref() {
            Some(N::DropRoleStmt(_)) => continue,
            Some(N::CreateRoleStmt(_)) => {
                if transaction {
                    let _ = client.batch_execute("SAVEPOINT typedpg_role");
                }
                if let Err(e) = client.batch_execute(text) {
                    let (code, message) = pg_error(&e);
                    if code != "42710" {
                        return fail(client, format!("{code}: {message}"));
                    }
                    if transaction {
                        let _ = client.batch_execute("ROLLBACK TO SAVEPOINT typedpg_role");
                    }
                }
            }
            _ => {
                if let Err(e) = client.batch_execute(text) {
                    let (code, message) = pg_error(&e);
                    return fail(client, format!("{code}: {message}\nstatement: {text}"));
                }
            }
        }
    }
    if transaction {
        client.batch_execute("COMMIT").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Seed every table with the adversarial rows (each mode committed: a new
/// enum label can't be used in the transaction that adds it).
fn seed(client: &mut Client) {
    for mode in 1..=5 {
        let _ = client.query(
            "SELECT kind, detail FROM pg_temp.typedpg_seed($1, '{}')",
            &[&mode],
        );
    }
}

/// Empty every user table (for the new schema's own rows).
fn truncate(client: &mut Client) {
    let tables: Vec<String> = client
        .query(
            "SELECT c.oid::regclass::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition
               AND n.nspname NOT IN ('pg_catalog', 'information_schema')
               AND n.nspname NOT LIKE 'pg\\_%'",
            &[],
        )
        .map(|rows| rows.iter().map(|r| r.get(0)).collect())
        .unwrap_or_default();
    if !tables.is_empty() {
        let _ = client.batch_execute(&format!(
            "TRUNCATE {} RESTART IDENTITY CASCADE",
            tables.join(", ")
        ));
    }
}

/// Prepare a target as the superuser: its shape, or why it fails.
fn shape(client: &mut Client, target: &Target<'_>) -> Result<Shape, String> {
    let sql = target.describe.as_deref().unwrap_or(&target.sql);
    let statement = client.prepare(sql).map_err(|e| pg_error(&e).1)?;
    let name = |t: &PgType| format!("{}.{}", t.schema(), t.name());
    Ok(Shape {
        params: statement.params().iter().map(name).collect(),
        columns: statement
            .columns()
            .iter()
            .map(|c| (c.name().to_owned(), name(c.type_())))
            .collect(),
    })
}

/// The parameter (or COPY column) types of a target, as OIDs.
fn input_types(client: &mut Client, target: &Target<'_>) -> Option<Vec<u32>> {
    let statement = match &target.describe {
        Some(describe) => client
            .prepare(describe)
            .ok()?
            .columns()
            .iter()
            .map(|c| c.type_().oid())
            .collect(),
        None => client
            .prepare(&target.sql)
            .ok()?
            .params()
            .iter()
            .map(PgType::oid)
            .collect(),
    };
    Some(statement)
}

fn candidates(client: &mut Client, oid: u32, cache: &mut HashMap<u32, Vec<String>>) -> Vec<String> {
    if let Some(found) = cache.get(&oid) {
        return found.clone();
    }
    let found: Vec<String> = if oid == 0 {
        Vec::new()
    } else {
        client
            .query_one(
                &format!("SELECT pg_temp.typedpg_deployed_candidates({oid}::oid)"),
                &[],
            )
            .ok()
            .and_then(|row| row.try_get::<_, Vec<String>>(0).ok())
            .unwrap_or_default()
    };
    cache.insert(oid, found.clone());
    found
}

/// The values each call of a target binds: a baseline (each parameter's
/// first value), it with one parameter varied, and random combinations.
fn value_sets(options: &[Vec<Option<String>>], seed: u64) -> Vec<Vec<Option<String>>> {
    // Two baselines: each parameter's first value (one already stored, so
    // a key matches a row) and its last (a sample or an extreme, so an
    // INSERT doesn't meet a stored key).
    let first: Vec<Option<String>> = options
        .iter()
        .map(|o| o.first().cloned().flatten())
        .collect();
    let last: Vec<Option<String>> = options
        .iter()
        .map(|o| o.last().cloned().flatten())
        .collect();
    let mut sets = vec![first.clone(), last.clone()];
    for baseline in [&first, &last] {
        for (i, choices) in options.iter().enumerate() {
            for choice in choices {
                let mut set = baseline.clone();
                set[i] = choice.clone();
                sets.push(set);
            }
        }
    }
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    for _ in 0..16 {
        let set: Vec<Option<String>> = options
            .iter()
            .map(|choices| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let n = choices.len().max(1);
                choices
                    .get(usize::try_from(state >> 33).unwrap_or(0) % n)
                    .cloned()
                    .flatten()
            })
            .collect();
        sets.push(set);
    }
    let mut seen = BTreeSet::new();
    sets.retain(|set| seen.insert(set.clone()));
    sets.truncate(MAX_CALLS);
    sets
}

/// A COPY text-format field.
fn copy_field(value: &Option<String>) -> String {
    match value {
        None => "\\N".to_owned(),
        Some(text) => text
            .replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r"),
    }
}

/// Run one call, in a transaction rolled back after.
fn execute(client: &mut Client, target: &Target<'_>, call: &Call) -> Outcome {
    let failed = |e: &typedpg_analyzer::pg_sanity_support::postgres::Error| {
        let (code, mut message) = pg_error(e);
        // Raised inside a function or trigger body: see `IN_FUNCTION`.
        if let Some(context) = e.as_db_error().and_then(|d| d.where_())
            && context.contains("function")
        {
            message.push_str(&format!(" {IN_FUNCTION}{}]", context.replace('\n', " / ")));
        }
        Outcome::Failed { code, message }
    };
    let mut tx = match client.transaction() {
        Ok(tx) => tx,
        Err(e) => return failed(&e),
    };
    if let Err(e) =
        tx.batch_execute("SET LOCAL statement_timeout = '5s'; SET LOCAL lock_timeout = '2s'")
    {
        return failed(&e);
    }
    // A role's own settings (`ALTER ROLE … SET search_path`) apply to
    // the sessions it logs in, which `SET ROLE` doesn't: they are set too —
    // but `session_replication_role`, which another oracle's migrations
    // leave on the cluster's roles (see `Scratch::connect`).
    if let Some(role) = &call.role
        && let Err(e) = tx
            .query(
                "SELECT pg_catalog.set_config(split_part(c, '=', 1), substr(c, strpos(c, '=') + 1), true)
                 FROM pg_catalog.pg_db_role_setting s, unnest(s.setconfig) AS c
                 WHERE s.setrole = (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = $1)
                   AND split_part(c, '=', 1) <> 'session_replication_role'
                   AND s.setdatabase IN (0, (SELECT oid FROM pg_catalog.pg_database
                                             WHERE datname = current_database()))
                 ORDER BY s.setdatabase",
                &[role],
            )
            .and_then(|_| tx.batch_execute(&format!("SET LOCAL ROLE {}", quote_identifier(role))))
    {
        let (_, message) = pg_error(&e);
        return Outcome::Failed {
            code: "(role)".to_owned(),
            message,
        };
    }
    let outcome = if target.describe.is_some() {
        let line = call
            .values
            .iter()
            .map(copy_field)
            .collect::<Vec<_>>()
            .join("\t");
        match tx.copy_in(&target.sql) {
            Ok(mut writer) => match writer.write_all(format!("{line}\n").as_bytes()) {
                Ok(()) => match writer.finish() {
                    Ok(count) => Outcome::Ran {
                        rows: Vec::new(),
                        types: Vec::new(),
                        count,
                    },
                    Err(e) => failed(&e),
                },
                Err(e) => Outcome::Failed {
                    code: "(client)".to_owned(),
                    message: format!("{e} {COPY_UNSTARTED}"),
                },
            },
            Err(e) => match failed(&e) {
                Outcome::Failed { code, message } => Outcome::Failed {
                    code,
                    message: format!("{message} {COPY_UNSTARTED}"),
                },
                ran => ran,
            },
        }
    } else {
        let params: Vec<Param> = call.values.iter().map(|v| Param(v.clone())).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        match tx.prepare(&target.sql) {
            Err(e) => failed(&e),
            Ok(statement) if statement.columns().is_empty() => {
                match tx.execute(&statement, &refs) {
                    Ok(count) => Outcome::Ran {
                        rows: Vec::new(),
                        types: Vec::new(),
                        count,
                    },
                    Err(e) => failed(&e),
                }
            }
            Ok(statement) => {
                let types: Vec<PgType> = statement
                    .columns()
                    .iter()
                    .map(|c| c.type_().clone())
                    .collect();
                match tx
                    .bind(&statement, &refs)
                    .and_then(|portal| tx.query_portal(&portal, 1000))
                {
                    Ok(rows) => {
                        let rows: Vec<Vec<Option<Vec<u8>>>> = rows
                            .iter()
                            .map(|row| {
                                (0..row.len())
                                    .map(|i| {
                                        row.try_get::<_, Raw>(i)
                                            .ok()
                                            .and_then(|Raw(raw)| raw.map(<[u8]>::to_vec))
                                    })
                                    .collect()
                            })
                            .collect();
                        let count = rows.len() as u64;
                        Outcome::Ran { rows, types, count }
                    }
                    Err(e) => failed(&e),
                }
            }
        }
    };
    let _ = tx.rollback();
    outcome
}

/// What a result value breaks of the deployed analysis of its column, as
/// the deployed code decodes it.
fn value_break(
    column: &AnalyzedColumn,
    pg_type: &PgType,
    raw: Option<&[u8]>,
    decoding: &Decoding<'_>,
) -> Option<String> {
    if let Some(found) = support::value_violation(column, raw) {
        let values_only = found.ends_with("(refinement: values)");
        if !values_only || decoding.narrows_values(&column.pg_type) {
            return Some(found);
        }
    }
    // A label the deployed enum doesn't have.
    let labels = enum_labels(&column.pg_type)?;
    let bytes = raw?;
    if !matches!(pg_type.kind(), Kind::Enum(_)) {
        return None;
    }
    let label = std::str::from_utf8(bytes).ok()?;
    if labels.iter().any(|l| l == label) {
        return None;
    }
    let added = BTreeSet::from([label.to_owned()]);
    decoding
        .unknown_labels(&column.pg_type, &added)
        .map(|_| format!("the value is '{label}', a label the deployed enum doesn't have"))
}

fn enum_labels(ty: &Type) -> Option<&[String]> {
    match ty {
        Type::Enum { labels, .. } => Some(labels),
        Type::Domain { base, .. } => enum_labels(base),
        _ => None,
    }
}

/// The calls' outcomes in one phase, by call.
type Phase = Vec<Outcome>;

/// Run `case`: see the module's documentation.
pub fn run(case: &Case<'_>) -> Report {
    let mut report = Report::default();
    let Ok(admin) = std::env::var("POSTGRES_URL") else {
        report.inconclusive = Some("POSTGRES_URL is not set".to_owned());
        return report;
    };
    let mut db = match Scratch::create(&admin) {
        Ok(db) => db,
        Err(e) => {
            report.inconclusive = Some(format!("no scratch database: {e}"));
            return report;
        }
    };
    for (i, migration) in case.old.iter().enumerate() {
        if let Err(e) = apply(db.client(), migration) {
            report.inconclusive = Some(format!("deployed migration {i} fails: {e}"));
            return report;
        }
    }

    // The targets, as the deployed analysis sees them.
    let mut targets: Vec<Target<'_>> = Vec::new();
    for query in case.queries {
        let location = query.locations.first().map(String::as_str);
        match query.kind {
            DeployedKind::Query => {
                let (Ok(view), Ok(analyzed)) = (
                    case.old_catalog.query_view(&query.sql),
                    case.old_catalog.analyze_located(&query.sql),
                ) else {
                    continue;
                };
                if !analyzed.spreads.is_empty() {
                    continue;
                }
                targets.push(Target {
                    query,
                    location,
                    sql: view.sql,
                    columns: analyzed.columns.clone(),
                    nullable: analyzed.params.iter().map(|p| p.nullable).collect(),
                    describe: None,
                });
            }
            DeployedKind::CopyIn => {
                let Ok(copy) = case.old_catalog.analyze_copy_in_located(&query.sql) else {
                    continue;
                };
                targets.push(Target {
                    query,
                    location,
                    sql: copy.copy_sql.replace(" (FORMAT binary)", ""),
                    columns: Vec::new(),
                    nullable: copy.columns.iter().map(|c| c.nullable).collect(),
                    describe: Some(copy.describe_sql.clone()),
                });
            }
        }
    }
    report.queries = targets.len();

    seed(db.client());
    // Each target's shape and calls.
    let mut cache = HashMap::new();
    let mut before_shapes: Vec<Result<Shape, String>> = Vec::new();
    let mut calls: Vec<Call> = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        before_shapes.push(shape(db.client(), target));
        let Some(types) = input_types(db.client(), target) else {
            continue;
        };
        let options: Vec<Vec<Option<String>>> = types
            .iter()
            .enumerate()
            .map(|(i, &oid)| {
                let mut choices: Vec<Option<String>> = candidates(db.client(), oid, &mut cache)
                    .into_iter()
                    .map(Some)
                    .collect();
                if target.nullable.get(i).copied().unwrap_or(true) || choices.is_empty() {
                    choices.push(None);
                }
                choices
            })
            .collect();
        let roles: Vec<Option<String>> = if target.query.roles.is_empty() {
            vec![None]
        } else {
            target.query.roles.iter().cloned().map(Some).collect()
        };
        for values in value_sets(&options, index as u64) {
            for role in &roles {
                calls.push(Call {
                    target: index,
                    role: role.clone(),
                    values: values.clone(),
                });
            }
        }
    }
    let run_all = |db: &mut Scratch| -> Phase {
        calls
            .iter()
            .map(|call| {
                let target = &targets[call.target];
                let outcome = execute(db.client(), target, call);
                // The client loses track of the session after a COPY that
                // fails to start (it closes it, or reads the next reply as
                // the COPY's): a new session for the next call.
                if matches!(&outcome, Outcome::Failed { message, .. }
                    if message.ends_with(COPY_UNSTARTED))
                {
                    db.client = None;
                }
                outcome
            })
            .collect()
    };
    let before = run_all(&mut db);

    // The new migrations over the seeded rows; failing that, on empty
    // tables.
    let mut migrated: Option<Phase> = None;
    let over_rows = case
        .new
        .iter()
        .try_for_each(|migration| apply(db.client(), migration));
    if over_rows.is_ok() {
        migrated = Some(run_all(&mut db));
        truncate(db.client());
    } else {
        drop(db);
        db = match Scratch::create(&admin) {
            Ok(db) => db,
            Err(e) => {
                report.inconclusive = Some(format!("no scratch database: {e}"));
                return report;
            }
        };
        for (i, migration) in case.old.iter().chain(case.new).enumerate() {
            if let Err(e) = apply(db.client(), migration) {
                report.inconclusive = Some(format!("migration {i} fails on empty tables: {e}"));
                return report;
            }
        }
    }
    seed(db.client());
    let reseeded = run_all(&mut db);
    let after_shapes: Vec<Result<Shape, String>> =
        targets.iter().map(|t| shape(db.client(), t)).collect();
    report.calls = calls.len() * (2 + usize::from(migrated.is_some()));

    // What breaks each target.
    let mut breaks: Vec<Vec<String>> = vec![Vec::new(); targets.len()];
    for (index, target) in targets.iter().enumerate() {
        if let (Ok(old), Ok(new)) = (&before_shapes[index], &after_shapes[index]) {
            if old.params != new.params {
                breaks[index].push(format!(
                    "its parameter types change: {:?} to {:?}",
                    old.params, new.params
                ));
            }
            if old.columns != new.columns {
                breaks[index].push(format!(
                    "its result columns change: {:?} to {:?}",
                    old.columns, new.columns
                ));
            }
        }
        // The columns the deployed analysis already gets wrong (the
        // analyzer's own oracle's business), by index.
        let mut unsound: BTreeSet<usize> = BTreeSet::new();
        for (call, outcome) in calls.iter().zip(&before) {
            if call.target != index {
                continue;
            }
            if let Outcome::Ran { rows, types, .. } = outcome {
                for row in rows {
                    for (i, column) in target.columns.iter().enumerate() {
                        if let Some(pg_type) = types.get(i)
                            && value_break(
                                column,
                                pg_type,
                                row.get(i).and_then(|v| v.as_deref()),
                                &case.decoding,
                            )
                            .is_some()
                        {
                            unsound.insert(i);
                        }
                    }
                }
            }
        }
        for (phase, outcomes) in [
            ("migrated", migrated.as_ref()),
            ("reseeded", Some(&reseeded)),
        ] {
            let Some(outcomes) = outcomes else {
                continue;
            };
            for ((call, old), new) in calls.iter().zip(&before).zip(outcomes) {
                if call.target != index {
                    continue;
                }
                let described = describe_call(call);
                match (old, new) {
                    // Among the rows seeded again, which are others than
                    // the deployed schema's, a failure says something of
                    // the schema only where the migrated rows couldn't be
                    // run and the call reached rows before.
                    (Outcome::Ran { count, .. }, Outcome::Failed { code, message })
                        if (!DATA_DEPENDENT.contains(&code.as_str())
                            || code == FOREIGN_KEY_VIOLATION && phase == "migrated")
                            && code != "(role)"
                            && code != "(client)"
                            && (phase == "migrated"
                                || migrated.is_none() && *count > 0 && code != "21000") =>
                    {
                        breaks[index].push(format!(
                            "{described} ran before, fails after ({phase}): {code}: {message}"
                        ));
                    }
                    (_, Outcome::Ran { rows, types, .. }) => {
                        let same_columns = matches!(
                            (&before_shapes[index], &after_shapes[index]),
                            (Ok(a), Ok(b)) if a.columns == b.columns
                        );
                        if !same_columns {
                            continue;
                        }
                        'rows: for row in rows {
                            for (i, column) in target.columns.iter().enumerate() {
                                if unsound.contains(&i) {
                                    continue;
                                }
                                let Some(pg_type) = types.get(i) else {
                                    continue;
                                };
                                let raw = row.get(i).and_then(|v| v.as_deref());
                                if let Some(found) =
                                    value_break(column, pg_type, raw, &case.decoding)
                                {
                                    breaks[index].push(format!(
                                        "{described} returns, in column \"{}\" ({phase}): {found}",
                                        column.name
                                    ));
                                    break 'rows;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // What the findings say of each target.
    for (index, target) in targets.iter().enumerate() {
        let texts: Vec<String> = target
            .location
            .map(|location| {
                let at = format!("the deployed query at {location} (");
                case.findings
                    .iter()
                    .filter(|f| f.finding.message.contains(&at))
                    .flat_map(|f| {
                        let changes = f
                            .finding
                            .message
                            .split_once("): ")
                            .map_or("", |(_, rest)| rest);
                        changes.split("; ").map(str::to_owned).collect::<Vec<_>>()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let header = || case_header(case, target);
        // An allow comment, in the query or a new migration, accepts it.
        let allowed = crate::allowed_rules(&target.query.sql)
            .contains(&crate::Rule::DeployedQueryBreakage)
            || case
                .new
                .iter()
                .any(|m| crate::allowed_rules(m).contains(&crate::Rule::DeployedQueryBreakage));
        if texts.is_empty() {
            if !breaks[index].is_empty() && !allowed {
                report.missed.push(format!(
                    "deployed oracle: deployed_query_breakage misses a break: {}\n{}",
                    breaks[index]
                        .iter()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n"),
                    header()
                ));
            }
            continue;
        }
        let outcomes: Vec<(&Call, &Outcome, &str)> = calls
            .iter()
            .zip(&reseeded)
            .map(|(c, o)| (c, o, "reseeded"))
            .chain(
                migrated
                    .iter()
                    .flat_map(|m| calls.iter().zip(m).map(|(c, o)| (c, o, "migrated"))),
            )
            .filter(|(c, _, _)| c.target == index)
            .collect();
        let sql = target.query.sql.trim_start().to_ascii_uppercase();
        let plain_insert = (sql.starts_with("INSERT") && !sql.contains("ON CONFLICT"))
            || target.describe.is_some();
        for text in &texts {
            // The calls the claim says fail, and what a call that runs must
            // have done to contradict it.
            let claim: Option<(Option<&str>, Contradicted)> =
                if text.starts_with("it no longer runs: ") {
                    Some((None, Contradicted::Ran))
                } else if let Some(rest) = text.strip_prefix("it no longer runs as role \"") {
                    rest.split_once("\":")
                        .map(|(role, _)| (Some(role), Contradicted::Ran))
                } else if text.contains("so inserting any row fails") && plain_insert {
                    Some((None, Contradicted::Wrote))
                } else if let Some((_, rest)) = text.split_once(" fails as role \"")
                    && text.starts_with("its INSERT into ")
                    && plain_insert
                {
                    rest.split_once('"')
                        .map(|(role, _)| (Some(role), Contradicted::Wrote))
                } else if text.contains(", which now fails for every row it writes: ") {
                    Some((None, Contradicted::Wrote))
                } else if text.contains(", which now fails every time it runs: ") {
                    Some((None, Contradicted::Always))
                } else {
                    None
                };
            if let Some((role, contradicted)) = claim {
                let ran = outcomes.iter().find(|(call, outcome, _)| {
                    role.is_none_or(|r| call.role.as_deref() == Some(r))
                        && match outcome {
                            Outcome::Ran { count, .. } => match contradicted {
                                Contradicted::Ran => {
                                    !is_write_without_rows(&sql, *count) || *count > 0
                                }
                                Contradicted::Wrote => *count > 0,
                                Contradicted::Always => true,
                            },
                            Outcome::Failed { .. } => false,
                        }
                });
                if let Some((call, outcome, phase)) = ran {
                    let count = match outcome {
                        Outcome::Ran { count, .. } => *count,
                        Outcome::Failed { .. } => 0,
                    };
                    report.false_claims.push(format!(
                        "deployed oracle: deployed_query_breakage claims \"{text}\", but {} runs \
                         after the migrations ({phase}, {count} rows)\n{}",
                        describe_call(call),
                        header()
                    ));
                    continue;
                }
            }
            // Rows seen or changed before and none after, on the same
            // rows (row-level security hiding them): no failure, but what
            // a finding about row security says.
            let ran_before = calls
                .iter()
                .zip(&before)
                .any(|(c, o)| c.target == index && matches!(o, Outcome::Ran { .. }));
            let emptied = migrated.as_ref().is_some_and(|migrated| {
                calls
                    .iter()
                    .zip(&before)
                    .zip(migrated)
                    .any(|((c, old), new)| {
                        c.target == index
                            && matches!(old, Outcome::Ran { count, .. } if *count > 0)
                            && matches!(new, Outcome::Ran { count: 0, .. })
                    })
            });
            if !ran_before {
                report.unobservable += 1;
            } else if breaks[index].is_empty() && !emptied {
                report
                    .unjustified
                    .push(format!("\"{text}\" — no call broke\n{}", header()));
            } else {
                report.justified += 1;
            }
        }
    }
    if verbose() {
        let mut out = String::new();
        for (index, target) in targets.iter().enumerate() {
            let _ = writeln!(out, "deployed oracle: {}", target.sql);
            for b in &breaks[index] {
                let _ = writeln!(out, "  break: {b}");
            }
            for (i, ((call, old), new)) in calls.iter().zip(&before).zip(&reseeded).enumerate() {
                if call.target == index {
                    let middle = migrated
                        .as_ref()
                        .map_or(String::new(), |m| format!("{} -> ", brief(&m[i])));
                    let _ = writeln!(
                        out,
                        "  {}: {} -> {middle}{}",
                        describe_call(call),
                        brief(old),
                        brief(new)
                    );
                }
            }
        }
        eprint!("{out}");
    }
    report
}

/// What a call that runs after the migrations must have done to
/// contradict a finding claiming it fails.
#[derive(Clone, Copy)]
enum Contradicted {
    /// Run, and returned rows or wrote some (a write changing no row runs
    /// whatever its writes would have broken).
    Ran,
    /// Written a row.
    Wrote,
    /// Run at all.
    Always,
}

/// An UPDATE, DELETE or MERGE that changed no row: it runs whatever its
/// writes would have broken.
fn is_write_without_rows(sql: &str, count: u64) -> bool {
    count == 0
        && ["UPDATE", "DELETE", "MERGE", "INSERT", "WITH"]
            .iter()
            .any(|k| sql.starts_with(k))
}

fn brief(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ran { count, .. } => format!("ran ({count})"),
        Outcome::Failed { code, message } => format!("{code} {message}"),
    }
}

fn describe_call(call: &Call) -> String {
    let values = call
        .values
        .iter()
        .enumerate()
        .map(|(i, v)| match v {
            Some(v) => format!("${} = '{}'", i + 1, v.chars().take(40).collect::<String>()),
            None => format!("${} = NULL", i + 1),
        })
        .collect::<Vec<_>>()
        .join(", ");
    match &call.role {
        Some(role) => format!("the call as \"{role}\" with ({values})"),
        None => format!("the call with ({values})"),
    }
}

fn case_header(case: &Case<'_>, target: &Target<'_>) -> String {
    let findings: Vec<&str> = case
        .findings
        .iter()
        .map(|f| f.finding.message.as_str())
        .collect();
    format!(
        "deployed migrations:\n---\n{}\n---\nnew migrations:\n---\n{}\n---\nquery{}{}:\n---\n{}\n---\nfindings:\n{}",
        case.old.join("\n---\n"),
        case.new.join("\n---\n"),
        if target.describe.is_some() {
            " (copy_in!)"
        } else {
            ""
        },
        if target.query.roles.is_empty() {
            String::new()
        } else {
            format!(" as {}", target.query.roles.join(", "))
        },
        target.query.sql,
        if findings.is_empty() {
            "  (none)".to_owned()
        } else {
            findings
                .iter()
                .map(|f| format!("  {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    )
}
