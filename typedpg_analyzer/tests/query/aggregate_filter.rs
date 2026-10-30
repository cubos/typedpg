//! Aggregate modifiers: `FILTER (WHERE cond)`, `DISTINCT`, per-aggregate
//! `ORDER BY`, and `WITHIN GROUP (ORDER BY …)` for ordered-set aggregates.
//! These sit on top of an ordinary aggregate call and must not change its
//! return type — only the set of rows the aggregate sees, or (for
//! ordered-set aggregates) the order in which they're consumed.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            views   INT NOT NULL,
            body    TEXT
         );",
    )
    .unwrap();
    db
}

// ── FILTER (WHERE …) ────────────────────────────────────────────────────────

#[test]
fn count_filter_where_literal() {
    let db = setup();
    // COUNT is always NOT NULL, regardless of the FILTER predicate.
    let s = db
        .analyze("SELECT COUNT(*) FILTER (WHERE views > 10) AS popular FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("popular", int8())]);
}

#[test]
fn sum_filter_where_with_param() {
    let db = setup();
    // SUM with FILTER stays nullable (the FILTER predicate may eliminate
    // every row in the group, leaving SUM with an empty set → NULL).
    // The param inside FILTER is resolved from the column type.
    let s = db
        .analyze(
            "SELECT user_id, SUM(views) FILTER (WHERE views > $p1) AS big_views \
             FROM posts GROUP BY user_id",
        )
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), cn("big_views", int8())]);
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn count_filter_always_false_is_still_non_null() {
    let db = setup();
    // Even when FILTER excludes every row, COUNT still returns 0 — never NULL.
    let s = db
        .analyze("SELECT COUNT(*) FILTER (WHERE false) AS c FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("c", int8())]);
}

#[test]
fn multiple_filters_side_by_side() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT \
               COUNT(*) FILTER (WHERE views > 100) AS many, \
               COUNT(*) FILTER (WHERE views = 0)   AS none \
             FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("many", int8()), c("none", int8())]);
}

/// An aggregate of an outer query in a subquery's FILTER belongs to the
/// outer query: the inner FILTER sees it as a constant.
#[test]
fn filter_may_hold_an_outer_level_aggregate() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE a (x int); CREATE TABLE b (y int);")
        .unwrap();
    for sql in [
        "SELECT (SELECT max(y) FILTER (WHERE sum(x) > 0) FROM b) FROM a",
        "SELECT (SELECT count(*) FILTER (WHERE count(a.x) > y) FROM b) FROM a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // The subquery's own aggregate still can't sit in its FILTER.
    assert_err_prefix!(
        db.analyze("SELECT (SELECT max(y) FILTER (WHERE sum(y) > 0) FROM b) FROM a"),
        AnalyzeError::GroupingError(_),
        "aggregate functions are not allowed in FILTER"
    );
}

/// PG rejects a call FILTER can't hold where it transforms it, after its
/// arguments: the error points at that call, not at the aggregate the
/// FILTER belongs to.
#[test]
fn filter_errors_point_at_the_offending_call() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE a (x int); CREATE TABLE b (y int);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT max(y) FILTER (WHERE sum(y) > 0) FROM b"),
        AnalyzeError::GroupingError(_),
        "\
aggregate functions are not allowed in FILTER
  ╭────
1 │ SELECT max(y) FILTER (WHERE sum(y) > 0) FROM b
  ·                             ─┬─
  ·                              ╰─ aggregate not allowed here
  ╰────
",
    );
    // `sum(sum(y))`: the inner call is transformed, and rejected, first.
    assert_analyze_err!(
        db.analyze("SELECT max(y) FILTER (WHERE y + sum(sum(y)) > 0) FROM b"),
        AnalyzeError::GroupingError(_),
        "\
aggregate functions are not allowed in FILTER
  ╭────
1 │ SELECT max(y) FILTER (WHERE y + sum(sum(y)) > 0) FROM b
  ·                                     ─┬─
  ·                                      ╰─ aggregate not allowed here
  ╰────
",
    );
    assert_analyze_err!(
        db.analyze("SELECT (SELECT max(y) FILTER (WHERE sum(y) > 0) FROM b) FROM a"),
        AnalyzeError::GroupingError(_),
        "\
aggregate functions are not allowed in FILTER
  ╭────
1 │ SELECT (SELECT max(y) FILTER (WHERE sum(y) > 0) FROM b) FROM a
  ·                                     ─┬─
  ·                                      ╰─ aggregate not allowed here
  ╰────
",
    );
    assert_analyze_err!(
        db.analyze("SELECT max(y) FILTER (WHERE row_number() OVER () > 0) FROM b"),
        AnalyzeError::WindowingError(_),
        "\
window functions are not allowed in FILTER
  ╭────
1 │ SELECT max(y) FILTER (WHERE row_number() OVER () > 0) FROM b
  ·                             ─────┬────
  ·                                  ╰─ window function not allowed here
  ╰────
",
    );
    assert_err_prefix!(
        db.analyze("SELECT count(*) FILTER (WHERE grouping(y) = 0) FROM b GROUP BY y"),
        AnalyzeError::GroupingError(_),
        "grouping operations are not allowed in FILTER"
    );
    // A window function or an aggregate in a sublink of the FILTER is the
    // sublink's own.
    for sql in [
        "SELECT y FROM b GROUP BY y \
         HAVING max(y) FILTER (WHERE (SELECT row_number() OVER ()) > 0) > 0",
        "SELECT count(*) FILTER (WHERE (SELECT sum(x) FROM a) > 0) FROM b",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

// ── DISTINCT inside an aggregate ─────────────────────────────────────────────

#[test]
fn count_distinct_int_column() {
    let db = setup();
    // COUNT(DISTINCT x) → int8, still NOT NULL.
    let s = db
        .analyze("SELECT COUNT(DISTINCT user_id) AS authors FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("authors", int8())]);
}

#[test]
fn count_distinct_nullable_column() {
    let db = setup();
    // COUNT(DISTINCT col) ignores NULLs, so still NOT NULL.
    let s = db
        .analyze("SELECT COUNT(DISTINCT body) AS bodies FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("bodies", int8())]);
}

// ── ORDER BY inside an aggregate ─────────────────────────────────────────────

#[test]
fn array_agg_with_order_by() {
    let db = setup();
    // array_agg over a NOT NULL source stays nullable without GROUP BY
    // (empty input → NULL result), and the inner sort doesn't affect the
    // element type.
    let s = db
        .analyze("SELECT array_agg(title ORDER BY id) AS titles FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("titles", array_of(text()))]);
}

#[test]
fn string_agg_with_order_by_desc() {
    let db = setup();
    let s = db
        .analyze("SELECT string_agg(title, ', ' ORDER BY id DESC) AS joined FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("joined", text())]);
}

#[test]
fn array_agg_with_filter_and_order_by() {
    let db = setup();
    // FILTER + ORDER BY stack cleanly on top of array_agg.
    let s = db
        .analyze(
            "SELECT array_agg(title ORDER BY views DESC) FILTER (WHERE views > 0) AS top \
             FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![cn("top", array_of(text()))]);
}

#[test]
fn filter_unknown_column_rejected() {
    // FILTER's predicate must type-check against the FROM scope; typos
    // were historically swallowed because the modifier walk used
    // `let _ = infer_expr(...)`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FILTER (WHERE ghost > 0) FROM posts"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FILTER (WHERE ghost > 0) FROM posts\n",
            "  ·                               ──┬──\n",
            "  ·                                 ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn agg_order_by_unknown_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT array_agg(title ORDER BY ghost) FROM posts"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT array_agg(title ORDER BY ghost) FROM posts\n",
            "  ·                                 ──┬──\n",
            "  ·                                   ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── WITHIN GROUP — ordered-set aggregates ────────────────────────────────────

#[test]
fn percentile_cont_returns_float8() {
    let db = setup();
    // PG: percentile_cont(0.5) WITHIN GROUP (ORDER BY int) -> float8.
    // Result is nullable: WITHIN GROUP yields NULL for empty input.
    let s = db
        .analyze("SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY views) AS median FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("median", float8())]);
}

#[test]
fn percentile_disc_returns_input_type() {
    let db = setup();
    // PG: percentile_disc(numeric) WITHIN GROUP (ORDER BY int) -> int4.
    let s = db
        .analyze("SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY views) AS p50 FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("p50", int4())]);
}

#[test]
fn mode_returns_input_type() {
    let db = setup();
    // PG: mode() WITHIN GROUP (ORDER BY text) -> text.
    let s = db
        .analyze("SELECT mode() WITHIN GROUP (ORDER BY title) AS top_title FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("top_title", text())]);
}

#[test]
fn percentile_cont_array_form_returns_float8_array() {
    let db = setup();
    // PG: percentile_cont(numeric[]) WITHIN GROUP (ORDER BY int) -> float8[].
    let s = db
        .analyze(
            "SELECT percentile_cont(ARRAY[0.25, 0.5, 0.75]) \
                 WITHIN GROUP (ORDER BY views) AS quartiles FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![cn("quartiles", array_of(float8()))]);
}
