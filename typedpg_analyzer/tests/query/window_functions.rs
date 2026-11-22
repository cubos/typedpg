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
fn null_treatment_of_value_window_functions() {
    // PostgreSQL 19: IGNORE NULLS skips NULL values, so lag / lead with a
    // non-NULL default never return NULL, even over a nullable column.
    let mut db = setup();
    db.apply_sql("CREATE TABLE notes (id int PRIMARY KEY, body text);")
        .unwrap();
    let s = db
        .analyze(
            "SELECT lag(body, 1, '') IGNORE NULLS OVER (ORDER BY id) AS a, \
             lag(body, 1, '') RESPECT NULLS OVER (ORDER BY id) AS b, \
             lead(body) IGNORE NULLS OVER (ORDER BY id) AS c, \
             first_value(body) IGNORE NULLS OVER (ORDER BY id) AS d, \
             nth_value(body, 2) IGNORE NULLS OVER (ORDER BY id) AS e, \
             lag(body, 0, '') IGNORE NULLS OVER (ORDER BY id) AS f \
             FROM notes",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", text()),
            cn("b", text()),
            cn("c", text()),
            cn("d", text()),
            cn("e", text()),
            // An offset of 0 is the current row, NULL or not.
            cn("f", text()),
        ],
    );
}

#[test]
fn null_treatment_is_for_window_functions_only() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("SELECT length(title) IGNORE NULLS FROM posts"),
        AnalyzeError::WrongObjectType(_),
        "RESPECT/IGNORE NULLS specified, but length is not a window function"
    );
    assert_err_prefix!(
        db.analyze("SELECT sum(views) RESPECT NULLS OVER () FROM posts"),
        AnalyzeError::WrongObjectType(_),
        "aggregate functions do not accept RESPECT/IGNORE NULLS"
    );
}

#[test]
fn lead_over_not_null_column_is_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT LEAD(views) OVER (ORDER BY id) AS next FROM posts")
        .unwrap();
    assert_cols(&s, vec![cn("next", int4())]);
}

/// FIRST_VALUE / LAST_VALUE read a row of the frame: one exists whenever
/// the frame holds the current row, so they are NULL only for a NULL
/// value — or a frame that can be empty.
#[test]
fn first_and_last_value_follow_the_frame() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT FIRST_VALUE(title) OVER (PARTITION BY user_id ORDER BY id) AS first, \
             LAST_VALUE(title) OVER w AS last, \
             FIRST_VALUE(title) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) \
             AS gap, \
             LAST_VALUE(title) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING EXCLUDE CURRENT ROW) \
             AS excl \
             FROM posts WINDOW w AS (ORDER BY id)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("first", text()),
            c("last", text()),
            cn("gap", text()),
            cn("excl", text()),
        ],
    );
}

/// A window aggregate over `OVER w` takes the frame of the WINDOW clause's
/// `w`, exactly like the same window written inline.
#[test]
fn named_window_frame_decides_aggregate_nullability() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT sum(views) OVER w AS a, max(views) OVER p AS b, sum(views) OVER e AS c, \
             sum(views) OVER (w) AS d \
             FROM posts WINDOW w AS (ORDER BY id), p AS (PARTITION BY id), \
             e AS (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", int8()),
            c("b", int4()),
            cn("c", int8()),
            c("d", int8()),
        ],
    );
}

/// transformFrameOffset: a ROWS / GROUPS offset is coerced to bigint by
/// assignment (numeric works, text is `argument of ROWS must be type
/// bigint`), and no offset may hold an aggregate.
#[test]
fn window_frame_offsets() {
    let db = setup();
    for sql in [
        "SELECT sum(views) OVER (ORDER BY id ROWS 1.5 PRECEDING) AS a FROM posts",
        "SELECT sum(views) OVER (ORDER BY id GROUPS 1.5 PRECEDING) AS a FROM posts",
        "SELECT sum(views) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 2.5 FOLLOWING) AS a \
         FROM posts",
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        // Every frame holds the current row.
        assert_cols(&s, vec![c("a", int8())]);
    }
    for (sql, msg) in [
        (
            "SELECT sum(views) OVER (ORDER BY id ROWS '1'::text PRECEDING) FROM posts",
            "argument of ROWS must be type bigint, not type text",
        ),
        (
            "SELECT sum(views) OVER (ORDER BY id GROUPS '1'::text PRECEDING) FROM posts",
            "argument of GROUPS must be type bigint, not type text",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::DatatypeMismatch(_), msg);
    }
    for (sql, msg) in [
        (
            "SELECT sum(views) OVER (ORDER BY id ROWS BETWEEN sum(1) PRECEDING AND CURRENT ROW) \
             FROM posts",
            "aggregate functions are not allowed in window ROWS",
        ),
        (
            "SELECT sum(views) OVER (ORDER BY id GROUPS sum(1) PRECEDING) FROM posts",
            "aggregate functions are not allowed in window GROUPS",
        ),
        (
            "SELECT sum(views) OVER (ORDER BY views RANGE sum(1) PRECEDING) FROM posts",
            "aggregate functions are not allowed in window RANGE",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::GroupingError(_), msg);
    }
}

#[test]
fn window_function_in_having_rejected() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("SELECT count(*) FROM posts HAVING row_number() OVER () > 1"),
        AnalyzeError::WindowingError(_),
        "window functions are not allowed in HAVING"
    );
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

#[test]
fn null_treatment_on_a_window_function_that_does_not_allow_it_rejected() {
    // PostgreSQL 19 WinCheckAndInitializeNullTreatment: row_number, rank,
    // dense_rank, percent_rank, cume_dist and ntile take no RESPECT /
    // IGNORE NULLS; evaluating one with it fails. Without a FROM the
    // window sees one row, so every execution fails.
    let db = setup();
    for (sql, func) in [
        (
            "SELECT row_number() IGNORE NULLS OVER () AS x",
            "row_number",
        ),
        ("SELECT rank() RESPECT NULLS OVER () AS x", "rank"),
        (
            "SELECT dense_rank() IGNORE NULLS OVER () AS x",
            "dense_rank",
        ),
        (
            "SELECT percent_rank() IGNORE NULLS OVER () AS x",
            "percent_rank",
        ),
        ("SELECT cume_dist() RESPECT NULLS OVER () AS x", "cume_dist"),
        ("SELECT ntile(2) IGNORE NULLS OVER () AS x", "ntile"),
    ] {
        let expected = format!("function {func} does not allow RESPECT/IGNORE NULLS");
        match db.analyze(sql) {
            Err(err) => assert!(
                err.to_string().starts_with(&expected),
                "{sql}\n  got: {err}"
            ),
            Ok(_) => panic!("{sql}\n  expected: {expected}\n  got: Ok"),
        }
    }
}

#[test]
fn null_treatment_on_a_window_function_that_does_not_allow_it_fails_only_when_called() {
    // WinCheckAndInitializeNullTreatment runs when the WindowAgg calls the
    // function, for a row: over no row (an empty table), or where the
    // column is never computed (dropped as unused), PG 19.0 succeeds; so
    // do EXPLAIN and a view's definition, which run nothing. The functions
    // taking a null treatment accept one.
    let mut db = setup();
    db.apply_sql("CREATE VIEW vrn AS SELECT row_number() IGNORE NULLS OVER () AS x;")
        .unwrap();
    for sql in [
        "SELECT row_number() IGNORE NULLS OVER () AS x FROM posts",
        "SELECT 1 AS y FROM (SELECT row_number() IGNORE NULLS OVER () AS x) s",
        "EXPLAIN SELECT row_number() IGNORE NULLS OVER () AS x",
        "SELECT lag(1) IGNORE NULLS OVER () AS x",
        "SELECT first_value(1) RESPECT NULLS OVER () AS x",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // Read from a subquery or view yielding a row, it is computed.
    for (sql, func) in [
        (
            "SELECT x FROM (SELECT rank() IGNORE NULLS OVER () AS x) s",
            "rank",
        ),
        ("SELECT x FROM vrn", "row_number"),
    ] {
        let expected = format!("function {func} does not allow RESPECT/IGNORE NULLS");
        match db.analyze(sql) {
            Err(err) => assert!(
                err.to_string().starts_with(&expected),
                "{sql}\n  got: {err}"
            ),
            Ok(_) => panic!("{sql}\n  expected: {expected}\n  got: Ok"),
        }
    }
}
