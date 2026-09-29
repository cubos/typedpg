//! `CALL procedure(args)`.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE PROCEDURE p_out(IN a int, OUT b int) AS $$ SELECT a $$ LANGUAGE sql;
         CREATE PROCEDURE p_in(a int, t text) AS $$ SELECT 1 $$ LANGUAGE sql;
         CREATE PROCEDURE p_io(INOUT x int, OUT y text) AS $$ SELECT x, 'a' $$ LANGUAGE sql;
         CREATE FUNCTION f_ov(a int) RETURNS text AS $$ SELECT 'int' $$ LANGUAGE sql;",
    )
    .unwrap();
    db
}

/// PG 18: the result row is the procedure's INOUT / OUT parameters; OUT
/// parameters take a placeholder argument.
#[test]
fn call_returns_out_parameters() {
    let db = setup();
    let s = db.analyze("CALL p_out(1, NULL)").unwrap();
    assert_cols(&s, vec![cn("b", int4())]);
    let s = db.analyze("CALL p_in($a, $t)").unwrap();
    assert_cols(&s, vec![]);
    assert_params(&s, vec![p(int4()), p(text())]);
    let s = db.analyze("CALL p_io($x, NULL)").unwrap();
    assert_cols(&s, vec![cn("x", int4()), cn("y", text())]);
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn call_errors() {
    let db = setup();
    let cases: &[(&str, &str)] = &[
        ("CALL f_ov(1)", "f_ov(integer) is not a procedure"),
        ("CALL p_out(1)", "procedure p_out(integer) does not exist"),
        ("CALL nosuch(1)", "procedure nosuch(integer) does not exist"),
        (
            "CALL p_in(1, 2)",
            "procedure p_in(integer, integer) does not exist",
        ),
        (
            "CALL p_in('x', 'y')",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "CALL p_in(1, 'x', 3)",
            "procedure p_in(integer, unknown, integer) does not exist",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}

/// CALL resolves through ParseFuncOrColumn like any call — defaults, named
/// notation, VARIADIC, polymorphism and the unknown-literal preference
/// rules — over candidates whose OUT parameters take arguments.
#[test]
fn call_resolves_like_a_function_call() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE PROCEDURE p_def(a int, b int DEFAULT 2) LANGUAGE plpgsql AS 'begin end';
         CREATE PROCEDURE p_named(a int, b text) LANGUAGE plpgsql AS 'begin end';
         CREATE PROCEDURE p_var(VARIADIC a int[]) LANGUAGE plpgsql AS 'begin end';
         CREATE PROCEDURE p_poly(a anyelement, INOUT b anyelement)
             LANGUAGE plpgsql AS 'begin end';
         CREATE PROCEDURE p_ov(a int) LANGUAGE plpgsql AS 'begin end';
         CREATE PROCEDURE p_ov(a text) LANGUAGE plpgsql AS 'begin end';
         CREATE PROCEDURE p_out_named(a int, OUT b int) LANGUAGE plpgsql AS 'begin end';",
    )
    .unwrap();
    for sql in [
        "CALL p_def(1)",
        "CALL p_def(1, 3)",
        "CALL p_def(b => 3, a => 1)",
        "CALL p_named(b => 'x'::text, a => 1)",
        "CALL p_var(1, 2, 3)",
        "CALL p_var(VARIADIC ARRAY[1, 2])",
        "CALL p_ov('x')",
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_cols(&s, vec![]);
    }
    let s = db.analyze("CALL p_poly(1, 2)").unwrap();
    assert_cols(&s, vec![cn("b", int4())]);
    let s = db.analyze("CALL p_out_named(b => NULL, a => $a)").unwrap();
    assert_cols(&s, vec![cn("b", int4())]);
    assert_params(&s, vec![p(int4())]);
    let s = db.analyze("CALL p_def($x)").unwrap();
    assert_params(&s, vec![p(int4())]);
    assert_err_prefix!(
        db.analyze("CALL p_ov(1.5)"),
        AnalyzeError::UndefinedFunction(_),
        "procedure p_ov(numeric) does not exist"
    );
    // An unknown argument prefers the string category, as for functions.
    let s = db.analyze("CALL p_ov($x)").unwrap();
    assert_params(&s, vec![p(text())]);
}

/// Native positional placeholders (`$1`) are PG parameters too.
#[test]
