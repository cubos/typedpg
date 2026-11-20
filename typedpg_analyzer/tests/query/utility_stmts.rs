//! `EXPLAIN`, `NOTIFY`, `LISTEN`, `UNLISTEN` — utility statements.
//!
//! Their query-analysis surface is small: EXPLAIN produces a single
//! text `QUERY PLAN` column (NOT NULL), and NOTIFY/LISTEN/UNLISTEN have
//! no result rows. Parameters inside an EXPLAIN-wrapped query still get
//! harvested.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── EXPLAIN ──────────────────────────────────────────────────────────────────

#[test]
fn explain_select_produces_single_query_plan_column() {
    let db = setup();
    let s = db.analyze("EXPLAIN SELECT id FROM t").unwrap();
    assert_cols(&s, vec![c("QUERY PLAN", text())]);
    assert!(s.params.is_empty());
}

#[test]
fn explain_with_inner_param_extracts_param_type() {
    // Params inside the wrapped query still get harvested — the
    // analyzer just replaces the *output* with the QUERY PLAN row.
    let db = setup();
    let s = db
        .analyze("EXPLAIN SELECT id FROM t WHERE id = $p1")
        .unwrap();
    assert_cols(&s, vec![c("QUERY PLAN", text())]);
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn explain_propagates_inner_query_errors() {
    // PG: even EXPLAIN-wrapped queries fail when the inner statement is
    // ill-formed. We must not silently swallow the inner error.
    let db = setup();
    assert_analyze_err!(
        db.analyze("EXPLAIN SELECT bogus_col FROM t"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"bogus_col\" does not exist\n",
            "  ╭────\n",
            "1 │ EXPLAIN SELECT bogus_col FROM t\n",
            "  ·                ────┬────\n",
            "  ·                    ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn explain_insert_and_update_produce_query_plan_column() {
    // EXPLAIN can wrap any DML — INSERT/UPDATE/DELETE all produce the
    // same single-column row description as SELECT.
    let db = setup();
    let s = db
        .analyze("EXPLAIN INSERT INTO t (id, name) VALUES ($i, $n)")
        .unwrap();
    assert_cols(&s, vec![c("QUERY PLAN", text())]);
    assert_params(&s, vec![p(int8()), p(text())]);
}

#[test]
fn explain_query_plan_type_follows_format() {
    // PG 18 ExplainResultDesc: FORMAT JSON yields a `json` column, FORMAT
    // XML an `xml` one, TEXT and YAML `text`.
    let db = setup();
    for (sql, ty) in [
        (
            "EXPLAIN (FORMAT json) SELECT id FROM t",
            basic("pg_catalog", "json"),
        ),
        (
            "EXPLAIN (FORMAT JSON, COSTS off) SELECT id FROM t",
            basic("pg_catalog", "json"),
        ),
        (
            "EXPLAIN (FORMAT xml) SELECT id FROM t",
            basic("pg_catalog", "xml"),
        ),
        ("EXPLAIN (FORMAT yaml) SELECT id FROM t", text()),
        ("EXPLAIN (FORMAT text) SELECT id FROM t", text()),
        (
            "EXPLAIN (FORMAT json, FORMAT text) SELECT id FROM t",
            text(),
        ),
        (
            "EXPLAIN (ANALYZE, FORMAT json) SELECT id FROM t",
            basic("pg_catalog", "json"),
        ),
    ] {
        let s = db.analyze(sql).expect(sql);
        assert_cols(&s, vec![c("QUERY PLAN", ty)]);
    }
}

// ── NOTIFY / LISTEN / UNLISTEN ──────────────────────────────────────────────

#[test]
fn notify_has_no_output_columns_or_params() {
    let db = setup();
    let s = db.analyze("NOTIFY my_channel").unwrap();
    assert!(s.columns.is_empty());
    assert!(s.params.is_empty());
}

#[test]
fn notify_with_payload_literal_has_no_output_columns() {
    // PG: payload is a *string literal* in the NOTIFY grammar — no
    // expressions, no parameters. (For parameterized notifications
    // callers use `SELECT pg_notify($1, $2)`, which goes through the
    // regular function-call path.)
    let db = setup();
    let s = db.analyze("NOTIFY my_channel, 'hello'").unwrap();
    assert!(s.columns.is_empty());
    assert!(s.params.is_empty());
}

#[test]
fn listen_has_no_output_columns_or_params() {
    let db = setup();
    let s = db.analyze("LISTEN my_channel").unwrap();
    assert!(s.columns.is_empty());
    assert!(s.params.is_empty());
}

#[test]
fn unlisten_has_no_output_columns_or_params() {
    let db = setup();
    let s = db.analyze("UNLISTEN my_channel").unwrap();
    assert!(s.columns.is_empty());
    assert!(s.params.is_empty());
}

#[test]
fn unlisten_star_accepted() {
    // `UNLISTEN *` removes every active LISTEN — same empty shape.
    let db = setup();
    let s = db.analyze("UNLISTEN *").unwrap();
    assert!(s.columns.is_empty());
    assert!(s.params.is_empty());
}

// ── TRUNCATE, LOCK, ANALYZE, REFRESH MATERIALIZED VIEW, SET / RESET ─────────
//
// PostgreSQL prepares these without looking at their relations: everything
// is checked when they execute. What fails on every execution is a
// compile-time error, with PostgreSQL's message (verified on 18.7).

fn utility_setup() -> PgCatalog {
    build_db(&[(
        "0001.sql",
        "CREATE TABLE ut_parent (id int PRIMARY KEY, v text);
         CREATE TABLE ut_child (id int REFERENCES ut_parent);
         CREATE TABLE ut_inh () INHERITS (ut_parent);
         CREATE TABLE ut_part (id int) PARTITION BY RANGE (id);
         CREATE TABLE ut_part1 PARTITION OF ut_part FOR VALUES FROM (0) TO (10);
         CREATE VIEW ut_view AS SELECT * FROM ut_parent;
         CREATE MATERIALIZED VIEW ut_mv AS SELECT id FROM ut_parent;
         CREATE MATERIALIZED VIEW ut_mv_unique AS SELECT id FROM ut_parent;
         CREATE UNIQUE INDEX ON ut_mv_unique (id);
         CREATE SEQUENCE ut_seq;",
    )])
}

#[test]
fn utility_statements_return_no_rows_and_take_no_parameters() {
    let db = utility_setup();
    for sql in [
        "TRUNCATE ut_parent, ut_child",
        "TRUNCATE TABLE ut_parent CASCADE",
        "TRUNCATE ONLY ut_part1 RESTART IDENTITY",
        "TRUNCATE ut_part",
        "LOCK ut_parent",
        "LOCK TABLE ut_parent, ut_view IN ACCESS SHARE MODE NOWAIT",
        "LOCK ut_part IN ROW EXCLUSIVE MODE",
        "ANALYZE",
        "ANALYZE VERBOSE ut_parent (id, v), ut_mv",
        "ANALYZE (SKIP_LOCKED, BUFFER_USAGE_LIMIT '256kB') ut_parent",
        // A view or a sequence is skipped with a WARNING.
        "ANALYZE ut_view, ut_seq",
        "REFRESH MATERIALIZED VIEW ut_mv",
        "REFRESH MATERIALIZED VIEW ut_mv WITH NO DATA",
        "REFRESH MATERIALIZED VIEW CONCURRENTLY ut_mv_unique",
        "SET statement_timeout = '5s'",
        "SET LOCAL lock_timeout TO 1000",
        "SET app.tenant = '42'",
        "SET TIME ZONE 'UTC'",
        "RESET work_mem",
        "RESET ALL",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(s.columns.is_empty(), "{sql}");
        assert!(s.params.is_empty(), "{sql}");
    }
}

#[test]
fn truncate_reports_what_every_execution_raises() {
    let db = utility_setup();
    assert_err_prefix!(
        db.analyze("TRUNCATE ut_nope"),
        AnalyzeError::UndefinedTable(_),
        "relation \"ut_nope\" does not exist"
    );
    for (sql, name) in [
        ("TRUNCATE ut_view", "ut_view"),
        ("TRUNCATE ut_mv", "ut_mv"),
        ("TRUNCATE ut_seq", "ut_seq"),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::WrongObjectType(_),
            &format!("\"{name}\" is not a table")
        );
    }
    assert_err_prefix!(
        db.analyze("TRUNCATE ONLY ut_part"),
        AnalyzeError::WrongObjectType(_),
        "cannot truncate only a partitioned table"
    );
    // RESTRICT: whatever the rows, a table another one references must be
    // truncated with it.
    assert_err_prefix!(
        db.analyze("TRUNCATE ut_parent"),
        AnalyzeError::FeatureNotSupported(_),
        "cannot truncate a table referenced in a foreign key constraint"
    );
    assert_err_prefix!(
        db.analyze("TRUNCATE pg_catalog.pg_class"),
        AnalyzeError::InsufficientPrivilege(_),
        "permission denied: \"pg_class\" is a system catalog"
    );
}

#[test]
fn lock_reports_what_every_execution_raises() {
    let db = utility_setup();
    for (sql, name) in [("LOCK ut_mv", "ut_mv"), ("LOCK ut_seq", "ut_seq")] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::WrongObjectType(_),
            &format!("cannot lock relation \"{name}\"")
        );
    }
    assert_err_prefix!(
        db.analyze("LOCK ut_nope"),
        AnalyzeError::UndefinedTable(_),
        "relation \"ut_nope\" does not exist"
    );
}

#[test]
fn analyze_reports_what_every_execution_raises() {
    let db = utility_setup();
    assert_err_prefix!(
        db.analyze("ANALYZE ut_nope"),
        AnalyzeError::UndefinedTable(_),
        "relation \"ut_nope\" does not exist"
    );
    assert_err_prefix!(
        db.analyze("ANALYZE ut_parent (nope)"),
        AnalyzeError::UndefinedColumn(_),
        "column \"nope\" of relation \"ut_parent\" does not exist"
    );
    assert_err_prefix!(
        db.analyze("ANALYZE ut_parent (id, id)"),
        AnalyzeError::DuplicateColumn(_),
        "column \"id\" of relation \"ut_parent\" appears more than once"
    );
    assert_err_prefix!(
        db.analyze("ANALYZE (FULL) ut_parent"),
        AnalyzeError::SyntaxError(_),
        "unrecognized ANALYZE option \"full\""
    );
    assert_err_prefix!(
        db.analyze("ANALYZE (VERBOSE maybe) ut_parent"),
        AnalyzeError::SyntaxError(_),
        "verbose requires a Boolean value"
    );
}

#[test]
fn refresh_reports_what_every_execution_raises() {
    let db = utility_setup();
    assert_err_prefix!(
        db.analyze("REFRESH MATERIALIZED VIEW ut_parent"),
        AnalyzeError::FeatureNotSupported(_),
        "\"ut_parent\" is not a materialized view"
    );
    assert_err_prefix!(
        db.analyze("REFRESH MATERIALIZED VIEW CONCURRENTLY ut_mv"),
        AnalyzeError::ObjectNotInPrerequisiteState(_),
        "cannot refresh materialized view \"public.ut_mv\" concurrently"
    );
    assert_err_prefix!(
        db.analyze("REFRESH MATERIALIZED VIEW CONCURRENTLY ut_mv_unique WITH NO DATA"),
        AnalyzeError::SyntaxError(_),
        "CONCURRENTLY and WITH NO DATA options cannot be used together"
    );
}

#[test]
fn set_reports_what_every_execution_raises() {
    let db = utility_setup();
    assert_err_prefix!(
        db.analyze("SET ut_nope = 1"),
        AnalyzeError::UndefinedObject(_),
        "unrecognized configuration parameter \"ut_nope\""
    );
    for (sql, message) in [
        (
            "SET work_mem = 'abc'",
            "invalid value for parameter \"work_mem\": \"abc\"",
        ),
        (
            "SET shared_buffers = '1MB'",
            "parameter \"shared_buffers\" cannot be changed without restarting the server",
        ),
    ] {
        let e = db.analyze(sql).unwrap_err().to_string();
        assert!(e.starts_with(message), "{sql}: {e}");
    }
}
