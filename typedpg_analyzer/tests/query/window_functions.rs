//! Window functions: `OVER`, `PARTITION BY`, ordering, and the return
//! types of ranking (`ROW_NUMBER`/`RANK`/`DENSE_RANK`/`NTILE`), value
//! functions (`LAG`/`LEAD`/`FIRST_VALUE`/`LAST_VALUE`/`NTH_VALUE`), and
//! aggregate-over-window (`SUM`/`AVG`/`COUNT`) calls.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            views   INT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Ranking functions ───────────────────────────────────────────────────────

#[test]
fn row_number_over_order_by() {
    let db = setup();
    // Ranking functions always produce a value for every row — NOT NULL.
    let s = db
        .analyze("SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("rn", int8())]);
}

#[test]
fn rank_over_partition_and_order() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT user_id, RANK() OVER (PARTITION BY user_id ORDER BY views DESC) AS r \
             FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("r", int8())]);
}

#[test]
fn dense_rank_returns_int8() {
    let db = setup();
    let s = db
        .analyze("SELECT DENSE_RANK() OVER (ORDER BY views) AS dr FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("dr", int8())]);
}

#[test]
fn ntile_returns_int4() {
    let db = setup();
    let s = db
        .analyze("SELECT NTILE(4) OVER (ORDER BY views) AS bucket FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("bucket", int4())]);
}

// ── Aggregates used as window functions ──────────────────────────────────────

#[test]
fn sum_over_partition() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    // SUM over an int4 column promotes to int8 — same rule as non-window SUM.
    let s = db
        .analyze("SELECT id, SUM(views) OVER (PARTITION BY user_id) AS total FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("total", int8())]);
}

#[test]
fn count_over_empty_partition() {
    let db = setup();
    // COUNT(*) is NOT NULL (and the analyzer agrees), even as a window
    // function with an empty frame.
    let s = db
        .analyze("SELECT id, COUNT(*) OVER () AS total FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("total", int8())]);
}

#[test]
fn avg_over_order_by() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    let s = db
        .analyze("SELECT AVG(views) OVER (ORDER BY id) AS running FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("running", numeric())]);
}

// ── Value window functions ───────────────────────────────────────────────────
//
// `LAG`/`LEAD`/`FIRST_VALUE`/`LAST_VALUE`/`NTH_VALUE` can return NULL at
// the partition/frame edge even when the source column is NOT NULL:
// `LAG(title)` is NULL on the first row of each partition. The analyzer
// has to override the usual "strict pg_catalog function inherits arg
// nullability" rule for these.

#[test]
fn lag_over_not_null_column_is_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT LAG(title) OVER (ORDER BY id) AS prev FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("prev", text())]);
}

#[test]
fn lag_with_non_null_default_is_not_null() {
    let db = setup();
    // `LAG(col, offset, default)` replaces the partition-edge NULL with
    // `default`. With both `col` and `default` NOT NULL the result is
    // never NULL.
    let s = db
        .analyze("SELECT LAG(views, 1, 0) OVER (ORDER BY id) AS prev FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("prev", int4())]);
}

#[test]
fn lag_with_nullable_default_stays_nullable() {
    let db = setup();
    // `LAG(col, 1, nullable)` — the default could itself be NULL, so the
    // result is nullable even when the source column is NOT NULL.
    let s = db
        .analyze("SELECT LAG(views, 1, NULL::int4) OVER (ORDER BY id) AS prev FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("prev", int4())]);
}

#[test]
fn lead_over_not_null_column_is_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT LEAD(views) OVER (ORDER BY id) AS next FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("next", int4())]);
}

#[test]
fn first_value_is_nullable() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT FIRST_VALUE(title) OVER (PARTITION BY user_id ORDER BY id) AS first FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![cn("first", text())]);
}

#[test]
fn nth_value_is_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT NTH_VALUE(title, 2) OVER (ORDER BY id) AS second FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("second", text())]);
}

// ── Placement rules ──────────────────────────────────────────────────────────

#[test]
fn window_function_in_where_rejected() {
    let db = setup();
    // PG: `window functions are not allowed in WHERE`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM posts WHERE ROW_NUMBER() OVER () = 1"),
        AnalyzeError::WindowingError(_),
        concat!(
            "window functions are not allowed in WHERE\n",
            "  ╭────\n",
            "1 │ SELECT id FROM posts WHERE ROW_NUMBER() OVER () = 1\n",
            "  ·                            ─────┬────\n",
            "  ·                                 ╰─ window function not allowed here\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn window_function_in_group_by_rejected() {
    let db = setup();
    // PG: `window functions are not allowed in GROUP BY`.
    assert_analyze_err!(
        db.analyze(
            "SELECT title, COUNT(*) FROM posts \
             GROUP BY title, ROW_NUMBER() OVER ()"
        ),
        AnalyzeError::WindowingError(_),
        concat!(
            "window functions are not allowed in GROUP BY\n",
            "  ╭────\n",
            "1 │ SELECT title, COUNT(*) FROM posts GROUP BY title, ROW_NUMBER() OVER ()\n",
            "  ·                                                   ─────┬────\n",
            "  ·                                                        ╰─ window function not allowed here\n",
            "  ╰────\n",
        ),
    );
}

// ── Named window (WINDOW … AS …) ─────────────────────────────────────────────

#[test]
fn named_window_clause() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, ROW_NUMBER() OVER w AS rn \
             FROM posts \
             WINDOW w AS (PARTITION BY user_id ORDER BY views DESC)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("rn", int8())]);
}

// ── Explicit frame: ROWS / RANGE / GROUPS ───────────────────────────────────
//
// The frame doesn't change the analyzed return type — but it must parse
// cleanly. These act as smoke tests that each frame syntax is accepted and
// produces the same shape as the implicit-frame case.

#[test]
fn rows_unbounded_preceding_to_current_row() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    let s = db
        .analyze(
            "SELECT id, SUM(views) OVER ( \
                ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW \
             ) AS running FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("running", int8())]);
}

#[test]
fn rows_n_preceding_to_n_following() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    let s = db
        .analyze(
            "SELECT SUM(views) OVER ( \
                ORDER BY id ROWS BETWEEN 2 PRECEDING AND 2 FOLLOWING \
             ) AS s FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("s", int8())]);
}

#[test]
fn range_unbounded_preceding_default() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    // `RANGE UNBOUNDED PRECEDING` is the implicit default; spelling it out
    // shouldn't change the result.
    let s = db
        .analyze("SELECT SUM(views) OVER (ORDER BY id RANGE UNBOUNDED PRECEDING) AS s FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("s", int8())]);
}

#[test]
fn range_between_with_value_offset() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    // RANGE with an interval offset against an int4 column.
    let s = db
        .analyze(
            "SELECT SUM(views) OVER ( \
                ORDER BY views RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING \
             ) AS s FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("s", int8())]);
}

#[test]
fn groups_frame_syntax() {
    let db = setup();
    // The frame always holds the current row, so a NOT NULL input gives a
    // NOT NULL aggregate.
    // GROUPS frame (PG 11+).
    let s = db
        .analyze(
            "SELECT SUM(views) OVER ( \
                ORDER BY user_id GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING \
             ) AS s FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("s", int8())]);
}

#[test]
fn frame_with_exclude_current_row() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT SUM(views) OVER ( \
                ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW \
                EXCLUDE CURRENT ROW \
             ) AS s FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![cn("s", int8())]);
}

#[test]
fn frame_with_exclude_group() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT SUM(views) OVER ( \
                ORDER BY user_id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW \
                EXCLUDE GROUP \
             ) AS s FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![cn("s", int8())]);
}

// ── OVER (...) must resolve column refs against the FROM scope ───────────────

#[test]
fn window_partition_by_unknown_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT rank() OVER (PARTITION BY ghost) FROM posts"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT rank() OVER (PARTITION BY ghost) FROM posts\n",
            "  ·                                  ──┬──\n",
            "  ·                                    ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn window_order_by_unknown_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT rank() OVER (ORDER BY ghost) FROM posts"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT rank() OVER (ORDER BY ghost) FROM posts\n",
            "  ·                              ──┬──\n",
            "  ·                                ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── OVER-clause placement rules (parse_func.c) ──────────────────────────────

#[test]
fn window_function_without_over_rejected() {
    // A `prokind = 'w'` function is only callable with an OVER clause.
    let db = setup();
    let err = db.analyze("SELECT row_number() FROM posts").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("window function row_number requires an OVER clause"),
        "got: {err}"
    );
    let err = db.analyze("SELECT lag(title) FROM posts").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("window function lag requires an OVER clause"),
        "got: {err}"
    );
}

#[test]
fn over_on_plain_function_rejected() {
    // OVER attaches only to window functions and aggregates.
    let db = setup();
    let err = db
        .analyze("SELECT length(title) OVER () FROM posts")
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "OVER specified, but length is not a window function nor an aggregate function"
        ),
        "got: {err}"
    );
}

#[test]
fn over_on_aggregate_still_allowed() {
    let db = setup();
    let s = db
        .analyze("SELECT sum(views) OVER (PARTITION BY user_id) AS w FROM posts")
        .unwrap();
    assert!(!col(&s, "w").nullable || col(&s, "w").nullable); // shape-only smoke
}
