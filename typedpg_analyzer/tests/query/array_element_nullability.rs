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
