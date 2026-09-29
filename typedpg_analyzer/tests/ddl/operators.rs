//! CREATE / DROP OPERATOR: operator signatures, schema-qualified drops,
//! user-defined operators.

use crate::common::*;

// ── DROP OPERATOR ───────────────────────────────────────────────────────────

#[test]
fn drop_operator_removes_only_matching_signature() {
    // Dropping the (int, int) overload must leave the (text, text) one.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION ieq(int, int) RETURNS bool LANGUAGE sql AS 'select $1 = $2';
         CREATE FUNCTION teq(text, text) RETURNS bool LANGUAGE sql AS 'select $1 = $2';
         CREATE OPERATOR <==> (leftarg = int, rightarg = int, function = ieq);
         CREATE OPERATOR <==> (leftarg = text, rightarg = text, function = teq);
         DROP OPERATOR <==> (int, int);",
    )]);
    let public_oid = snap.namespace_oid("public").unwrap();
    let ops: Vec<&PgOperator> = snap
        .pg_operator()
        .values()
        .filter(|o| o.oprnamespace == public_oid && o.oprname == "<==>")
        .collect();
    let int4_oid = snap.resolve_type_by_name(None, "int4").unwrap().oid;
    let text_oid = snap.resolve_type_by_name(None, "text").unwrap().oid;
    assert!(
        !ops.iter()
            .any(|o| o.oprleft == Some(int4_oid) && o.oprright == int4_oid),
        "(int, int) overload should have been dropped"
    );
    assert!(
        ops.iter()
            .any(|o| o.oprleft == Some(text_oid) && o.oprright == text_oid),
        "(text, text) overload should still be registered"
    );
}

#[test]
fn an_extension_operator_only_goes_with_its_extension() {
    let result = try_apply(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         DROP OPERATOR <=> (vector, vector);",
    )]);
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "cannot drop operator <=>(vector,vector) because extension vector requires it (You can \
         drop extension vector instead.)"
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
fn alter_operator_sets_estimators_of_boolean_operators_only() {
    let _snap = build(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         ALTER OPERATOR < (vector, vector) SET (RESTRICT = scalarltsel);",
    )]);
    // `<=>` is a distance (float8): OperatorValidateParams refuses it.
    let result = try_apply(&[(
        "0001.sql",
        "CREATE EXTENSION vector;
         ALTER OPERATOR <=> (vector, vector) SET (RESTRICT = scalarlesel);",
    )]);
    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "only boolean operators can have restriction selectivity"
    );
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

#[test]
fn user_operator_backed_by_a_non_strict_function_is_nullable() {
    // The function runs on any operands and may return NULL (a SQL function
    // is CALLED ON NULL INPUT by default); a STRICT one is NULL only on a
    // NULL operand.
    let db = build_db(&[(
        "0001.sql",
        "CREATE FUNCTION eq3(int, int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'select null::bool';
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = eq3);
         CREATE FUNCTION eq4(int, int) RETURNS bool LANGUAGE sql IMMUTABLE STRICT AS 'select $1 = $2';
         CREATE OPERATOR ==== (LEFTARG = int, RIGHTARG = int, FUNCTION = eq4);",
    )]);
    assert_cols(
        &db.analyze("SELECT 1 === 2 AS e").unwrap(),
        vec![cn("e", bool_ty())],
    );
    assert_cols(
        &db.analyze("SELECT 1 ==== 2 AS e").unwrap(),
        vec![c("e", bool_ty())],
    );
}

#[test]
fn create_operator_follows_operator_create_checks() {
    let setup = "CREATE FUNCTION myeq(int, int) RETURNS bool LANGUAGE sql AS 'select $1 = $2';
                 CREATE FUNCTION myf(int, int) RETURNS int LANGUAGE sql AS 'select $1';";
    let with_op = format!(
        "{setup} CREATE OPERATOR === (leftarg = int, rightarg = int, function = myeq);
                 CREATE OPERATOR ==> (leftarg = int, rightarg = int, function = myf);"
    );
    assert_ddl_rejections(&[
        (
            &with_op,
            "CREATE OPERATOR === (leftarg = int, rightarg = int, function = myeq);",
            "operator === already exists",
        ),
        (
            setup,
            "CREATE OPERATOR =!= (leftarg = int, rightarg = int, function = myeq, negator = =!=);",
            "operator cannot be its own negator",
        ),
        (
            setup,
            "CREATE OPERATOR ==> (leftarg = int, rightarg = int, function = myf, restrict = eqsel);",
            "only boolean operators can have restriction selectivity",
        ),
        (
            setup,
            "CREATE OPERATOR ==> (leftarg = int, function = myf);",
            "operator right argument type must be specified",
        ),
        (
            &with_op,
            "ALTER OPERATOR ==> (int, int) SET (restrict = eqsel);",
            "only boolean operators can have restriction selectivity",
        ),
    ]);
}

#[test]
fn a_commutator_reference_makes_a_shell_operator() {
    let db = build(&[(
        "0001.sql",
        "CREATE FUNCTION myte(int, text) RETURNS bool LANGUAGE sql AS 'select true';
         CREATE OPERATOR <=> (leftarg = int, rightarg = text, function = myte, commutator = <==>);",
    )]);
    assert_err_prefix!(
        db.analyze("SELECT 'x'::text <==> 1 AS x"),
        AnalyzeError::UndefinedOperator(_),
        "operator is only a shell: text <==> integer"
    );
}
