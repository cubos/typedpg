//! Set-returning functions: element nullability, lockstep padding in the
//! select list, and function RTEs in FROM (`ROWS FROM`, column definition
//! lists, composite-returning functions).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, arr int[] NOT NULL, s text NOT NULL);
         CREATE TABLE n (id int PRIMARY KEY, x int);",
    )
    .unwrap();
    db
}

// ── Element nullability ──────────────────────────────────────────────────────

/// PG 18: with `arr = '{1,NULL}'`, `unnest(arr) IS NULL` is true for the
/// second row — a NOT NULL array column can still hold NULL elements.
#[test]
fn unnest_of_not_null_array_column_is_nullable() {
    let db = setup();
    let s = db.analyze("SELECT unnest(arr) FROM t").unwrap();
    assert_cols(&s, vec![cn("unnest", int4())]);
    let s = db.analyze("SELECT u FROM t, unnest(arr) AS u(u)").unwrap();
    assert_cols(&s, vec![cn("u", int4())]);
    let s = db
        .analyze("SELECT v FROM t CROSS JOIN LATERAL unnest(t.arr) AS u(v)")
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

#[test]
fn unnest_of_array_literal_with_null_is_nullable() {
    let db = setup();
    let s = db.analyze("SELECT unnest(ARRAY[1, NULL])").unwrap();
    assert_cols(&s, vec![cn("unnest", int4())]);
    let s = db
        .analyze("SELECT * FROM unnest(ARRAY[1, NULL]) u")
        .unwrap();
    assert_cols(&s, vec![cn("u", int4())]);
}

/// An `ARRAY[…]` constructor whose elements are all NOT NULL provably
/// unnests to NOT NULL elements.
#[test]
fn unnest_of_not_null_array_constructor_stays_not_null() {
    let db = setup();
    let s = db.analyze("SELECT unnest(ARRAY[1, 2])").unwrap();
    assert_cols(&s, vec![c("unnest", int4())]);
    let s = db
        .analyze("SELECT e FROM t, unnest(ARRAY[t.id, 3]) AS u(e)")
        .unwrap();
    assert_cols(&s, vec![c("e", int4())]);
}

/// `json[b]_array_elements_text` maps a JSON `null` element to SQL NULL
/// (PG 18: `x IS NULL` is true for `'[null]'`); `jsonb_array_elements`
/// yields the JSON value `null`, which is not SQL NULL.
#[test]
fn json_array_elements_text_is_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT jsonb_array_elements_text('[null]'), jsonb_array_elements('[null]')")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("jsonb_array_elements_text", text()),
            cn("jsonb_array_elements", jsonb()),
        ],
    );
    let s = db
        .analyze("SELECT * FROM jsonb_array_elements('[null]') AS x(v)")
        .unwrap();
    assert_cols(&s, vec![c("v", jsonb())]);
}

/// A strict SRF is not called for a NULL argument — PG returns zero rows
/// (`SELECT generate_series(1, NULL::int)` is empty), so its elements stay
/// NOT NULL even when an argument is nullable.
#[test]
fn strict_srf_with_nullable_argument_is_not_null() {
    let db = setup();
    let s = db.analyze("SELECT generate_series(1, x) FROM n").unwrap();
    assert_cols(&s, vec![c("generate_series", int4())]);
    let s = db
        .analyze("SELECT g FROM n, generate_series(1, n.x) AS g")
        .unwrap();
    assert_cols(&s, vec![c("g", int4())]);
}

// ── Lockstep padding ─────────────────────────────────────────────────────────

/// PG 18: `unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c'])` row 3 is `(NULL, 'c')`.
#[test]
fn multi_arg_unnest_pads_with_null() {
    let db = setup();
    let s = db
        .analyze("SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c'])")
        .unwrap();
    assert_cols(&s, vec![cn("unnest", int4()), cn("unnest", text())]);
}

/// PG 18: `SELECT generate_series(1, 3) a, generate_series(1, 2) b` row 3 is
/// `(3, NULL)` — select-list SRFs run in lockstep (ProjectSet).
#[test]
fn select_list_srfs_in_lockstep_are_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT generate_series(1, 3) a, generate_series(1, 2) b")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
    let s = db
        .analyze("SELECT unnest(ARRAY[1, 2]), unnest(ARRAY['a','b','c'])")
        .unwrap();
    assert_cols(&s, vec![cn("unnest", int4()), cn("unnest", text())]);
    let s = db
        .analyze("SELECT generate_series(1, 3) + 1 AS a, generate_series(1, 2) AS b")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
}

// ── Composite-returning functions in FROM ────────────────────────────────────

/// A row-type value does not enforce its table's NOT NULL constraints —
/// PG 18: `SELECT * FROM jsonb_populate_record(NULL::t, '{}')` returns
/// `id IS NULL` and `s IS NULL` both true.
#[test]
fn composite_srf_columns_ignore_table_not_null() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE r (id int PRIMARY KEY, s text NOT NULL);
         CREATE FUNCTION f_setof_r() RETURNS SETOF r AS $$ SELECT NULL::int, NULL::text $$ LANGUAGE sql;",
    )
    .unwrap();
    let s = db
        .analyze("SELECT * FROM jsonb_populate_record(NULL::r, '{}')")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
    let s = db
        .analyze("SELECT id, s FROM jsonb_populate_recordset(NULL::r, '[{}]')")
        .unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
    let s = db.analyze("SELECT * FROM unnest(ARRAY[NULL::r])").unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
    let s = db.analyze("SELECT * FROM f_setof_r()").unwrap();
    assert_cols(&s, vec![cn("id", int4()), cn("s", text())]);
}

// ── Column definition lists (`AS x(a int, b text)`) ──────────────────────────

fn setup_rte() -> PgCatalog {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION f_rec() RETURNS SETOF record AS $$ SELECT 1, 'x'::text $$ LANGUAGE sql;
         CREATE FUNCTION f_tab(a int) RETURNS TABLE (k int, v text) AS $$ SELECT a, 'x' $$ LANGUAGE sql;
         CREATE TABLE tt (id int, s text);
         CREATE FUNCTION f_comp() RETURNS SETOF tt AS $$ SELECT 1, 'x' $$ LANGUAGE sql;",
    )
    .unwrap();
    db
}

#[test]
fn coldeflist_defines_record_function_columns() {
    let db = setup_rte();
    let s = db
        .analyze("SELECT * FROM f_rec() AS x(a int, b text)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db
        .analyze("SELECT a, b FROM f_rec() AS x(a int, b text)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db
        .analyze("SELECT * FROM jsonb_to_record('{}') AS x(a int, b text)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db
        .analyze("SELECT * FROM json_to_recordset('[{\"a\":1}]') AS x(a int)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4())]);
    let s = db
        .analyze("SELECT * FROM f_rec() AS x(a varchar(3), b numeric(5,2))")
        .unwrap();
    assert_cols(&s, vec![cn("a", varchar_n(3)), cn("b", numeric_ps(5, 2))]);
}

/// get_type_func_class: a function returning a pseudo-type makes no
/// relation, except `record`, `void` and `cstring` (42804).
#[test]
fn function_in_from_returning_a_pseudo_type() {
    let db = setup_rte();
    for (sql, function, type_name) in [
        ("SELECT * FROM unknownin('a')", "unknownin", "unknown"),
        (
            "SELECT x::text AS x FROM unknownin('a') x",
            "unknownin",
            "unknown",
        ),
        (
            "SELECT * FROM unknownin('a') WITH ORDINALITY",
            "unknownin",
            "unknown",
        ),
        (
            "SELECT * FROM ROWS FROM (generate_series(1, 2), pg_catalog.unknownin('a'))",
            "unknownin",
            "unknown",
        ),
        (
            "SELECT * FROM suppress_redundant_updates_trigger()",
            "suppress_redundant_updates_trigger",
            "trigger",
        ),
        (
            "SELECT * FROM plpgsql_call_handler()",
            "plpgsql_call_handler",
            "language_handler",
        ),
    ] {
        let message =
            format!("function \"{function}\" in FROM has unsupported return type {type_name}");
        assert_err_prefix!(db.analyze(sql), AnalyzeError::DatatypeMismatch(_), &message);
    }
    for sql in [
        "SELECT * FROM pg_sleep(0)",
        "SELECT * FROM textout('a')",
        // Called in the select list, they are values like any other.
        "SELECT suppress_redundant_updates_trigger() AS a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // A column definition list is refused first.
    assert_err_prefix!(
        db.analyze("SELECT * FROM unknownin('a') AS x(a int)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is only allowed for functions returning \"record\""
    );
}

#[test]
fn coldeflist_rejected_on_non_record_functions() {
    let db = setup_rte();
    assert_analyze_err!(
        db.analyze("SELECT * FROM generate_series(1,2) AS g(a int)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is only allowed for functions returning \"record\""
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_tab(1) AS g(a int)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is redundant for a function with OUT parameters"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_comp() AS g(a int)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is redundant for a function returning a named composite type"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_rec()"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is required for functions returning \"record\"\n  \
         help: add one after the alias, e.g. `AS x(a int, b text)`\n"
    );
}

#[test]
fn coldeflist_errors() {
    let db = setup_rte();
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_rec() AS x(a int, a text)"),
        AnalyzeError::DuplicateColumn(_),
        "column name \"a\" specified more than once"
    );
    let err = db
        .analyze("SELECT * FROM f_rec() AS x(a int, b nosuchtype)")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::UndefinedType(_)), "{err:?}");
    assert!(
        err.to_string()
            .starts_with("type \"nosuchtype\" does not exist"),
        "{err}"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM unnest(ARRAY[1], ARRAY[2]) AS x(a int, b int)"),
        AnalyzeError::SyntaxError(_),
        "UNNEST() with multiple arguments cannot have a column definition list\n  \
         help: Use separate UNNEST() calls inside ROWS FROM(), and attach a column definition list to each one.\n"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM ROWS FROM (f_rec(), f_rec()) AS x(a int, b text)"),
        AnalyzeError::SyntaxError(_),
        "ROWS FROM() with multiple functions cannot have a column definition list\n  \
         help: Put a separate column definition list for each function inside ROWS FROM().\n"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_rec() WITH ORDINALITY AS x(a int, b text)"),
        AnalyzeError::SyntaxError(_),
        "WITH ORDINALITY cannot be used with a column definition list\n  \
         help: Put the column definition list inside ROWS FROM().\n"
    );
}

/// A function returning a domain over a composite type expands like one
/// returning the composite (TYPEFUNC_COMPOSITE_DOMAIN): its attributes are
/// the columns, and a column definition list is redundant.
#[test]
fn function_returning_a_composite_domain_expands_in_from() {
    let mut db = setup();
    db.apply_sql(
        "CREATE TYPE comptype AS (r float8, i float8);
         CREATE DOMAIN dcomptype AS comptype CHECK ((value).r > 0);
         CREATE FUNCTION makedcomp(r float8, i float8) RETURNS dcomptype
             AS 'select row(r, i)' LANGUAGE sql;",
    )
    .unwrap();
    let s = db.analyze("SELECT * FROM makedcomp(1,2) m").unwrap();
    assert_cols(&s, vec![cn("r", float8()), cn("i", float8())]);
    let s = db
        .analyze("SELECT x, y FROM makedcomp(1,2) AS m(x, y)")
        .unwrap();
    assert_cols(&s, vec![cn("x", float8()), cn("y", float8())]);
    let s = db.analyze("SELECT (m).r FROM makedcomp(1,2) m").unwrap();
    assert_cols(&s, vec![cn("r", float8())]);
    assert_analyze_err!(
        db.analyze("SELECT * FROM makedcomp(1,2) AS m(a int)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is redundant for a function returning a named composite type"
    );
}

/// PG names an unnamed OUT parameter of a row `columnN`, after its
/// position among the OUT parameters (`build_function_result_tupdesc_d`);
/// a lone unnamed one leaves the function scalar, its column named after
/// the alias.
#[test]
fn unnamed_out_parameters_are_result_columns() {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION f_out(in f1 int, out f2 int, out text)
             AS 'select $1, ''x''' LANGUAGE sql;
         CREATE FUNCTION f_inout(in f1 int, inout f2 int, out text)
             AS 'select $2, ''x''' LANGUAGE sql;
         CREATE FUNCTION f_anon(out int, out text) AS 'select 1, ''x''' LANGUAGE sql;
         CREATE FUNCTION f_one(out int) AS 'select 1' LANGUAGE sql;",
    )
    .unwrap();
    let s = db.analyze("SELECT * FROM f_out(42)").unwrap();
    assert_cols(&s, vec![cn("f2", int4()), cn("column2", text())]);
    let s = db.analyze("SELECT * FROM f_inout(42, 99)").unwrap();
    assert_cols(&s, vec![cn("f2", int4()), cn("column2", text())]);
    let s = db.analyze("SELECT * FROM f_out(42) AS p(a, b)").unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db.analyze("SELECT * FROM f_anon()").unwrap();
    assert_cols(&s, vec![cn("column1", int4()), cn("column2", text())]);
    let s = db.analyze("SELECT (f_anon()).column2").unwrap();
    assert_cols(&s, vec![cn("column2", text())]);
    let s = db.analyze("SELECT * FROM f_one() z").unwrap();
    assert_cols(&s, vec![cn("z", int4())]);
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_anon() AS x(a int, b text)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is redundant for a function with OUT parameters"
    );
}

/// A pseudo-type result other than `void` and `cstring` — here `anyarray`
/// that no polymorphic argument resolves — is TYPEFUNC_OTHER, which a
/// function RTE rejects; the same call is fine in the select list.
#[test]
fn anyarray_returning_function_in_from_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT * FROM array_in('{1,2,3}', 'int4'::regtype, -1)"),
        AnalyzeError::DatatypeMismatch(_),
        "function \"array_in\" in FROM has unsupported return type anyarray"
    );
    db.analyze("SELECT array_in('{1,2,3}', 'int4'::regtype, -1)")
        .unwrap();
    let s = db.analyze("SELECT * FROM textout('abc')").unwrap();
    assert_cols(&s, vec![c("textout", basic("pg_catalog", "cstring"))]);
}

// ── Other expressions in FROM ────────────────────────────────────────────────

/// `func_expr_windowless` also admits `COALESCE`, `CAST`, `NULLIF`, …: PG
/// classifies their result type like a function's, so a `record` takes a
/// column definition list and a scalar is one column named after the
/// alias or the expression.
#[test]
fn coalesce_in_from_with_a_column_definition_list() {
    let db = setup();
    let s = db
        .analyze("SELECT * FROM coalesce(row(1,2)) AS (a int, b int)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
    let s = db
        .analyze("SELECT * FROM coalesce(NULL, row(1,2)) AS (a int, b int)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
    assert_analyze_err!(
        db.analyze("SELECT * FROM coalesce(row(1,2)) AS x"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is required for functions returning \"record\"\n  \
         help: add one after the alias, e.g. `AS x(a int, b text)`\n"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM coalesce(1) AS x(a int)"),
        AnalyzeError::SyntaxError(_),
        "a column definition list is only allowed for functions returning \"record\""
    );
    let s = db.analyze("SELECT * FROM coalesce(1, 2)").unwrap();
    assert_cols(&s, vec![c("coalesce", int4())]);
    let s = db.analyze("SELECT * FROM coalesce(1, 2) AS x").unwrap();
    assert_cols(&s, vec![c("x", int4())]);
    let s = db.analyze("SELECT * FROM cast('1' as int)").unwrap();
    assert_cols(&s, vec![c("int4", int4())]);
    let s = db.analyze("SELECT * FROM n, nullif(n.x, 2) v").unwrap();
    assert_cols(&s, vec![c("id", int4()), cn("x", int4()), cn("v", int4())]);
}

/// The row a `record` item returns must match the column definition list
/// exactly (`tupledesc_match`): where it is a `ROW(…)` the query fixes,
/// every execution fails with this error.
#[test]
fn record_expression_in_from_must_match_its_column_definition_list() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT * FROM coalesce(row(1)) AS (a int, b int)"),
        AnalyzeError::DatatypeMismatch(_),
        "function return row and query-specified return row do not match \
         (Returned row contains 1 attribute, but query expects 2.)"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM coalesce(row(1,2)) AS (a int8, b int)"),
        AnalyzeError::DatatypeMismatch(_),
        "function return row and query-specified return row do not match \
         (Returned type integer at ordinal position 1, but query expects bigint.)"
    );
    // The second row is never returned: the first is never NULL.
    let s = db
        .analyze("SELECT * FROM coalesce(row(1,2), row(1)) AS (a int, b int)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", int4())]);
}

// ── ROWS FROM (…) ────────────────────────────────────────────────────────────

/// Every function contributes its columns; the shorter results are padded
/// with NULL (PG 18: row 3 of the first query is `(3, NULL)`).
#[test]
fn rows_from_keeps_every_function() {
    let db = setup_rte();
    let s = db
        .analyze(
            "SELECT * FROM ROWS FROM (generate_series(1, 3), unnest(ARRAY['a', 'b'])) AS r(a, b)",
        )
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db
        .analyze("SELECT * FROM ROWS FROM (generate_series(1, 3), f_tab(1)) WITH ORDINALITY")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("generate_series", int4()),
            cn("k", int4()),
            cn("v", text()),
            c("ordinality", int8()),
        ],
    );
    let s = db
        .analyze("SELECT * FROM ROWS FROM (f_rec() AS (a int, b text), generate_series(1,2)) x")
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", int4()),
            cn("b", text()),
            cn("generate_series", int4()),
        ],
    );
    let s = db
        .analyze("SELECT * FROM ROWS FROM (f_rec()) AS x(a int, b text)")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db
        .analyze("SELECT * FROM ROWS FROM (unnest(ARRAY[1,2], ARRAY[3]))")
        .unwrap();
    assert_cols(&s, vec![cn("unnest", int4()), cn("unnest", int4())]);
    // A lone function keeps its own nullability and the alias names it.
    let s = db
        .analyze("SELECT * FROM ROWS FROM (generate_series(1,2)) AS g")
        .unwrap();
    assert_cols(&s, vec![c("g", int4())]);
}

// ── Column alias count ───────────────────────────────────────────────────────

#[test]
fn too_many_column_aliases_for_function_rte() {
    let db = setup_rte();
    assert_analyze_err!(
        db.analyze("SELECT * FROM generate_series(1, 3) AS g(a, b)"),
        AnalyzeError::InvalidColumnReference(_),
        "table \"g\" has 1 columns available but 2 columns specified"
    );
    assert_analyze_err!(
        db.analyze("SELECT * FROM f_tab(1) AS ft(a, b, c)"),
        AnalyzeError::InvalidColumnReference(_),
        "table \"ft\" has 2 columns available but 3 columns specified"
    );
    assert_analyze_err!(
        db.analyze(
            "SELECT * FROM ROWS FROM (generate_series(1, 3), f_tab(1)) WITH ORDINALITY AS r(a,b,c,d,e)"
        ),
        AnalyzeError::InvalidColumnReference(_),
        "table \"r\" has 4 columns available but 5 columns specified"
    );
}

// ── Placement ────────────────────────────────────────────────────────────────

/// PG's `check_srf_call_placement` and the CASE / COALESCE / aggregate /
/// window nesting rules (all 0A000).
#[test]
fn srf_placement_rules() {
    let db = setup();
    let cases: &[(&str, &str)] = &[
        (
            "SELECT count(*) FROM t WHERE generate_series(1, 2) > 0",
            "set-returning functions are not allowed in WHERE",
        ),
        (
            "SELECT id FROM t GROUP BY id HAVING generate_series(1,2) > 0",
            "set-returning functions are not allowed in HAVING",
        ),
        (
            "SELECT CASE WHEN true THEN generate_series(1, 3) END",
            "set-returning functions are not allowed in CASE",
        ),
        (
            "SELECT COALESCE(generate_series(1, 3), 0)",
            "set-returning functions are not allowed in COALESCE",
        ),
        (
            "SELECT sum(generate_series(1, 3))",
            "aggregate function calls cannot contain set-returning function calls",
        ),
        (
            "SELECT lag(generate_series(1,2)) OVER ()",
            "window function calls cannot contain set-returning function calls",
        ),
        (
            "SELECT count(*) FILTER (WHERE generate_series(1,2) > 0) FROM t",
            "set-returning functions are not allowed in FILTER",
        ),
        (
            "SELECT * FROM t JOIN t t2 ON generate_series(1,2) = 1",
            "set-returning functions are not allowed in JOIN conditions",
        ),
        (
            "SELECT * FROM t LIMIT generate_series(1,2)",
            "set-returning functions are not allowed in LIMIT",
        ),
        (
            "VALUES (generate_series(1,2)), (1)",
            "set-returning functions are not allowed in VALUES",
        ),
        (
            "UPDATE n SET x = generate_series(1,2)",
            "set-returning functions are not allowed in UPDATE",
        ),
        (
            "DELETE FROM t WHERE generate_series(1,2) = 1",
            "set-returning functions are not allowed in WHERE",
        ),
        (
            "SELECT * FROM generate_series(1, generate_series(1,2))",
            "set-returning functions must appear at top level of FROM",
        ),
        (
            "SELECT * FROM cast(generate_series(1,2) as int8)",
            "set-returning functions must appear at top level of FROM",
        ),
        (
            "SELECT * FROM nullif(generate_series(1,2), 1)",
            "set-returning functions must appear at top level of FROM",
        ),
        (
            "SELECT * FROM coalesce(generate_series(1,2))",
            "set-returning functions are not allowed in COALESCE",
        ),
    ];
    for (sql, msg) in cases {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::FeatureNotSupported(_)),
            "{sql}: {err:?}"
        );
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
    for sql in [
        "SELECT row_number() OVER (PARTITION BY generate_series(1,2))",
        "SELECT id FROM t GROUP BY generate_series(1,2), id",
        "INSERT INTO n VALUES (generate_series(1,2), 1)",
        "SELECT generate_series(1, generate_series(1, 2))",
        // A cast to the call's own type is no cast: the call is top level.
        "SELECT * FROM cast(generate_series(1,2) as int4)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A FROM-clause function is resolved through the same `ParseFuncOrColumn`
/// as a call in the select list, so its untyped arguments are coerced to
/// the chosen signature: parameters take the declared types and literal
/// contents are validated by the argument type's input function.
#[test]
fn from_function_untyped_args_take_the_resolved_types() {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION f_def(a int, b text DEFAULT 'x', c int DEFAULT 3) RETURNS text
         LANGUAGE sql AS $$ SELECT b $$;",
    )
    .unwrap();
    for (sql, expected) in [
        ("SELECT * FROM generate_series(1, $a) g", vec![p(int4())]),
        ("SELECT * FROM generate_series($a, 10) g", vec![p(int4())]),
        ("SELECT * FROM abs($a) g", vec![p(float8())]),
        ("SELECT * FROM int4pl($a, 1) g", vec![p(int4())]),
        ("SELECT * FROM f_def($a) g", vec![p(int4())]),
        ("SELECT * FROM f_def(1, c => $x) g", vec![p(int4())]),
        (
            "SELECT * FROM ROWS FROM (generate_series(1, $a), generate_series($b, 2)) g",
            vec![p(int4()), p(int4())],
        ),
        (
            "SELECT g FROM t JOIN LATERAL generate_series(1, $a) g ON true",
            vec![p(int4())],
        ),
        (
            "SELECT * FROM n, generate_series($a, n.id) g",
            vec![p(int4())],
        ),
        (
            "SELECT * FROM generate_series($a::timestamptz, $b, $c) g",
            vec![p(timestamptz()), p(timestamptz()), p(interval())],
        ),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_params(&s, expected);
    }
    assert_err_prefix!(
        db.analyze("SELECT * FROM generate_series(1, 'a') g"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"a\""
    );
    assert_err_prefix!(
        db.analyze(
            "SELECT * FROM generate_series('2020-01-01'::timestamptz, '2020-02-01', '1 xday') g"
        ),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type interval: \"1 xday\""
    );
}
