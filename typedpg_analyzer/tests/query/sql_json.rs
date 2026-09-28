//! SQL/JSON expressions (PG 16/17): JSON_VALUE / JSON_QUERY / JSON_EXISTS,
//! JSON() / JSON_SCALAR / JSON_SERIALIZE, JSON_OBJECT / JSON_ARRAY,
//! JSON_OBJECTAGG / JSON_ARRAYAGG and IS JSON. Every expectation is what
//! PG 18 reported for the same statement (view column types, prepared
//! statement parameter types, error messages).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id int PRIMARY KEY,
            j jsonb NOT NULL,
            jn jsonb,
            s text NOT NULL,
            js json,
            vc varchar(5),
            b bytea
         );",
    )
    .unwrap();
    db
}

fn json() -> Type {
    json_ty()
}

#[test]
fn query_functions_result_types() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT JSON_VALUE(j, '$.a') AS v1, JSON_VALUE(j, '$.a' RETURNING int) AS v2, \
             JSON_VALUE(j, '$.a' RETURNING varchar(3)) AS v3, JSON_VALUE(js, '$.a') AS v4, \
             JSON_VALUE(s, '$.a') AS v5, JSON_QUERY(j, '$.a') AS q1, JSON_QUERY(js, '$.a') AS q2, \
             JSON_QUERY(j, '$.a' RETURNING json) AS q3, JSON_QUERY(j, '$.a' RETURNING text) AS q4, \
             JSON_EXISTS(j, '$.a') AS e1, JSON_EXISTS(jn, '$.a') AS e2 FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("v1", text()),
            cn("v2", int4()),
            cn("v3", varchar_n(3)),
            cn("v4", text()),
            cn("v5", text()),
            cn("q1", jsonb()),
            cn("q2", jsonb()),
            cn("q3", json()),
            cn("q4", text()),
            c("e1", bool_ty()),
            cn("e2", bool_ty()),
        ],
    );
    // The default column names follow the function names.
    let s = db
        .analyze("SELECT JSON_VALUE(j, '$.a'), JSON_QUERY(j, '$'), JSON_EXISTS(j, '$') FROM t")
        .unwrap();
    let names: Vec<_> = s.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["json_value", "json_query", "json_exists"]);
}

#[test]
fn constructors_and_parse_functions_result_types() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT JSON('{\"a\":1}') AS p1, JSON(s) AS p2, JSON_SCALAR(1) AS sc1, \
             JSON_SCALAR(id) AS sc2, JSON_SERIALIZE(js) AS se1, \
             JSON_SERIALIZE(j RETURNING bytea) AS se2, \
             JSON_SERIALIZE(j RETURNING varchar(10)) AS se3, JSON_OBJECT('a' VALUE 1) AS o1, \
             JSON_OBJECT('a' VALUE id RETURNING jsonb) AS o2, JSON_OBJECT() AS o3, \
             JSON_OBJECT('a': s RETURNING text) AS o4, JSON_ARRAY(1, 2) AS a1, \
             JSON_ARRAY(SELECT 1) AS a2, JSON_ARRAY() AS a3, \
             JSON_ARRAY(id, s RETURNING jsonb) AS a4 FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("p1", json()),
            c("p2", json()),
            c("sc1", json()),
            c("sc2", json()),
            cn("se1", text()),
            c("se2", bytea()),
            c("se3", varchar_n(10)),
            c("o1", json()),
            c("o2", jsonb()),
            c("o3", json()),
            c("o4", text()),
            c("a1", json()),
            cn("a2", json()),
            c("a3", json()),
            c("a4", jsonb()),
        ],
    );
    let s = db
        .analyze(
            "SELECT s IS JSON AS i1, js IS JSON OBJECT AS i2, \
             j IS JSON ARRAY WITH UNIQUE KEYS AS i3, vc IS JSON SCALAR AS i4, \
             b IS JSON VALUE AS i5, s IS NOT JSON AS i6 FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("i1", bool_ty()),
            cn("i2", bool_ty()),
            c("i3", bool_ty()),
            cn("i4", bool_ty()),
            cn("i5", bool_ty()),
            c("i6", bool_ty()),
        ],
    );
}

#[test]
fn constructors_return_jsonb_when_an_argument_is_jsonb() {
    // transformJsonConstructorOutput: no RETURNING and a jsonb argument
    // makes the constructor build (and return) jsonb.
    let db = setup();
    let s = db
        .analyze(
            "SELECT JSON_OBJECT('a' VALUE j) AS a, JSON_ARRAY(j) AS b, JSON_ARRAY(1, j) AS c, \
             JSON_OBJECT('a' VALUE j FORMAT JSON) AS d, JSON_ARRAY(SELECT j FROM t) AS e, \
             JSON_ARRAY(js, j) AS g, JSON_ARRAY(js) AS h FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", jsonb()),
            c("b", jsonb()),
            c("c", jsonb()),
            c("d", jsonb()),
            cn("e", jsonb()),
            c("g", jsonb()),
            c("h", json()),
        ],
    );
    let s = db
        .analyze("SELECT JSON_ARRAYAGG(j) AS a, JSON_OBJECTAGG(s VALUE j) AS b FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("a", jsonb()), cn("b", jsonb())]);
}

#[test]
fn json_aggregates() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT JSON_OBJECTAGG(s VALUE id) AS g1, JSON_ARRAYAGG(id ORDER BY id) AS g2, \
             JSON_ARRAYAGG(s RETURNING jsonb) AS g3, JSON_OBJECTAGG(s: j RETURNING text) AS g4 \
             FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("g1", json()),
            cn("g2", json()),
            cn("g3", jsonb()),
            cn("g4", text()),
        ],
    );
    db.analyze("SELECT JSON_ARRAYAGG(id) OVER () AS w FROM t")
        .unwrap();
    assert_err_prefix!(
        db.analyze("SELECT JSON_OBJECTAGG(s VALUE id) FROM t WHERE JSON_ARRAYAGG(id) IS NULL"),
        AnalyzeError::GroupingError(_),
        "aggregate functions are not allowed in WHERE"
    );
}

#[test]
fn sql_json_parameter_types() {
    // pg_prepared_statements.parameter_types on PG 18.
    let db = setup();
    for (sql, expected) in [
        ("SELECT JSON_VALUE($p, '$.a') AS a", vec![text()]),
        ("SELECT $p IS JSON AS a", vec![text()]),
        ("SELECT JSON_OBJECT('a' VALUE $p) AS a", vec![text()]),
        ("SELECT JSON_SCALAR($p) AS a", vec![text()]),
        (
            "SELECT JSON_VALUE(j, '$.a' PASSING $p AS x) AS a FROM t",
            vec![text()],
        ),
        (
            "SELECT JSON_VALUE(j, $p) AS a FROM t",
            vec![basic("pg_catalog", "jsonpath")],
        ),
        ("SELECT JSON_ARRAY($p, $q) AS a", vec![text(), text()]),
        ("SELECT JSON($p) AS a", vec![text()]),
        ("SELECT JSON_SERIALIZE($p) AS a", vec![text()]),
        ("SELECT JSON_QUERY($p, '$') AS a", vec![text()]),
        ("SELECT JSON_EXISTS($p, '$') AS a", vec![text()]),
        ("SELECT JSON_ARRAYAGG($p) AS a FROM t", vec![text()]),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let got: Vec<_> = s.params.iter().map(|p| p.pg_type.clone()).collect();
        assert_eq!(got, expected, "{sql}");
    }
    // An object key is an `"any"` argument: nothing types a bare $N.
    for sql in [
        "SELECT JSON_OBJECT($p VALUE 1) AS a",
        "SELECT JSON_OBJECTAGG($k VALUE $v) AS a FROM t",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::IndeterminateType(_),
            "could not determine data type of parameter $1"
        );
    }
}

#[test]
fn sql_json_errors_match_pg() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT JSON_VALUE(id, '$.a') FROM t",
            "cannot cast type integer to jsonb",
        ),
        ("SELECT JSON(id) FROM t", "cannot cast type integer to json"),
        (
            "SELECT JSON_OBJECT('a' VALUE 1 RETURNING int)",
            "cannot cast type json to integer",
        ),
        (
            "SELECT JSON_ARRAY(1 RETURNING int)",
            "cannot cast type json to integer",
        ),
        (
            "SELECT JSON_VALUE(j, '$.a' RETURNING record) FROM t",
            "returning pseudo-types is not supported in SQL/JSON functions",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), msg);
    }
    for (sql, msg) in [
        (
            "SELECT id IS JSON FROM t",
            "cannot use type integer in IS JSON predicate",
        ),
        (
            "SELECT JSON_SERIALIZE(id) FROM t",
            "cannot use non-string types with implicit FORMAT JSON clause",
        ),
        (
            "SELECT JSON_OBJECT('a' VALUE id FORMAT JSON) FROM t",
            "cannot use non-string types with explicit FORMAT JSON clause",
        ),
        (
            "SELECT JSON_SERIALIZE(j RETURNING int) FROM t",
            "cannot use type integer in RETURNING clause of JSON_SERIALIZE()",
        ),
        (
            "SELECT JSON_VALUE(j, 1) FROM t",
            "JSON path expression must be of type jsonpath, not of type integer",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::DatatypeMismatch(_), msg);
    }
    for (sql, msg) in [
        (
            "SELECT JSON_VALUE(j, '$.a' EMPTY ARRAY ON ERROR) FROM t",
            "invalid ON ERROR behavior",
        ),
        (
            "SELECT JSON_QUERY(j, '$.a' TRUE ON EMPTY) FROM t",
            "invalid ON EMPTY behavior",
        ),
        (
            "SELECT JSON_EXISTS(j, '$.a' NULL ON ERROR) FROM t",
            "invalid ON ERROR behavior",
        ),
        (
            "SELECT JSON_ARRAY(SELECT 1, 2)",
            "subquery must return only one column",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::SyntaxError(_), msg);
    }
    assert_err_prefix!(
        db.analyze("SELECT JSON_VALUE(j, 'x') FROM t"),
        AnalyzeError::InvalidLiteral(_),
        "syntax error at end of jsonpath input"
    );
    assert_err_prefix!(
        db.analyze("SELECT JSON_VALUE(j, '$.a' RETURNING int DEFAULT 'x' ON EMPTY) FROM t"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"x\""
    );
    for sql in [
        "SELECT JSON_VALUE(j, '$.a' DEFAULT id ON EMPTY) FROM t",
        "SELECT JSON_VALUE(j, '$.a' RETURNING int DEFAULT $p ON EMPTY) FROM t",
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(
            err.to_string().starts_with(
                "can only specify a constant, non-aggregate function, or operator expression \
                 for DEFAULT"
            ),
            "{sql}: {err}"
        );
    }
    for sql in [
        "SELECT JSON_VALUE(j, '$.a' RETURNING int DEFAULT '1' ON EMPTY) AS a FROM t",
        "SELECT JSON_VALUE(j, '$.a' RETURNING int DEFAULT 1 + 1 ON ERROR) AS a FROM t",
        "SELECT JSON_VALUE(j, '$.a' RETURNING int DEFAULT true ON ERROR) AS a FROM t",
        "SELECT JSON_EXISTS(j, '$.a' UNKNOWN ON ERROR) AS a FROM t",
        "SELECT JSON_QUERY(j, '$' WITH WRAPPER KEEP QUOTES) AS a FROM t",
        "SELECT JSON_VALUE(j, '$.a' PASSING j AS x, id AS y) AS a FROM t",
        "SELECT JSON_VALUE(j, '$.a' RETURNING int[]) AS a FROM t",
        "SELECT JSON_SCALAR(j) AS a FROM t",
        "SELECT JSON_OBJECT('a' VALUE j FORMAT JSON) AS a FROM t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn sql_json_returning_format_and_wrapper_rules() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT JSON_VALUE(j, '$.a' RETURNING text FORMAT JSON) FROM t",
            "cannot specify FORMAT JSON in RETURNING clause of JSON_VALUE()",
        ),
        (
            "SELECT JSON_QUERY(j, '$' WITH CONDITIONAL WRAPPER OMIT QUOTES) FROM t",
            "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::SyntaxError(_), msg);
    }
    assert_err_prefix!(
        db.analyze("SELECT JSON_VALUE(j, '$' DEFAULT count(*) ON EMPTY) FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "can only specify a constant, non-aggregate function, or operator expression for DEFAULT"
    );
    for (sql, msg) in [
        (
            "SELECT JSON_SERIALIZE(j RETURNING text FORMAT JSON ENCODING UTF8) FROM t",
            "cannot set JSON encoding for non-bytea output types",
        ),
        (
            "SELECT JSON_VALUE(b, '$.a') FROM t",
            "cannot cast type bytea to jsonb",
        ),
        (
            "SELECT JSON_QUERY(j, '$.a' RETURNING int FORMAT JSON) FROM t",
            "cannot use JSON format with non-string output types",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), msg);
    }
    // Encoded JSON in bytea is decoded under an explicit FORMAT JSON.
    db.analyze("SELECT JSON_VALUE(b FORMAT JSON ENCODING UTF8, '$') AS r FROM t")
        .unwrap();
}
