//! The migration oracle's corpus run: lints a corpus of migration sets,
//! each replayed on a real PostgreSQL by the migration oracle
//! (`src/migration_oracle.rs`), which checks every migration rule's claim
//! and silence against the locks, rewrites and scans PostgreSQL makes.
//!
//! The corpus:
//!
//! - the migration examples of `docs/lint/*.md` (each `-- migrations/…`
//!   marker starts a file; a later block on the page replaces the files it
//!   names);
//! - the successful migrations of every catalog the analyzer's test suite
//!   builds, recorded by its `record_corpus` feature into the directory
//!   `$LINT_ORACLE_CORPUS` names (`scripts/run-migration-oracle.sh` records
//!   them first) — as recorded, and with every statement of a file in a
//!   file of its own, so each acts on tables that already exist;
//! - generated migrations: every `ALTER TABLE` subcommand the generator
//!   knows, over each column type and a plain, a partitioned, an
//!   inheritance parent and a just-created table, and the other statements
//!   that lock, rewrite or scan tables (indexes, partitions, triggers,
//!   policies, rules, domains, `VACUUM FULL`, `REINDEX`, …), each with and
//!   without `lock_timeout`, in transactional and `-- no-transaction`
//!   migrations.
//!
//! Findings go to `$MIGRATION_ORACLE_OUT` (default
//! `target/migration-oracle-findings`), one file per distinct finding; the
//! test fails when there is any. `$MIGRATION_ORACLE_LIMIT` caps the
//! recorded sets and `$MIGRATION_ORACLE_FILTER` keeps the sets whose origin
//! contains it (for a quick run).
#![cfg(feature = "pg_sanity")]

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use typedpg_lint::{LintOptions, MigrationFile, MigrationLint, lint_migrations};

/// One set of migrations.
struct Case {
    origin: String,
    files: Vec<String>,
    use_transaction: bool,
}

#[derive(Default)]
struct Totals {
    cases: u64,
    rejected: u64,
    statements: u64,
    panics: BTreeMap<String, String>,
    broken: BTreeMap<String, String>,
    stats: BTreeMap<(String, String), u64>,
}

#[test]
#[ignore = "needs a PostgreSQL (scripts/run-migration-oracle.sh)"]
fn migration_oracle_corpus() {
    let mut cases = docs_cases();
    let docs = cases.len();
    if let Some(dir) = std::env::var_os("LINT_ORACLE_CORPUS") {
        cases.extend(recorded_cases(std::path::Path::new(&dir)));
    }
    let recorded = cases.len() - docs;
    cases.extend(generated_cases());
    let generated = cases.len() - docs - recorded;
    if let Ok(filter) = std::env::var("MIGRATION_ORACLE_FILTER") {
        cases.retain(|case| case.origin.contains(&filter));
    }
    eprintln!(
        "migration oracle: {} migration sets ({docs} from the docs, {recorded} recorded, \
         {generated} generated)",
        cases.len()
    );

    let threads: usize = std::env::var("LINT_ORACLE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let totals = Mutex::new(Totals::default());
    let next = Mutex::new(0usize);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = {
                        let mut n = next.lock().unwrap();
                        let i = *n;
                        *n += 1;
                        i
                    };
                    let Some(case) = cases.get(index) else {
                        break;
                    };
                    run_case(case, &totals);
                    if index % 500 == 0 {
                        eprintln!("migration oracle: {index}/{} sets", cases.len());
                    }
                }
            });
        }
    });
    let totals = totals.into_inner().unwrap();

    let out_dir = std::env::var("MIGRATION_ORACLE_OUT").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/migration-oracle-findings"
        )
        .to_owned()
    });
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    for (i, (key, message)) in totals.broken.iter().enumerate() {
        std::fs::write(
            format!("{out_dir}/finding-{i:03}.txt"),
            format!("{key}\n\n{message}\n"),
        )
        .unwrap();
    }
    for (i, (key, message)) in totals.panics.iter().enumerate() {
        std::fs::write(
            format!("{out_dir}/panic-{i:03}.txt"),
            format!("{key}\n\n{message}\n"),
        )
        .unwrap();
    }
    eprintln!(
        "migration oracle: {} sets linted ({} the analyzer rejects), {} statements, {} distinct \
         broken claims, {} distinct panics; written to {out_dir}",
        totals.cases,
        totals.rejected,
        totals.statements,
        totals.broken.len(),
        totals.panics.len()
    );
    let mut by_rule: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for ((rule, what), n) in &totals.stats {
        by_rule.entry(rule).or_default().push(format!("{what}={n}"));
    }
    for (rule, counts) in by_rule {
        eprintln!("  {rule:<32} {}", counts.join("  "));
    }
    for key in totals.broken.keys() {
        eprintln!("  BROKEN {key}");
    }
    assert!(
        totals.broken.is_empty() && totals.panics.is_empty(),
        "the migration oracle found {} broken claims and {} panics (see {out_dir})",
        totals.broken.len(),
        totals.panics.len()
    );
}

fn run_case(case: &Case, totals: &Mutex<Totals>) {
    let files: Vec<MigrationFile> = case
        .files
        .iter()
        .enumerate()
        .map(|(i, sql)| MigrationFile {
            path: format!("{:03}.sql", i + 1),
            sql: sql.clone(),
            lint: MigrationLint::All,
        })
        .collect();
    let statements: u64 = case
        .files
        .iter()
        .filter_map(|sql| typedpg_pg_query::parse(sql).ok())
        .map(|parsed| parsed.tree.stmts.len() as u64)
        .sum();
    // The opinion rules (off by default) are checked too.
    let mut config = typedpg_core::config::LintConfig::default();
    for rule in [
        "prefer_timestamptz",
        "prefer_identity",
        "prefer_text",
        "prefer_bigint_pk",
        "prefer_jsonb",
    ] {
        config
            .levels
            .insert(rule.to_owned(), typedpg_core::config::LintLevel::Info);
    }
    let options = LintOptions::from_config(&config);
    let run = catch_unwind(AssertUnwindSafe(|| {
        typedpg_lint::oracle::collect(|| {
            lint_migrations(&files, case.use_transaction, &options).is_ok()
        })
    }));
    let stats = typedpg_lint::oracle::take_stats();
    let mut t = totals.lock().unwrap();
    for (key, n) in stats {
        *t.stats.entry(key).or_default() += n;
    }
    match run {
        Ok((ok, broken)) => {
            t.cases += 1;
            t.statements += statements;
            if !ok {
                t.rejected += 1;
            }
            for message in broken {
                let key = first_line(&message).to_owned();
                t.broken
                    .entry(format!("{key} [{}]", case.origin))
                    .or_insert(message);
            }
        }
        Err(panic) => {
            let message = panic_message(&panic);
            let mut sql = String::new();
            for (i, file) in case.files.iter().enumerate() {
                sql.push_str(&format!("-- {:03}.sql\n{file}\n", i + 1));
            }
            t.panics.insert(
                format!("{}: {}", case.origin, first_line(&message)),
                format!("{message}\nmigrations:\n{sql}"),
            );
        }
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "(no message)".to_owned())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

// ── docs/lint/*.md ─────────────────────────────────────────────────────────

/// Each page's `sql` blocks with `-- migrations/<file>` markers: the first
/// such block's files, then each later block's files over them.
fn docs_cases() -> Vec<Case> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/lint");
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    entries.sort();
    let mut out = Vec::new();
    for path in entries {
        let text = std::fs::read_to_string(&path).unwrap();
        let page = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        for (i, block) in text.split("```sql\n").skip(1).enumerate() {
            let Some((block, _)) = block.split_once("```") else {
                continue;
            };
            let mut current: Option<String> = None;
            let mut found = BTreeMap::new();
            for line in block.lines() {
                if let Some(name) = line.trim().strip_prefix("-- migrations/") {
                    current = Some(name.trim().to_owned());
                    found.insert(name.trim().to_owned(), String::new());
                    continue;
                }
                if let Some(name) = &current {
                    let sql = found.get_mut(name).unwrap();
                    sql.push_str(line);
                    sql.push('\n');
                }
            }
            if found.is_empty() {
                continue;
            }
            files.extend(found);
            for use_transaction in [true, false] {
                out.push(Case {
                    origin: format!("docs/lint/{page} block {}", i + 1),
                    files: files.values().cloned().collect(),
                    use_transaction,
                });
            }
        }
    }
    out
}

// ── the recorded analyzer corpus ───────────────────────────────────────────

fn recorded_cases(dir: &std::path::Path) -> Vec<Case> {
    let mut sets: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if value["migrations"].as_str().is_none() {
                continue;
            }
            let Some(ddl) = value["ddl"].as_array() else {
                continue;
            };
            sets.insert(
                ddl.iter()
                    .filter_map(|d| d.as_str().map(str::to_owned))
                    .collect(),
            );
        }
    }
    // A set that another one extends is replayed as part of it.
    let all: Vec<Vec<String>> = sets.into_iter().collect();
    let mut maximal = Vec::new();
    for (i, set) in all.iter().enumerate() {
        let extended = all
            .iter()
            .skip(i + 1)
            .take_while(|other| other.starts_with(set))
            .any(|other| other.len() > set.len());
        if !extended {
            maximal.push(set.clone());
        }
    }
    if let Some(limit) = std::env::var("MIGRATION_ORACLE_LIMIT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        maximal.truncate(limit);
    }
    let mut out = Vec::new();
    for (i, set) in maximal.into_iter().enumerate() {
        if let Some(split) = split_statements(&set) {
            out.push(Case {
                origin: format!("recorded set #{i}, one statement per file"),
                files: split,
                use_transaction: true,
            });
        }
        out.push(Case {
            origin: format!("recorded set #{i}"),
            files: set,
            use_transaction: true,
        });
    }
    out
}

/// The files with every statement in a file of its own, when that changes
/// anything: a file that controls transactions or session settings, or
/// runs outside a transaction, stays whole.
fn split_statements(files: &[String]) -> Option<Vec<String>> {
    use typedpg_pg_query::ast::Node as N;
    let mut out = Vec::new();
    let mut changed = false;
    for sql in files {
        let parsed = typedpg_pg_query::parse(sql).ok()?;
        let whole = typedpg_core::migration_marker::has_no_transaction_marker(sql)
            || parsed.tree.stmts.len() < 2
            || parsed.tree.stmts.iter().any(|raw| {
                matches!(
                    raw.stmt.as_ref(),
                    Some(
                        N::TransactionStmt(_)
                            | N::VariableSetStmt(_)
                            | N::DoStmt(_)
                            | N::CopyStmt(_)
                            | N::CallStmt(_)
                    ) | None
                )
            });
        if whole {
            out.push(sql.clone());
            continue;
        }
        changed = true;
        for raw in &parsed.tree.stmts {
            let start = raw.stmt_location as usize;
            let end = if raw.stmt_len == 0 {
                sql.len()
            } else {
                start + raw.stmt_len as usize
            };
            out.push(format!("{};\n", sql[start..end].trim()));
        }
    }
    changed.then_some(out)
}

// ── the generator ──────────────────────────────────────────────────────────

const BASE: &str = r#"
CREATE SCHEMA other;
CREATE TYPE mood AS ENUM ('a', 'b');
CREATE TYPE pair AS (x integer, y integer);
CREATE DOMAIN posint AS integer CHECK (VALUE > 0);
CREATE DOMAIN plain_int AS integer;
CREATE TABLE ref (id integer PRIMARY KEY, code text UNIQUE, big bigint UNIQUE);
CREATE TABLE t (
    id integer PRIMARY KEY,
    i integer,
    b bigint,
    s smallint,
    n numeric(10,2),
    f float8,
    v varchar(10),
    c char(5),
    tx text,
    ts timestamp,
    tz timestamptz,
    j json,
    jb jsonb,
    m mood,
    d date,
    p plain_int,
    ok boolean,
    ref_id integer REFERENCES ref (id),
    nn integer NOT NULL DEFAULT 0,
    chk integer CHECK (chk > 0),
    cn integer,
    cnv integer,
    g integer GENERATED ALWAYS AS (i * 2) STORED,
    gv integer GENERATED ALWAYS AS (i * 2) VIRTUAL,
    CONSTRAINT t_cn_nn CHECK (cn IS NOT NULL)
);
ALTER TABLE t ADD CONSTRAINT t_cnv_nn CHECK (cnv IS NOT NULL) NOT VALID;
ALTER TABLE t ADD CONSTRAINT t_s_check CHECK (s > 0) NOT VALID;
ALTER TABLE t ADD CONSTRAINT t_i_fk FOREIGN KEY (i) REFERENCES ref (id) NOT VALID;
CREATE INDEX t_i_idx ON t (i);
CREATE INDEX t_v_idx ON t (v);
CREATE INDEX t_tx_idx ON t (tx);
CREATE UNIQUE INDEX t_b_key ON t (b);
CREATE UNIQUE INDEX t_nn_key ON t (nn);
CREATE TABLE parted (id integer NOT NULL, k integer NOT NULL, v varchar(10), tx text, i integer)
    PARTITION BY RANGE (k);
CREATE TABLE parted_1 PARTITION OF parted FOR VALUES FROM (0) TO (10);
CREATE TABLE parted_2 PARTITION OF parted FOR VALUES FROM (10) TO (20);
CREATE TABLE parted_d PARTITION OF parted DEFAULT;
CREATE INDEX parted_v_idx ON parted (v);
CREATE TABLE parted_nd (id integer NOT NULL, k integer NOT NULL, v varchar(10), tx text, i integer)
    PARTITION BY LIST (k);
CREATE TABLE parted_nd_1 PARTITION OF parted_nd FOR VALUES IN (1);
CREATE TABLE loose (id integer NOT NULL, k integer NOT NULL, v varchar(10), tx text, i integer);
CREATE TABLE loose_chk (id integer NOT NULL, k integer NOT NULL, v varchar(10), tx text, i integer,
    CHECK (k >= 30 AND k < 40));
CREATE TABLE loose_in (id integer NOT NULL, k integer NOT NULL, v varchar(10), tx text, i integer,
    CHECK (k IN (5, 6)));
CREATE TABLE inh_parent (id integer NOT NULL, v varchar(10), tx text, i integer);
CREATE TABLE inh_child (extra integer) INHERITS (inh_parent);
CREATE TABLE inh_other (id integer NOT NULL, v varchar(10), tx text, i integer);
CREATE TABLE typed_t OF pair;
CREATE TABLE fits_pair (x integer, y integer);
CREATE TABLE other.o (id integer PRIMARY KEY, i integer);
CREATE MATERIALIZED VIEW mv AS SELECT id, i FROM t;
CREATE UNIQUE INDEX mv_id ON mv (id);
CREATE VIEW vw AS SELECT id, i FROM t;
CREATE TABLE fk_child (id integer PRIMARY KEY, t_id integer REFERENCES t (id));
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE TRIGGER t_trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION trg();
CREATE POLICY t_pol ON t USING (true);
CREATE RULE t_rule AS ON DELETE TO fk_child DO NOTHING;
CREATE SEQUENCE seq;
CREATE STATISTICS t_stats ON i, b FROM t;
"#;

/// A table the generator alters, and its columns.
struct Target {
    name: &'static str,
    columns: &'static [&'static str],
}

const TARGETS: [Target; 4] = [
    Target {
        name: "t",
        columns: &[
            "i", "b", "s", "n", "f", "v", "c", "tx", "ts", "tz", "j", "jb", "m", "d", "p", "ok",
            "ref_id", "nn", "chk", "cn", "cnv", "g",
        ],
    },
    Target {
        name: "parted",
        columns: &["v", "tx", "i"],
    },
    Target {
        name: "inh_parent",
        columns: &["v", "tx", "i"],
    },
    // Created in the migration itself (see `generated_cases`).
    Target {
        name: "fresh",
        columns: &["i", "v", "tx"],
    },
];

const TYPES: [&str; 22] = [
    "integer",
    "bigint",
    "smallint",
    "numeric",
    "numeric(12,2)",
    "numeric(8,2)",
    "float8",
    "real",
    "varchar(20)",
    "varchar(5)",
    "varchar",
    "text",
    "char(5)",
    "char(10)",
    "timestamptz",
    "timestamp",
    "date",
    "jsonb",
    "mood",
    "posint",
    "plain_int",
    "text COLLATE \"C\"",
];

fn generated_cases() -> Vec<Case> {
    let mut statements: Vec<String> = Vec::new();
    for target in &TARGETS {
        let table = target.name;
        for column in target.columns {
            for ty in TYPES {
                statements.push(format!(
                    "ALTER TABLE {table} ALTER COLUMN {column} TYPE {ty}"
                ));
                let plain = ty.split(' ').next().unwrap();
                statements.push(format!(
                    "ALTER TABLE {table} ALTER COLUMN {column} TYPE {ty} USING {column}::{plain}"
                ));
            }
            statements.push(format!(
                "ALTER TABLE {table} ALTER COLUMN {column} TYPE text USING {column}::text || ''"
            ));
            for command in [
                "SET NOT NULL",
                "DROP NOT NULL",
                "SET DEFAULT NULL",
                "DROP DEFAULT",
                "SET STATISTICS 100",
                "SET STORAGE EXTERNAL",
                "SET STORAGE MAIN",
                "SET COMPRESSION pglz",
                "SET (n_distinct = 10)",
                "RESET (n_distinct)",
            ] {
                statements.push(format!(
                    "ALTER TABLE {table} ALTER COLUMN {column} {command}"
                ));
                statements.push(format!(
                    "ALTER TABLE ONLY {table} ALTER COLUMN {column} {command}"
                ));
            }
            statements.push(format!("ALTER TABLE {table} DROP COLUMN {column}"));
            statements.push(format!("ALTER TABLE {table} DROP COLUMN {column} CASCADE"));
            statements.push(format!(
                "ALTER TABLE {table} RENAME COLUMN {column} TO renamed"
            ));
            statements.push(format!(
                "ALTER TABLE {table} ADD CONSTRAINT added CHECK ({column} IS NOT NULL)"
            ));
            statements.push(format!(
                "ALTER TABLE {table} ADD CONSTRAINT added CHECK ({column} IS NOT NULL) NOT VALID"
            ));
            statements.push(format!(
                "ALTER TABLE {table} ADD CONSTRAINT added NOT NULL {column}"
            ));
            statements.push(format!(
                "ALTER TABLE {table} ADD CONSTRAINT added NOT NULL {column} NOT VALID"
            ));
            statements.push(format!("ALTER TABLE {table} ADD UNIQUE ({column})"));
            statements.push(format!(
                "ALTER TABLE {table} ADD CONSTRAINT added EXCLUDE USING btree ({column} WITH =)"
            ));
            statements.push(format!("CREATE INDEX ON {table} ({column})"));
            statements.push(format!("CREATE UNIQUE INDEX ON {table} ({column})"));
            statements.push(format!("CREATE INDEX ON ONLY {table} ({column})"));
            statements.push(format!("CREATE INDEX ON {table} (lower({column}::text))"));
            statements.push(format!(
                "CREATE INDEX ON {table} ({column}) WHERE {column} IS NOT NULL"
            ));
        }
        for add in [
            "x integer",
            "x integer DEFAULT 1",
            "x integer NOT NULL DEFAULT 1",
            "x integer DEFAULT NULL",
            "x timestamptz DEFAULT now()",
            "x timestamptz DEFAULT clock_timestamp()",
            "x float8 DEFAULT random()",
            "x uuid DEFAULT gen_random_uuid()",
            "x serial",
            "x bigserial",
            "x integer GENERATED ALWAYS AS IDENTITY",
            "x integer GENERATED BY DEFAULT AS IDENTITY",
            "x integer GENERATED ALWAYS AS (id * 2) STORED",
            "x integer GENERATED ALWAYS AS (id * 2) VIRTUAL",
            "x integer UNIQUE",
            "x integer PRIMARY KEY",
            "x integer REFERENCES ref (id)",
            "x integer DEFAULT 1 REFERENCES ref (id)",
            "x integer GENERATED ALWAYS AS IDENTITY REFERENCES ref (id)",
            "x integer CHECK (x > 0)",
            "x integer DEFAULT 1 CHECK (x > 0)",
            "x integer NOT NULL",
            "x integer NOT NULL DEFAULT 1 CHECK (x > 0)",
            "x posint",
            "x posint DEFAULT 1",
            "x mood DEFAULT 'a'",
            "x text COLLATE \"C\"",
            "x integer[] DEFAULT '{}'",
            "x jsonb DEFAULT '{}' NOT NULL",
            "x varchar(10) DEFAULT 'a'",
            "x integer DEFAULT nextval('seq')",
        ] {
            statements.push(format!("ALTER TABLE {table} ADD COLUMN {add}"));
            statements.push(format!(
                "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {add}"
            ));
        }
        statements.push(format!(
            "ALTER TABLE {table} ADD COLUMN x integer, ADD COLUMN y integer DEFAULT 1 REFERENCES \
             ref (id)"
        ));
        statements.push(format!(
            "ALTER TABLE {table} ADD COLUMN x integer REFERENCES ref (id), ADD COLUMN y \
             integer DEFAULT 1"
        ));
        statements.push(format!(
            "ALTER TABLE {table} ADD COLUMN x integer, ADD FOREIGN KEY (x) REFERENCES ref (id)"
        ));
        for command in [
            "ADD CONSTRAINT added CHECK (i > 0)",
            "ADD CONSTRAINT added CHECK (i > 0) NOT VALID",
            "ADD CONSTRAINT added CHECK (i > 0) NO INHERIT",
            "ADD CONSTRAINT added CHECK (i > 0) NOT ENFORCED",
            "ADD CONSTRAINT added FOREIGN KEY (i) REFERENCES ref (id)",
            "ADD CONSTRAINT added FOREIGN KEY (i) REFERENCES ref (id) NOT VALID",
            "ADD CONSTRAINT added FOREIGN KEY (i) REFERENCES ref (id) NOT ENFORCED",
            "ADD CONSTRAINT added FOREIGN KEY (tx) REFERENCES ref (code)",
            "ADD CONSTRAINT added UNIQUE (i)",
            "ADD CONSTRAINT added UNIQUE NULLS NOT DISTINCT (i)",
            "ADD CONSTRAINT added PRIMARY KEY (id)",
            "ADD CONSTRAINT added UNIQUE USING INDEX t_nn_key",
            "ADD CONSTRAINT added PRIMARY KEY USING INDEX t_nn_key",
            "ADD CONSTRAINT added UNIQUE (i) DEFERRABLE",
            "ADD CONSTRAINT added CHECK (i > 0) NOT VALID, VALIDATE CONSTRAINT added",
            "VALIDATE CONSTRAINT t_s_check",
            "VALIDATE CONSTRAINT t_cnv_nn",
            "VALIDATE CONSTRAINT t_i_fk",
            "DROP CONSTRAINT t_s_check",
            "DROP CONSTRAINT t_pkey CASCADE",
            "RENAME CONSTRAINT t_s_check TO renamed",
            "RENAME TO renamed",
            "SET (fillfactor = 70)",
            "SET (autovacuum_enabled = false)",
            "SET (parallel_workers = 2)",
            "SET (user_catalog_table = true)",
            "SET (toast_tuple_target = 256)",
            "RESET (fillfactor)",
            "SET LOGGED",
            "SET UNLOGGED",
            "SET ACCESS METHOD heap",
            "SET ACCESS METHOD DEFAULT",
            "SET WITHOUT OIDS",
            "CLUSTER ON t_i_idx",
            "SET WITHOUT CLUSTER",
            "ENABLE TRIGGER t_trg",
            "DISABLE TRIGGER t_trg",
            "DISABLE TRIGGER ALL",
            "DISABLE TRIGGER USER",
            "ENABLE ALWAYS TRIGGER t_trg",
            "ENABLE REPLICA TRIGGER t_trg",
            "ENABLE ROW LEVEL SECURITY",
            "DISABLE ROW LEVEL SECURITY",
            "FORCE ROW LEVEL SECURITY",
            "NO FORCE ROW LEVEL SECURITY",
            "ENABLE RULE t_rule",
            "REPLICA IDENTITY FULL",
            "REPLICA IDENTITY USING INDEX t_pkey",
            "REPLICA IDENTITY NOTHING",
            "ALTER COLUMN nn ADD GENERATED ALWAYS AS IDENTITY",
            "ALTER COLUMN i SET EXPRESSION AS (1)",
            "ALTER COLUMN g SET EXPRESSION AS (i * 3)",
            "ALTER COLUMN gv SET EXPRESSION AS (i * 3)",
            "ALTER COLUMN g DROP EXPRESSION",
            "OWNER TO CURRENT_USER",
            "SET SCHEMA other",
            "INHERIT inh_parent",
            "NO INHERIT inh_parent",
            "OF pair",
            "NOT OF",
            "ALTER COLUMN i TYPE bigint, ALTER COLUMN v TYPE varchar(20)",
            "ALTER COLUMN v TYPE varchar(20), ADD CONSTRAINT added CHECK (i > 0)",
            "ADD COLUMN x integer, ALTER COLUMN x SET NOT NULL",
            "SET TABLESPACE pg_default",
        ] {
            statements.push(format!("ALTER TABLE {table} {command}"));
        }
        for statement in [
            "CREATE TRIGGER added AFTER INSERT ON {t} FOR EACH ROW EXECUTE FUNCTION trg()",
            "CREATE TRIGGER added AFTER INSERT ON {t} FOR EACH STATEMENT EXECUTE FUNCTION trg()",
            "CREATE OR REPLACE TRIGGER t_trg BEFORE UPDATE ON {t} FOR EACH ROW EXECUTE FUNCTION \
             trg()",
            "CREATE CONSTRAINT TRIGGER added AFTER INSERT ON {t} FOR EACH ROW EXECUTE FUNCTION \
             trg()",
            "DROP TRIGGER t_trg ON {t}",
            "ALTER TRIGGER t_trg ON {t} RENAME TO renamed",
            "CREATE POLICY added ON {t} USING (true)",
            "ALTER POLICY t_pol ON {t} USING (false)",
            "DROP POLICY t_pol ON {t}",
            "CREATE RULE added AS ON INSERT TO {t} DO NOTHING",
            "DROP RULE t_rule ON {t}",
            "TRUNCATE {t}",
            "TRUNCATE {t} CASCADE",
            "LOCK TABLE {t}",
            "LOCK TABLE {t} IN ACCESS SHARE MODE",
            "LOCK TABLE {t} IN ROW EXCLUSIVE MODE",
            "LOCK TABLE {t} IN SHARE UPDATE EXCLUSIVE MODE",
            "LOCK TABLE {t} IN SHARE MODE",
            "LOCK TABLE {t} IN SHARE ROW EXCLUSIVE MODE",
            "LOCK TABLE {t} IN EXCLUSIVE MODE",
            "LOCK TABLE {t} IN ACCESS EXCLUSIVE MODE",
            "CLUSTER {t} USING t_pkey",
            "REPACK {t}",
            "VACUUM FULL {t}",
            "VACUUM {t}",
            "ANALYZE {t}",
            "REINDEX TABLE {t}",
            "REINDEX TABLE CONCURRENTLY {t}",
            "COMMENT ON TABLE {t} IS 'x'",
            "COMMENT ON COLUMN {t}.i IS 'x'",
            "GRANT SELECT ON {t} TO PUBLIC",
            "REVOKE ALL ON {t} FROM PUBLIC",
            "DROP TABLE {t} CASCADE",
            "CREATE VIEW added AS SELECT * FROM {t}",
            "CREATE MATERIALIZED VIEW added AS SELECT * FROM {t}",
            "CREATE TABLE added AS SELECT * FROM {t}",
            "CREATE TABLE added (id integer REFERENCES {t} (id))",
            "CREATE TABLE added (LIKE {t} INCLUDING ALL)",
            "CREATE TABLE added () INHERITS ({t})",
            "CREATE STATISTICS added ON i, id FROM {t}",
            "INSERT INTO {t} (id, k) VALUES (1, 1)",
            "UPDATE {t} SET i = 1",
            "DELETE FROM {t}",
            "SELECT * FROM {t}",
        ] {
            statements.push(statement.replace("{t}", table));
        }
    }
    for statement in [
        "DROP INDEX t_i_idx",
        "DROP INDEX t_i_idx, t_v_idx",
        "DROP INDEX CONCURRENTLY t_i_idx",
        "DROP INDEX parted_v_idx",
        "DROP INDEX mv_id",
        "ALTER INDEX t_i_idx RENAME TO renamed",
        "ALTER INDEX t_i_idx SET (fillfactor = 50)",
        "ALTER INDEX t_i_idx ALTER COLUMN 1 SET STATISTICS 10",
        "ALTER INDEX t_i_idx SET TABLESPACE pg_default",
        "ALTER INDEX parted_v_idx ATTACH PARTITION t_v_idx",
        "REINDEX INDEX t_i_idx",
        "REINDEX INDEX CONCURRENTLY t_i_idx",
        "REINDEX INDEX parted_v_idx",
        "REINDEX SCHEMA public",
        "REINDEX DATABASE",
        "VACUUM FULL",
        "VACUUM FULL t, ref",
        "REPACK",
        "REPACK (CONCURRENTLY) t",
        "CLUSTER t",
        "CLUSTER",
        "REFRESH MATERIALIZED VIEW mv",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY mv",
        "REFRESH MATERIALIZED VIEW mv WITH NO DATA",
        "ALTER MATERIALIZED VIEW mv RENAME TO renamed",
        "ALTER MATERIALIZED VIEW mv SET (fillfactor = 50)",
        "ALTER MATERIALIZED VIEW mv ALTER COLUMN i SET STATISTICS 10",
        "DROP MATERIALIZED VIEW mv",
        "DROP VIEW vw",
        "CREATE OR REPLACE VIEW vw AS SELECT id, i, b FROM t",
        "ALTER VIEW vw RENAME TO renamed",
        "DROP TABLE fk_child",
        "DROP TABLE ref CASCADE",
        "DROP TABLE t, ref CASCADE",
        "ALTER TABLE parted ATTACH PARTITION loose FOR VALUES FROM (20) TO (30)",
        "ALTER TABLE parted ATTACH PARTITION loose_chk FOR VALUES FROM (30) TO (40)",
        "ALTER TABLE parted ATTACH PARTITION loose_in FOR VALUES FROM (5) TO (7)",
        "ALTER TABLE parted_nd ATTACH PARTITION loose FOR VALUES IN (5)",
        "ALTER TABLE parted_nd ATTACH PARTITION loose_in FOR VALUES IN (5, 6)",
        "ALTER TABLE parted_nd ATTACH PARTITION loose_in FOR VALUES IN (5, 6, 7)",
        "ALTER TABLE parted_nd ATTACH PARTITION loose DEFAULT",
        "ALTER TABLE parted_nd ATTACH PARTITION loose_chk FOR VALUES IN (30, 31)",
        "ALTER TABLE parted DETACH PARTITION parted_1",
        "ALTER TABLE parted DETACH PARTITION parted_d",
        "ALTER TABLE parted_nd DETACH PARTITION parted_nd_1",
        "ALTER TABLE parted_nd DETACH PARTITION parted_nd_1 CONCURRENTLY",
        "ALTER TABLE parted DETACH PARTITION parted_1 FINALIZE",
        "CREATE TABLE parted_3 PARTITION OF parted FOR VALUES FROM (20) TO (30)",
        "CREATE TABLE parted_nd_2 PARTITION OF parted_nd FOR VALUES IN (2)",
        "CREATE TABLE parted_nd_d PARTITION OF parted_nd DEFAULT",
        "CREATE TABLE added PARTITION OF parted (CHECK (k >= 20 AND k < 30)) FOR VALUES FROM \
         (20) TO (30)",
        "ALTER TABLE parted SPLIT PARTITION parted_2 INTO (PARTITION parted_2a FOR VALUES FROM \
         (10) TO (15), PARTITION parted_2b FOR VALUES FROM (15) TO (20))",
        "ALTER TABLE parted MERGE PARTITIONS (parted_1, parted_2) INTO parted_12",
        "ALTER TABLE inh_other INHERIT inh_parent",
        "ALTER TABLE inh_child NO INHERIT inh_parent",
        "ALTER TABLE fits_pair OF pair",
        "ALTER TABLE typed_t NOT OF",
        "ALTER TABLE t ADD CONSTRAINT added FOREIGN KEY (i) REFERENCES ref (id) NOT VALID",
        "ALTER TABLE fk_child ADD CONSTRAINT added FOREIGN KEY (id) REFERENCES t (id)",
        "ALTER TABLE ref ADD CONSTRAINT added FOREIGN KEY (big) REFERENCES t (b)",
        "ALTER DOMAIN posint ADD CONSTRAINT added CHECK (VALUE < 100)",
        "ALTER DOMAIN posint ADD CONSTRAINT added CHECK (VALUE < 100) NOT VALID",
        "ALTER DOMAIN plain_int ADD CONSTRAINT added CHECK (VALUE < 100)",
        "ALTER DOMAIN plain_int ADD CONSTRAINT added NOT NULL",
        "ALTER DOMAIN plain_int SET NOT NULL",
        "ALTER DOMAIN posint DROP CONSTRAINT posint_check",
        "ALTER DOMAIN posint VALIDATE CONSTRAINT posint_check",
        "ALTER DOMAIN plain_int SET DEFAULT 1",
        "ALTER DOMAIN plain_int RENAME TO renamed",
        "ALTER TYPE mood ADD VALUE 'c'",
        "ALTER TYPE mood RENAME VALUE 'a' TO 'z'",
        "ALTER TYPE mood RENAME TO renamed",
        "ALTER TYPE pair ADD ATTRIBUTE z integer",
        "ALTER TYPE pair ADD ATTRIBUTE z integer CASCADE",
        "ALTER TYPE pair ALTER ATTRIBUTE x TYPE bigint CASCADE",
        "DROP TYPE mood CASCADE",
        "DROP DOMAIN plain_int CASCADE",
        "DROP DOMAIN posint CASCADE",
        "DROP FUNCTION trg() CASCADE",
        "DROP SCHEMA other CASCADE",
        "DROP SEQUENCE seq",
        "DROP STATISTICS t_stats",
        "ALTER SEQUENCE seq RESTART",
        "ALTER TABLE ref RENAME COLUMN code TO renamed",
        "ALTER TABLE ref ALTER COLUMN code TYPE varchar(20)",
        "ALTER TABLE ref ALTER COLUMN id TYPE bigint",
        "ALTER TABLE ref DROP CONSTRAINT ref_pkey CASCADE",
        "ALTER TABLE other.o ADD COLUMN x integer",
        "ALTER TABLE mv ADD COLUMN x integer",
        "ALTER FOREIGN TABLE IF EXISTS nothing ADD COLUMN x integer",
        "SELECT pg_advisory_xact_lock(1)",
    ] {
        statements.push(statement.to_owned());
    }

    let mut out = Vec::new();
    let fresh = "CREATE TABLE fresh (id integer PRIMARY KEY, i integer, v varchar(10), tx text);\n";
    for statement in statements {
        let body = if statement.contains("fresh") {
            format!("{fresh}{statement};\n")
        } else {
            format!("{statement};\n")
        };
        let upper = statement.to_ascii_uppercase();
        let outside = upper.contains("CONCURRENTLY")
            || upper.starts_with("VACUUM")
            || upper.starts_with("REINDEX SCHEMA")
            || upper.starts_with("REINDEX DATABASE")
            || upper == "REPACK"
            || upper == "CLUSTER";
        if outside {
            out.push(Case {
                origin: format!("generated: {statement}"),
                files: vec![BASE.to_owned(), format!("-- no-transaction\n{body}")],
                use_transaction: true,
            });
            continue;
        }
        out.push(Case {
            origin: format!("generated: {statement}"),
            files: vec![BASE.to_owned(), body.clone()],
            use_transaction: true,
        });
        out.push(Case {
            origin: format!("generated, with lock_timeout: {statement}"),
            files: vec![
                BASE.to_owned(),
                format!("SET LOCAL lock_timeout = '1s';\n{body}"),
            ],
            use_transaction: true,
        });
    }
    // Pairs whose second statement depends on the first one's transaction.
    for (first, second) in [
        (
            "ALTER TABLE t ADD CONSTRAINT added CHECK (i > 0) NOT VALID",
            "ALTER TABLE t VALIDATE CONSTRAINT added",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT added FOREIGN KEY (i) REFERENCES ref (id) NOT VALID",
            "ALTER TABLE t VALIDATE CONSTRAINT added",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT added CHECK (i > 0) NOT VALID",
            "COMMIT; BEGIN; ALTER TABLE t VALIDATE CONSTRAINT added",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT added CHECK (i IS NOT NULL) NOT VALID",
            "ALTER TABLE t VALIDATE CONSTRAINT added; ALTER TABLE t ALTER COLUMN i SET NOT NULL",
        ),
        ("CREATE INDEX added ON t (tx)", "DROP INDEX added"),
        (
            "LOCK TABLE t IN SHARE MODE",
            "ALTER TABLE t ADD COLUMN x integer",
        ),
        (
            "SELECT set_config('lock_timeout', '2s', true)",
            "ALTER TABLE t ADD COLUMN x integer",
        ),
        (
            "SET lock_timeout = '2s'",
            "ALTER TABLE t ADD COLUMN x integer",
        ),
        ("SET lock_timeout = 0", "ALTER TABLE t ADD COLUMN x integer"),
        (
            "SET LOCAL lock_timeout = '2s'; COMMIT",
            "ALTER TABLE t ADD COLUMN x integer",
        ),
        (
            "BEGIN; SET LOCAL lock_timeout = '2s'; ROLLBACK",
            "ALTER TABLE t ADD COLUMN x integer",
        ),
        (
            "SAVEPOINT s; SET LOCAL lock_timeout = '2s'; ROLLBACK TO s",
            "ALTER TABLE t ADD COLUMN x integer",
        ),
        (
            "CREATE TABLE added (id integer)",
            "ALTER TABLE parted ATTACH PARTITION added FOR VALUES FROM (40) TO (50)",
        ),
    ] {
        for (use_transaction, marker) in [(true, ""), (false, ""), (true, "-- no-transaction\n")] {
            out.push(Case {
                origin: format!(
                    "generated pair (use_transaction = {use_transaction}, marker = {}): \
                     {first}; {second}",
                    !marker.is_empty()
                ),
                files: vec![BASE.to_owned(), format!("{marker}{first};\n{second};\n")],
                use_transaction,
            });
        }
    }
    out
}
