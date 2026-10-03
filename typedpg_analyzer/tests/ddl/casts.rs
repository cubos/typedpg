//! CREATE / DROP CAST: implicit vs explicit, WITHOUT FUNCTION, binary
//! coercibility, user-defined type casts.

use crate::common::*;

// ── Binary coercibility ─────────────────────────────────────────────────────

#[test]
fn binary_coercible_reflexive_and_domain() {
    let snap = build(&[("0001.sql", "CREATE DOMAIN user_id AS INT;")]);

    let int4 = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8 = snap
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    let user_id = snap.resolve_type_by_name(None, "user_id").unwrap().oid;

    // Reflexive.
    assert!(snap.is_binary_coercible(int4, int4));
    // Domain to its base type.
    assert!(snap.is_binary_coercible(user_id, int4));
    // Not binary coercible: different base types (pg_cast is 'f'/Function).
    assert!(!snap.is_binary_coercible(int4, int8));
    // Domain to an unrelated base is not binary coercible.
    assert!(!snap.is_binary_coercible(user_id, int8));
}

#[test]
fn create_cast_with_enum_without_function_is_rejected() {
    // PG (SQLSTATE 42P17) rejects WITHOUT FUNCTION casts touching enums —
    // they have an internal sort order that isn't safe to bit-cast through.
    let result = try_apply(&[(
        "0001.sql",
        "CREATE TYPE color_a AS ENUM ('red');
         CREATE TYPE color_b AS ENUM ('red');
         CREATE CAST (color_a AS color_b) WITHOUT FUNCTION AS IMPLICIT;",
    )]);

    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "enum data types are not binary-compatible",
    );
}

// ── CREATE / DROP CAST ──────────────────────────────────────────────────────

#[test]
fn create_cast_registers_implicit_cast() {
    // A basic CREATE CAST between two built-in types with WITH INOUT.
    let snap = build(&[(
        "0001.sql",
        "CREATE CAST (int4 AS text) WITH INOUT AS IMPLICIT;",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    assert!(snap.has_implicit_cast(int4_oid, text_oid));
}

#[test]
fn drop_cast_removes_cast() {
    let snap = build(&[(
        "0001.sql",
        "CREATE CAST (int4 AS text) WITH INOUT AS IMPLICIT;
         DROP CAST (int4 AS text);",
    )]);

    let int4_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let text_oid = snap
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;
    // The user-defined cast is gone. (Built-in int4→text cast is explicit,
    // not implicit, so this check still holds.)
    assert!(!snap.has_implicit_cast(int4_oid, text_oid));
}

#[test]
fn drop_cast_if_exists_no_error() {
    // There's no user-defined int2 → uuid cast; IF EXISTS must silence it.
    let _snap = build(&[("0001.sql", "DROP CAST IF EXISTS (int2 AS uuid);")]);
}

#[test]
fn drop_cast_missing_errors_without_if_exists() {
    let result = try_apply(&[("0001.sql", "DROP CAST (int2 AS uuid);")]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cast from type smallint to type uuid does not exist"
    );
}

#[test]
fn create_cast_with_domain_without_function_is_rejected() {
    // PG (SQLSTATE 42P17) rejects WITHOUT FUNCTION / WITH INOUT casts that
    // touch a domain — domains carry CHECK constraints that only a casting
    // function can run. The user must define a function-based cast instead.
    let result = try_apply(&[(
        "0001.sql",
        "CREATE DOMAIN email AS TEXT;
         CREATE CAST (email AS text) WITHOUT FUNCTION AS IMPLICIT;",
    )]);

    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "domain data types must not be marked binary-compatible",
    );
}

#[test]
fn user_cast_backed_by_a_non_strict_function_is_nullable() {
    // The cast function runs on any input and may return NULL (a SQL
    // function is CALLED ON NULL INPUT by default) — and so may a STRICT
    // one in SQL: STRICT only skips the call on a NULL input (only a
    // compiled one, C or internal, is taken to be NULL just then).
    let db = build_db(&[(
        "0001.sql",
        "CREATE TYPE mood AS ENUM ('a');
         CREATE FUNCTION mood_int(mood) RETURNS int LANGUAGE sql AS 'select null::int';
         CREATE CAST (mood AS int) WITH FUNCTION mood_int(mood);
         CREATE TYPE mood2 AS ENUM ('a');
         CREATE FUNCTION mood2_int(mood2) RETURNS int LANGUAGE sql STRICT AS 'select 1';
         CREATE CAST (mood2 AS int) WITH FUNCTION mood2_int(mood2);",
    )]);
    assert_cols(
        &db.analyze("SELECT 'a'::mood::int AS m").unwrap(),
        vec![cn("m", int4())],
    );
    assert_cols(
        &db.analyze("SELECT 'a'::mood2::int AS m").unwrap(),
        vec![cn("m", int4())],
    );
}

#[test]
fn create_cast_checks_its_function_and_types() {
    let mood = "CREATE TYPE mood AS ENUM ('a');";
    let fns = "CREATE TYPE mood AS ENUM ('a');
               CREATE FUNCTION m2t(mood) RETURNS text LANGUAGE sql AS 'select $1::text';
               CREATE FUNCTION m2i(mood, text) RETURNS int LANGUAGE sql AS 'select 1';
               CREATE FUNCTION m2s(mood) RETURNS SETOF int LANGUAGE sql AS 'select 1';";
    assert_ddl_rejections(&[
        (
            fns,
            "CREATE CAST (mood AS int) WITH FUNCTION m2t(mood);",
            "return data type of cast function must match or be binary-coercible to target data \
             type",
        ),
        (
            fns,
            "CREATE CAST (mood AS int) WITH FUNCTION m2i(mood, text);",
            "second argument of cast function must be type integer",
        ),
        (
            fns,
            "CREATE CAST (mood AS int) WITH FUNCTION m2s(mood);",
            "cast function must not return a set",
        ),
        (
            mood,
            "CREATE CAST (mood AS int) WITHOUT FUNCTION;",
            "enum data types are not binary-compatible",
        ),
        (
            mood,
            "CREATE CAST (mood AS text) WITHOUT FUNCTION;",
            "source and target data types are not physically compatible",
        ),
        (
            "",
            "CREATE CAST (int AS int) WITH FUNCTION int4abs(int);",
            "source data type and target data type are the same",
        ),
        (
            "",
            "CREATE CAST (int AS anyelement) WITH INOUT;",
            "target data type anyelement is a pseudo-type",
        ),
    ]);
}
