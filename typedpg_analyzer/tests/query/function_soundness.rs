//! Soundness of what function, aggregate and window calls are inferred to
//! return, and of the expression matching their rules lean on: rules keyed
//! to `pg_catalog` routines, to parameter positions, to a proof covering
//! every aggregated value, to a frame offset's value, to HAVING's
//! arithmetic, and to PG's `equal()` between two spellings of an
//! expression. Each regression below was NOT NULL (or rejected) though
//! PostgreSQL 18 returns NULL (or accepts it); each near miss pins the
//! narrowing that stays sound.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
             id int PRIMARY KEY, a int NOT NULL, b int, c int, s text NOT NULL,
             ts timestamp NOT NULL, j jsonb NOT NULL
         );",
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

// ── Aggregates of several values ─────────────────────────────────────────────

#[test]
fn separate_having_proofs_do_not_prove_a_row_with_every_value() {
    let db = setup();
    // Rows (1, NULL) and (NULL, 1) pass both counts, yet no row has both:
    // the strict two-value aggregates read nothing.
    assert_nullable(
        &db,
        "SELECT regr_sxx(b, c) AS r, covar_pop(b, c) AS cp, regr_avgx(b, c) AS ax
         FROM t HAVING count(b) > 0 AND count(c) > 0",
        &[true, true, true],
    );
    assert_nullable(
        &db,
        "SELECT regr_sxy(b, c) AS r FROM t GROUP BY id HAVING count(b) > 0 AND count(c) > 0",
        &[true],
    );
}

#[test]
fn a_having_proof_of_the_only_nullable_value_still_proves_a_row() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT regr_sxx(b, a) AS r, covar_pop(a, b) AS cp, regr_avgx(b, a) AS ax
         FROM t HAVING count(b) > 0",
        &[false, false, false],
    );
    assert_nullable(
        &db,
        "SELECT max(b) AS m FROM t GROUP BY id HAVING count(b) > 0",
        &[false],
    );
}

// ── Frame offsets ────────────────────────────────────────────────────────────

#[test]
fn a_prefixed_integer_frame_offset_is_not_zero() {
    let db = setup();
    // `int8` reads `'0xa'` as 10, `'0o7'` as 7, `'0b1'` as 1: frames past
    // the current row, empty near the partition's end.
    assert_nullable(
        &db,
        "SELECT first_value(a) OVER (ORDER BY id ROWS BETWEEN '0xa' FOLLOWING AND '0xa' FOLLOWING) AS f,
                sum(a) OVER (ORDER BY id ROWS BETWEEN '0xa' FOLLOWING AND '0xa' FOLLOWING) AS g,
                last_value(a) OVER (ORDER BY id ROWS BETWEEN '0o7' FOLLOWING AND '0o7' FOLLOWING) AS h,
                max(a) OVER (ORDER BY id ROWS BETWEEN '0b1' FOLLOWING AND '0b1' FOLLOWING) AS i
         FROM t",
        &[true, true, true, true],
    );
}

#[test]
fn a_zero_frame_offset_is_the_current_row() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT first_value(a) OVER (ORDER BY id ROWS BETWEEN '0' PRECEDING AND ' 00 ' FOLLOWING) AS f,
                sum(a) OVER (ORDER BY id ROWS BETWEEN 0 FOLLOWING AND 0 FOLLOWING) AS g,
                last_value(a) OVER (ORDER BY ts RANGE BETWEEN '0 days' PRECEDING
                                    AND interval '0 hour' FOLLOWING) AS h,
                max(a) OVER (ORDER BY id GROUPS BETWEEN '0'::int8 PRECEDING AND 0 FOLLOWING) AS i
         FROM t",
        &[false, false, false, false],
    );
}

// ── Rules of the pg_catalog window functions ─────────────────────────────────

fn setup_shadowing_aggregates() -> PgCatalog {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION null2(int, int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT NULL::int';
         CREATE FUNCTION null4(int, int, int, int) RETURNS int
             LANGUAGE sql IMMUTABLE AS 'SELECT NULL::int';
         CREATE AGGREGATE public.first_value(int) (sfunc = null2, stype = int, initcond = '0');
         CREATE AGGREGATE public.last_value(int) (sfunc = null2, stype = int, initcond = '0');
         CREATE AGGREGATE public.lag(int, int, int) (sfunc = null4, stype = int, initcond = '0');
         CREATE AGGREGATE public.count(int) (sfunc = null2, stype = int, initcond = '0');",
    )
    .unwrap();
    db
}

#[test]
fn an_aggregate_named_like_a_value_window_function_is_its_own() {
    let db = setup_shadowing_aggregates();
    // `public.*(int…)` match the integer arguments exactly: the calls are
    // the user aggregates, whose transition function returns NULL.
    assert_nullable(
        &db,
        "SELECT first_value(a) OVER () AS f, last_value(a) OVER (ORDER BY id) AS l,
                lag(a, 1, 0) OVER () AS g, public.lag(a, 1, 0) OVER (ORDER BY id) AS h,
                count(a) OVER () AS c, count(a) AS d
         FROM t GROUP BY a, id",
        &[true, true, true, true, true, true],
    );
}

#[test]
fn the_pg_catalog_value_window_functions_keep_their_rules() {
    let db = setup_shadowing_aggregates();
    assert_nullable(
        &db,
        "SELECT pg_catalog.first_value(a) OVER () AS f,
                pg_catalog.last_value(a) OVER (ORDER BY id) AS l,
                pg_catalog.lag(a, 1, 0) OVER (ORDER BY id) AS g,
                lag(s, 2, 'x') OVER (ORDER BY id) AS h,
                last_value(s) OVER (ORDER BY id) AS k,
                pg_catalog.count(a) OVER () AS c
         FROM t",
        &[false, false, false, false, false, false],
    );
}

// ── Named arguments ──────────────────────────────────────────────────────────

#[test]
fn string_agg_reads_the_argument_named_value() {
    let db = setup();
    // The value is `b`, written second: NULL over rows whose `b` is NULL.
    assert_nullable(
        &db,
        "SELECT string_agg(delimiter => ',', value => b::text) OVER () AS x FROM t",
        &[true],
    );
}

#[test]
fn named_arguments_bind_by_name_for_the_positional_rules() {
    let db = setup();
    // A NULL delimiter appends nothing; the value is never NULL.
    assert_nullable(
        &db,
        "SELECT string_agg(delimiter => c::text, value => s) OVER () AS x,
                string_agg(value => s, delimiter => ',') OVER (ORDER BY id) AS y
         FROM t",
        &[false, false],
    );
    // `silent` (parameter 3) bound by name, after a defaulted `vars`.
    assert_nullable(
        &db,
        "SELECT jsonb_path_exists(path => '$.a', target => j) AS x,
                jsonb_path_exists(silent => false, path => '$.a', target => j) AS y,
                jsonb_path_exists(target => j, path => '$.a', silent => true) AS z
         FROM t",
        &[false, false, true],
    );
}

// ── HAVING arithmetic ────────────────────────────────────────────────────────

#[test]
fn having_arithmetic_is_exact_over_an_empty_input() {
    let db = setup();
    // Each HAVING is TRUE for the empty input's aggregate row (count 0):
    // `0.1 + 0.2 = 0.3` in numeric, `0.6` and `0.5` round to 1 as int,
    // `9007199254740993 > 9007199254740992` — none an f64 sees.
    for having in [
        "count(*) + 0.1 + 0.2 = 0.3",
        "(count(*) + 0.6)::int = 1",
        "(count(*) + 0.5)::int = 1",
        "(count(*) - 0.5)::bigint = -1",
        "count(*) + 9007199254740993 > 9007199254740992",
        "count(*) * 0.1 + 1e-1 = 0.1",
        "count(b) + 0.1 + 0.2 = 0.3",
        "count(*)::float8 + 0.1 > 0",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT max(a) AS m FROM t HAVING {having}"),
            &[true],
        );
    }
}

#[test]
fn having_arithmetic_still_refutes_an_empty_input() {
    let db = setup();
    for having in [
        "count(*) + 0.1 > 0.2",
        "(count(*) + 0.4)::int = 1",
        "(count(*) + 1.5)::int = 3",
        "count(*) * 2.5 >= 2.5",
        "-count(*) < 0",
        "count(*) + 9007199254740992 > 9007199254740992",
        "count(*)::numeric - 0.5 > -0.5",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT max(a) AS m FROM t HAVING {having}"),
            &[false],
        );
    }
}

#[test]
fn a_partial_index_predicate_is_implied_by_exact_decimals() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE p (x numeric, y int);
         CREATE UNIQUE INDEX p_y ON p (y) WHERE x < 0.3;",
    )
    .unwrap();
    // `x < 0.30000000000000001` admits values `x < 0.3` doesn't; as f64s
    // the two literals are one number.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO p VALUES (0.1, 1) ON CONFLICT (y) WHERE x < 0.30000000000000001 DO NOTHING"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "there is no unique or exclusion constraint matching the ON CONFLICT specification on table \"p\"",
    );
    for clause in [
        "x < 0.29999999999999999",
        "x < 0.3",
        "x <= 0.2e0",
        "x = .25",
    ] {
        db.analyze(&format!(
            "INSERT INTO p VALUES (0.1, 1) ON CONFLICT (y) WHERE {clause} DO NOTHING"
        ))
        .unwrap();
    }
}

// ── Expressions compared as PG's equal() does ────────────────────────────────

#[test]
fn a_grouped_list_expression_is_one_key_wherever_it_is_written() {
    let db = setup();
    // PG 18 records where `IN (…)` and `ARRAY[…]` lists start: positions
    // that differ between the select list and GROUP BY, and that equal()
    // ignores. The total row of the ROLLUP has them NULL.
    assert_nullable(
        &db,
        "SELECT a IN (1, 2) AS x, array[a] AS y, a = ANY (array[1]) AS z
         FROM t GROUP BY ROLLUP (a IN (1, 2), array[a], a = ANY (array[1]))",
        &[true, true, true],
    );
    assert_nullable(
        &db,
        "SELECT a IN (1, 2) AS x, array[a] AS y FROM t GROUP BY a IN (1, 2), array[a]",
        &[false, false],
    );
}

#[test]
fn a_list_expression_matches_itself_in_order_by_and_aggregate_arguments() {
    let db = setup();
    db.analyze("SELECT DISTINCT a IN (1, 2) AS x FROM t ORDER BY a IN (1, 2)")
        .unwrap();
    db.analyze("SELECT DISTINCT array[a] AS x FROM t ORDER BY array[a]")
        .unwrap();
    db.analyze("SELECT DISTINCT ON (array[a]) id FROM t ORDER BY array[a], id")
        .unwrap();
    db.analyze("SELECT array_agg(DISTINCT a IN (1, 2) ORDER BY a IN (1, 2)) AS x FROM t")
        .unwrap();
}

#[test]
fn an_expression_proven_non_null_is_found_again_with_its_list() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT b = ANY (array[1, 2]) AS x FROM t WHERE (b = ANY (array[1, 2])) IS NOT NULL",
        &[false],
    );
}

#[test]
fn two_spellings_of_a_cast_are_one_grouping_expression() {
    let db = setup();
    // `CAST(a AS bigint)` names `pg_catalog.int8`, `a::int8` just `int8`:
    // one coercion once transformed.
    assert_nullable(
        &db,
        "SELECT cast(a AS bigint) AS x FROM t GROUP BY ROLLUP (a::int8)",
        &[true],
    );
    assert_nullable(
        &db,
        "SELECT a::int8 AS x, array[a]::_int4 AS y
         FROM t GROUP BY ROLLUP (cast(a AS bigint), array[a]::int[])",
        &[true, true],
    );
    db.analyze("SELECT DISTINCT cast(a AS bigint) AS x FROM t ORDER BY a::int8")
        .unwrap();
    db.analyze("SELECT array_agg(DISTINCT a::int8 ORDER BY cast(a AS bigint)) AS x FROM t")
        .unwrap();
    db.analyze("SELECT cast(a AS bigint) AS x, a::int8 AS x FROM t ORDER BY x")
        .unwrap();
}

#[test]
fn casts_to_other_types_are_other_grouping_expressions() {
    let db = setup();
    for sql in [
        "SELECT a::int4 AS x FROM t GROUP BY ROLLUP (a::int8)",
        "SELECT a::text AS x FROM t GROUP BY ROLLUP (a::varchar)",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "column \"t.a\" must appear in the GROUP BY clause or be used in an aggregate function"
        );
    }
}

// ── Rules of other pg_catalog functions ──────────────────────────────────────

#[test]
fn a_user_similar_to_escape_is_not_the_builtin_one() {
    let mut db = setup();
    // pg_catalog's escapes `(` into no valid regex.
    for sql in [
        "SELECT id FROM t WHERE s ~ similar_to_escape('(')",
        "SELECT id FROM t WHERE s ~ pg_catalog.similar_to_escape('(')",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    db.apply_sql(
        "CREATE FUNCTION public.similar_to_escape(text) RETURNS text
             LANGUAGE sql IMMUTABLE AS $$ SELECT 'a' $$;",
    )
    .unwrap();
    // The pattern is whatever the user function returns: no regex to check.
    db.analyze("SELECT id FROM t WHERE s ~ public.similar_to_escape('(')")
        .unwrap();
}
