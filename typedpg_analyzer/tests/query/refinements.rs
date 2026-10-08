//! Refinements: what the analysis knows of a value beyond its type and
//! nullability (`AnalyzedColumn::refinement`), and what it proves with it.
//! Under `pg_sanity`, every column refined finite is checked never to come
//! back infinite, over rows seeded with infinite dates, timestamps and
//! intervals.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, ts timestamptz NOT NULL, d date NOT NULL,
                         i interval NOT NULL, n int NOT NULL,
                         fin timestamptz NOT NULL CHECK (isfinite(fin)),
                         fin_d date CHECK (isfinite(fin_d)));",
    )
    .unwrap();
    db
}

/// Each column's (name, nullable, finite).
#[track_caller]
fn assert_finite(db: &PgCatalog, sql: &str, expected: &[(&str, bool, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable, c.refinement.finite))
        .collect();
    assert_eq!(
        actual, expected,
        "(name, nullable, finite) mismatch for `{sql}`"
    );
}

#[test]
fn the_current_time_literals_and_arithmetic_over_them_are_finite() {
    let db = setup();
    assert_finite(
        &db,
        "SELECT now() AS a, current_date AS b, CURRENT_TIMESTAMP AS c, LOCALTIMESTAMP AS e,
                '2024-01-01'::date AS f, DATE '2024-01-01' AS g, interval '1 day' AS h,
                now() + interval '1 day' AS j, current_date + 1 AS k, current_date - 1 AS l,
                now() - now() AS m, date_trunc('day', now()) AS o, now()::date AS p,
                now() AT TIME ZONE 'UTC' AS q, make_date(2024, 1, 1) AS r,
                to_timestamp(0) AS s, -interval '1 day' AS u, interval '1 day' * 2 AS v",
        &[
            ("a", false, true),
            ("b", false, true),
            ("c", false, true),
            ("e", false, true),
            ("f", false, true),
            ("g", false, true),
            ("h", false, true),
            ("j", false, true),
            ("k", false, true),
            ("l", false, true),
            ("m", false, true),
            ("o", false, true),
            ("p", false, true),
            ("q", false, true),
            ("r", false, true),
            ("s", false, true),
            ("u", false, true),
            ("v", false, true),
        ],
    );
}

#[test]
fn what_may_be_infinite_is_not_finite() {
    let db = setup();
    assert_finite(
        &db,
        "SELECT ts, d, i, 'infinity'::date AS a, now() + 'infinity'::interval AS b,
                now() + i AS c, ts + interval '1 day' AS e, '-Infinity'::timestamp AS f,
                interval '1 day' * 'inf'::float8 AS g, to_timestamp('2024', 'YYYY') AS h
         FROM t",
        &[
            ("ts", false, false),
            ("d", false, false),
            ("i", false, false),
            ("a", false, false),
            ("b", false, false),
            ("c", false, false),
            ("e", false, false),
            ("f", false, false),
            ("g", false, false),
            ("h", false, false),
        ],
    );
}

#[test]
fn a_check_or_a_domain_keeps_a_column_finite() {
    let mut db = setup();
    db.apply_sql(
        "CREATE DOMAIN moment AS timestamptz CHECK (isfinite(VALUE));
         CREATE DOMAIN later_moment AS moment;
         CREATE TABLE u (id int PRIMARY KEY, m moment, l later_moment, raw timestamptz,
                         off_ts timestamptz CHECK (isfinite(off_ts)) NOT ENFORCED,
                         either_ts timestamptz CHECK (isfinite(either_ts) OR id > 0));
         CREATE TABLE p (a date, CHECK (isfinite(a)) NO INHERIT);
         CREATE TABLE p_child () INHERITS (p);
         CREATE FUNCTION moment_of(timestamptz) RETURNS moment LANGUAGE sql AS 'SELECT $1';",
    )
    .unwrap();
    assert_finite(
        &db,
        "SELECT fin, fin_d, ts FROM t",
        &[
            ("fin", false, true),
            ("fin_d", true, true),
            ("ts", false, false),
        ],
    );
    assert_finite(
        &db,
        "SELECT m, l, off_ts, either_ts FROM u",
        &[
            ("m", true, true),
            ("l", true, true),
            ("off_ts", true, false),
            ("either_ts", true, false),
        ],
    );
    // Coerced to the domain, a value is checked: an infinite one fails the
    // query.
    assert_finite(
        &db,
        "SELECT raw::moment AS a, moment_of(raw) AS b FROM u",
        &[("a", true, true), ("b", true, true)],
    );
    assert_finite(&db, "SELECT a FROM p", &[("a", true, false)]);
}

/// With `pg_catalog` searched after `public`, the user's `isfinite` of the
/// same signature is the one a CHECK calls.
#[test]
fn a_user_isfinite_proves_nothing() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION public.isfinite(date) RETURNS boolean LANGUAGE sql IMMUTABLE
             AS 'SELECT true';
         SET search_path = public, pg_catalog;
         CREATE TABLE c (a date CHECK (isfinite(a)), b date CHECK (pg_catalog.isfinite(b)));
         RESET search_path;",
    )
    .unwrap();
    assert_finite(
        &db,
        "SELECT a, b FROM c",
        &[("a", true, false), ("b", true, true)],
    );
}

#[test]
fn finiteness_travels_through_queries_and_conditionals() {
    let db = setup();
    assert_finite(
        &db,
        "SELECT a, b FROM (SELECT fin AS a, ts AS b FROM t) s",
        &[("a", false, true), ("b", false, false)],
    );
    assert_finite(
        &db,
        "WITH c AS (SELECT fin AS a, ts AS b FROM t) SELECT a, b FROM c",
        &[("a", false, true), ("b", false, false)],
    );
    assert_finite(
        &db,
        "SELECT fin AS a, fin AS b FROM t UNION ALL SELECT now(), ts FROM t",
        &[("a", false, true), ("b", false, false)],
    );
    assert_finite(
        &db,
        "SELECT CASE WHEN n > 0 THEN fin ELSE now() END AS a,
                CASE WHEN n > 0 THEN fin END AS b,
                CASE WHEN n > 0 THEN fin ELSE ts END AS c,
                coalesce(fin_d, current_date) AS e, coalesce(fin_d, d) AS f,
                greatest(fin, now()) AS g, least(fin, ts) AS h
         FROM t",
        &[
            ("a", false, true),
            ("b", true, true),
            ("c", false, false),
            ("e", false, true),
            ("f", false, false),
            ("g", false, true),
            ("h", false, false),
        ],
    );
    // A date is converted to a timestamp, which keeps it finite.
    assert_finite(
        &db,
        "SELECT CASE WHEN n > 0 THEN fin ELSE current_date END AS a FROM t",
        &[("a", false, true)],
    );
    assert_finite(
        &db,
        "SELECT max(fin) AS a, min(ts) AS b, max(fin) OVER () AS c,
                lag(fin) OVER (ORDER BY id) AS e, lag(fin, 1, ts) OVER (ORDER BY id) AS f
         FROM t GROUP BY id, fin, ts",
        &[
            ("a", false, true),
            ("b", false, false),
            ("c", false, true),
            ("e", true, true),
            ("f", false, false),
        ],
    );
    assert_finite(
        &db,
        "SELECT g, h FROM generate_series(now(), now() + interval '1 day', interval '1 hour') g,
                          generate_series(now(), now() + interval '1 day', (SELECT i FROM t LIMIT 1)) h",
        &[("g", false, true), ("h", false, false)],
    );
    // A recursive CTE's column is finite when both arms are.
    assert_finite(
        &db,
        "WITH RECURSIVE r(a, b, k) AS (SELECT now(), now(), 1 UNION ALL
                                        SELECT a + interval '1 day', b + (SELECT i FROM t LIMIT 1), k + 1
                                        FROM r WHERE k < 3)
         SELECT a, b FROM r",
        &[("a", false, true), ("b", true, false)],
    );
}

#[test]
fn a_finite_value_formats_and_extracts_to_non_null() {
    let db = setup();
    assert_finite(
        &db,
        "SELECT to_char(fin, 'YYYY') AS a, extract(month FROM fin) AS b,
                date_part('day', fin_d) AS c, to_char(ts, 'YYYY') AS e,
                extract(month FROM d) AS f, to_char(i, 'HH24') AS g,
                to_char(x, 'YYYY') AS h
         FROM t, LATERAL (SELECT CASE WHEN n > 0 THEN fin ELSE now() END AS x) s",
        &[
            ("a", false, false),
            ("b", false, false),
            ("c", true, false),
            ("e", true, false),
            ("f", true, false),
            ("g", true, false),
            ("h", false, false),
        ],
    );
}
