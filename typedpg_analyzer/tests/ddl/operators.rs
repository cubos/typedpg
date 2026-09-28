//! CREATE / DROP OPERATOR: operator signatures, schema-qualified drops,
//! user-defined operators.

use crate::common::*;

// ── DROP OPERATOR ───────────────────────────────────────────────────────────

#[test]
fn drop_operator_removes_only_matching_signature() {
    // pgvector registers multiple `<=>` overloads (vector/halfvec/sparsevec).
    // Dropping the (vector, vector) overload must leave the others alone.
    let snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         DROP OPERATOR <=> (vector, vector);",
    )]);

    let vector_oid = snap.resolve_type_by_name(None, "vector").unwrap().oid;
    let halfvec_oid = snap.resolve_type_by_name(None, "halfvec").unwrap().oid;

    let public_oid = snap.namespace_oid("public").unwrap();
    let ops: Vec<&PgOperator> = snap
        .pg_operator()
        .values()
        .filter(|o| o.oprnamespace == public_oid && o.oprname == "<=>")
        .collect();
    assert!(
        !ops.is_empty(),
        "other overloads should still be registered"
    );

    assert!(
        !ops.iter()
            .any(|o| o.oprleft == Some(vector_oid) && o.oprright == vector_oid),
        "(vector, vector) overload should have been dropped"
    );
    assert!(
        ops.iter()
            .any(|o| o.oprleft == Some(halfvec_oid) && o.oprright == halfvec_oid),
        "(halfvec, halfvec) overload should still be registered"
    );
}

#[test]
fn drop_operator_if_exists_no_error() {
    let _snap = build(&[("0001.sql", "DROP OPERATOR IF EXISTS <=> (int4, int4);")]);
}

#[test]
fn drop_operator_missing_errors_without_if_exists() {
    let result = try_apply(&[("0001.sql", "DROP OPERATOR <=> (int4, int4);")]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "operator does not exist: integer <=> integer",
    );
}

// ── ALTER OPERATOR ──────────────────────────────────────────────────────────

#[test]
fn alter_operator_is_noop_but_does_not_crash() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         ALTER OPERATOR <=> (vector, vector) SET (RESTRICT = scalarlesel);",
    )]);
}

// ── CREATE OPERATOR / CREATE CAST validation ────────────────────────────────

#[test]
fn create_operator_and_cast_require_their_functions() {
    // PG 18: 42883 function nosuch(integer, integer) does not exist /
    // function nosuch(integer) does not exist; 42710 for a repeated cast.
    for (sql, msg) in [
        (
            "CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = nosuch);",
            "function nosuch(integer, integer) does not exist",
        ),
        (
            "CREATE CAST (int AS text) WITH FUNCTION nosuch(int);",
            "function nosuch(integer) does not exist",
        ),
        (
            "CREATE CAST (int AS bigint) WITH INOUT;",
            "cast from type integer to type bigint already exists",
        ),
        (
            "CREATE CAST (nosuch AS text) WITH INOUT;",
            "type \"nosuch\" does not exist",
        ),
    ] {
        let err = try_apply(&[("0001.sql", sql)]).expect_err(sql);
        assert!(err.to_string().starts_with(msg), "{sql}\n  got: {err}");
    }
    build_db(&[(
        "0001.sql",
        "CREATE FUNCTION eq3(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select $1 = $2';
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = eq3);
         CREATE TYPE mood AS ENUM ('a');
         CREATE FUNCTION mood_int(mood) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE CAST (mood AS int) WITH FUNCTION mood_int(mood);",
    )]);
}

