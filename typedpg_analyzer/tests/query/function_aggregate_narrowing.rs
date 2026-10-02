//! Nullability narrowed by what builtin functions, set-returning functions
//! and aggregates do with their inputs: which arguments' NULL reaches the
//! result, which OUT columns a SRF fills, how many rows a SRF over
//! constants yields, which aggregates keep NULL inputs, when a group or a
//! window frame has rows, and what HAVING proves of a group. Each NOT
//! NULL below is checked against PostgreSQL 18 by the pg_sanity soundness
//! oracle; each near miss pins a case that can be NULL.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
             id int PRIMARY KEY, a int NOT NULL, b int, g int NOT NULL,
             s text NOT NULL, n text, ts timestamptz NOT NULL,
             arr int[] NOT NULL, narr int[], j jsonb NOT NULL,
             f bool NOT NULL, d date NOT NULL
         );
         CREATE TABLE u (id int PRIMARY KEY, t_id int NOT NULL REFERENCES t, v int NOT NULL, w int);",
    )
    .unwrap();
    db
}

/// Each output column's nullability (`true` = nullable).
#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[bool]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<bool> = s.columns.iter().map(|c| c.nullable).collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

/// The first output column's element nullability.
#[track_caller]
fn elements(db: &PgCatalog, sql: &str) -> Option<bool> {
    match db.analyze(sql).unwrap().columns[0].pg_type {
        Type::Array {
            element_nullable, ..
        } => element_nullable,
        ref other => panic!("`{sql}` is not an array: {other:?}"),
    }
}

// ── Builtin scalar functions ─────────────────────────────────────────────────

#[test]
fn extract_over_an_interval_with_an_infinite_safe_field() {
    let db = setup();
    // ±Infinity for an infinite interval, never NULL.
    assert_nullable(
        &db,
        "SELECT extract(epoch FROM now() - ts), extract(hour FROM now() - ts),
                date_part('day', now() - ts), extract(year FROM age(ts)),
                date_part('millennium', ts - now())
         FROM t",
        &[false, false, false, false, false],
    );
    // NULL for an infinite interval: `minute`, `second`, `month`, `week`.
    assert_nullable(
        &db,
        "SELECT extract(minute FROM now() - ts), date_part('second', now() - ts),
                extract(month FROM now() - ts), date_part('week', now() - ts)
         FROM t",
        &[true, true, true, true],
    );
}

#[test]
fn non_strict_builtins_null_only_on_some_arguments() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT format('%s-%s', s, n), format('%L', n), string_to_array(s, ','),
                string_to_array(s, n), array_remove(arr, NULL), array_replace(arr, NULL, 0),
                array_positions(arr, NULL), array_remove(arr, b), arr || narr, narr || arr,
                array_to_string(arr, ',', n), array_cat(narr, arr)
         FROM t",
        &[false; 12],
    );
    // The argument whose NULL is the result's.
    assert_nullable(
        &db,
        "SELECT format(n, s), string_to_array(n, ','), array_remove(narr, NULL),
                array_replace(narr, 1, 2), narr || narr, array_to_string(arr, n),
                array_to_string(narr, ',', 'x')
         FROM t",
        &[true; 7],
    );
}

#[test]
fn array_literals_and_scalar_subqueries_keep_element_nullability() {
    let db = setup();
    assert_eq!(elements(&db, "SELECT '{1,2}'::int[]"), Some(false));
    assert_eq!(elements(&db, "SELECT '{1,NULL}'::int[]"), Some(true));
    assert_eq!(elements(&db, "SELECT ARRAY[1, 2]::int[]"), Some(false));
    // An element conversion may map elements to NULL (`int4(jsonb)` for a
    // JSON null).
    assert_eq!(elements(&db, "SELECT ARRAY[j]::int[] FROM t"), Some(true));
    assert_eq!(
        elements(&db, "SELECT ARRAY[s]::varchar[] FROM t"),
        Some(false)
    );
    assert_eq!(elements(&db, "SELECT (SELECT ARRAY[1, 2])"), Some(false));
    assert_eq!(
        elements(&db, "SELECT (SELECT array_agg(v) FROM u)"),
        Some(false)
    );
    assert_eq!(
        elements(&db, "SELECT (SELECT array_agg(w) FROM u)"),
        Some(true)
    );
    assert_eq!(
        elements(&db, "SELECT (SELECT coalesce(array_agg(v), '{}') FROM u)"),
        Some(false)
    );
    assert_eq!(
        elements(&db, "SELECT coalesce((SELECT array_agg(v) FROM u), '{}')"),
        Some(false)
    );
    assert_eq!(
        elements(
            &db,
            "SELECT coalesce((SELECT array_agg(v) FROM u), '{NULL}')"
        ),
        Some(true)
    );
    assert_eq!(
        elements(
            &db,
            "SELECT CASE WHEN f THEN ARRAY[a] ELSE '{1}' END FROM t"
        ),
        Some(false)
    );
    assert_eq!(
        elements(
            &db,
            "SELECT CASE WHEN f THEN ARRAY[a] ELSE ARRAY[b] END FROM t"
        ),
        Some(true)
    );
}

// ── Set-returning functions ──────────────────────────────────────────────────

#[test]
fn json_each_fills_its_key_and_its_json_value() {
    let db = setup();
    for sql in [
        "SELECT kv.key, kv.value FROM t, jsonb_each(t.j) kv",
        "SELECT e.key, e.value FROM t CROSS JOIN LATERAL json_each(t.j::json) e",
    ] {
        assert_nullable(&db, sql, &[false, false]);
    }
    // The `_text` variants map a JSON null to SQL NULL.
    for sql in [
        "SELECT e.key, e.value FROM t, jsonb_each_text(t.j) e",
        "SELECT e.key, e.value FROM t, json_each_text(t.j::json) e",
    ] {
        assert_nullable(&db, sql, &[false, true]);
    }
    // In the select list too.
    assert_nullable(
        &db,
        "SELECT (jsonb_each(j)).key, (jsonb_each_text(j)).value FROM t",
        &[true, true],
    );
    assert_nullable(&db, "SELECT (jsonb_each(j)).key FROM t", &[false]);
}

#[test]
fn unnest_emits_the_elements_its_argument_is_known_to_have() {
    let db = setup();
    for sql in [
        "SELECT x FROM t, unnest(ARRAY(SELECT id FROM t)) x",
        "SELECT x FROM unnest('{1,2}'::int[]) x",
        "SELECT x FROM t, unnest(array_remove(ARRAY[a, b], NULL)) x",
        "SELECT x FROM t, unnest(string_to_array(t.s, ',')) x",
        "SELECT x FROM t, unnest(regexp_split_to_array(t.s, ',')) x",
        "SELECT x FROM t, unnest(array_remove(t.arr, NULL)) x",
        "SELECT unnest(array_remove(t.arr, NULL)) FROM t",
    ] {
        assert_nullable(&db, sql, &[false]);
    }
    for sql in [
        "SELECT x FROM unnest('{1,NULL}'::int[]) x",
        "SELECT x FROM t, unnest(t.arr) x",
        "SELECT x FROM t, unnest(string_to_array(t.s, ',', 'a')) x",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}

#[test]
fn srfs_of_the_same_static_length_are_not_padded() {
    let db = setup();
    for sql in [
        "SELECT unnest(ARRAY[1, 2]), unnest(ARRAY[3, 4])",
        "SELECT a, b FROM unnest(ARRAY[1, 2], ARRAY[3, 4]) z(a, b)",
        "SELECT generate_series(1, 3), unnest('{7,8,9}'::int[])",
        "SELECT x, y FROM ROWS FROM (generate_series(5, 1, -2), unnest(ARRAY[1, 2, 3])) z(x, y)",
        "SELECT unnest(ARRAY[a, a]), unnest(ARRAY[1, 2]) FROM t",
    ] {
        assert_nullable(&db, sql, &[false, false]);
    }
    for sql in [
        "SELECT unnest(ARRAY[1, 2]), unnest(ARRAY[3])",
        "SELECT a, b FROM unnest(ARRAY[1, 2], ARRAY[3]) z(a, b)",
        "SELECT generate_series(1, 3), unnest(ARRAY[1, 2])",
        // A multidimensional constructor unrolls every dimension.
        "SELECT unnest(ARRAY[arr, arr]), unnest(ARRAY[1, 2]) FROM t",
        "SELECT unnest(arr), unnest(arr) FROM t",
    ] {
        assert_nullable(&db, sql, &[true, true]);
    }
}

// ── Set operations ───────────────────────────────────────────────────────────

#[test]
fn except_a_null_row_removes_every_null() {
    let db = setup();
    for sql in [
        "SELECT b FROM t EXCEPT SELECT NULL::int",
        "SELECT b FROM t EXCEPT SELECT NULL",
        "SELECT b FROM t EXCEPT VALUES (2), (NULL)",
    ] {
        assert_nullable(&db, sql, &[false]);
    }
    for sql in [
        // A single NULL right row removes only one NULL left row.
        "SELECT b FROM t EXCEPT ALL SELECT NULL::int",
        // The right arm may be empty.
        "SELECT b FROM t EXCEPT SELECT NULL::int FROM t",
        "SELECT b FROM t EXCEPT SELECT NULL::int WHERE false",
        "SELECT b FROM t EXCEPT VALUES (2)",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
    // Rows compare whole: the other column must match too.
    assert_nullable(
        &db,
        "SELECT b, a FROM t EXCEPT SELECT NULL::int, 1",
        &[true, false],
    );
}

// ── Aggregates ───────────────────────────────────────────────────────────────

#[test]
fn aggregates_keeping_null_inputs_are_never_null_over_rows() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT t.id, array_agg(u.w), json_agg(u.w), jsonb_agg(u), json_object_agg(t.s, u.w),
                jsonb_object_agg(t.s, u.w), json_arrayagg(u.w), json_objectagg(t.s : u.w),
                json_agg_strict(u.w), jsonb_object_agg_strict(t.s, u.w),
                json_arrayagg(u.w NULL ON NULL RETURNING jsonb)
         FROM t LEFT JOIN u ON u.t_id = t.id GROUP BY t.id",
        &[false; 11],
    );
    assert_eq!(
        elements(&db, "SELECT array_agg(b) FROM t GROUP BY g"),
        Some(true)
    );
    // A window frame holding the current row has rows.
    assert_nullable(
        &db,
        "SELECT array_agg(b) OVER (ORDER BY id), json_agg(b) OVER w FROM t
         WINDOW w AS (ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING)",
        &[false, false],
    );
    // No rows: no GROUP BY, a FILTER, an empty frame, the empty grouping set.
    for sql in [
        "SELECT array_agg(a) FROM t",
        "SELECT json_arrayagg(a) FROM t",
        "SELECT array_agg(a) FILTER (WHERE f) FROM t GROUP BY g",
        "SELECT json_objectagg(s : a) FILTER (WHERE f) FROM t GROUP BY g",
        "SELECT array_agg(a) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM t",
        "SELECT jsonb_agg(a) FROM t GROUP BY ROLLUP (g)",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}

#[test]
fn string_agg_ignores_its_delimiter() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT string_agg(s, NULL), string_agg(s, n) FROM t GROUP BY g",
        &[false, false],
    );
    // A NULL value is skipped: an all-NULL group gives NULL.
    assert_nullable(&db, "SELECT string_agg(n, ',') FROM t GROUP BY g", &[true]);
}

#[test]
fn hypothetical_set_aggregates_and_regr_count_are_never_null() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT rank(3) WITHIN GROUP (ORDER BY a), percent_rank(3) WITHIN GROUP (ORDER BY a),
                cume_dist(3) WITHIN GROUP (ORDER BY b), dense_rank(NULL) WITHIN GROUP (ORDER BY a),
                rank(1) WITHIN GROUP (ORDER BY a) FILTER (WHERE f),
                regr_count(a, b), regr_count(b, b) FILTER (WHERE f)
         FROM t WHERE false",
        &[false; 7],
    );
    // Ordered-set aggregates that aren't hypothetical are NULL over no rows.
    assert_nullable(
        &db,
        "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY a), mode() WITHIN GROUP (ORDER BY a)
         FROM t",
        &[true, true],
    );
}

#[test]
fn aggregates_over_sources_that_always_have_rows() {
    let db = setup();
    for sql in [
        "SELECT max(v) FROM (VALUES (1), (2)) v(v)",
        "SELECT max(x) FROM unnest(ARRAY[1, 2, 3]) x",
        "SELECT max(x) FROM generate_series(1, 10) x",
        "SELECT max(x) FROM generate_series(1, 2) x, (VALUES (1)) v(y)",
        "SELECT max(x) FROM (SELECT 1) s(x)",
        "SELECT sum(1)",
        "SELECT max(x) FROM unnest(ARRAY[1]) x LEFT JOIN t ON t.id = x",
    ] {
        assert_nullable(&db, sql, &[false]);
    }
    for sql in [
        "SELECT max(x) FROM generate_series(1, 0) x",
        "SELECT max(x) FROM unnest('{}'::int[]) x",
        "SELECT max(x) FROM generate_series(1, 2) x WHERE x > 5",
        "SELECT max(t.a) FROM t, (VALUES (1)) v(y)",
        "SELECT max(x) FROM unnest(ARRAY[1]) x JOIN t ON t.id = x",
        "SELECT max(v) FROM (VALUES (1) LIMIT 0) v(v)",
        "SELECT max(x) FROM unnest(ARRAY[NULL::int]) x",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}

// ── HAVING ───────────────────────────────────────────────────────────────────

#[test]
fn having_false_over_no_rows_proves_rows() {
    let db = setup();
    for having in [
        "max(a) > 0",
        "NOT (count(*) <= 0)",
        "count(*) FILTER (WHERE f) > 0",
        "count(*) > 0.5",
        "count(*) - 1 >= 0",
        "sum(a) IS NOT NULL",
        "count(*) > 0 OR max(a) > 1",
        "NOT (max(a) > 0)",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT max(a) FROM t HAVING {having}"),
            &[false],
        );
    }
    for having in [
        "count(*) >= 0",
        "max(a) IS NULL",
        "count(*) > 0 OR true",
        "rank(1) WITHIN GROUP (ORDER BY a) > 0",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT max(a) FROM t HAVING {having}"),
            &[true],
        );
    }
    // An aggregate of the outer query says nothing of this one's rows.
    assert_nullable(
        &db,
        "SELECT (SELECT max(u.v) FROM u HAVING count(t.a) > 0) FROM t GROUP BY t.g",
        &[true],
    );
}

#[test]
fn having_a_non_null_value_makes_strict_aggregates_over_it_not_null() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT g, max(b), percentile_cont(0.5) WITHIN GROUP (ORDER BY b), string_agg(b::text, n)
         FROM t GROUP BY g HAVING count(b) > 0",
        &[false, false, false, true],
    );
    assert_nullable(&db, "SELECT sum(b) FROM t HAVING count(b) > 0", &[false]);
    assert_nullable(&db, "SELECT avg(b) FROM t HAVING max(b) > 1", &[false]);
    // Near misses: another column, a FILTER, a statistic NULL over one row,
    // two aggregated columns, a count of rows.
    for sql in [
        "SELECT max(b) FROM t GROUP BY g HAVING count(n) > 0",
        "SELECT max(b) FILTER (WHERE f) FROM t GROUP BY g HAVING count(b) > 0",
        "SELECT stddev_samp(b) FROM t GROUP BY g HAVING count(b) > 0",
        "SELECT regr_avgx(b, b + 1) FROM t GROUP BY g HAVING count(b) > 0",
        "SELECT max(b) FROM t GROUP BY g HAVING count(*) > 0",
        "SELECT max(b) FROM t GROUP BY g HAVING count(b) >= 0",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}

#[test]
fn rows_proved_under_grouping_sets() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT g, max(a) FROM t GROUP BY ROLLUP (g) HAVING count(*) > 0",
        &[true, false],
    );
    assert_nullable(
        &db,
        "SELECT g, max(a) FROM t GROUP BY ROLLUP (g)",
        &[true, true],
    );
    assert_nullable(
        &db,
        "SELECT max(y) FROM generate_series(1, 2) x, generate_series(1, 2) y GROUP BY CUBE (x)",
        &[false],
    );
}

#[test]
fn a_grouping_guard_keeps_the_grouped_value() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT CASE WHEN grouping(g) = 0 THEN g ELSE -1 END,
                CASE WHEN 0 = grouping(g, a) AND f THEN a ELSE 0 END,
                CASE WHEN grouping(g, a) = 0 THEN g + a ELSE 0 END
         FROM t GROUP BY CUBE (g, a, f)",
        &[false, false, false],
    );
    for sql in [
        "SELECT CASE WHEN grouping(g) = 1 THEN g ELSE -1 END FROM t GROUP BY ROLLUP (g)",
        "SELECT CASE WHEN grouping(a) = 0 THEN g ELSE -1 END FROM t GROUP BY ROLLUP (g, a)",
        "SELECT CASE WHEN grouping(g) = 0 OR f THEN g ELSE -1 END FROM t GROUP BY ROLLUP (g, f)",
        // A nullable column stays so.
        "SELECT CASE WHEN grouping(b) = 0 THEN b ELSE -1 END FROM t GROUP BY ROLLUP (b)",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}

#[test]
fn the_first_element_and_length_of_a_non_empty_array_agg() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT (array_agg(a ORDER BY id DESC))[1], array_length(array_agg(a), 1),
                array_upper(array_agg(b), 1), array_lower(array_agg(a), 1)
         FROM t GROUP BY g",
        &[false, false, false, false],
    );
    for sql in [
        "SELECT (array_agg(b))[1] FROM t GROUP BY g",
        "SELECT (array_agg(a))[2] FROM t GROUP BY g",
        "SELECT array_length(array_agg(a), 2) FROM t GROUP BY g",
        "SELECT (array_agg(a))[1] FROM t",
        "SELECT array_length(array_agg(a), 1) FROM t",
        // `array_agg(anyarray)` stacks a dimension.
        "SELECT (array_agg(arr))[1] FROM t GROUP BY g",
        "SELECT (array_agg(a) FILTER (WHERE f))[1] FROM t GROUP BY g",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}

// ── Window functions ─────────────────────────────────────────────────────────

#[test]
fn zero_window_offsets_are_the_current_row() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT lead(a, 0) OVER (ORDER BY id), lag(a, 0, NULL) OVER (ORDER BY id),
                nth_value(a, 1) OVER (PARTITION BY g ORDER BY id),
                max(a) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 0 PRECEDING),
                max(a) OVER (ORDER BY id ROWS BETWEEN 0 FOLLOWING AND 1 FOLLOWING),
                max(a) OVER (ORDER BY d RANGE BETWEEN INTERVAL '1 day' PRECEDING
                                            AND INTERVAL '0 day' PRECEDING),
                max(a) OVER (ORDER BY id GROUPS BETWEEN 0 PRECEDING AND 0 FOLLOWING),
                max(a) OVER w
         FROM t WINDOW w AS (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 0 PRECEDING)",
        &[false; 8],
    );
    for sql in [
        "SELECT lead(a, 1) OVER (ORDER BY id) FROM t",
        "SELECT lag(b, 0) OVER (ORDER BY id) FROM t",
        "SELECT nth_value(a, 2) OVER (ORDER BY id) FROM t",
        "SELECT nth_value(a, 1) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM t",
        "SELECT max(a) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 PRECEDING) FROM t",
        "SELECT max(a) OVER (ORDER BY id ROWS BETWEEN 0 PRECEDING AND 0 FOLLOWING EXCLUDE CURRENT ROW) FROM t",
    ] {
        assert_nullable(&db, sql, &[true]);
    }
}
