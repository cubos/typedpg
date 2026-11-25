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

#[test]
fn an_element_by_element_cast_keeps_known_elements() {
    let mut db = PgCatalog::new().unwrap();
    // `int4(jsonb)` maps a JSON null to NULL; `to_big` is a cast function
    // between the array types themselves, which may return any elements.
    db.apply_sql(
        "CREATE TABLE ev (id int PRIMARY KEY,
                          dates date[] NOT NULL CHECK (array_position(dates, NULL) IS NULL),
                          js jsonb[] NOT NULL CHECK (array_position(js, NULL) IS NULL),
                          raw date[] NOT NULL, loose int[] NOT NULL,
                          ints int[] NOT NULL CHECK (array_position(ints, NULL) IS NULL));
         CREATE TYPE big AS (v bigint);
         CREATE FUNCTION to_big(int[]) RETURNS big[] LANGUAGE sql
             AS 'SELECT ARRAY[NULL::big]';
         CREATE CAST (int[] AS big[]) WITH FUNCTION to_big(int[]);",
    )
    .unwrap();
    assert_elements(
        &db,
        "SELECT dates::text[] AS a, dates::timestamp[] AS b, dates::varchar(3)[] AS c,
                raw::text[] AS d, js::int[] AS e, js::text[] AS f
         FROM ev",
        &[
            ("a", Some(false)),
            ("b", Some(false)),
            ("c", Some(false)),
            ("d", None),
            ("e", None),
            ("f", Some(false)),
        ],
    );
    assert_elements(
        &db,
        "SELECT ints::big[] AS a, array_agg(id)::text[] AS b FROM ev GROUP BY id",
        &[("a", None), ("b", Some(false))],
    );
    // A CASE / COALESCE branch converted to the result's type the same way.
    assert_elements(
        &db,
        "SELECT CASE WHEN id > 0 THEN ARRAY[id] ELSE ARRAY[1::bigint] END AS a,
                COALESCE(js::int[], ARRAY[1]) AS b, GREATEST(ints, ARRAY[1::bigint]) AS c,
                LEAST(ints, loose) AS d
         FROM ev",
        &[
            ("a", Some(false)),
            ("b", None),
            ("c", Some(false)),
            ("d", None),
        ],
    );
}

#[test]
fn aggregated_values_proven_non_null_make_non_null_elements() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE cl (id int PRIMARY KEY, tz text NOT NULL);
         CREATE TABLE ev (id uuid PRIMARY KEY, cid int NOT NULL REFERENCES cl,
                          \"startsAt\" timestamptz NOT NULL,
                          fin timestamptz NOT NULL CHECK (isfinite(fin)));",
    )
    .unwrap();
    // `extract` of an infinite timestamp is NULL for `isodow`: only a
    // finite one makes a non-NULL element.
    assert_elements(
        &db,
        "SELECT array_agg(DISTINCT extract(isodow FROM s.fin AT TIME ZONE c.tz)::int) AS a,
                array_agg(DISTINCT extract(isodow FROM s.\"startsAt\" AT TIME ZONE c.tz)::int) AS b,
                array_agg(s.id ORDER BY s.id) AS c, array_agg(lower(c.tz) || 'x') AS d
         FROM ev s JOIN cl c ON c.id = s.cid",
        &[
            ("a", Some(false)),
            ("b", Some(true)),
            ("c", Some(false)),
            ("d", Some(false)),
        ],
    );
    // A primary key on the nullable side of an outer join may be NULL,
    // unless the FILTER or the WHERE clause keeps those rows out.
    assert_elements(
        &db,
        "SELECT array_agg(e.id) AS a, array_agg(e.id) FILTER (WHERE e.id IS NOT NULL) AS b,
                COALESCE(array_agg(e.id) FILTER (WHERE e.id IS NOT NULL), '{}') AS c
         FROM cl c LEFT JOIN ev e ON e.cid = c.id GROUP BY c.id",
        &[("a", Some(true)), ("b", Some(false)), ("c", Some(false))],
    );
    assert_elements(
        &db,
        "SELECT array_agg(e.id) AS a FROM ev e RIGHT JOIN cl c ON e.cid = c.id
         WHERE e.id IS NOT NULL GROUP BY c.id",
        &[("a", Some(false))],
    );
    assert_elements(
        &db,
        "SELECT (SELECT array_agg(e.id) FROM ev e WHERE e.cid = c.id) AS a,
                ARRAY(SELECT e.id FROM ev e WHERE e.cid = c.id) AS b,
                x.a AS c
         FROM cl c CROSS JOIN LATERAL (SELECT array_agg(e.id) AS a FROM ev e
                                       WHERE e.cid = c.id) x",
        &[("a", Some(false)), ("b", Some(false)), ("c", Some(false))],
    );
}

#[test]
fn multidimensional_arrays_and_slices_keep_their_elements() {
    let db = setup();
    // A multidimensional array's elements are its sub-arrays' (a NULL
    // sub-array fails the query); a slice is some of the array's elements.
    assert_elements(
        &db,
        "SELECT array_agg(ARRAY[n]) AS a, array_agg(ARRAY[age]) AS b, array_agg(tags) AS c,
                ARRAY(SELECT ARRAY[n] FROM t) AS d, ARRAY(SELECT ARRAY[age] FROM t) AS e,
                ARRAY[ARRAY[1], ARRAY[2]] AS f, ARRAY[ARRAY[1], ARRAY[NULL::int]] AS g,
                (array_agg(n))[1:2] AS h, (array_agg(age))[1:2] AS i,
                ARRAY[array_agg(n), array_agg(n)] AS j
         FROM t",
        &[
            ("a", Some(false)),
            ("b", Some(true)),
            ("c", None),
            ("d", Some(false)),
            ("e", Some(true)),
            ("f", Some(false)),
            ("g", Some(true)),
            ("h", Some(false)),
            ("i", Some(true)),
            ("j", Some(false)),
        ],
    );
    assert_elements(
        &db,
        "SELECT array_agg(x) AS a FROM unnest(ARRAY(SELECT ARRAY[n] FROM t)) x",
        &[("a", Some(false))],
    );
}
