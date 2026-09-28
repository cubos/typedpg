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
