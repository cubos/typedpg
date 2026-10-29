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

/// A COLUMNS list's own columns come first, then each NESTED PATH's, in
/// turn and recursively — wherever the NESTED PATH is written
/// (transformJsonTableColumns).
#[test]
fn json_table_parent_columns_come_before_nested_ones() {
    let db = setup();
    let names = |sql: &str| -> Vec<String> {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .columns
            .into_iter()
            .map(|c| c.name)
            .collect()
    };
    assert_eq!(
        names(
            "SELECT * FROM JSON_TABLE(jsonb '{}', '$' COLUMNS (
                 NESTED PATH '$.a' COLUMNS (c int PATH '$'),
                 xx1 int PATH '$.c'))"
        ),
        ["xx1", "c"]
    );
    assert_eq!(
        names(
            "SELECT * FROM JSON_TABLE(jsonb '{}', '$' COLUMNS (
                 NESTED PATH '$.a' COLUMNS (
                     c int PATH '$', NESTED PATH '$.b' COLUMNS (d int), e int),
                 xx1 int PATH '$.c',
                 NESTED PATH '$.z' COLUMNS (z int),
                 yy int))"
        ),
        ["xx1", "yy", "c", "e", "d", "z"]
    );
    // The column alias list follows that order.
    assert_eq!(
        names(
            "SELECT * FROM JSON_TABLE(jsonb '{}', '$' COLUMNS (
                 NESTED PATH '$.a' COLUMNS (c int PATH '$'),
                 xx1 int PATH '$.c')) jt (p, q)"
        ),
        ["p", "q"]
    );
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

/// A JSON_TABLE's own ON ERROR is only ERROR or EMPTY [ARRAY]; a column's
/// ON EMPTY / ON ERROR are those of what it runs as: JSON_EXISTS for an
/// EXISTS column, JSON_QUERY for a FORMAT JSON column or a regular one with
/// a wrapper / quotes clause or of a json, array or composite type,
/// JSON_VALUE otherwise. The messages name the column.
#[test]
fn json_table_rejects_invalid_behaviors() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int) NULL ON ERROR)",
            "invalid ON ERROR behavior",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]', 'strict $.a' COLUMNS (js2 int PATH '$') DEFAULT 1 ON ERROR)",
            "invalid ON ERROR behavior",
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int) EMPTY OBJECT ON ERROR)",
            "invalid ON ERROR behavior",
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int true on empty))",
            r#"invalid ON EMPTY behavior for column "a""#,
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int PATH '$' EMPTY ON ERROR))",
            r#"invalid ON ERROR behavior for column "a""#,
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int omit quotes true on error))",
            r#"invalid ON ERROR behavior for column "a""#,
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int FORMAT JSON TRUE ON ERROR))",
            r#"invalid ON ERROR behavior for column "a""#,
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int exists empty object on error))",
            r#"invalid ON ERROR behavior for column "a""#,
        ),
        (
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (\
             NESTED PATH '$' COLUMNS (\"B c\" int NULL ON EMPTY TRUE ON ERROR)))",
            r#"invalid ON ERROR behavior for column "B c""#,
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::SyntaxError(_), msg);
    }
    assert_err_prefix!(
        db.analyze(
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a bit(3) DEFAULT 1 ON ERROR))"
        ),
        AnalyzeError::CannotCoerce(_),
        "cannot cast behavior expression of type integer to bit"
    );
    for sql in [
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int) ERROR ON ERROR)",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int) EMPTY ON ERROR)",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int) EMPTY ARRAY ON ERROR)",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int[] EMPTY ARRAY ON ERROR))",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a jsonb EMPTY OBJECT ON ERROR))",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a text KEEP QUOTES EMPTY ON EMPTY))",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' \
         COLUMNS (a text WITHOUT WRAPPER EMPTY OBJECT ON ERROR))",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int EXISTS UNKNOWN ON ERROR))",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int PATH '$' DEFAULT 2 ON EMPTY))",
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int PATH '$' DEFAULT true ON ERROR))",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A column's DEFAULT is checked as JSON_VALUE's is (transformJsonBehavior):
/// a constant, function or operator expression without column references
/// — parameters and subqueries inside an operator are fine.
#[test]
fn json_table_column_default_shape() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT * FROM t, JSON_TABLE(jsonb '1', '$' \
             COLUMNS (a int PATH '$' DEFAULT t.id + 1 ON ERROR)) jt",
            "DEFAULT expression must not contain column references",
        ),
        (
            "SELECT * FROM t, JSON_TABLE(jsonb '1', '$' \
             COLUMNS (a int PATH '$' DEFAULT t.id ON ERROR)) jt",
            "can only specify a constant, non-aggregate function, or operator expression \
             for DEFAULT",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::DatatypeMismatch(_), msg);
    }
    let s = db
        .analyze(
            "SELECT * FROM JSON_TABLE(jsonb '1', '$' \
             COLUMNS (a int PATH '$' DEFAULT $1 + 1 ON ERROR)) jt",
        )
        .unwrap();
    assert_params(&s, vec![p(int4())]);
    db.analyze(
        "SELECT * FROM JSON_TABLE(jsonb '1', '$' \
         COLUMNS (a int PATH '$' DEFAULT (SELECT 1) + 1 ON ERROR)) jt",
    )
    .unwrap();
}

/// Too many column aliases: PG names the function, not the alias
/// (addRangeTableEntryForTableFunc).
#[test]
fn json_table_alias_count_message() {
    let db = setup();
    assert_err_prefix!(
        db.analyze(
            "SELECT * FROM JSON_TABLE(NULL::jsonb, '$' COLUMNS (v1 timestamp)) AS f (v1, v2)"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "JSON_TABLE function has 1 columns available but 2 columns specified"
    );
    assert_err_prefix!(
        db.analyze(
            "SELECT * FROM JSON_TABLE(jsonb '{}', '$' COLUMNS (a int, \
             NESTED PATH '$' COLUMNS (b int))) AS f (x, y, z)"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "JSON_TABLE function has 2 columns available but 3 columns specified"
    );
    db.analyze("SELECT * FROM JSON_TABLE(NULL::jsonb, '$' COLUMNS (v1 timestamp)) AS f (v1)")
        .unwrap();
}

/// A non-constant JSON_TABLE path is rejected by the grammar as a feature
/// not supported (0A000), not as a syntax error.
#[test]
fn json_table_non_constant_path_is_not_supported() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("SELECT * FROM JSON_TABLE(jsonb '[]', $p COLUMNS (a int)) jt"),
        AnalyzeError::FeatureNotSupported(_),
        "only string constants are supported in JSON_TABLE path specification"
    );
}

/// Every JSON_TABLE path — the row pattern, a column's PATH / EXISTS PATH,
/// a NESTED PATH — becomes a jsonpath constant at parse time, so its
/// content is parsed by jsonpath_in.
#[test]
fn json_table_paths_are_parsed_as_jsonpath() {
    let db = setup();
    for sql in [
        "SELECT * FROM JSON_TABLE(jsonb '[1]', '$[*' COLUMNS (a int)) jt",
        "SELECT * FROM JSON_TABLE(jsonb '[1]', '$[*]' COLUMNS (a int PATH '$[*')) jt",
        "SELECT * FROM JSON_TABLE(jsonb '[1]', '$[*]' COLUMNS (a int EXISTS PATH '$[*')) jt",
        "SELECT * FROM JSON_TABLE(jsonb '[1]', '$[*]' \
         COLUMNS (NESTED PATH '$[*' COLUMNS (a int))) jt",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::InvalidLiteral(_),
            "syntax error at end of jsonpath input"
        );
    }
}
