//! What the analysis knows of an array column's element nullability
//! (`Type::Array::element_nullable`): `Some(true)` makes `sql!` read the
//! column as `Vec<Option<T>>`. Each `Some(true)` case below returns a NULL
//! element on PostgreSQL 18 for a NULL input; each `Some(false)` one can't.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, n int NOT NULL, age int, tags text[] NOT NULL,
                         s text NOT NULL);",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_elements(db: &PgCatalog, sql: &str, expected: &[(&str, Option<bool>)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, Option<bool>)> = s
        .columns
        .iter()
        .map(|c| {
            let mut ty = &c.pg_type;
            while let Type::Domain { base, .. } = ty {
                ty = base;
            }
            let element_nullable = match ty {
                Type::Array {
                    element_nullable, ..
                } => *element_nullable,
                other => panic!("column {} is not an array: {other:?}", c.name),
            };
            (c.name.as_str(), element_nullable)
        })
        .collect();
    assert_eq!(actual, expected, "element nullability mismatch for `{sql}`");
}

#[test]
fn aggregates_and_constructors_know_their_elements() {
    let db = setup();
    assert_elements(
        &db,
        "SELECT array_agg(age) AS a, array_agg(n) AS b, ARRAY[n, age] AS c, ARRAY[n, 1] AS d,
                ARRAY(SELECT age FROM t) AS e, ARRAY(SELECT n FROM t) AS f,
                ARRAY[n, NULL] AS g
         FROM t GROUP BY n, age",
        &[
            ("a", Some(true)),
            ("b", Some(false)),
            ("c", Some(true)),
            ("d", Some(false)),
            ("e", Some(true)),
            ("f", Some(false)),
            ("g", Some(true)),
        ],
    );
}

#[test]
fn array_functions_and_operators_propagate() {
    let db = setup();
    assert_elements(
        &db,
        "SELECT array_append(ARRAY[n], age) AS a, array_prepend(n, ARRAY[n]) AS b,
                array_cat(ARRAY[n], ARRAY[age]) AS c, ARRAY[n] || age AS d,
                ARRAY[n] || ARRAY[n] AS e, array_remove(ARRAY[age], NULL) AS f,
                string_to_array(s, ',') AS g, string_to_array(s, ',', 'x') AS h,
                regexp_match(s, '(a)|(b)') AS i, array_append(tags, s) AS j
         FROM t",
        &[
            ("a", Some(true)),
            ("b", Some(false)),
            ("c", Some(true)),
            ("d", Some(true)),
            ("e", Some(false)),
            ("f", Some(false)),
            ("g", Some(false)),
            ("h", Some(true)),
            ("i", Some(true)),
            // A table's array column says nothing about its elements.
            ("j", None),
        ],
    );
}

#[test]
fn table_columns_and_union_arms() {
    let db = setup();
    assert_elements(&db, "SELECT tags FROM t", &[("tags", None)]);
    assert_elements(
        &db,
        "SELECT ARRAY[n] AS a FROM t UNION ALL SELECT ARRAY[age] FROM t",
        &[("a", Some(true))],
    );
    assert_elements(
        &db,
        "SELECT ARRAY[n] AS a FROM t UNION ALL SELECT ARRAY[n] FROM t",
        &[("a", Some(false))],
    );
}

#[test]
fn subqueries_and_ctes_keep_what_is_known() {
    let db = setup();
    // Passing an array through a subquery, a CTE or `*` doesn't change its
    // elements: a pass-through wrap reports what the query itself does.
    for sql in [
        "SELECT a, b FROM (SELECT array_agg(n) AS a, array_agg(age) AS b FROM t) s",
        "WITH c AS (SELECT array_agg(n) AS a, array_agg(age) AS b FROM t) SELECT a, b FROM c",
        "SELECT * FROM (SELECT array_agg(n) AS a, array_agg(age) AS b FROM t) s",
        // An outer join may make the whole array NULL, not an element.
        "SELECT s.a, s.b FROM t LEFT JOIN (SELECT array_agg(n) AS a, array_agg(age) AS b \
         FROM t) s ON false",
    ] {
        assert_elements(&db, sql, &[("a", Some(false)), ("b", Some(true))]);
    }
    // A recursive CTE's column is either arm's.
    assert_elements(
        &db,
        "WITH RECURSIVE r(a, k) AS (SELECT ARRAY[1], 1 UNION ALL \
         SELECT ARRAY[NULL::int], k + 1 FROM r WHERE k < 3) SELECT a FROM r",
        &[("a", Some(true))],
    );
}

#[test]
fn a_check_keeping_null_elements_out_of_a_column() {
    let mut db = PgCatalog::new().unwrap();
    // `loose` holds what proves nothing, apart from `c`: the oracle seeds
    // its arrays with a NULL element (`c`'s constraints reject such rows).
    db.apply_sql(
        "CREATE TABLE c (id int PRIMARY KEY,
                         a int[] CHECK (array_position(a, NULL) IS NULL),
                         b text[],
                         CONSTRAINT b_set CHECK (b IS NOT NULL AND array_position(b, NULL::text) IS NULL));
         CREATE TABLE loose (id int PRIMARY KEY, d int[], e int[], f int[],
                             CONSTRAINT d_any CHECK (array_position(d, NULL) IS NULL OR id > 0),
                             CONSTRAINT e_off CHECK (array_position(e, NULL) IS NULL) NOT ENFORCED,
                             CONSTRAINT f_from CHECK (array_position(f, NULL, 2) IS NULL));",
    )
    .unwrap();
    let expected = [("a", Some(false)), ("b", Some(false))];
    assert_elements(&db, "SELECT a, b FROM c", &expected);
    assert_elements(
        &db,
        "INSERT INTO c (id, b) VALUES (1, '{}') RETURNING a, b",
        &expected,
    );
    assert_elements(
        &db,
        "SELECT x.a, x.b FROM c LEFT JOIN c x ON false",
        &expected,
    );
    assert_elements(
        &db,
        "SELECT a || 1 AS a, array_cat(a, a) AS b, a || NULL::int AS c FROM c",
        &[("a", Some(false)), ("b", Some(false)), ("c", Some(true))],
    );
    // Only when it is a whole conjunct, enforced, and searches from the
    // first element.
    assert_elements(
        &db,
        "SELECT d, e, f FROM loose",
        &[("d", None), ("e", None), ("f", None)],
    );
}

#[test]
fn a_no_inherit_check_binds_no_child() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE p (a int[], CHECK (array_position(a, NULL) IS NULL) NO INHERIT);
         CREATE TABLE q (a int[], CHECK (array_position(a, NULL) IS NULL) NO INHERIT);
         CREATE TABLE r (a int[], CHECK (array_position(a, NULL) IS NULL));
         CREATE TABLE p_child () INHERITS (p);
         CREATE TABLE r_child () INHERITS (r);",
    )
    .unwrap();
    assert_elements(&db, "SELECT a FROM p", &[("a", None)]);
    assert_elements(&db, "SELECT a FROM ONLY p", &[("a", None)]);
    assert_elements(&db, "SELECT a FROM q", &[("a", Some(false))]);
    assert_elements(&db, "SELECT a FROM r", &[("a", Some(false))]);
    assert_elements(&db, "SELECT a FROM r_child", &[("a", Some(false))]);
}

#[test]
fn a_user_array_position_proves_nothing() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION array_position(int[], int) RETURNS int
             LANGUAGE sql IMMUTABLE AS 'SELECT NULL::int';
         CREATE TABLE c (a int[] CHECK (array_position(a, NULL) IS NULL),
                         b int[] CHECK (pg_catalog.array_position(b, NULL) IS NULL));",
    )
    .unwrap();
    assert_elements(
        &db,
        "SELECT a, b FROM c",
        &[("a", None), ("b", Some(false))],
    );
}

#[test]
fn a_domain_keeping_null_elements_out() {
    let mut db = PgCatalog::new().unwrap();
    // NOT NULL arrays the oracle seeds with a NULL element where the type
    // takes one (`loose`'s columns), apart from the domains that don't.
    db.apply_sql(
        "CREATE DOMAIN ids AS int[] CHECK (array_position(VALUE, NULL) IS NULL);
         CREATE DOMAIN some_ids AS ids CHECK (cardinality(VALUE) > 0);
         CREATE DOMAIN maybe_ids AS int[] CHECK (array_position(VALUE, NULL) IS NULL OR true);
         CREATE TABLE t (a ids NOT NULL, b some_ids NOT NULL);
         CREATE TABLE loose (c maybe_ids NOT NULL, raw int[] NOT NULL);
         CREATE FUNCTION f(int[]) RETURNS ids LANGUAGE sql AS 'SELECT $1';",
    )
    .unwrap();
    assert_elements(
        &db,
        "SELECT a, b, a::int[] AS c, a::ids AS d FROM t",
        &[
            ("a", Some(false)),
            ("b", Some(false)),
            ("c", Some(false)),
            ("d", Some(false)),
        ],
    );
    assert_elements(
        &db,
        "SELECT c, raw FROM loose",
        &[("c", None), ("raw", None)],
    );
    // A value coerced to the domain was checked: one with a NULL element
    // fails the query.
    assert_elements(
        &db,
        "SELECT raw::ids AS d, raw::some_ids AS e, f(raw) AS g FROM loose",
        &[("d", Some(false)), ("e", Some(false)), ("g", Some(false))],
    );
}
