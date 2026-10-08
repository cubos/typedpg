//! CHECK constraints about JSON values as knowledge about expressions: a
//! key a constraint requires (`data ? 'a'` on an object) makes `data ->
//! 'a'` non-NULL, and a JSON type other than `null` (`jsonb_typeof(data ->
//! 'a') = 'string'`) makes `data ->> 'a'` non-NULL too — written in the
//! constraint, or in the body of a `LANGUAGE sql` function it calls (as
//! compiled JSON schemas are).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE j (id int PRIMARY KEY,
                         data jsonb NOT NULL CHECK (jsonb_typeof(data) = 'object' AND data ? 'a'
                                                    AND jsonb_typeof(data -> 'a') = 'string'));
         -- Apart: the oracle seeds it with an array holding \"a\".
         CREATE TABLE ja (id int PRIMARY KEY, any_type jsonb NOT NULL CHECK (any_type ? 'a'));
         CREATE FUNCTION inner_s(v jsonb) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
           SELECT CASE WHEN v IS NULL THEN true ELSE (jsonb_typeof(v) = 'object')
             AND (CASE WHEN jsonb_typeof(v) = 'object' THEN v ?& ARRAY['x']::text[] ELSE true END) END $$;
         CREATE FUNCTION outer_s(v jsonb) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
           SELECT CASE WHEN v IS NULL THEN true ELSE (jsonb_typeof(v) = 'object')
             AND (CASE WHEN jsonb_typeof(v) = 'object' THEN v ?& ARRAY['a', 'k', 'n']::text[] ELSE true END)
             AND (CASE WHEN v ? 'a' THEN CASE WHEN (v -> 'a') IS NULL THEN true
                                              ELSE jsonb_typeof((v -> 'a')) = 'string' END
                       ELSE true END)
             AND (CASE WHEN v ? 'n' THEN inner_s((v -> 'n')) ELSE true END) END $$;
         CREATE DOMAIN doc_d AS jsonb;
         CREATE TABLE f (id int PRIMARY KEY,
                         doc doc_d NOT NULL
                           CHECK (CASE WHEN doc IS NULL THEN true ELSE outer_s((doc)::jsonb) END),
                         loose jsonb CHECK (CASE WHEN loose IS NULL THEN true ELSE outer_s(loose) END));",
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
fn a_required_key_of_an_object_is_there() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT data -> 'a' AS a, data ->> 'a' AS b, j.data -> 'a' AS c, data -> 'z' AS d,
                any_type -> 'a' AS e
         FROM j, ja",
        &[
            ("a", false),
            // A string, not JSON null.
            ("b", false),
            ("c", false),
            ("d", true),
            // An array holding "a" passes `? 'a'` too.
            ("e", true),
        ],
    );
}

#[test]
fn a_json_schema_function_a_check_calls_is_read() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT doc -> 'a' AS a, doc ->> 'a' AS b, doc -> 'k' AS c, doc ->> 'k' AS d,
                doc -> 'n' -> 'x' AS e, f.doc -> 'n' AS g, loose -> 'a' AS h
         FROM f",
        &[
            ("a", false),
            ("b", false),
            ("c", false),
            // Of any JSON type: maybe JSON null.
            ("d", true),
            // Required by the function the schema of `n` is.
            ("e", false),
            ("g", false),
            // The column may be NULL.
            ("h", true),
        ],
    );
    assert_nullable(
        &db,
        "SELECT loose -> 'a' AS h FROM f WHERE loose IS NOT NULL",
        &[("h", false)],
    );
}

#[test]
fn a_null_extended_row_has_no_keys() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT x.doc -> 'a' AS a FROM j LEFT JOIN f x ON x.id = j.id",
        &[("a", true)],
    );
}

#[test]
fn a_discriminated_union_has_its_discriminant() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION variant_a(v jsonb) RETURNS boolean LANGUAGE sql IMMUTABLE
             AS $$ SELECT v ?& ARRAY['k']::text[] $$;
         CREATE FUNCTION tagged(v jsonb) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
           SELECT CASE WHEN v IS NULL THEN true ELSE (CASE WHEN jsonb_typeof(v) = 'object' AND v ? 'a'
             THEN CASE (v -> 'a') WHEN '\"x\"'::jsonb THEN (variant_a(v)) WHEN '\"y\"'::jsonb THEN true
                  ELSE false END ELSE false END) END $$;
         CREATE TABLE u (id int PRIMARY KEY,
                         doc jsonb NOT NULL CHECK (CASE WHEN doc IS NULL THEN true ELSE tagged(doc) END));",
    )
    .unwrap();
    assert_nullable(
        &db,
        "SELECT doc -> 'a' AS a, doc ->> 'a' AS b, doc -> 'k' AS k FROM u",
        // `"x"` or `"y"`; `k` only in one variant.
        &[("a", false), ("b", false), ("k", true)],
    );
}
