//! Refinements: what the analysis knows of a value beyond its type and
//! nullability (`AnalyzedColumn::refinement`), and what it proves with it.
//! Under `pg_sanity`, every column refined finite is checked never to come
//! back infinite, over rows seeded with infinite dates, timestamps and
//! intervals.

use crate::common::*;
use typedpg_analyzer::{OrdPoint, OrdRange};

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

/// PG 17+ reserves an interval's extreme field values for ±infinity, so a
/// literal spelling them is infinite: `interval '2147483647 months
/// 2147483647 days 9223372036854775807 us' = 'infinity'` is true. One
/// microsecond short of them is finite.
#[test]
fn extreme_interval_literals_are_infinite() {
    let db = setup();
    assert_finite(
        &db,
        "SELECT interval '2147483647 months 2147483647 days 9223372036854775807 us' AS a,
                interval '-2147483648 months -2147483648 days -9223372036854775808 us' AS b,
                '178956970 years 7 months 2147483647 days 9223372036854775807 us'::interval AS c,
                interval '2147483647 months 2147483647 days 9223372036854775806 us' AS d",
        &[
            ("a", false, false),
            ("b", false, false),
            ("c", false, false),
            ("d", false, true),
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
    // A record's field is either arm's.
    assert_finite(
        &db,
        "SELECT (u.r).f2 AS a FROM (SELECT ROW(1, now()) AS r
                                    UNION ALL SELECT ROW(2, 'infinity'::timestamptz)) u",
        &[("a", false, false)],
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

// ── Value sets ───────────────────────────────────────────────────────────────

fn values_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
         CREATE DOMAIN ab AS text CHECK (VALUE IN ('a', 'b'));
         CREATE DOMAIN a_of_ab AS ab CHECK (VALUE = 'a' OR VALUE = 'c');
         CREATE TABLE v (id int PRIMARY KEY, kind text NOT NULL CHECK (kind IN ('a', 'b')),
                         m mood NOT NULL, n int NOT NULL CHECK (n = 1 OR n = 2),
                         flag bool, free text, padded char(3) CHECK (padded IN ('a')),
                         amount numeric CHECK (amount IN (1, 2)), d ab, e a_of_ab);",
    )
    .unwrap();
    db
}

/// Each column's (name, value set).
#[track_caller]
fn assert_values(db: &PgCatalog, sql: &str, expected: &[(&str, Option<&[&str]>)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, Option<Vec<&str>>)> = s
        .columns
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.refinement
                    .values
                    .as_ref()
                    .map(|vs| vs.iter().map(String::as_str).collect()),
            )
        })
        .collect();
    let expected: Vec<(&str, Option<Vec<&str>>)> = expected
        .iter()
        .map(|(n, v)| (*n, v.map(<[&str]>::to_vec)))
        .collect();
    assert_eq!(actual, expected, "value sets mismatch for `{sql}`");
}

#[test]
fn a_column_holds_what_its_type_and_checks_allow() {
    let db = values_setup();
    assert_values(
        &db,
        "SELECT kind, n, m, flag, free, padded, amount, d, e FROM v",
        &[
            ("kind", Some(&["a", "b"])),
            ("n", Some(&["1", "2"])),
            ("m", Some(&["happy", "ok", "sad"])),
            ("flag", Some(&["false", "true"])),
            ("free", None),
            // `char(3)` stores `'a  '`; `numeric` 1 may be `1.0`.
            ("padded", None),
            ("amount", None),
            ("d", Some(&["a", "b"])),
            ("e", Some(&["a"])),
        ],
    );
}

#[test]
fn conditions_narrow_a_column_where_it_is_read() {
    let db = values_setup();
    assert_values(
        &db,
        "SELECT kind, m, free FROM v WHERE kind = 'a' AND m <> 'sad' AND free IN ('x', 'y')",
        &[
            ("kind", Some(&["a"])),
            ("m", Some(&["happy", "ok"])),
            ("free", Some(&["x", "y"])),
        ],
    );
    assert_values(
        &db,
        "SELECT CASE WHEN kind = 'a' THEN kind END AS a, CASE m WHEN 'ok' THEN m END AS b FROM v",
        &[("a", Some(&["a"])), ("b", Some(&["ok"]))],
    );
}

#[test]
fn literals_and_conditionals_have_their_values() {
    let db = values_setup();
    assert_values(
        &db,
        "SELECT 'x' AS a, 1 AS b, CASE WHEN n = 1 THEN 'one' ELSE 'two' END AS c,
                CASE WHEN n = 1 THEN 'one' END AS d, CASE WHEN n = 1 THEN 'one' ELSE free END AS e,
                coalesce(free, 'none') AS f, true AS g, 1::int8 AS h, ' 07'::int AS i,
                'yes'::bool AS j, 1.5 AS k, 'abc'::varchar(2) AS l, coalesce(d, 'b') AS o
         FROM v",
        &[
            ("a", Some(&["x"])),
            ("b", Some(&["1"])),
            ("c", Some(&["one", "two"])),
            ("d", Some(&["one"])),
            ("e", None),
            ("f", None),
            ("g", Some(&["true"])),
            ("h", Some(&["1"])),
            ("i", Some(&["7"])),
            ("j", Some(&["true"])),
            ("k", None),
            ("l", None),
            ("o", Some(&["a", "b"])),
        ],
    );
    assert_values(
        &db,
        "SELECT 'a' AS x UNION ALL SELECT 'b' UNION ALL SELECT kind FROM v",
        &[("x", Some(&["a", "b"]))],
    );
    assert_values(
        &db,
        "SELECT 'a' AS x UNION ALL SELECT free FROM v",
        &[("x", None)],
    );
    assert_values(
        &db,
        "SELECT (u.r).f2 AS x FROM (SELECT ROW(1, 'x'::text) AS r
                                    UNION ALL SELECT ROW(2, 'y'::text)) u",
        &[("x", Some(&["x", "y"]))],
    );
    assert_values(
        &db,
        "SELECT free::ab AS a, free::a_of_ab AS b FROM v",
        &[("a", Some(&["a", "b"])), ("b", Some(&["a"]))],
    );
}

#[test]
fn a_case_over_a_refined_column_can_cover_every_value() {
    let db = values_setup();
    let s = db
        .analyze(
            "WITH c AS (SELECT CASE WHEN n = 1 THEN 'one' ELSE 'two' END AS w FROM v)
             SELECT w, CASE w WHEN 'one' THEN 1 WHEN 'two' THEN 2 END AS x,
                    CASE w WHEN 'one' THEN 1 END AS y
             FROM c WHERE w <> 'three'",
        )
        .unwrap();
    let got: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(got, [("w", false), ("x", false), ("y", true)]);
    assert_eq!(
        s.columns[0].refinement.values,
        Some(["one".to_owned(), "two".to_owned()].into())
    );
}

#[test]
fn a_nondeterministic_collation_lets_other_spellings_through() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE DOMAIN ci_ab AS text COLLATE ci CHECK (VALUE IN ('a', 'b'));
         CREATE TABLE c (x text COLLATE ci CHECK (x IN ('a', 'b')), y ci_ab,
                         z text CHECK (z IN ('a', 'b')));",
    )
    .unwrap();
    // `'A'` passes `IN ('a', 'b')` under `ci`.
    assert_values(
        &db,
        "SELECT x, y, z FROM c WHERE x = 'a'",
        &[("x", None), ("y", None), ("z", Some(&["a", "b"]))],
    );
}

// ── Integer ranges ───────────────────────────────────────────────────────────

fn ranges_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN positive AS int CHECK (VALUE > 0);
         CREATE TABLE q (id int PRIMARY KEY, n int NOT NULL CHECK (n BETWEEN 1 AND 100),
                         m int NOT NULL, small int NOT NULL CHECK (small >= 0 AND small < 3),
                         d positive, name text NOT NULL);",
    )
    .unwrap();
    db
}

/// Each column's (name, nullable, lower bound, upper bound).
#[track_caller]
fn assert_ranges(db: &PgCatalog, sql: &str, expected: &[(&str, bool, Option<i128>, Option<i128>)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool, Option<i128>, Option<i128>)> = s
        .columns
        .iter()
        .map(|c| {
            let r = c.refinement.range.unwrap_or_default();
            (c.name.as_str(), c.nullable, r.lo, r.hi)
        })
        .collect();
    assert_eq!(
        actual, expected,
        "(name, nullable, lo, hi) mismatch for `{sql}`"
    );
}

#[test]
fn checks_conditions_and_literals_bound_integers() {
    let db = ranges_setup();
    assert_ranges(
        &db,
        "SELECT n, m, small, d, 7 AS seven FROM q",
        &[
            ("n", false, Some(1), Some(100)),
            ("m", false, None, None),
            ("small", false, Some(0), Some(2)),
            ("d", true, Some(1), None),
            ("seven", false, Some(7), Some(7)),
        ],
    );
    assert_ranges(
        &db,
        "SELECT m FROM q WHERE m > 5 AND m <= 10",
        &[("m", false, Some(6), Some(10))],
    );
    // A few integers are listed as values too.
    assert_values(
        &db,
        "SELECT small, m FROM q WHERE m > 5 AND m <= 7",
        &[("small", Some(&["0", "1", "2"])), ("m", Some(&["6", "7"]))],
    );
}

#[test]
fn arithmetic_counts_and_conditionals_carry_bounds() {
    let db = ranges_setup();
    assert_ranges(
        &db,
        "SELECT n + 1 AS a, n - small AS b, n * 2 AS c, -n AS e, abs(m) AS f,
                length(name) AS g, row_number() OVER () AS h,
                CASE WHEN m > 0 THEN 1 ELSE n END AS i, greatest(m, 0) AS j,
                least(n, 10) AS k, n * m AS l
         FROM q",
        &[
            ("a", false, Some(2), Some(101)),
            ("b", false, Some(-1), Some(100)),
            ("c", false, Some(2), Some(200)),
            ("e", false, Some(-100), Some(-1)),
            ("f", false, Some(0), None),
            ("g", false, Some(0), None),
            ("h", false, Some(1), None),
            ("i", false, Some(1), Some(100)),
            ("j", false, Some(0), None),
            ("k", false, Some(1), Some(10)),
            ("l", false, None, None),
        ],
    );
    assert_ranges(
        &db,
        "SELECT count(*) AS a, sum(small) AS b, min(n) AS c, max(d) AS e FROM q",
        &[
            ("a", false, Some(0), None),
            ("b", true, Some(0), None),
            ("c", true, Some(1), Some(100)),
            ("e", true, Some(1), None),
        ],
    );
}

#[test]
fn nullif_of_a_value_out_of_range_is_not_null() {
    let db = ranges_setup();
    assert_ranges(
        &db,
        "SELECT nullif(n, 0) AS a, nullif(m, 0) AS b, nullif(row_number() OVER (), 0) AS c,
                nullif(small, 3) AS e, nullif(small, 2) AS f
         FROM q",
        &[
            ("a", false, Some(1), Some(100)),
            ("b", true, None, None),
            ("c", false, Some(1), None),
            ("e", false, Some(0), Some(2)),
            ("f", true, Some(0), Some(2)),
        ],
    );
    // NULLIF never returns the value it compares with.
    assert_values(
        &db,
        "SELECT nullif(small, 2) AS f FROM q",
        &[("f", Some(&["0", "1"]))],
    );
}

#[test]
fn a_case_over_a_checked_interval_covers_it() {
    let db = ranges_setup();
    let s = db
        .analyze(
            "SELECT CASE small WHEN 0 THEN 'a' WHEN 1 THEN 'b' WHEN 2 THEN 'c' END AS x FROM q",
        )
        .unwrap();
    assert!(!s.columns[0].nullable);
}

#[test]
fn a_recursive_counter_settles() {
    let db = ranges_setup();
    // The range grows each step: it is dropped rather than guessed.
    assert_ranges(
        &db,
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 100)
         SELECT n FROM r",
        &[("n", false, None, None)],
    );
    assert_ranges(
        &db,
        "WITH RECURSIVE r(n, k) AS (SELECT 1, 0 UNION ALL SELECT n + 1, k FROM r WHERE n < 3)
         SELECT k FROM r",
        &[("k", false, Some(0), Some(0))],
    );
}

// ── Views ────────────────────────────────────────────────────────────────────

#[test]
fn a_view_keeps_what_its_query_knows() {
    let mut db = values_setup();
    db.apply_sql(
        "CREATE VIEW vv AS
           SELECT kind, n, now() AS at, CASE WHEN n = 1 THEN 'one' ELSE 'two' END AS w,
                  ARRAY[n] AS ns, free
           FROM v WHERE kind = 'a';
         CREATE VIEW over_vv AS SELECT * FROM vv;",
    )
    .unwrap();
    for view in ["vv", "over_vv"] {
        assert_values(
            &db,
            &format!("SELECT kind, n, w, free FROM {view}"),
            &[
                ("kind", Some(&["a"])),
                ("n", Some(&["1", "2"])),
                ("w", Some(&["one", "two"])),
                ("free", None),
            ],
        );
        let s = db.analyze(&format!("SELECT at, ns FROM {view}")).unwrap();
        assert!(s.columns[0].refinement.finite, "{view}.at is finite");
        assert!(
            matches!(
                &s.columns[1].pg_type,
                Type::Array {
                    element_nullable: Some(false),
                    ..
                }
            ),
            "{view}.ns has no NULL element: {:?}",
            s.columns[1].pg_type
        );
    }
    // A view's WHERE narrows what is read through it, the outer one too.
    assert_values(
        &db,
        "SELECT w FROM vv WHERE w <> 'two'",
        &[("w", Some(&["one"]))],
    );
}

#[test]
fn a_view_follows_its_tables_constraints_as_they_change() {
    let mut db = values_setup();
    db.apply_sql("CREATE VIEW kinds AS SELECT kind FROM v;")
        .unwrap();
    assert_values(
        &db,
        "SELECT kind FROM kinds",
        &[("kind", Some(&["a", "b"]))],
    );
    db.apply_sql(
        "ALTER TABLE v DROP CONSTRAINT v_kind_check;
         ALTER TABLE v ADD CONSTRAINT v_kind_check CHECK (kind IN ('a', 'b', 'c'));",
    )
    .unwrap();
    assert_values(
        &db,
        "SELECT kind FROM kinds",
        &[("kind", Some(&["a", "b", "c"]))],
    );
}

#[test]
fn parameters_a_generator_binds_finite_are_finite() {
    let mut db = setup();
    db.apply_sql("CREATE DOMAIN moment_d AS timestamptz;")
        .unwrap();
    let sql = "SELECT to_char($t::timestamptz, 'YYYY') AS a, to_char($u::timestamp, 'YYYY') AS b,
                      extract(month FROM $v::moment_d) AS c, to_char($w::date, 'YYYY') AS e";
    assert_finite(
        &db,
        sql,
        &[
            ("a", true, false),
            ("b", true, false),
            ("c", true, false),
            ("e", true, false),
        ],
    );
    db.assume_finite_parameters(&[
        QualifiedName::new("pg_catalog", "timestamptz"),
        QualifiedName::new("pg_catalog", "timestamp"),
    ])
    .unwrap();
    assert_finite(
        &db,
        &sql.replace("$t", "$t!")
            .replace("$u", "$u!")
            .replace("$v", "$v!")
            .replace("$w", "$w!"),
        &[
            ("a", false, false),
            ("b", false, false),
            ("c", false, false),
            ("e", true, false),
        ],
    );
    assert!(
        db.assume_finite_parameters(&[QualifiedName::new("pg_catalog", "nosuch")])
            .is_err()
    );
}

// ── Bounds of numerics, dates and timestamps ─────────────────────────────────
//
// A date's point counts days from 2000-01-01, a timestamp's microseconds
// from 2000-01-01 00:00 (UTC for timestamptz): 2020-01-01 is day 7305,
// 2024-01-01 day 8766, 2030-12-31 day 11322, 1970-01-01 day -10957.

fn bounds_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN price AS numeric CHECK (VALUE >= 0);
         CREATE DOMAIN decade AS date CHECK (VALUE BETWEEN '2020-01-01' AND '2030-12-31');
         CREATE TABLE b (id int PRIMARY KEY,
                         amount numeric NOT NULL CHECK (amount >= 0),
                         capped numeric NOT NULL CHECK (capped > 0 AND capped <= 99.50),
                         two numeric NOT NULL CHECK (two IN (1.5, 2)),
                         free numeric NOT NULL,
                         d date NOT NULL CHECK (d BETWEEN '2020-01-01' AND '2030-12-31'),
                         dd date NOT NULL,
                         ts timestamp NOT NULL CHECK (ts >= '2020-01-01 00:00:00'),
                         tz timestamptz NOT NULL CHECK (tz >= '2020-01-01 00:00:00+00'),
                         tz_local timestamptz NOT NULL CHECK (tz_local >= '2020-01-01'),
                         p price, r decade,
                         f float8 NOT NULL CHECK (f >= 0));",
    )
    .unwrap();
    db
}

/// A bound as the tests write it: `[lo, hi)`, an end left out when open.
fn render_bounds(b: &Option<OrdRange>) -> Option<String> {
    let b = b.as_ref()?;
    let point = |p: &OrdPoint| match p {
        OrdPoint::NegInfinity => "-inf".to_owned(),
        OrdPoint::Infinity => "inf".to_owned(),
        OrdPoint::NaN => "nan".to_owned(),
        OrdPoint::Finite(n) => n.to_string(),
    };
    let (open, lo) = match &b.lo {
        Some(l) => (if l.inclusive { "[" } else { "(" }, point(&l.point)),
        None => ("(", String::new()),
    };
    let (hi, close) = match &b.hi {
        Some(h) => (point(&h.point), if h.inclusive { "]" } else { ")" }),
        None => (String::new(), ")"),
    };
    Some(format!("{open}{lo}, {hi}{close}"))
}

/// Each column's (name, bounds).
#[track_caller]
fn assert_bounds(db: &PgCatalog, sql: &str, expected: &[(&str, Option<&str>)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, Option<String>)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), render_bounds(&c.refinement.bounds)))
        .collect();
    let expected: Vec<(&str, Option<String>)> = expected
        .iter()
        .map(|(n, b)| (*n, b.map(str::to_owned)))
        .collect();
    assert_eq!(actual, expected, "bounds mismatch for `{sql}`");
}

#[test]
fn checks_and_domains_bound_numerics_dates_and_timestamps() {
    let db = bounds_setup();
    assert_bounds(
        &db,
        "SELECT amount, capped, two, free, d, dd, ts, tz, tz_local, p, r, f FROM b",
        &[
            // `Infinity` and `NaN` are above 0: the top stays open.
            ("amount", Some("[0, )")),
            ("capped", Some("(0, 99.5]")),
            ("two", Some("[1.5, 2]")),
            ("free", None),
            ("d", Some("[7305, 11322]")),
            ("dd", None),
            ("ts", Some("[631152000000000, )")),
            ("tz", Some("[631152000000000, )")),
            // Without a zone, the literal is the migration session's time.
            ("tz_local", None),
            ("p", Some("[0, )")),
            ("r", Some("[7305, 11322]")),
            // Floats are never bounded.
            ("f", None),
        ],
    );
    // A date bounded on both sides is finite; one bounded below isn't.
    assert_finite(
        &db,
        "SELECT d, r, ts FROM b",
        &[("d", false, true), ("r", true, true), ("ts", false, false)],
    );
    // No value set: `numeric` 2 may print `2.0`.
    assert_values(&db, "SELECT two FROM b", &[("two", None)]);
}

#[test]
fn conditions_bound_a_column_where_it_is_read() {
    let db = bounds_setup();
    assert_bounds(
        &db,
        "SELECT free, dd, tz, capped FROM b
         WHERE free > 5 AND dd BETWEEN '2024-01-01' AND '2024-12-31'
           AND tz < '2024-01-01 00:00:00+02' AND capped < 10",
        &[
            // `NaN` and `Infinity` are above 5.
            ("free", Some("(5, )")),
            ("dd", Some("[8766, 9131]")),
            ("tz", Some("[631152000000000, 757375200000000)")),
            ("capped", Some("(0, 10)")),
        ],
    );
    assert_bounds(
        &db,
        "SELECT a.free FROM b a JOIN b c ON a.free <= 1e3 AND c.id = a.id",
        &[("free", Some("(, 1000]"))],
    );
    // `epoch` is exact; `today` is not, nor a zone-less timestamptz, nor
    // a comparison with another type.
    assert_bounds(
        &db,
        "SELECT dd FROM b WHERE dd >= 'epoch'",
        &[("dd", Some("[-10957, )"))],
    );
    assert_bounds(
        &db,
        "SELECT dd, tz, d FROM b
         WHERE dd >= 'today' AND tz < '2024-01-01' AND d < '2024-01-01'::timestamp",
        &[
            ("dd", None),
            ("tz", Some("[631152000000000, )")),
            ("d", Some("[7305, 11322]")),
        ],
    );
    // Special values sit at the ends of the order.
    assert_bounds(
        &db,
        "SELECT free, dd FROM b WHERE free < 'Infinity' AND dd < 'infinity'",
        &[("free", Some("(, inf)")), ("dd", Some("(, inf)"))],
    );
    assert_finite(
        &db,
        "SELECT dd FROM b WHERE dd < 'infinity'",
        &[("dd", false, false)],
    );
    assert_finite(
        &db,
        "SELECT dd FROM b WHERE dd > '-infinity' AND dd < 'infinity'",
        &[("dd", false, true)],
    );
}

#[test]
fn literals_conditionals_and_aggregates_carry_bounds() {
    let db = bounds_setup();
    assert_bounds(
        &db,
        "SELECT 1.50 AS a, '2024-01-01'::date AS c, 'NaN'::numeric AS e,
                '-inf'::numeric AS g, CASE WHEN id > 0 THEN 1.5 ELSE 2 END AS h,
                greatest(capped, 200) AS i, least(two, 0.5) AS j,
                coalesce(p, 7) AS k, '2024-01-01 00:00:00Z'::timestamptz AS l
         FROM b",
        &[
            ("a", Some("[1.5, 1.5]")),
            ("c", Some("[8766, 8766]")),
            ("e", Some("[nan, nan]")),
            ("g", Some("[-inf, -inf]")),
            ("h", Some("[1.5, 2]")),
            ("i", Some("(0, 200]")),
            ("j", Some("[0.5, 2]")),
            ("k", Some("[0, )")),
            ("l", Some("[757382400000000, 757382400000000]")),
        ],
    );
    assert_bounds(
        &db,
        "SELECT 1.5 AS x UNION ALL SELECT 3 UNION ALL SELECT capped FROM b",
        &[("x", Some("(0, 99.5]"))],
    );
    assert_bounds(
        &db,
        "SELECT min(d) AS a, max(capped) AS c FROM b",
        &[("a", Some("[7305, 11322]")), ("c", Some("(0, 99.5]"))],
    );
    assert_bounds(
        &db,
        "SELECT lag(d, 1, '2020-06-01'::date) OVER () AS e, lag(d, 1, '2040-01-01') OVER () AS g
         FROM b",
        &[("e", Some("[7305, 11322]")), ("g", Some("[7305, 14610]"))],
    );
    // Nothing exact: relative words, a zone-less timestamptz, a type
    // modifier (which rounds), a float, another type.
    assert_bounds(
        &db,
        "SELECT 'now'::timestamp AS a, 'today'::date AS c, '2024-01-01 10:00'::timestamptz AS e,
                1.234::numeric(5,2) AS g, capped::numeric(4,1) AS h,
                '2024-01-01'::timestamp(0) AS i, 1.5::float8 AS j, d::timestamp AS k
         FROM b",
        &[
            ("a", None),
            ("c", None),
            ("e", None),
            ("g", None),
            ("h", None),
            ("i", None),
            ("j", None),
            ("k", None),
        ],
    );
}

#[test]
fn bounds_refute_case_branches() {
    let db = bounds_setup();
    let nullable = |sql: &str| -> Vec<bool> {
        db.analyze(sql)
            .unwrap()
            .columns
            .iter()
            .map(|c| c.nullable)
            .collect()
    };
    // A CHECK leaves no value for the ELSE.
    assert_eq!(
        nullable(
            "SELECT CASE WHEN amount >= 0 THEN 'a' END AS a,
                    CASE WHEN d < '2020-01-01' THEN 'old' WHEN d <= '2030-12-31' THEN 'ok' END AS c,
                    CASE WHEN d < '2025-01-01' THEN 'old' END AS e
             FROM b"
        ),
        [false, false, true]
    );
    // `NaN` and `Infinity` are above 5: the order is total.
    assert_eq!(
        nullable("SELECT CASE WHEN free > 5 THEN 'a' WHEN free <= 5 THEN 'b' END AS a FROM b"),
        [false]
    );
    // A CTE's column keeps its bounds.
    assert_eq!(
        nullable(
            "WITH c AS (SELECT CASE WHEN id > 0 THEN 1.5 ELSE 2.5 END AS v FROM b)
             SELECT CASE WHEN v < 3 THEN 'x' END AS a, CASE WHEN v < 2 THEN 'x' END AS e FROM c"
        ),
        [false, true]
    );
}

#[test]
fn casts_that_keep_values() {
    let mut db = values_setup();
    db.apply_sql(
        "CREATE COLLATION und (provider = icu, locale = 'und');
         CREATE TABLE w (s text NOT NULL CHECK (s IN ('ok', 'meh')),
                         small int NOT NULL CHECK (small BETWEEN 1 AND 3),
                         u text COLLATE und CHECK (u IN ('a', 'b')),
                         cc text COLLATE \"C\" CHECK (cc IN ('x')));",
    )
    .unwrap();
    assert_values(
        &db,
        "SELECT m::text AS a, m::varchar AS c, s::mood AS e, n::text AS g, flag::text AS h,
                flag::varchar AS i, kind::varchar(1) AS j, small::text AS k, u, cc,
                kind::varchar::text AS l, s::char(3) AS o
         FROM v, w",
        &[
            ("a", Some(&["happy", "ok", "sad"])),
            ("c", Some(&["happy", "ok", "sad"])),
            // Only a label is an enum value.
            ("e", Some(&["ok"])),
            ("g", Some(&["1", "2"])),
            // `text(boolean)`, not `boolout`.
            ("h", Some(&["false", "true"])),
            ("i", Some(&["false", "true"])),
            // `varchar(1)` truncates; `char(3)` pads.
            ("j", None),
            ("k", Some(&["1", "2", "3"])),
            // A deterministic ICU collation compares strings as written.
            ("u", Some(&["a", "b"])),
            ("cc", Some(&["x"])),
            ("l", Some(&["a", "b"])),
            ("o", None),
        ],
    );
}

#[test]
fn value_sets_are_capped() {
    let mut db = PgCatalog::new().unwrap();
    let list = |n: usize| {
        (1..=n)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    db.apply_sql(&format!(
        "CREATE TABLE many (a int NOT NULL CHECK (a IN ({})), b int NOT NULL CHECK (b IN ({})),
                            c int NOT NULL CHECK (c BETWEEN 1 AND 17),
                            e int NOT NULL CHECK (e BETWEEN 1 AND 16));",
        list(32),
        list(33)
    ))
    .unwrap();
    let s = db.analyze("SELECT a, b, c, e FROM many").unwrap();
    let sizes: Vec<Option<usize>> = s
        .columns
        .iter()
        .map(|c| c.refinement.values.as_ref().map(|v| v.len()))
        .collect();
    // 32 listed values are kept, 33 dropped; a range of up to 16
    // integers is listed.
    assert_eq!(sizes, [Some(32), None, None, Some(16)]);
    // Narrowed under the cap, a set is listed again.
    let s = db.analyze("SELECT b FROM many WHERE b <> 33").unwrap();
    assert_eq!(
        s.columns[0].refinement.values.as_ref().map(|v| v.len()),
        Some(32)
    );
}

/// Constraints the oracle's seeded rows satisfy (numbers 1 and 2, dates
/// 2000-01-01, 2001-01-01 and `infinity`), so under `pg_sanity` every bound
/// below is checked against rows PG returns.
#[test]
fn bounds_hold_over_seeded_rows() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE s (id int PRIMARY KEY, n numeric NOT NULL CHECK (n >= 1),
                         m numeric CHECK (m BETWEEN 1 AND 2),
                         d date NOT NULL CHECK (d >= '2000-01-01'),
                         e date CHECK (e BETWEEN '2000-01-01' AND '2001-12-31'),
                         t timestamp CHECK (t >= '2000-01-01'),
                         tz timestamptz CHECK (tz > '1999-12-31 00:00:00-03'));",
    )
    .unwrap();
    assert_bounds(
        &db,
        "SELECT n, m, d, e, t, tz FROM s",
        &[
            ("n", Some("[1, )")),
            ("m", Some("[1, 2]")),
            ("d", Some("[0, )")),
            ("e", Some("[0, 730]")),
            ("t", Some("[0, )")),
            ("tz", Some("(-75600000000, )")),
        ],
    );
    assert_bounds(
        &db,
        "SELECT n, d, max(e) OVER () AS e, CASE WHEN m > 1.5 THEN m ELSE 1.25 END AS m
         FROM s WHERE n > 1 AND d < 'infinity'",
        &[
            ("n", Some("(1, )")),
            ("d", Some("[0, inf)")),
            ("e", Some("[0, 730]")),
            ("m", Some("[1.25, 2]")),
        ],
    );
}

/// A constant a domain's bounds exclude fails every execution.
#[test]
fn a_constant_out_of_a_domains_bounds_fails() {
    let db = bounds_setup();
    for (sql, domain) in [
        ("SELECT '2019-12-31'::decade AS x", "decade"),
        ("SELECT (-0.5)::price AS x", "price"),
        ("SELECT '-Infinity'::price AS x", "price"),
    ] {
        let err = db.analyze(sql).unwrap_err().to_string();
        let want =
            format!("value for domain {domain} violates check constraint \"{domain}_check\"");
        assert!(err.starts_with(&want), "`{sql}`: {err}");
    }
    // `NaN` and `Infinity` are above 0.
    for sql in [
        "SELECT 'NaN'::price AS x",
        "SELECT 'Infinity'::price AS x",
        "SELECT '2020-01-01'::decade AS x",
    ] {
        db.analyze(sql).unwrap();
    }
}
