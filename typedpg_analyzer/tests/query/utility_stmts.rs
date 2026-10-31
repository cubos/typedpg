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
