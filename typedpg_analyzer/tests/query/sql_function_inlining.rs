//! A call of a `LANGUAGE sql` function of one expression read as its body
//! over the call's arguments (`expr::inline`): its nullability, refinement
//! and array elements are what the body makes of them, as well as what its
//! declaration says.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, a int, n int NOT NULL, ts timestamptz);
         CREATE FUNCTION zero_if_null(x int) RETURNS int LANGUAGE sql AS 'SELECT coalesce(x, 0)';
         CREATE FUNCTION plus_one(x int) RETURNS int LANGUAGE sql STRICT RETURN x + 1;
         CREATE FUNCTION ident(x int) RETURNS int LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION label(x int) RETURNS text LANGUAGE sql
             AS $$ SELECT CASE WHEN x > 0 THEN 'pos' ELSE 'other' END $$;
         CREATE FUNCTION stamp(x timestamptz) RETURNS timestamptz LANGUAGE sql AS 'SELECT now()';
         CREATE FUNCTION wrap(x int) RETURNS int[] LANGUAGE sql AS 'SELECT ARRAY[$1]';
         CREATE FUNCTION twice(x int) RETURNS int LANGUAGE sql AS 'SELECT plus_one(plus_one(x))';
         CREATE FUNCTION counted(x int) RETURNS bigint LANGUAGE sql
             AS 'SELECT (SELECT count(*) FROM t WHERE t.a = x)';
         CREATE FUNCTION configured(x int) RETURNS int LANGUAGE sql
             SET search_path = public AS 'SELECT coalesce(x, 0)';
         CREATE FUNCTION defaulted(x int, y int DEFAULT 1) RETURNS int LANGUAGE sql
             AS 'SELECT coalesce(x, y, 0)';
         CREATE FUNCTION countdown(x int) RETURNS int LANGUAGE sql
             AS 'SELECT CASE WHEN x <= 0 THEN 0 ELSE countdown(x - 1) END';
         CREATE FUNCTION replaced(x int) RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE OR REPLACE FUNCTION replaced(x int) RETURNS int LANGUAGE sql
             AS 'SELECT a FROM t WHERE id = x';
         CREATE FUNCTION altered(x int) RETURNS int LANGUAGE sql AS 'SELECT coalesce(x, 0)';
         ALTER FUNCTION altered(int) SET search_path = public;",
    )
    .unwrap();
    db
}

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

#[test]
fn a_call_is_as_null_as_its_body() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT zero_if_null(a) AS a, plus_one(n) AS b, plus_one(a) AS c, twice(n) AS d,
                counted(a) AS e, label(a) AS f, ident(a) AS g, ident(n) AS h
         FROM t",
        &[
            // `coalesce(x, 0)` is never NULL, whatever `x` is.
            ("a", false),
            ("b", false),
            // STRICT: NULL for a NULL argument.
            ("c", true),
            ("d", false),
            ("e", false),
            ("f", false),
            // Not STRICT: the body runs with the NULL.
            ("g", true),
            ("h", false),
        ],
    );
}

#[test]
fn a_call_has_its_bodys_refinement_and_elements() {
    let db = setup();
    let s = db
        .analyze("SELECT label(a) AS l, stamp(ts) AS s, wrap(n) AS w, plus_one(3) AS p FROM t")
        .unwrap();
    assert_eq!(
        s.columns[0].refinement.values,
        Some(["other".to_owned(), "pos".to_owned()].into())
    );
    assert!(s.columns[1].refinement.finite);
    assert!(matches!(
        &s.columns[2].pg_type,
        Type::Array {
            element_nullable: Some(false),
            ..
        }
    ));
    assert_eq!(
        s.columns[3].refinement.values,
        Some(["4".to_owned()].into())
    );
}

#[test]
fn some_calls_are_not_read_as_their_body() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT configured(a) AS a, altered(a) AS b, defaulted(a) AS c, defaulted(a, 2) AS d,
                countdown(n) AS e, replaced(n) AS f
         FROM t",
        &[
            // Its own search_path.
            ("a", true),
            ("b", true),
            // A parameter left to its default.
            ("c", true),
            ("d", false),
            // Recursion stops reading; the inner call is just a call.
            ("e", true),
            // The body as replaced: no row.
            ("f", true),
        ],
    );
}

#[test]
fn a_view_reads_calls_as_bodies_when_queried() {
    let mut db = setup();
    // At CREATE VIEW calls are only resolved; reading the view, its query
    // reads `zero_if_null` as its body.
    db.apply_sql("CREATE VIEW zv AS SELECT zero_if_null(a) AS z, ident(a) AS i FROM t;")
        .unwrap();
    assert_nullable(&db, "SELECT z, i FROM zv", &[("z", false), ("i", true)]);
}
