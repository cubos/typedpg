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

// ── Window definitions (#56, #57) ────────────────────────────────────────────

fn window_setup() -> PgCatalog {
    let mut db = setup();
    db.apply_sql("CREATE TABLE t56 (id int PRIMARY KEY, g int NOT NULL, x int NOT NULL, y int, s text NOT NULL, d date NOT NULL)")
        .unwrap();
    db
}

#[test]
fn window_definition_rules() {
    let db = window_setup();
    for (sql, msg) in [
        (
            "SELECT sum(x) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t56",
            "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
        ),
        (
            "SELECT sum(x) OVER (GROUPS 1 PRECEDING) FROM t56",
            "GROUPS mode requires an ORDER BY clause",
        ),
        (
            "SELECT sum(x) OVER (w ORDER BY id) FROM t56 WINDOW w AS (PARTITION BY g ORDER BY x)",
            "cannot override ORDER BY clause of window \"w\"",
        ),
        (
            "SELECT sum(x) OVER (w PARTITION BY id) FROM t56 WINDOW w AS (ORDER BY x)",
            "cannot override PARTITION BY clause of window \"w\"",
        ),
        (
            "SELECT sum(x) OVER (w) FROM t56 WINDOW w AS (ORDER BY x ROWS 1 PRECEDING)",
            "cannot copy window \"w\" because it has a frame clause",
        ),
        (
            "SELECT sum(x) OVER w FROM t56 WINDOW w AS (ORDER BY x), w AS (ORDER BY id)",
            "window \"w\" is already defined",
        ),
        (
            "SELECT sum(x) OVER (ORDER BY row_number() OVER ()) FROM t56",
            "window functions are not allowed in window definitions",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::WindowingError(_), msg);
    }
    assert_err_kind!(
        db,
        "SELECT sum(x) OVER w2 FROM t56 WINDOW w AS (PARTITION BY g), w2 AS (w3)",
        AnalyzeError::UndefinedObject(_),
        "window \"w3\" does not exist"
    );
    for (sql, msg) in [
        (
            "SELECT sum(x) OVER (ORDER BY id ROWS BETWEEN y PRECEDING AND CURRENT ROW) FROM t56",
            "argument of ROWS must not contain variables",
        ),
        (
            "SELECT sum(x) OVER (ORDER BY id GROUPS BETWEEN y PRECEDING AND CURRENT ROW) FROM t56",
            "argument of GROUPS must not contain variables",
        ),
        (
            "SELECT sum(x) OVER (ORDER BY id RANGE BETWEEN y PRECEDING AND CURRENT ROW) FROM t56",
            "argument of RANGE must not contain variables",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::InvalidColumnReference(_), msg);
    }
    // Inheriting only what the base window leaves open is fine.
    db.analyze(
        "SELECT sum(x) OVER w2 FROM t56 WINDOW w AS (PARTITION BY g),
                w2 AS (w ORDER BY x RANGE BETWEEN 1 PRECEDING AND CURRENT ROW)",
    )
    .unwrap();
}

#[test]
fn range_offsets_need_in_range_support() {
    let db = window_setup();
    for (sql, msg) in [
        (
            "SELECT sum(x) OVER (ORDER BY s RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t56",
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type text",
        ),
        (
            "SELECT sum(x) OVER (ORDER BY d RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t56",
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type date and offset type integer",
        ),
        (
            "SELECT sum(x) OVER (ORDER BY id RANGE BETWEEN 1.5 PRECEDING AND CURRENT ROW) FROM t56",
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type integer and offset type numeric",
        ),
    ] {
        assert_err_kind!(db, sql, AnalyzeError::FeatureNotSupported(_), msg);
    }
    assert_err_starts_with(
        &db,
        "SELECT sum(x) OVER (ORDER BY id RANGE BETWEEN 'x' PRECEDING AND CURRENT ROW) FROM t56",
        "invalid input syntax for type integer: \"x\"",
    );
}

#[test]
fn range_offset_params_take_the_in_range_offset_type() {
    let db = window_setup();
    for (sql, param) in [
        (
            "SELECT sum(x) OVER (ORDER BY id RANGE BETWEEN $a PRECEDING AND CURRENT ROW) FROM t56",
            int4(),
        ),
        (
            "SELECT sum(x) OVER (ORDER BY ts RANGE BETWEEN $a PRECEDING AND CURRENT ROW) FROM t",
            interval(),
        ),
        (
            "SELECT sum(x) OVER (ORDER BY d RANGE BETWEEN $a PRECEDING AND CURRENT ROW) FROM t56",
            interval(),
        ),
        (
            "SELECT sum(x) OVER (ORDER BY x::numeric RANGE BETWEEN $a PRECEDING AND CURRENT ROW) FROM t56",
            numeric(),
        ),
        (
            "SELECT sum(x) OVER (ORDER BY id ROWS BETWEEN $a PRECEDING AND CURRENT ROW) FROM t56",
            int8(),
        ),
    ] {
        let s = db.analyze(sql).unwrap();
        assert_params(&s, vec![p(param)]);
    }
}
