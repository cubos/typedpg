//! SQL/XML expressions (XMLPARSE, XMLELEMENT, XMLFOREST, XMLCONCAT, XMLPI,
//! XMLROOT, IS DOCUMENT, XMLSERIALIZE) — expectations are PG 18's view
//! column types, prepared-statement parameter types and error messages.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (x xml, s text NOT NULL, n int NOT NULL, vc varchar(5));")
        .unwrap();
    db
}

fn xml() -> Type {
    basic("pg_catalog", "xml")
}

#[test]
fn xml_expression_result_types() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT XMLPARSE(DOCUMENT '<a/>') AS a, xmlserialize(content x as text) AS b, \
             xmlelement(name foo) AS c, x IS DOCUMENT AS d, xmlforest(n, s) AS e, \
             xmlconcat(x, x) AS f, xmlpi(name php, 'echo') AS g, \
             xmlroot(x, version '1.0') AS h, xmlserialize(document x as varchar(10)) AS i, \
             xmlserialize(content x as bpchar) AS j, \
             xmlelement(name foo, xmlattributes(n as a, s), 'content', n) AS k FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", xml()),
            cn("b", text()),
            c("c", xml()),
            cn("d", bool_ty()),
            c("e", xml()),
            cn("f", xml()),
            c("g", xml()),
            cn("h", xml()),
            cn("i", varchar_n(10)),
            cn("j", bpchar()),
            c("k", xml()),
        ],
    );
    let s = db
        .analyze(
            "SELECT XMLPARSE(DOCUMENT '<a/>'), xmlserialize(content x as text), \
             xmlelement(name foo), x IS DOCUMENT, xmlforest(n, s), xmlconcat(x, x), \
             xmlpi(name php, 'echo'), xmlroot(x, version '1.0') FROM t",
        )
        .unwrap();
    let names: Vec<_> = s.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "xmlparse",
            "xmlserialize",
            "xmlelement",
            "?column?",
            "xmlforest",
            "xmlconcat",
            "xmlpi",
            "xmlroot"
        ]
    );
}

#[test]
fn xml_expression_parameter_types() {
    let db = setup();
    for (sql, expected) in [
        ("SELECT xmlparse(content $p) AS a", vec![text()]),
        ("SELECT xmlconcat($p, $q) AS a", vec![xml(), xml()]),
        ("SELECT xmlserialize(content $p as text) AS a", vec![xml()]),
        ("SELECT $p IS DOCUMENT AS a", vec![xml()]),
        ("SELECT xmlpi(name foo, $p) AS a", vec![text()]),
        ("SELECT xmlroot($p, version $q) AS a", vec![xml(), text()]),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let got: Vec<_> = s.params.iter().map(|p| p.pg_type.clone()).collect();
        assert_eq!(got, expected, "{sql}");
    }
    // Element content and XMLFOREST values are not coerced.
    for sql in [
        "SELECT xmlelement(name foo, $p) AS a",
        "SELECT xmlforest($p AS a) AS a",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::IndeterminateType(_),
            "could not determine data type of parameter $1"
        );
    }
}

#[test]
fn xml_expression_errors_match_pg() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT xmlconcat(n) FROM t",
            "argument of XMLCONCAT must be type xml, not type integer",
        ),
        (
            "SELECT n IS DOCUMENT FROM t",
            "argument of IS DOCUMENT must be type xml, not type integer",
        ),
        (
            "SELECT xmlroot(n, version '1') FROM t",
            "argument of XMLROOT must be type xml, not type integer",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::DatatypeMismatch(_), msg);
    }
    for (sql, msg) in [
        (
            "SELECT xmlelement(name foo, xmlattributes(n + 1)) FROM t",
            "unnamed XML attribute value must be a column reference",
        ),
        (
            "SELECT xmlforest(n + 1) FROM t",
            "unnamed XML element value must be a column reference",
        ),
        (
            "SELECT xmlelement(name foo, xmlattributes(n, n)) FROM t",
            "XML attribute name \"n\" appears more than once",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::SyntaxError(_), msg);
    }
    assert_err_prefix!(
        db.analyze("SELECT xmlserialize(content x as int) FROM t"),
        AnalyzeError::Invalid(_),
        "cannot cast XMLSERIALIZE result to integer"
    );
    // Assignment casts are accepted (int → text).
    for sql in [
        "SELECT xmlparse(content n) AS a FROM t",
        "SELECT xmlroot(x, version 1) AS a FROM t",
        "SELECT xmlpi(name foo, n) AS a FROM t",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
