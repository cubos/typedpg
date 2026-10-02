//! Nullability narrowed by what conditions prove beyond the columns they
//! are strict in: three-valued AND / OR whose arms guard each other, IS
//! [NOT] DISTINCT FROM, quals that fail for a NULL column through
//! non-strict wrappers (`coalesce(b, 0) > 0`), row comparisons, facts about
//! whole expressions reused where the same expression is read again
//! (`j ->> 'k'`, a scalar subquery, an aggregate in HAVING), row-wise IS
//! NOT NULL, quantified comparisons over non-NULL sets, built-ins whose
//! NULL depends only on arguments the call spells out, and EXISTS /
//! LATERAL correlation quals. Every NOT NULL below is checked against
//! PostgreSQL 18 by the pg_sanity soundness oracle; every nullable one pins
//! a near miss that must stay nullable.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE pair AS (x int, y int);
         CREATE TABLE t (
             id int PRIMARY KEY, a int NOT NULL, b int, c int, n text, fn boolean,
             j jsonb NOT NULL, pn pair, g int NOT NULL
         );
         CREATE TABLE u (id int PRIMARY KEY, v int NOT NULL, w int);
         CREATE TABLE fa (id int PRIMARY KEY, v int NOT NULL);
         CREATE TABLE fb (id int PRIMARY KEY, w int REFERENCES fa);
         CREATE TABLE fc (id int PRIMARY KEY);
         CREATE FUNCTION lax(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE FUNCTION vol(int) RETURNS int LANGUAGE sql VOLATILE AS 'SELECT $1';",
    )
    .unwrap();
    db
}

/// Each output column's nullability (`true` = nullable).
#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

// ── AND / OR: an arm read knowing the arms before it ─────────────────────────

#[test]
fn guarded_and_or_and_constant_arms_are_never_null() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT (b IS NULL OR b > 0) AS c1, (b IS NOT NULL AND b > 0) AS c2,
                fn AND false AS c3, fn OR true AS c4,
                (NOT (b IS NOT NULL) OR b = 1) AS c5,
                (b IS NULL OR c IS NULL OR b < c) AS c6
         FROM t",
        &[
            ("c1", false),
            ("c2", false),
            ("c3", false),
            ("c4", false),
            ("c5", false),
            ("c6", false),
        ],
    );
}

#[test]
fn unguarded_and_or_stay_nullable() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT (b IS NULL OR c > 0) AS c1, (b IS NOT NULL AND c > 0) AS c2,
                fn AND true AS c3, fn OR false AS c4,
                (b IS NOT NULL OR b > 0) AS c5, (b IS NULL AND b > 0) AS c6
         FROM t",
        &[
            ("c1", true),
            ("c2", true),
            ("c3", true),
            ("c4", true),
            ("c5", true),
            ("c6", true),
        ],
    );
}

// ── IS [NOT] DISTINCT FROM ───────────────────────────────────────────────────

#[test]
fn not_distinct_from_a_non_null_value_narrows() {
    let db = setup();
    for qual in [
        "b IS NOT DISTINCT FROM 1",
        "b IS NOT DISTINCT FROM a",
        "1 IS NOT DISTINCT FROM b",
        "NOT (b IS DISTINCT FROM 1)",
        "b IS DISTINCT FROM NULL",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT b FROM t WHERE {qual}"),
            &[("b", false)],
        );
    }
    assert_nullable(
        &db,
        "SELECT CASE WHEN b IS DISTINCT FROM NULL THEN b ELSE 0 END AS c1,
                CASE WHEN b IS NOT DISTINCT FROM NULL THEN 0 ELSE b END AS c2,
                CASE WHEN b IS DISTINCT FROM 1 THEN 0 ELSE b END AS c3
         FROM t",
        &[("c1", false), ("c2", false), ("c3", false)],
    );
}

#[test]
fn distinct_from_or_from_a_nullable_value_does_not_narrow() {
    let db = setup();
    for qual in [
        "b IS NOT DISTINCT FROM c",
        "b IS DISTINCT FROM 1",
        "b IS NOT DISTINCT FROM NULL",
        "NOT (b IS NOT DISTINCT FROM 1)",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT b FROM t WHERE {qual}"),
            &[("b", true)],
        );
    }
}

// ── NULL substitution through non-strict wrappers ────────────────────────────

#[test]
fn quals_false_for_a_null_column_narrow_it() {
    let db = setup();
    for qual in [
        "coalesce(b, 0) > 0",
        "greatest(b, 0) > 0",
        "least(b, 5) < 2",
        "nullif(b, 0) IS NOT NULL",
        "coalesce(b > 0, false)",
        "coalesce(b, 0) <> 0",
        "num_nulls(b) = 0",
        "coalesce(b, lax(a)) > 0 AND coalesce(b, 0) = 1",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT b FROM t WHERE {qual}"),
            &[("b", false)],
        );
    }
    assert_nullable(
        &db,
        "SELECT n FROM t WHERE concat(n, 'x') = 'ax'",
        &[("n", false)],
    );
}

#[test]
fn quals_that_may_hold_for_a_null_column_do_not_narrow() {
    let db = setup();
    for qual in [
        "coalesce(b, 1) > 0",
        "coalesce(b, c) > 0",
        "greatest(b, c) > 0",
        "lax(b) > 0",
        "nullif(b, 0) IS NULL",
        "coalesce(b > 0, true)",
        "num_nulls(b, c) = 1",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT b FROM t WHERE {qual}"),
            &[("b", true)],
        );
    }
    assert_nullable(
        &db,
        "SELECT n FROM t WHERE concat(n, 'x') = 'x'",
        &[("n", true)],
    );
}

#[test]
fn when_true_for_a_null_column_narrows_the_branches_after_it() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT CASE WHEN coalesce(b, 0) = 0 THEN 0 ELSE b END AS c1,
                CASE WHEN (b IS NULL) = true THEN 0 ELSE b END AS c2,
                CASE WHEN num_nulls(b) > 0 THEN 0 ELSE b END AS c3,
                CASE WHEN coalesce(b, 1) = 0 THEN 0 ELSE b END AS c4,
                CASE WHEN coalesce(b, c) = 0 THEN 0 ELSE b END AS c5
         FROM t",
        &[
            ("c1", false),
            ("c2", false),
            ("c3", false),
            ("c4", true),
            ("c5", true),
        ],
    );
}

// ── Row comparisons ──────────────────────────────────────────────────────────

#[test]
fn row_equality_narrows_every_field() {
    let db = setup();
    for qual in [
        "(a, b) = (1, 2)",
        "ROW(b) = ROW(1)",
        "(a, b) IN ((1, 2), (2, 1))",
        "(b, c) = (c, 1)",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT b FROM t WHERE {qual}"),
            &[("b", false)],
        );
    }
}

#[test]
fn row_inequality_and_ordering_do_not_narrow() {
    let db = setup();
    for qual in [
        "(a, b) < (1, 2)",
        "(a, b) <> (1, 2)",
        "NOT ((a, b) = (1, 2))",
        "(a, b) NOT IN ((1, 2))",
        "((a, b) = (1, 2)) IS NOT TRUE",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT b FROM t WHERE {qual}"),
            &[("b", true)],
        );
    }
}

// ── Facts about whole expressions ────────────────────────────────────────────

#[test]
fn an_expression_proven_non_null_is_non_null_where_read_again() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT j ->> 'k' AS v FROM t WHERE j ->> 'k' IS NOT NULL",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "SELECT j ->> 'x' AS v FROM t WHERE j ->> 'x' = 'a'",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "SELECT (pn).x AS x FROM t WHERE (pn).x = 1",
        &[("x", false)],
    );
    assert_nullable(
        &db,
        "SELECT j -> 'y' AS v, length(j ->> 'y') AS l FROM t WHERE (j -> 'y') IS NOT NULL
             AND j ->> 'y' IS NOT NULL",
        &[("v", false), ("l", false)],
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN j ->> 'k' IS NOT NULL THEN j ->> 'k' ELSE 'z' END AS v,
                (j ->> 'k' IS NULL OR length(j ->> 'k') > 1) AS w
         FROM t",
        &[("v", false), ("w", false)],
    );
    // Grouped by what the expression is computed from: still the WHERE's
    // rows' values.
    assert_nullable(
        &db,
        "SELECT j ->> 'k' AS v FROM t WHERE j ->> 'k' IS NOT NULL GROUP BY j",
        &[("v", false)],
    );
}

#[test]
fn expression_facts_need_the_same_reusable_expression() {
    let db = setup();
    // Volatile: each evaluation may differ.
    assert_nullable(
        &db,
        "SELECT vol(b) AS v FROM t WHERE vol(b) IS NOT NULL",
        &[("v", true)],
    );
    // Another entry's column.
    assert_nullable(
        &db,
        "SELECT t.j ->> 'k' AS v FROM t JOIN t AS t2 ON t2.id = t.id
         WHERE t2.j ->> 'k' IS NOT NULL",
        &[("v", true)],
    );
    // Another key.
    assert_nullable(
        &db,
        "SELECT j ->> 'k' AS v FROM t WHERE j ->> 'x' IS NOT NULL",
        &[("v", true)],
    );
    // A grouping set nulls `j` out after WHERE.
    assert_nullable(
        &db,
        "SELECT j ->> 'k' AS v FROM t WHERE j ->> 'k' IS NOT NULL GROUP BY ROLLUP (j)",
        &[("v", true)],
    );
    // Not where the arm proving it may be the one that is TRUE.
    assert_nullable(
        &db,
        "SELECT j ->> 'k' AS v FROM t WHERE j ->> 'k' IS NOT NULL OR a > 0",
        &[("v", true)],
    );
    // RETURNING reads the row as SET rewrote it.
    assert_nullable(
        &db,
        "UPDATE t SET j = '{}' WHERE j ->> 'k' IS NOT NULL RETURNING j ->> 'k' AS v",
        &[("v", true)],
    );
    // Facts don't reach into a subquery.
    assert_nullable(
        &db,
        "SELECT (SELECT t2.j ->> 'k' FROM t AS t2 LIMIT 1) AS v FROM t AS t2
         WHERE t2.j ->> 'k' IS NOT NULL",
        &[("v", true)],
    );
}

#[test]
fn a_scalar_subquery_proven_non_null_is_non_null_where_read_again() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT (SELECT max(v) FROM u) AS m WHERE (SELECT max(v) FROM u) IS NOT NULL",
        &[("m", false)],
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN (SELECT max(v) FROM u WHERE u.id = t.a) IS NOT NULL
                     THEN (SELECT max(v) FROM u WHERE u.id = t.a) ELSE 0 END AS m
         FROM t",
        &[("m", false)],
    );
    // Grouped: every group has a row WHERE passed. Not grouped, an
    // aggregate query yields its row even when WHERE passes none.
    assert_nullable(
        &db,
        "SELECT (SELECT max(v) FROM u) AS m, count(*) AS n FROM t
         WHERE (SELECT max(v) FROM u) IS NOT NULL GROUP BY a",
        &[("m", false), ("n", false)],
    );
    assert_nullable(
        &db,
        "SELECT (SELECT max(v) FROM u) AS m, count(*) AS n FROM t
         WHERE (SELECT max(v) FROM u) IS NOT NULL",
        &[("m", true), ("n", false)],
    );
    assert_nullable(
        &db,
        "SELECT (SELECT max(v) FROM u) AS m, (SELECT max(t.b)) AS x FROM t
         WHERE (SELECT max(v) FROM u) IS NOT NULL",
        &[("m", true), ("x", true)],
    );
    // Rows a LIMIT picks may differ between two scans; a volatile function
    // differs between two calls.
    assert_nullable(
        &db,
        "SELECT (SELECT w FROM u LIMIT 1) AS m WHERE (SELECT w FROM u LIMIT 1) IS NOT NULL",
        &[("m", true)],
    );
    assert_nullable(
        &db,
        "SELECT (SELECT max(vol(w)) FROM u) AS m WHERE (SELECT max(vol(w)) FROM u) IS NOT NULL",
        &[("m", true)],
    );
}

#[test]
fn having_facts_about_an_aggregate_narrow_the_same_aggregate() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT g, max(b) FROM t GROUP BY g HAVING max(b) > 0",
        &[("g", false), ("max", false)],
    );
    assert_nullable(
        &db,
        "SELECT g, sum(b) FROM t GROUP BY g HAVING sum(b) IS NOT NULL",
        &[("g", false), ("sum", false)],
    );
    assert_nullable(
        &db,
        "SELECT max(a) FROM t HAVING max(a) IS NOT NULL",
        &[("max", false)],
    );
    assert_nullable(
        &db,
        "SELECT g, min(b), max(c) FROM t GROUP BY g HAVING max(b) > 0 OR max(c) IS NULL",
        &[("g", false), ("min", true), ("max", true)],
    );
}

// ── Composite fields ─────────────────────────────────────────────────────────

#[test]
fn row_wise_is_not_null_narrows_the_fields() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT (pn).x AS x, (pn).y AS y FROM t WHERE pn IS NOT NULL",
        &[("x", false), ("y", false)],
    );
    // Not NULL is not every field non-NULL.
    assert_nullable(
        &db,
        "SELECT (pn).x AS x FROM t WHERE NOT (pn IS NULL)",
        &[("x", true)],
    );
    assert_nullable(
        &db,
        "SELECT (pn).x AS x FROM t WHERE pn IS DISTINCT FROM NULL",
        &[("x", true)],
    );
}

#[test]
fn a_row_cast_to_a_composite_keeps_its_fields_nullability() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT (ROW(a, a)::pair).x AS x, (ROW(a, b)::pair).y AS y,
                (ROW(1, b)::pair).x AS z
         FROM t",
        &[("x", false), ("y", true), ("z", false)],
    );
}

// ── Quantified comparisons ───────────────────────────────────────────────────

#[test]
fn quantified_comparisons_over_non_null_sets_are_never_null() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT a IN (SELECT id FROM t) AS c1, a = ANY(ARRAY(SELECT id FROM t)) AS c2,
                a = ALL(SELECT id FROM t) AS c3, a > ANY(SELECT v FROM u) AS c4,
                (a, g) IN (SELECT id, v FROM u) AS c5
         FROM t",
        &[
            ("c1", false),
            ("c2", false),
            ("c3", false),
            ("c4", false),
            ("c5", false),
        ],
    );
    assert_nullable(
        &db,
        "SELECT b IN (SELECT id FROM t) AS c1, a IN (SELECT b FROM t) AS c2,
                a = ANY(ARRAY(SELECT b FROM t)) AS c3, a = ALL(SELECT w FROM u) AS c4
         FROM t",
        &[("c1", true), ("c2", true), ("c3", true), ("c4", true)],
    );
}

// ── Built-ins NULL only for argument values the call spells out ──────────────

#[test]
fn value_gated_builtins_are_non_null_with_safe_arguments() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT jsonb_path_exists(j, '$.a') AS c1, to_char(now(), 'YYYY-MM') AS c2,
                extract(day from now()) AS c3, array_length(ARRAY[a, b], 1) AS c4,
                array_upper(ARRAY[a], 1) AS c5, array_ndims(ARRAY[a]) AS c6,
                to_char(interval '1 day', 'HH') AS c7,
                jsonb_path_exists(j, '$.a', silent => false) AS c8,
                extract(month from current_date) AS c9,
                to_char(CURRENT_TIMESTAMP, 'YYYY') AS c10
         FROM t",
        &[
            ("c1", false),
            ("c2", false),
            ("c3", false),
            ("c4", false),
            ("c5", false),
            ("c6", false),
            ("c7", false),
            ("c8", false),
            ("c9", false),
            ("c10", false),
        ],
    );
    assert_nullable(
        &db,
        "SELECT jsonb_path_exists(j, '$.a', '{}', true) AS c1, to_char(now(), '') AS c2,
                array_length(ARRAY[a], 2) AS c3, to_char(interval 'infinity', 'HH') AS c4,
                array_length(ARRAY[ARRAY[a]], 1) AS c5,
                to_char(now() + interval '1 day', 'YYYY') AS c6
         FROM t",
        &[
            ("c1", true),
            ("c2", true),
            ("c3", true),
            ("c4", true),
            ("c5", true),
            ("c6", true),
        ],
    );
}

// ── EXISTS / LATERAL correlation quals ───────────────────────────────────────

#[test]
fn exists_quals_narrow_the_outer_columns() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT fb.w FROM fb WHERE EXISTS (SELECT 1 FROM fa WHERE fa.id = fb.w)",
        &[("w", false)],
    );
    assert_nullable(
        &db,
        "SELECT fa.v FROM fb LEFT JOIN fa ON fa.id = fb.w
         WHERE EXISTS (SELECT 1 FROM fc WHERE fc.id = fa.id)",
        &[("v", false)],
    );
    assert_nullable(
        &db,
        "SELECT t.b FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.v = t.b)",
        &[("b", false)],
    );
    assert_nullable(
        &db,
        "SELECT t.b FROM t WHERE a > 0 AND EXISTS (SELECT 1 FROM u WHERE u.v = t.b GROUP BY u.v)",
        &[("b", false)],
    );
    assert_nullable(
        &db,
        "SELECT t.b FROM t, LATERAL (SELECT 1 FROM u WHERE u.v = t.b) s",
        &[("b", false)],
    );
    assert_nullable(
        &db,
        "SELECT t.b FROM t JOIN LATERAL (SELECT 1 FROM u WHERE u.v = t.b) s ON true",
        &[("b", false)],
    );
    assert_nullable(
        &db,
        "SELECT CASE WHEN EXISTS (SELECT 1 FROM u WHERE u.v = t.b) THEN t.b ELSE 0 END AS b
         FROM t",
        &[("b", false)],
    );
}

#[test]
fn exists_that_may_hold_without_the_qual_does_not_narrow() {
    let db = setup();
    for qual in [
        "NOT EXISTS (SELECT 1 FROM u WHERE u.v = t.b)",
        "EXISTS (SELECT count(*) FROM u WHERE u.v = t.b)",
        "EXISTS (SELECT 1 FROM u WHERE u.v = t.b HAVING true)",
        "EXISTS (SELECT 1 FROM u WHERE u.v = t.b GROUP BY ())",
        "EXISTS (SELECT 1 FROM u WHERE u.v = t.b UNION ALL SELECT 1)",
        "EXISTS (SELECT 1 FROM u WHERE u.v = t.b) OR a > 0",
        "EXISTS (SELECT 1 FROM u WHERE u.v = t.b) IS NOT TRUE",
        // The inner `t` hides the outer one.
        "EXISTS (SELECT 1 FROM t WHERE t.b = 1)",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT t.b FROM t WHERE {qual}"),
            &[("b", true)],
        );
    }
    for from in [
        "t LEFT JOIN LATERAL (SELECT 1 FROM u WHERE u.v = t.b) s ON true",
        "t, LATERAL (SELECT count(*) FROM u WHERE u.v = t.b) s",
        "u LEFT JOIN (t CROSS JOIN LATERAL (SELECT 1 FROM u AS u2 WHERE u2.v = t.b) s) ON true",
    ] {
        assert_nullable(&db, &format!("SELECT t.b FROM {from}"), &[("b", true)]);
    }
}
