//! GROUPING SETS / CUBE / ROLLUP and the `GROUPING()` function.
//!
//! Key nullability rule: a non-NULL column referenced in a SELECT list
//! becomes nullable for grouping sets that omit it (PG fills those rows
//! with NULL for absent grouping columns). The analyzer must mirror that.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE sales (
            region   TEXT NOT NULL,
            product  TEXT NOT NULL,
            amount   INT  NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── GROUPING SETS ───────────────────────────────────────────────────────────

#[test]
fn grouping_sets_promotes_omitted_columns_to_nullable() {
    let db = setup();
    // Two grouping sets: one groups by `region`, the other groups by
    // nothing. PG fills `region` with NULL on the second-set rows, so the
    // analyzer should report `region` as nullable.
    let s = db
        .analyze(
            "SELECT region, SUM(amount) AS total FROM sales \
             GROUP BY GROUPING SETS ((region), ())",
        )
        .unwrap();
    assert_cols(&s, vec![cn("region", text()), cn("total", int8())]);
}

#[test]
fn grouping_sets_explicit_two_sets() {
    let db = setup();
    // GROUP BY GROUPING SETS ((region), (product)) — both columns become
    // nullable.
    let s = db
        .analyze(
            "SELECT region, product, COUNT(*) AS n FROM sales \
             GROUP BY GROUPING SETS ((region), (product))",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("region", text()), cn("product", text()), c("n", int8())],
    );
}

// ── ROLLUP / CUBE ───────────────────────────────────────────────────────────

#[test]
fn rollup_makes_grouped_columns_nullable() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT region, product, SUM(amount) AS total FROM sales \
             GROUP BY ROLLUP(region, product)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("region", text()),
            cn("product", text()),
            cn("total", int8()),
        ],
    );
}

#[test]
fn cube_makes_grouped_columns_nullable() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT region, product, COUNT(*) AS n FROM sales \
             GROUP BY CUBE(region, product)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("region", text()), cn("product", text()), c("n", int8())],
    );
}

// ── GROUPING() function ─────────────────────────────────────────────────────

#[test]
fn grouping_function_returns_int4_not_null() {
    let db = setup();
    // `GROUPING(col)` returns int4 marking whether `col` is part of the
    // current grouping set. Always defined → NOT NULL.
    let s = db
        .analyze(
            "SELECT region, GROUPING(region) AS g, COUNT(*) AS n FROM sales \
             GROUP BY ROLLUP(region)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("region", text()), c("g", int4()), c("n", int8())],
    );
}

// ── HAVING with GROUPING SETS ───────────────────────────────────────────────

#[test]
fn grouping_sets_with_having() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT region, SUM(amount) AS total FROM sales \
             GROUP BY GROUPING SETS ((region), ()) \
             HAVING SUM(amount) > 0",
        )
        .unwrap();
    assert_cols(&s, vec![cn("region", text()), cn("total", int8())]);
}

#[test]
fn ungrouped_column_with_rollup_rejected() {
    // The 42803 rule applies through the grouping-set family: `amount`
    // appears in no grouping set of ROLLUP(region), so projecting it bare
    // must error.
    let db = setup();
    let err = db
        .analyze("SELECT amount FROM sales GROUP BY ROLLUP (region)")
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "column \"sales.amount\" must appear in the GROUP BY clause or be used in an aggregate function"
        ),
        "got: {err}"
    );
    // Columns inside any set stay projectable.
    db.analyze("SELECT region, count(*) FROM sales GROUP BY ROLLUP (region)")
        .unwrap();
    db.analyze("SELECT region, product FROM sales GROUP BY GROUPING SETS ((region), (product))")
        .unwrap();
}

// ── GROUPING() placement and argument rules (parse_agg.c) ──────────────────

#[test]
fn grouping_arguments_must_be_grouping_expressions() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tg (n int NOT NULL, ni int);")
        .unwrap();
    for sql in [
        "SELECT GROUPING(ni) FROM tg GROUP BY n",
        "SELECT GROUPING(n) FROM tg",
        "SELECT n FROM tg GROUP BY n HAVING GROUPING(ni) = 0",
        "SELECT n FROM tg GROUP BY n ORDER BY GROUPING(ni)",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "arguments to GROUPING must be grouping expressions of the associated query level"
        );
    }
    for sql in [
        "SELECT GROUPING(tg.n) AS a, GROUPING(n + 1) AS b FROM tg GROUP BY n, n + 1",
        "SELECT GROUPING(n) AS g FROM tg GROUP BY ROLLUP(n)",
        "SELECT n, GROUPING(n) AS g FROM tg GROUP BY 1",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn grouping_placement_rules() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tg (n int NOT NULL, ni int);")
        .unwrap();
    for (sql, msg) in [
        (
            "SELECT n FROM tg WHERE GROUPING(n) = 0 GROUP BY n",
            "grouping operations are not allowed in WHERE",
        ),
        (
            "SELECT n FROM tg GROUP BY n, GROUPING(n)",
            "grouping operations are not allowed in GROUP BY",
        ),
        (
            "SELECT tg.n FROM tg JOIN tg t2 ON GROUPING(tg.n) = 0 GROUP BY tg.n",
            "grouping operations are not allowed in JOIN conditions",
        ),
        (
            "SELECT 1 FROM tg JOIN tg t2 ON count(*) > 0",
            "aggregate functions are not allowed in JOIN conditions",
        ),
        (
            "SELECT n, sum(GROUPING(n)) FROM tg GROUP BY n",
            "aggregate function calls cannot be nested",
        ),
        (
            "SELECT GROUPING(n) AS g FROM tg GROUP BY 1",
            "aggregate functions are not allowed in GROUP BY",
        ),
        (
            "SELECT GROUPING(n) AS g FROM tg GROUP BY g",
            "aggregate functions are not allowed in GROUP BY",
        ),
        (
            "SELECT count(*) AS c FROM tg GROUP BY 1",
            "aggregate functions are not allowed in GROUP BY",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::GroupingError(_), msg);
    }
}
