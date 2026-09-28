//! Result nullability of function, aggregate and window calls. Every
//! expectation below was observed on PostgreSQL 18 (a NULL-able column is
//! one PG returned NULL for, checked with `IS NULL` on suitable rows).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, s text NOT NULL, arr int[] NOT NULL, j jsonb NOT NULL,
                         r int4range NOT NULL, n numeric NOT NULL, ts timestamp NOT NULL,
                         ns text, narr int[]);",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_nullability(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

// ── Strict builtins that return NULL on non-NULL input (#31) ─────────────────

#[test]
fn strict_builtins_returning_null_for_non_null_input() {
    let db = setup();
    // PG returns NULL for each: r = 'empty' / '[1,)', arr = '{}',
    // n = 'NaN', j = '[null]', s without a 'q'.
    assert_nullability(
        &db,
        "SELECT lower(r) AS a, upper(r) AS b, lower(int4multirange(r)) AS c,
                array_dims(arr) AS d, array_ndims(arr) AS e, regexp_substr(s, 'q') AS f,
                to_regclass(s) AS g, to_regtype(s) AS h, scale(n) AS i, min_scale(n) AS k
         FROM t",
        &[
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", true),
            ("e", true),
            ("f", true),
            ("g", true),
            ("h", true),
            ("i", true),
            ("k", true),
        ],
    );
    assert_nullability(
        &db,
        "SELECT jsonb_array_elements_text(j) AS a FROM t",
        &[("a", true)],
    );
    // The same rule types a scalar function in FROM.
    assert_nullability(
        &db,
        "SELECT g, k FROM t, to_regclass(s) AS g, regexp_split_to_table(s, ',') AS k",
        &[("g", true), ("k", false)],
    );
    assert_nullability(
        &db,
        "SELECT current_setting('foo.bar', true) AS a, pg_get_serial_sequence('t', 'id') AS b,
                pg_get_viewdef(0::oid) AS c, pg_relation_size(0) AS d, pg_get_indexdef(0) AS e,
                uuid_extract_timestamp(gen_random_uuid()) AS f,
                json_array_elements_text('[null]'::json) AS g, to_regproc('nope') AS h,
                to_regnamespace('nope') AS i, to_regrole('nope') AS k, pg_table_size(0) AS l,
                pg_total_relation_size(0) AS m, pg_relation_filenode(0) AS o,
                pg_relation_filepath(0) AS p, pg_filenode_relation(0, 0) AS q,
                pg_get_constraintdef(0) AS r, pg_get_triggerdef(0) AS s, pg_get_ruledef(0) AS u,
                pg_get_functiondef(0) AS v, pg_get_partkeydef(0) AS w,
                pg_get_statisticsobjdef(0) AS x, pg_get_function_arguments(0) AS y,
                pg_describe_object(0, 0, 0) AS z, has_table_privilege(0::oid, 'select') AS aa,
                pg_stat_get_last_vacuum_time(0) AS bb,
                uuid_extract_version('00000000-0000-0000-0000-000000000000') AS cc",
        &[
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", true),
            ("e", true),
            ("f", true),
            ("g", true),
            ("h", true),
            ("i", true),
            ("k", true),
            ("l", true),
            ("m", true),
            ("o", true),
            ("p", true),
            ("q", true),
            ("r", true),
            ("s", true),
            ("u", true),
            ("v", true),
            ("w", true),
            ("x", true),
            ("y", true),
            ("z", true),
            ("aa", true),
            ("bb", true),
            ("cc", true),
        ],
    );
}

#[test]
fn nullability_is_per_overload() {
    let db = setup();
    // `lower(text)` / `substring(text, int)` / `int4(numeric)` never return
    // NULL for non-NULL input; their range / regex / jsonb siblings do.
    assert_nullability(
        &db,
        "SELECT lower(s) AS a, substring(s, 1) AS b, n::int4 AS c, int4(j) AS d,
                substring(s, 'x') AS e, pg_catalog.time(ts) AS f,
                to_char(ts, '') AS g, date_part('month', ts) AS h
         FROM t",
        &[
            ("a", false),
            ("b", false),
            ("c", false),
            ("d", true),
            ("e", true),
            ("f", true),
            ("g", true),
            ("h", true),
        ],
    );
}

#[test]
fn extract_fields_with_infinite_values_are_not_null() {
    let db = setup();
    // `extract(year FROM 'infinity')` is Infinity, `extract(month …)` NULL.
    assert_nullability(
        &db,
        "SELECT EXTRACT(YEAR FROM ts) AS a, EXTRACT(epoch FROM ts::date) AS b,
                EXTRACT(MONTH FROM ts) AS c, date_part('decade', ts) AS d
         FROM t",
        &[("a", false), ("b", false), ("c", true), ("d", false)],
    );
}

// ── Non-strict builtins (#34) and SQL value functions (#13) ──────────────────

#[test]
fn non_strict_builtins_that_return_null() {
    let db = setup();
    // format(NULL), array_cat(NULL, NULL), concat(VARIADIC NULL) and
    // txid_current_if_assigned() (no xid yet) are NULL; inet_client_addr()
    // is NULL over a Unix socket; current_schema() without a search-path
    // schema.
    assert_nullability(
        &db,
        "SELECT format(ns) AS a, format(NULL) AS b, array_cat(narr, narr) AS c,
                array_cat(NULL::int[], NULL::int[]) AS d, concat(VARIADIC NULL::text[]) AS e,
                txid_current_if_assigned() AS f, inet_client_addr() AS g,
                inet_server_addr() AS h, current_schema() AS i
         FROM t",
        &[
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", true),
            ("e", true),
            ("f", true),
            ("g", true),
            ("h", true),
            ("i", true),
        ],
    );
    // …and never for non-NULL arguments, or without VARIADIC.
    assert_nullability(
        &db,
        "SELECT format(s) AS a, array_cat(arr, arr) AS b, concat(ns, NULL) AS c,
                concat(VARIADIC arr) AS d, jsonb_build_object('k', ns) AS e
         FROM t",
        &[
            ("a", false),
            ("b", false),
            ("c", false),
            ("d", false),
            ("e", false),
        ],
    );
}

#[test]
fn system_user_and_current_schema_can_be_null() {
    let db = setup();
    // SYSTEM_USER is NULL under trust auth; CURRENT_SCHEMA with
    // `SET search_path = ''`.
    assert_nullability(
        &db,
        "SELECT system_user AS a, current_schema AS b, current_schema() AS c",
        &[("a", true), ("b", true), ("c", true)],
    );
}

// ── Aggregates and window aggregates (#35, #36, #37) ─────────────────────────

fn agg_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE ta (id int PRIMARY KEY, g int NOT NULL, x int NOT NULL, f float8 NOT NULL,
                          y int);
         CREATE FUNCTION f_sfunc_returning_null(int, int) RETURNS int
             AS $$ SELECT NULL::int $$ LANGUAGE sql;
         CREATE AGGREGATE my_nullagg(int) (SFUNC = f_sfunc_returning_null, STYPE = int,
                                           INITCOND = '0');",
    )
    .unwrap();
    db
}

#[test]
fn sample_statistics_are_null_for_a_single_row_group() {
    let db = agg_setup();
    // A one-row group gives NULL for the sample statistics, and corr /
    // regr_slope / regr_intercept / regr_r2 also for zero variance; avg and
    // the population statistics are never NULL over rows.
    assert_nullability(
        &db,
        "SELECT g, stddev(x) AS a, variance(x) AS b, stddev_samp(x) AS c, var_samp(x) AS d,
                corr(x, f) AS e, covar_samp(x, f) AS h, regr_slope(f, x) AS i,
                regr_intercept(f, x) AS k, regr_r2(f, x) AS l, avg(x) AS n, stddev_pop(x) AS o
         FROM ta GROUP BY g",
        &[
            ("g", false),
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", true),
            ("e", true),
            ("h", true),
            ("i", true),
            ("k", true),
            ("l", true),
            ("n", false),
            ("o", false),
        ],
    );
}

#[test]
fn user_defined_aggregate_can_return_null_for_a_group() {
    let db = agg_setup();
    assert_nullability(
        &db,
        "SELECT my_nullagg(x) AS m FROM ta GROUP BY g",
        &[("m", true)],
    );
}

#[test]
fn window_aggregate_over_a_frame_that_can_be_empty() {
    let db = agg_setup();
    // `a` and `b` are NULL on the first row (the frame is empty), `c` on
    // the last.
    assert_nullability(
        &db,
        "SELECT g, max(g) OVER (ORDER BY g ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS a,
                sum(g) OVER (ORDER BY g ROWS CURRENT ROW EXCLUDE CURRENT ROW) AS b,
                sum(g) OVER (ORDER BY g ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) AS c,
                sum(g) OVER (ORDER BY g ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS d
         FROM ta GROUP BY g",
        &[
            ("g", false),
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", false),
        ],
    );
}

#[test]
fn lag_with_default_and_a_nullable_offset() {
    let db = agg_setup();
    // NULL when the offset y is NULL.
    assert_nullability(
        &db,
        "SELECT lag(x, y, 0) OVER (ORDER BY id) AS a, lag(x, 1, 0) OVER (ORDER BY id) AS b
         FROM ta",
        &[("a", true), ("b", false)],
    );
}
