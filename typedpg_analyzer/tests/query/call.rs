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
