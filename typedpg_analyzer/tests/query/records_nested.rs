//! Anonymous records inside other values: arrays of `ROW(…)`s
//! (`array_agg`, `ARRAY[…]`, `ARRAY(SELECT …)`), scalar subqueries, FROM
//! subqueries and CTEs, CASE / COALESCE / GREATEST / NULLIF, subscripts
//! and the polymorphic built-ins passing records through.
//!
//! The analyzer keeps each record's field list (names, types,
//! nullability) wherever the value's records come from known shapes that
//! agree; PG types every one of these as plain `record` (or `record[]`)
//! with no tuple descriptor, so selecting a field from one fails as it
//! does there.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE mood AS ENUM ('sad', 'happy');
         CREATE TYPE address AS (street TEXT, city TEXT);
         CREATE TABLE t (
             a    INT NOT NULL,
             b    TEXT,
             m    mood,
             addr address
         );
         CREATE FUNCTION rec() RETURNS record LANGUAGE sql AS 'SELECT 1, 2';
         CREATE FUNCTION outrec(OUT x INT, OUT y TEXT)
             LANGUAGE sql AS 'SELECT 1, ''a''';",
    )
    .unwrap();
    db
}

/// `{f1 int4 NOT NULL, f2 text}`, the shape of `ROW(a, b)`.
fn a_b() -> Type {
    anon_record(vec![rf("f1", int4()), rfn("f2", text())])
}

fn mood() -> Type {
    enum_ty("public", "mood", &["sad", "happy"])
}

fn address() -> Type {
    composite(
        "public",
        "address",
        vec![rfn("street", text()), rfn("city", text())],
    )
}

// ── Arrays ───────────────────────────────────────────────────────────────────

#[test]
fn array_agg_of_row_keeps_the_fields() {
    let db = setup();
    for sql in [
        "SELECT array_agg(ROW(a, b)) AS r FROM t",
        "SELECT array_agg((a, b)) AS r FROM t",
        "SELECT array_agg(ROW(a, b) ORDER BY a DESC) AS r FROM t",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![cn("r", array_of(a_b()))]);
        // The elements are never NULL: each is a ROW.
        assert_eq!(
            col(&s, "r").pg_type,
            array_with_elems(a_b(), false),
            "{sql}"
        );
    }
}

#[test]
fn array_agg_of_table_row_stays_composite() {
    let db = setup();
    let s = db.analyze("SELECT array_agg(t) AS r FROM t").unwrap();
    assert_cols(
        &s,
        vec![cn(
            "r",
            array_of(composite(
                "public",
                "t",
                vec![
                    rfn("a", int4()),
                    rfn("b", text()),
                    rfn("m", mood()),
                    rfn("addr", address()),
                ],
            )),
        )],
    );
}

#[test]
fn array_agg_of_row_with_user_type_fields() {
    let db = setup();
    let s = db
        .analyze("SELECT array_agg(ROW(a, m, addr, ARRAY[m])) AS r FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![cn(
            "r",
            array_of(anon_record(vec![
                rf("f1", int4()),
                rfn("f2", mood()),
                rfn("f3", address()),
                rf("f4", array_with_elems(mood(), true)),
            ])),
        )],
    );
}

#[test]
fn array_constructor_of_rows_merges_the_fields() {
    let db = setup();
    // A field is NULL where any element's may be.
    let s = db
        .analyze("SELECT ARRAY[ROW(1, 'x'::text), ROW(2, NULL::text)] AS r")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            array_of(anon_record(vec![rf("f1", int4()), rfn("f2", text())])),
        )],
    );
    // A NULL element has no shape to agree on: the others say.
    let s = db
        .analyze("SELECT ARRAY[ROW(1, 'x'::text), NULL] AS r")
        .unwrap();
    assert_eq!(
        without_field_refinements(col(&s, "r").pg_type.clone()),
        array_with_elems(anon_record(vec![rf("f1", int4()), rf("f2", text())]), true)
    );
    // Multidimensional: the sub-arrays' elements.
    let s = db
        .analyze("SELECT ARRAY[ARRAY[ROW(1, 'x'::text)], ARRAY[ROW(2, 'y'::text)]] AS r")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            array_of(anon_record(vec![rf("f1", int4()), rf("f2", text())])),
        )],
    );
}

#[test]
fn array_constructor_of_rows_that_disagree_has_no_fields() {
    let db = setup();
    // Each element is a `record`, but no one shape describes them all.
    for sql in [
        "SELECT ARRAY[ROW(1, 'x'::text), ROW('x'::text, 1)] AS r",
        "SELECT ARRAY[ROW(1, 'x'::text), ROW(1)] AS r",
        "SELECT ARRAY[ROW(1, 'x'::text), outrec()] AS r",
        "SELECT ARRAY[ROW(1, 'x'::text), NULL::record] AS r",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![c("r", array_of(basic("pg_catalog", "record")))]);
    }
}

#[test]
fn array_of_function_records() {
    let db = setup();
    // OUT parameters name the fields.
    let s = db.analyze("SELECT ARRAY[outrec()] AS r").unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            array_of(anon_record(vec![rfn("x", int4()), rfn("y", text())])),
        )],
    );
    let s = db
        .analyze("SELECT array_agg(outrec()) AS r FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![cn(
            "r",
            array_of(anon_record(vec![rfn("x", int4()), rfn("y", text())])),
        )],
    );
    // A `RETURNS record` function without a column definition list: no
    // one knows its fields.
    let s = db.analyze("SELECT ARRAY[rec()] AS r").unwrap();
    assert_cols(&s, vec![c("r", array_of(basic("pg_catalog", "record")))]);
}

#[test]
fn array_sublink_of_rows() {
    let db = setup();
    let s = db
        .analyze("SELECT ARRAY(SELECT ROW(a, b) FROM t ORDER BY a) AS r")
        .unwrap();
    assert_cols(&s, vec![c("r", array_of(a_b()))]);
    // Of arrays of rows: a multidimensional array of them.
    let s = db
        .analyze("SELECT ARRAY(SELECT ARRAY[ROW(a, b)] FROM t) AS r")
        .unwrap();
    assert_cols(&s, vec![c("r", array_of(a_b()))]);
}

#[test]
fn rows_nested_in_arrays_nested_in_rows() {
    let db = setup();
    let s = db
        .analyze("SELECT ROW(a, ARRAY[ROW(b, ROW(a, m))]) AS r FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![
                rf("f1", int4()),
                rf(
                    "f2",
                    array_with_elems(
                        anon_record(vec![
                            rfn("f1", text()),
                            rf("f2", anon_record(vec![rf("f1", int4()), rfn("f2", mood())])),
                        ]),
                        false,
                    ),
                ),
            ]),
        )],
    );
}

#[test]
fn subscript_and_unnest_of_an_array_of_rows() {
    let db = setup();
    for sql in [
        "SELECT (array_agg(ROW(a, b)))[1] AS r FROM t",
        "SELECT s.arr[2] AS r FROM (SELECT array_agg(ROW(a, b)) AS arr FROM t) s",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![cn("r", a_b())]);
    }
    let s = db
        .analyze("SELECT (array_agg(ROW(a, b)))[1:2] AS r FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("r", array_of(a_b()))]);
    let s = db
        .analyze("SELECT unnest(ARRAY(SELECT ROW(a, b) FROM t)) AS r")
        .unwrap();
    assert_cols(&s, vec![c("r", a_b())]);
}

#[test]
fn array_builtins_pass_the_rows_through() {
    let db = setup();
    for (sql, nullable) in [
        // (Appending to a NULL array makes a one-element array.)
        (
            "SELECT array_append(array_agg(ROW(a, b)), ROW(0, NULL::text)) AS r FROM t",
            false,
        ),
        (
            "SELECT array_prepend(ROW(0, 'z'::text), array_agg(ROW(a, b))) AS r FROM t",
            false,
        ),
        (
            "SELECT array_cat(array_agg(ROW(a, b)), array_agg(ROW(a, b))) AS r FROM t",
            true,
        ),
        (
            "SELECT array_remove(array_agg(ROW(a, b)), NULL) AS r FROM t",
            true,
        ),
        (
            "SELECT trim_array(array_agg(ROW(a, b)), 1) AS r FROM t",
            true,
        ),
        (
            "SELECT array_reverse(array_agg(ROW(a, b))) AS r FROM t",
            true,
        ),
    ] {
        let s = db.analyze(sql).unwrap();
        let expected = if nullable { cn } else { c };
        assert_cols(&s, vec![expected("r", array_of(a_b()))]);
    }
    // Arguments that disagree: no shape.
    let s = db
        .analyze("SELECT array_append(array_agg(ROW(a, b)), ROW(1)) AS r FROM t")
        .unwrap();
    assert_cols(&s, vec![c("r", array_of(basic("pg_catalog", "record")))]);
}

#[test]
fn window_functions_pass_the_row_through() {
    let db = setup();
    // (Past the partition's first row, `lag` has no row: NULL.)
    let s = db
        .analyze("SELECT lag(ROW(a, b)) OVER (ORDER BY a) AS r FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("r", a_b())]);
    for sql in [
        "SELECT first_value(ROW(a, b)) OVER (ORDER BY a) AS r FROM t",
        "SELECT lead(ROW(a, b), 1, ROW(0, NULL::text)) OVER (ORDER BY a) AS r FROM t",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![c("r", a_b())]);
    }
}

// ── Scalar subqueries, FROM subqueries, CTEs ─────────────────────────────────

#[test]
fn scalar_subquery_of_a_row() {
    let db = setup();
    let s = db
        .analyze("SELECT (SELECT ROW(a, b) FROM t LIMIT 1) AS r")
        .unwrap();
    assert_cols(&s, vec![cn("r", a_b())]);
    let s = db
        .analyze("SELECT (SELECT array_agg(ROW(a, b)) FROM t) AS r")
        .unwrap();
    assert_cols(&s, vec![cn("r", array_of(a_b()))]);
}

#[test]
fn arrays_of_rows_through_subqueries_and_ctes() {
    let db = setup();
    for sql in [
        "SELECT s.r FROM (SELECT array_agg(ROW(a, b)) AS r FROM t) s",
        "WITH c AS (SELECT array_agg(ROW(a, b)) AS r FROM t) SELECT r FROM c",
        "WITH c AS MATERIALIZED (SELECT array_agg(ROW(a, b)) AS r FROM t) SELECT r FROM c",
        "SELECT array_agg(ROW(a, b)) AS r FROM t UNION ALL SELECT array_agg(ROW(a, b)) FROM t",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![cn("r", array_of(a_b()))]);
    }
    let s = db
        .analyze("SELECT s.r FROM (SELECT (SELECT ROW(a, b) FROM t LIMIT 1) AS r) s")
        .unwrap();
    assert_cols(&s, vec![cn("r", a_b())]);
    // A VALUES list's rows, and arrays of rows.
    let s = db
        .analyze("SELECT v.r FROM (VALUES (ROW(1, 'x'::text)), (ROW(2, NULL::text))) AS v(r)")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            anon_record(vec![rf("f1", int4()), rfn("f2", text())]),
        )],
    );
    let s = db
        .analyze("SELECT v.r FROM (VALUES (ARRAY[ROW(1, 'x'::text)])) AS v(r)")
        .unwrap();
    assert_cols(
        &s,
        vec![c(
            "r",
            array_of(anon_record(vec![rf("f1", int4()), rf("f2", text())])),
        )],
    );
}

// ── CASE / COALESCE / GREATEST / NULLIF ──────────────────────────────────────

#[test]
fn conditional_rows_keep_an_agreed_shape() {
    let db = setup();
    for sql in [
        "SELECT CASE WHEN a > 0 THEN ROW(a, b) ELSE ROW(0, 'x'::text) END AS r FROM t",
        "SELECT COALESCE((SELECT ROW(a, b) FROM t LIMIT 1), ROW(0, NULL::text)) AS r",
        "SELECT GREATEST(ROW(a, b), ROW(1, 'x'::text)) AS r FROM t",
    ] {
        let s = db.analyze(sql).unwrap();
        assert_cols(&s, vec![c("r", a_b())]);
    }
    // ELSE NULL is no row to agree with.
    let s = db
        .analyze("SELECT CASE WHEN a > 0 THEN ROW(a, b) END AS r FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("r", a_b())]);
    let s = db
        .analyze("SELECT NULLIF(ROW(a, b), ROW(1, 'x'::text)) AS r FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("r", a_b())]);
    let s = db
        .analyze(
            "SELECT CASE WHEN a > 0 THEN array_agg(ROW(a, b)) ELSE ARRAY[ROW(0, 'x'::text)] END \
             AS r FROM t GROUP BY a",
        )
        .unwrap();
    assert_cols(&s, vec![c("r", array_of(a_b()))]);
}

#[test]
fn conditional_rows_that_disagree_have_no_shape() {
    let db = setup();
    for (sql, nullable) in [
        (
            "SELECT CASE WHEN a > 0 THEN ROW(a, b) ELSE ROW(b, a) END AS r FROM t",
            false,
        ),
        (
            "SELECT CASE WHEN a > 0 THEN ROW(a, b) ELSE outrec() END AS r FROM t",
            true,
        ),
        ("SELECT COALESCE(rec(), ROW(1, 2)) AS r", false),
    ] {
        let s = db.analyze(sql).unwrap();
        let expected = if nullable { cn } else { c };
        assert_cols(&s, vec![expected("r", basic("pg_catalog", "record"))]);
    }
}

// ── What PG can't see ────────────────────────────────────────────────────────

#[test]
fn fields_of_records_pg_types_as_plain_record_are_rejected() {
    // The analyzer knows these records' fields, but PG has no tuple
    // descriptor for them: selecting a field fails as it does there.
    let db = setup();
    for sql in [
        "SELECT ((array_agg(ROW(a, b)))[1]).f1 FROM t",
        "SELECT ((SELECT ROW(a, b) FROM t LIMIT 1)).f1",
        "SELECT (CASE WHEN true THEN ROW(1, 2) END).f1",
        "SELECT (unnest(ARRAY[ROW(1, 2)])).f1",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::UndefinedColumn(_),
            "could not identify column \"f1\" in record data type"
        );
    }
    for sql in [
        "SELECT (s.r).f1 FROM (SELECT (SELECT ROW(a, b) FROM t LIMIT 1) AS r) s",
        "SELECT (s.r).f1 FROM (SELECT (array_agg(ROW(a, b)))[1] AS r FROM t) s",
        "WITH c AS (SELECT unnest(ARRAY[ROW(1, 2)]) AS r) SELECT (r).f1 FROM c",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::WrongObjectType(_),
            "record type has not been registered"
        );
    }
    // An array of rows is no composite.
    assert_err_prefix!(
        db.analyze("SELECT (array_agg(ROW(a, b))).f1 FROM t"),
        AnalyzeError::Unsupported(_),
        "column notation .f1 applied to type record[], which is not a composite type"
    );
}
