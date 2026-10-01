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
            let element_nullable = match &c.pg_type {
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
