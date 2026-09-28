//! Placement and validation rules of window-function and aggregate calls:
//! windows over grouped rows, call modifiers (DISTINCT / ORDER BY / FILTER
//! / WITHIN GROUP) and frame clauses. Every expectation below was observed
//! on PostgreSQL 18.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, g int NOT NULL, x int NOT NULL, y int,
                         s text NOT NULL, ts timestamptz NOT NULL);",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_err_starts_with(db: &PgCatalog, sql: &str, expected: &str) {
    let err = db.analyze(sql).unwrap_err();
    assert!(
        err.to_string().starts_with(expected),
        "expected `{expected}` for `{sql}`, got: {err}"
    );
}

/// Like [`assert_err_starts_with`], also pinning the SQLSTATE-carrying
/// variant.
macro_rules! assert_err_kind {
    ($db:expr, $sql:expr, $variant:pat, $expected:expr) => {{
        let sql: &str = $sql;
        let err = $db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, $variant),
            "wrong variant for `{sql}`: {err:?}"
        );
        assert_err_starts_with(&$db, sql, $expected);
    }};
}

// ── Windows over grouped rows (#52, #53) ─────────────────────────────────────

#[test]
fn window_over_an_aggregate() {
    let db = setup();
    let s = db
        .analyze("SELECT g, sum(sum(x)) OVER (ORDER BY g) AS running FROM t GROUP BY g")
        .unwrap();
    assert_cols(&s, vec![c("g", int4()), c("running", numeric())]);
    assert_err_kind!(
        db,
        "SELECT sum(row_number() OVER ()) OVER () FROM t",
        AnalyzeError::WindowingError(_),
        "window function calls cannot be nested"
    );
    assert_err_kind!(
        db,
        "SELECT sum(sum(x) OVER ()) FROM t",
        AnalyzeError::GroupingError(_),
        "aggregate function calls cannot contain window function calls"
    );
}

#[test]
fn window_calls_over_grouped_rows_see_only_grouped_columns() {
    let db = setup();
    for (sql, col) in [
        (
            "SELECT sum(x) OVER (PARTITION BY g ORDER BY id) FROM t GROUP BY g",
            "t.x",
        ),
        (
            "SELECT sum(g) OVER (PARTITION BY g ORDER BY id) FROM t GROUP BY g",
            "t.id",
        ),
        (
            "SELECT sum(g) OVER w FROM t GROUP BY g WINDOW w AS (ORDER BY x)",
            "t.x",
        ),
        (
            "SELECT row_number() OVER (ORDER BY x) FROM t GROUP BY g",
            "t.x",
        ),
        (
            "SELECT sum(g) FILTER (WHERE x > 1) OVER () FROM t GROUP BY g",
            "t.x",
        ),
    ] {
        assert_err_kind!(
            db,
            sql,
            AnalyzeError::GroupingError(_),
            &format!(
                "column \"{col}\" must appear in the GROUP BY clause or be used in an aggregate function"
            )
        );
    }
    // Grouped columns and aggregates are fine.
    db.analyze("SELECT sum(g) OVER (PARTITION BY g ORDER BY max(x)) FROM t GROUP BY g")
        .unwrap();
}

// ── Call modifiers on the wrong kind of routine (#55) ────────────────────────

#[test]
fn aggregate_modifiers_on_a_plain_function() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT lower(DISTINCT s) FROM t",
            "DISTINCT specified, but lower is not an aggregate function",
        ),
        (
            "SELECT lower(s ORDER BY s) FROM t",
            "ORDER BY specified, but lower is not an aggregate function",
        ),
        (
            "SELECT lower(s) FILTER (WHERE true) FROM t",
            "FILTER specified, but lower is not an aggregate function",
        ),
        (
            "SELECT int4(DISTINCT '1')",
            "DISTINCT specified, but int4 is not an aggregate function",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::WrongObjectType(_), msg);
    }
}

#[test]
fn ordered_set_and_plain_aggregate_call_shapes() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT count() FROM t",
            "count(*) must be used to call a parameterless aggregate function",
        ),
        (
            "SELECT count(*) WITHIN GROUP (ORDER BY x) FROM t",
            "count is not an ordered-set aggregate, so it cannot have WITHIN GROUP",
        ),
        (
            "SELECT mode(x) FROM t",
            "WITHIN GROUP is required for ordered-set aggregate mode",
        ),
        (
            "SELECT rank(1) OVER () FROM t",
            "WITHIN GROUP is required for ordered-set aggregate rank",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::WrongObjectType(_), msg);
    }
    assert_err_kind!(
        db,
        "SELECT rank(3, 4) WITHIN GROUP (ORDER BY x) FROM t",
        AnalyzeError::UndefinedFunction(_),
        "function rank(integer, integer, integer) does not exist"
    );
    assert_err_kind!(
        db,
        "SELECT mode() WITHIN GROUP (ORDER BY x) OVER () FROM t",
        AnalyzeError::FeatureNotSupported(_),
        "OVER is not supported for ordered-set aggregate mode"
    );
}

#[test]
fn hypothetical_arguments_take_the_ordering_column_type() {
    let db = setup();
    let s = db
        .analyze("SELECT rank($p) WITHIN GROUP (ORDER BY x) AS r FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("r", int8())]);
    assert_params(&s, vec![p(int4())]);
    assert_err_starts_with(
        &db,
        "SELECT rank('a') WITHIN GROUP (ORDER BY x) FROM t",
        "invalid input syntax for type integer: \"a\"",
    );
}

#[test]
fn aggregate_and_window_modifier_rules() {
    let db = setup();
    assert_err_kind!(
        db,
        "SELECT array_agg(DISTINCT x ORDER BY y) FROM t",
        AnalyzeError::InvalidColumnReference(_),
        "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list"
    );
    db.analyze("SELECT array_agg(DISTINCT t.x ORDER BY x) FROM t")
        .unwrap();
    assert_err_kind!(
        db,
        "SELECT count(*) FILTER (WHERE count(*) > 1) FROM t",
        AnalyzeError::GroupingError(_),
        "aggregate functions are not allowed in FILTER"
    );
    assert_err_kind!(
        db,
        "SELECT count(*) FILTER (WHERE row_number() OVER () > 1) FROM t",
        AnalyzeError::WindowingError(_),
        "window functions are not allowed in FILTER"
    );
    for (sql, msg) in [
        (
            "SELECT sum(DISTINCT x) OVER () FROM t",
            "DISTINCT is not implemented for window functions",
        ),
        (
            "SELECT array_agg(x ORDER BY x) OVER () FROM t",
            "aggregate ORDER BY is not implemented for window functions",
        ),
        (
            "SELECT row_number() FILTER (WHERE true) OVER () FROM t",
            "FILTER is not implemented for non-aggregate window functions",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::FeatureNotSupported(_), msg);
    }
}

// ── Frame clauses (#62) ──────────────────────────────────────────────────────

#[test]
fn frame_bound_errors_are_windowing_errors() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT sum(x) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM t",
            "frame starting from current row cannot have preceding rows",
        ),
        (
            "SELECT sum(x) OVER (ORDER BY id ROWS UNBOUNDED FOLLOWING) FROM t",
            "frame start cannot be UNBOUNDED FOLLOWING",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::WindowingError(_), msg);
    }
}
