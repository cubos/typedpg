//! JSON_TABLE (PG 17) in FROM.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id int PRIMARY KEY, j jsonb NOT NULL, tx text);")
        .unwrap();
    db
}

#[test]
fn json_table_columns_and_types() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT jt.* FROM t, JSON_TABLE(t.j, '$[*]' COLUMNS (a int PATH '$.a', b text, \
             c jsonb FORMAT JSON PATH '$.c', d boolean EXISTS PATH '$.d', ord FOR ORDINALITY)) AS jt",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", int4()),
            cn("b", text()),
            cn("c", jsonb()),
            cn("d", bool_ty()),
            c("ord", int4()),
        ],
    );
    // NESTED PATH columns, typmods, and the alias' column list.
    let s = db
        .analyze(
            "SELECT * FROM JSON_TABLE('[]', '$[*]' COLUMNS (a int PATH '$.a', \
             NESTED PATH '$.n[*]' COLUMNS (n numeric(5,2) PATH '$', v varchar(3) PATH '$'))) jt(x)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("x", int4()),
            cn("n", numeric_ps(5, 2)),
            cn("v", varchar_n(3)),
        ],
    );
    // LATERAL over a text column, and the default `json_table` name.
    let s = db
        .analyze(
            "SELECT * FROM t, LATERAL JSON_TABLE(t.tx, '$' COLUMNS (v int, o FOR ORDINALITY)) jt",
        )
        .unwrap();
    assert_eq!(s.columns.len(), 5);
    let s = db
        .analyze("SELECT json_table.v FROM JSON_TABLE('[]', '$' COLUMNS (v int))")
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

/// PG 18 types an untyped context-item or PASSING parameter as text.
#[test]
fn json_table_parameters() {
    let db = setup();
    let s = db
        .analyze("SELECT * FROM JSON_TABLE($doc, '$[*]' COLUMNS (a int PATH '$.a')) jt")
        .unwrap();
    assert_params(&s, vec![p(text())]);
    let s = db
        .analyze(
            "SELECT * FROM JSON_TABLE('[1]'::jsonb, '$[*]' PASSING $x AS x \
             COLUMNS (a int PATH '$ ? (@ > $x)')) jt",
        )
        .unwrap();
    assert_params(&s, vec![p(text())]);
}

#[test]
fn json_table_errors() {
    let db = setup();
    let cases: &[(&str, &str)] = &[
        (
            "SELECT * FROM JSON_TABLE('[]', '$' COLUMNS (v int, v text)) jt",
            "duplicate JSON_TABLE column or path name: v",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]', '$' AS p COLUMNS (v int, NESTED PATH '$' AS p COLUMNS (w int))) jt",
            "duplicate JSON_TABLE column or path name: p",
        ),
        (
            "SELECT jt.nope FROM JSON_TABLE('[]', '$' COLUMNS (v int)) jt",
            "column jt.nope does not exist",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]', '$' COLUMNS (v nosuchtype)) jt",
            "type \"nosuchtype\" does not exist",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]', '$' COLUMNS (v text PATH '$' WITH WRAPPER OMIT QUOTES)) jt",
            "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used",
        ),
        (
            "SELECT * FROM JSON_TABLE(t.tx, '$' COLUMNS (v int)) jt",
            "missing FROM-clause entry for table \"t\"",
        ),
        (
            "SELECT * FROM JSON_TABLE(1, '$' COLUMNS (v int)) jt",
            "cannot cast type integer to jsonb",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]', '$' COLUMNS (w int DEFAULT 'x' ON ERROR)) jt",
            "invalid input syntax for type integer: \"x\"",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(sql).unwrap_err();
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}
