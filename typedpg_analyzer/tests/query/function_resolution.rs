//! Function and operator overload resolution (PG §10.2/§10.3): candidate
//! gathering with variadic/default expansion, the `func_select_candidate`
//! heuristics for unknown-typed arguments, and polymorphic resolution
//! (`enforce_generic_type_consistency`). Every expectation below was
//! observed on PostgreSQL 18.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE m (vc varchar(10) NOT NULL, ch char(3) NOT NULL, ts timestamp NOT NULL,
                         d date NOT NULL, tz timestamptz NOT NULL);
         CREATE TABLE t (id int PRIMARY KEY, x int NOT NULL);
         CREATE TABLE tj (id int PRIMARY KEY, j jsonb NOT NULL);
         CREATE TYPE pair AS (a int, b text);
         CREATE TYPE mood AS ENUM ('sad', 'ok');
         CREATE DOMAIN posint AS int CHECK (VALUE > 0);
         CREATE FUNCTION f_polydef(a anyelement, b int DEFAULT 1) RETURNS anyelement
             AS $$ SELECT a $$ LANGUAGE sql;
         CREATE FUNCTION f_compat(a anycompatible, b anycompatible) RETURNS anycompatiblearray
             AS $$ SELECT ARRAY[a, b] $$ LANGUAGE sql;
         CREATE FUNCTION f_poly(a anyelement, b anyelement) RETURNS anyarray
             AS $$ SELECT ARRAY[a, b] $$ LANGUAGE sql;
         CREATE FUNCTION f_arr_elem(a anyarray, b anyelement) RETURNS int
             AS $$ SELECT 1 $$ LANGUAGE sql;
         CREATE FUNCTION f_enum(a anyenum) RETURNS anyenum AS $$ SELECT a $$ LANGUAGE sql;
         CREATE FUNCTION f_var_any(VARIADIC xs anyarray) RETURNS anyelement
             AS $$ SELECT xs[1] $$ LANGUAGE sql;
         CREATE FUNCTION f_dom(a posint) RETURNS posint AS $$ SELECT a $$ LANGUAGE sql;
         CREATE FUNCTION pk(a int) RETURNS text AS $$ SELECT 'a' $$ LANGUAGE sql;
         CREATE FUNCTION pk(a int, b text DEFAULT 'x') RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql;",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_err_starts_with(db: &PgCatalog, sql: &str, expected: &str) {
    let err = db.analyze(sql).unwrap_err();
    assert!(
        err.to_string().starts_with(expected),
        "expected `{expected}` for `{sql}`, got: {err}"
    );
}

/// Like [`assert_err_starts_with`], also pinning the SQLSTATE-carrying
/// variant.
macro_rules! assert_err_kind {
    ($db:expr, $sql:expr, $variant:pat, $expected:expr) => {{
        let sql: &str = $sql;
        let err = $db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, $variant),
            "wrong variant for `{sql}`: {err:?}"
        );
        assert_err_starts_with(&$db, sql, $expected);
    }};
}

// ── Unknown-typed operands against string types (#0) ─────────────────────────

#[test]
fn varchar_char_name_with_unknown_literal() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT vc = 'x' AS a, vc || 'x' AS b, 'x' || vc AS c, vc LIKE 'x' AS d,
                    vc ILIKE 'x' AS e, vc ~ 'x' AS f, vc < 'x' AS g, 'x'::name || 'y' AS h,
                    CURRENT_USER || 'x' AS i, ch || 'x' AS j
             FROM m",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", bool_ty()),
            c("b", text()),
            c("c", text()),
            c("d", bool_ty()),
            c("e", bool_ty()),
            c("f", bool_ty()),
            c("g", bool_ty()),
            c("h", text()),
            c("i", text()),
            c("j", text()),
        ],
    );
}

#[test]
fn varchar_compared_with_param_types_it_text() {
    let db = setup();
    let s = db.analyze("SELECT vc FROM m WHERE vc = $p").unwrap();
    assert_params(&s, vec![p(text())]);
    let s = db.analyze("SELECT vc FROM m WHERE vc ILIKE $p").unwrap();
    assert_params(&s, vec![p(text())]);
}

// ── Unknown-typed function arguments (#1, #49) ───────────────────────────────

#[test]
fn at_time_zone_with_literal_zone() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT ts AT TIME ZONE 'UTC' AS a, timezone('UTC', ts) AS b,
                    '2020-01-01'::timestamp AT TIME ZONE 'UTC' AS c,
                    (tz AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' AS d,
                    (ts, ts) OVERLAPS ('2020-01-01', '2020-01-02') AS e
             FROM m",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", timestamptz()),
            c("b", timestamptz()),
            c("c", timestamptz()),
            c("d", timestamptz()),
            cn("e", bool_ty()),
        ],
    );
}

#[test]
fn at_time_zone_and_overlaps_with_params() {
    let db = setup();
    let s = db.analyze("SELECT $p AT TIME ZONE 'UTC' AS a").unwrap();
    assert_cols(&s, vec![c("a", timestamp())]);
    assert_params(&s, vec![p(timestamptz())]);
    let s = db
        .analyze("SELECT (d, d) OVERLAPS ($p, $q) AS a FROM m")
        .unwrap();
    assert_cols(&s, vec![cn("a", bool_ty())]);
    assert_params(&s, vec![p(timestamptz()), p(timestamptz())]);
}

#[test]
fn generate_series_of_dates_with_unknown_step() {
    let db = setup();
    let s = db
        .analyze("SELECT generate_series('2020-01-01'::date, '2020-01-05'::date, '1 day') AS g")
        .unwrap();
    assert_cols(&s, vec![c("g", timestamptz())]);
}

#[test]
fn percentile_cont_with_param_fraction() {
    let db = setup();
    let s = db
        .analyze("SELECT percentile_cont($p) WITHIN GROUP (ORDER BY x) AS a FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("a", float8())]);
    assert_params(&s, vec![p(float8())]);
}

// ── Defaults and duplicate signatures (#44, extra) ───────────────────────────

#[test]
fn defaults_combined_with_polymorphic_params() {
    let db = setup();
    let pair = || composite("public", "pair", vec![rfn("a", int4()), rfn("b", text())]);
    let s = db
        .analyze("SELECT json_populate_record(NULL::pair, '{}') AS r")
        .unwrap();
    assert_cols(&s, vec![cn("r", pair())]);
    let s = db
        .analyze("SELECT * FROM json_populate_record(NULL::pair, '{}')")
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
    let s = db.analyze("SELECT f_polydef('x'::text) AS a").unwrap();
    assert_cols(&s, vec![cn("a", text())]);
}

#[test]
fn default_expansion_colliding_with_exact_overload_is_not_unique() {
    let db = setup();
    assert_err_kind!(
        db,
        "SELECT pk(1)",
        AnalyzeError::AmbiguousFunction(_),
        "function pk(integer) is not unique"
    );
}

#[test]
fn domain_parameter_accepts_its_base_type() {
    let db = setup();
    let s = db.analyze("SELECT f_dom(1) AS a").unwrap();
    // Its body, `a`: 1.
    assert_cols(&s, vec![c("a", domain("public", "posint", int4()))]);
}

// ── Polymorphic resolution (#45, #46, #47) ───────────────────────────────────

#[test]
fn anycompatible_family_unifies_to_a_common_type() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT array_append(ARRAY[1], 2.5) AS a, array_cat(ARRAY[1], ARRAY[2.5]) AS b,
                    f_compat(1, 2.5) AS c, f_compat(1::int8, 2.5::float4) AS d,
                    f_compat('a', 'b') AS e",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", array_of(numeric())),
            c("b", array_of(numeric())),
            // `ARRAY[a, b]`.
            c("c", array_of(numeric())),
            c("d", array_of(float4())),
            c("e", array_of(text())),
        ],
    );
    let s = db
        .analyze("SELECT lag(x, 1, 0.5) OVER (ORDER BY id) AS a FROM t")
        .unwrap();
    assert_cols(&s, vec![c("a", numeric())]);
    assert_err_starts_with(
        &db,
        "SELECT f_compat(1, 'x')",
        "invalid input syntax for type integer: \"x\"",
    );
}

#[test]
fn strict_polymorphic_family_requires_identical_types() {
    let db = setup();
    for (sql, msg) in [
        (
            "SELECT f_poly(1, 2.5)",
            "function f_poly(integer, numeric) does not exist",
        ),
        (
            "SELECT f_poly(1::int8, 1::int4)",
            "function f_poly(bigint, integer) does not exist",
        ),
        (
            "SELECT f_arr_elem(ARRAY[1], 1.5)",
            "function f_arr_elem(integer[], numeric) does not exist",
        ),
        (
            "SELECT f_var_any(1, 'x'::text)",
            "function f_var_any(integer, text) does not exist",
        ),
        (
            "SELECT f_enum('sad')",
            "function f_enum(unknown) does not exist",
        ),
    ] {
        assert_err_starts_with(&db, sql, msg);
    }
}

#[test]
fn polymorphic_call_with_only_unknown_inputs_is_rejected() {
    let db = setup();
    for sql in [
        "SELECT f_poly('a', 'b')",
        "SELECT to_jsonb('x')",
        "SELECT lag($p) OVER () FROM t",
        "SELECT jsonb_agg($p)",
    ] {
        assert_err_kind!(
            db,
            sql,
            AnalyzeError::DatatypeMismatch(_),
            "could not determine polymorphic type because input has type unknown"
        );
    }
}

#[test]
fn params_in_polymorphic_positions_take_the_resolved_type() {
    let db = setup();
    for (sql, param) in [
        ("SELECT array_append($p, 1)", array_of(int4())),
        ("SELECT array_position($p, 1)", array_of(int4())),
        ("SELECT f_poly($p, 1)", int4()),
        ("SELECT lag(x, 1, $p) OVER () FROM t", int4()),
        ("SELECT lead(x, 1, $p) OVER (ORDER BY id) FROM t", int4()),
        ("SELECT f_compat($p, 1.5)", numeric()),
    ] {
        let s = db.analyze(sql).unwrap();
        assert_params(&s, vec![p(param)]);
    }
}

// ── Variadic tails (#48) ─────────────────────────────────────────────────────

#[test]
fn unknown_arguments_in_a_variadic_tail_take_the_element_type() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT jsonb_extract_path(j, 'a', 'b') AS a, json_extract_path_text(j::json, 'a') AS b
             FROM tj",
        )
        .unwrap();
    assert_cols(&s, vec![cn("a", jsonb()), cn("b", text())]);
    let s = db
        .analyze("SELECT jsonb_extract_path_text(j, $a, $b) AS a FROM tj")
        .unwrap();
    assert_params(&s, vec![p(text()), p(text())]);
}

// ── Error wording (#63) ──────────────────────────────────────────────────────

#[test]
fn missing_schema_qualifier_is_reported() {
    let db = setup();
    assert_err_kind!(
        db,
        "SELECT nosuchschema.f(1)",
        AnalyzeError::UndefinedSchema(_),
        "schema \"nosuchschema\" does not exist"
    );
}

#[test]
fn from_function_argument_referencing_a_later_from_item() {
    let mut db = setup();
    db.apply_sql("CREATE TABLE ta (id int PRIMARY KEY, arr int[] NOT NULL)")
        .unwrap();
    assert_err_kind!(
        db,
        "SELECT * FROM unnest(ta.arr), ta",
        AnalyzeError::UndefinedTable(_),
        "missing FROM-clause entry for table \"ta\""
    );
}

#[test]
fn variadic_any_argument_must_be_an_array() {
    let db = setup();
    for sql in [
        "SELECT concat(VARIADIC $p)",
        "SELECT concat(VARIADIC NULL::int)",
    ] {
        assert_err_kind!(
            db,
            sql,
            AnalyzeError::DatatypeMismatch(_),
            "VARIADIC argument must be an array"
        );
    }
}

// ── Operators: qualified names and prefix wording (#7, #27) ──────────────────

#[test]
fn schema_qualified_operator_syntax() {
    let mut db = setup();
    db.apply_sql("CREATE TABLE t7 (n int NOT NULL, s text NOT NULL)")
        .unwrap();
    let s = db
        .analyze("SELECT 1 OPERATOR(pg_catalog.+) 2 AS a")
        .unwrap();
    assert_cols(&s, vec![c("a", int4())]);
    let s = db
        .analyze("SELECT n FROM t7 WHERE n OPERATOR(pg_catalog.=) 1")
        .unwrap();
    assert_cols(&s, vec![c("n", int4())]);
    let s = db
        .analyze("SELECT s OPERATOR(pg_catalog.||) 'x' AS a FROM t7")
        .unwrap();
    assert_cols(&s, vec![c("a", text())]);
    assert_err_kind!(
        db,
        "SELECT 1 OPERATOR(nope.+) 2",
        AnalyzeError::UndefinedSchema(_),
        "schema \"nope\" does not exist"
    );
    assert_err_kind!(
        db,
        "SELECT 1 OPERATOR(pg_catalog.+) 'x'::text",
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: integer pg_catalog.+ text"
    );
    assert_err_kind!(
        db,
        "SELECT OPERATOR(pg_catalog.||) 'x'",
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: pg_catalog.|| unknown"
    );
}

#[test]
fn prefix_operator_errors_have_no_left_operand() {
    let db = setup();
    assert_err_kind!(
        db,
        "SELECT -x::text FROM t",
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: - text"
    );
    assert_err_kind!(
        db,
        "SELECT !! 5",
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: !! integer"
    );
    assert_err_kind!(
        db,
        "SELECT @ 'x'::text",
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: @ text"
    );
    for sql in ["SELECT - '1'", "SELECT -$p"] {
        assert_err_kind!(
            db,
            sql,
            AnalyzeError::AmbiguousFunction(_),
            "operator is not unique: - unknown"
        );
    }
}
