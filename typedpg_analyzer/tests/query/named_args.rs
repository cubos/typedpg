//! Named and mixed notation in function calls (`f(a => 1)`, `f(1, b := 2)`):
//! matching argument names to parameters, defaults for the omitted ones,
//! overload selection by name, and PG's parse-time rules on the notation.
//! Every expectation below was observed on PostgreSQL 18.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL);
         CREATE FUNCTION f(a INT, b TEXT DEFAULT 'x') RETURNS TEXT
             LANGUAGE sql AS $$ SELECT b $$;
         CREATE FUNCTION pick(a INT, b TEXT DEFAULT 'x') RETURNS INT
             LANGUAGE sql AS $$ SELECT a $$;
         CREATE FUNCTION pick(c INT) RETURNS TEXT
             LANGUAGE sql AS $$ SELECT 'c' $$;
         CREATE FUNCTION ov(a INT) RETURNS INT LANGUAGE sql AS $$ SELECT 1 $$;
         CREATE FUNCTION ov(a TEXT) RETURNS TEXT LANGUAGE sql AS $$ SELECT 'x' $$;
         CREATE FUNCTION pa(a anyelement, b anyelement DEFAULT NULL) RETURNS anyelement
             LANGUAGE sql AS $$ SELECT a $$;
         CREATE FUNCTION h(a INT, OUT o INT, b INT DEFAULT 2)
             LANGUAGE sql AS $$ SELECT a + b $$;
         CREATE FUNCTION v(a INT, VARIADIC r INT[]) RETURNS INT
             LANGUAGE sql AS $$ SELECT a $$;
         CREATE FUNCTION gi(x FLOAT8) RETURNS FLOAT8
             LANGUAGE sql IMMUTABLE AS $$ SELECT x $$;
         CREATE AGGREGATE mysum(x INT) (sfunc = int4pl, stype = INT);",
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

// ── Resolution ───────────────────────────────────────────────────────────────

#[test]
fn builtin_with_named_args() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT make_interval(mins => 5) AS a,
                    make_interval(1, mins => 5) AS b,
                    make_interval(secs := 1.5) AS c",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("a", interval()), c("b", interval()), c("c", interval())],
    );
}

#[test]
fn named_args_in_any_order_with_defaults() {
    let db = setup();
    let s = db
        .analyze("SELECT f(b => 'y', a => 1) AS x, f(a => 1) AS y, f(1, b => 'q') AS z")
        .unwrap();
    assert_cols(&s, vec![cn("x", text()), cn("y", text()), cn("z", text())]);
}

#[test]
fn names_select_the_overload() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT pick(c => 1) AS by_c, pick(a => 1) AS by_a, pick(b => 'z', a => 1) AS by_ab",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("by_c", text()), cn("by_a", int4()), cn("by_ab", int4())],
    );
}

#[test]
fn types_select_among_same_named_overloads() {
    let db = setup();
    let s = db
        .analyze("SELECT ov(a => 1) AS i, ov(a => 'x') AS t")
        .unwrap();
    assert_cols(&s, vec![cn("i", int4()), cn("t", text())]);
}

#[test]
fn polymorphic_named_args() {
    let db = setup();
    let s = db
        .analyze("SELECT pa(b => 1, a => 2) AS x, pa(a => 2) AS y")
        .unwrap();
    assert_cols(&s, vec![cn("x", int4()), cn("y", int4())]);
}

#[test]
fn out_params_are_not_nameable() {
    let db = setup();
    let s = db.analyze("SELECT h(b => 3, a => 1) AS r").unwrap();
    assert_cols(&s, vec![cn("r", int4())]);
    assert_err_starts_with(
        &db,
        "SELECT h(o => 3, a => 1)",
        "function h(o => integer, a => integer) does not exist",
    );
}

#[test]
fn params_take_the_named_parameter_type() {
    let db = setup();
    let s = db.analyze("SELECT f(b => $x, a => $y) AS r").unwrap();
    assert_eq!(
        s.params
            .iter()
            .map(|p| p.pg_type.clone())
            .collect::<Vec<_>>(),
        vec![text(), int4()]
    );
}

#[test]
fn variadic_needs_the_variadic_keyword() {
    let db = setup();
    let s = db
        .analyze("SELECT v(1, VARIADIC r => ARRAY[2]) AS r")
        .unwrap();
    assert_cols(&s, vec![cn("r", int4())]);
    assert_err_starts_with(
        &db,
        "SELECT v(a => 1, r => 2)",
        "function v(a => integer, r => integer) does not exist",
    );
}

#[test]
fn aggregate_named_args_only_as_window_function() {
    let db = setup();
    let s = db.analyze("SELECT mysum(x => 1) OVER () AS s").unwrap();
    assert_cols(&s, vec![cn("s", int4())]);
    assert_err_starts_with(
        &db,
        "SELECT mysum(x => 1)",
        "aggregates cannot use named arguments",
    );
}

// ── Rejections ───────────────────────────────────────────────────────────────

#[test]
fn unknown_or_missing_names_do_not_exist() {
    let db = setup();
    assert_err_starts_with(
        &db,
        "SELECT f(c => 1)",
        "function f(c => integer) does not exist",
    );
    // `a` has no default.
    assert_err_starts_with(
        &db,
        "SELECT f(b => 'y')",
        "function f(b => unknown) does not exist",
    );
    // A name may not repeat a positional argument.
    assert_err_starts_with(
        &db,
        "SELECT f(1, a => 2)",
        "function f(integer, a => integer) does not exist",
    );
    assert_err_starts_with(
        &db,
        "SELECT upper(x => 'a')",
        "function upper(x => unknown) does not exist",
    );
}

#[test]
fn argument_names_render_unquoted_in_messages() {
    // Unlike type and relation names, PG prints argument names verbatim —
    // no quoting even when the name needs it.
    let db = setup();
    assert_err_starts_with(
        &db,
        r#"SELECT f("X y" => 1)"#,
        "function f(X y => integer) does not exist",
    );
    assert_err_starts_with(
        &db,
        r#"SELECT f("select" => 1)"#,
        "function f(select => integer) does not exist",
    );
    assert_err_starts_with(
        &db,
        r#"SELECT f("a""b" => 1)"#,
        r#"function f(a"b => integer) does not exist"#,
    );
}

#[test]
fn named_notation_is_never_a_cast() {
    let db = setup();
    assert_err_starts_with(
        &db,
        "SELECT float8(x => 1)",
        "function float8(x => integer) does not exist",
    );
}

#[test]
fn duplicate_name_is_rejected() {
    let db = setup();
    let err = db
        .analyze("SELECT make_interval(mins => 5, mins => 6)")
        .unwrap_err();
    assert!(matches!(err, AnalyzeError::SyntaxError(_)), "got: {err:?}");
    assert!(
        err.to_string()
            .starts_with("argument name \"mins\" used more than once"),
        "got: {err}"
    );
}

#[test]
fn positional_after_named_is_rejected() {
    let db = setup();
    let err = db.analyze("SELECT f(a => 1, 'x')").unwrap_err();
    assert!(matches!(err, AnalyzeError::SyntaxError(_)), "got: {err:?}");
    assert!(
        err.to_string()
            .starts_with("positional argument cannot follow named argument"),
        "got: {err}"
    );
}

#[test]
fn from_clause_function_with_named_args() {
    let db = setup();
    let s = db
        .analyze("SELECT * FROM make_interval(days => 2) AS i(v)")
        .unwrap();
    assert_cols(&s, vec![c("v", interval())]);
    // The builtin `generate_series` has no parameter names.
    assert_err_starts_with(
        &db,
        "SELECT * FROM generate_series(start => 1, stop => 3)",
        "function generate_series(start => integer, stop => integer) does not exist",
    );
}

// ── Named arguments seen by the clause walkers ───────────────────────────────

#[test]
fn ungrouped_column_inside_named_arg() {
    let db = setup();
    assert_err_starts_with(
        &db,
        "SELECT f(a => id) FROM t GROUP BY name",
        "column \"t.id\" must appear in the GROUP BY clause or be used in an aggregate function",
    );
}

#[test]
fn aggregate_inside_named_arg_in_where() {
    let db = setup();
    assert_err_starts_with(
        &db,
        "SELECT 1 FROM t WHERE f(a => count(*)::int) = 'x'",
        "aggregate functions are not allowed in WHERE",
    );
}

#[test]
fn undefined_window_inside_named_arg() {
    let db = setup();
    assert_err_starts_with(
        &db,
        "SELECT f(a => (row_number() OVER w)::int) FROM t",
        "window \"w\" does not exist",
    );
}

#[test]
fn volatile_call_inside_named_arg_in_generated_column() {
    let mut db = setup();
    let err = db
        .apply_sql(
            "CREATE TABLE g (x FLOAT8, y FLOAT8 GENERATED ALWAYS AS (gi(x => random())) STORED)",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("generation expression is not immutable"),
        "got: {err}"
    );
}

#[test]
fn view_depends_on_column_inside_named_arg() {
    let mut db = setup();
    db.apply_sql("CREATE VIEW vw AS SELECT f(a => id) FROM t")
        .unwrap();
    let err = db.apply_sql("ALTER TABLE t DROP COLUMN id").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("cannot drop column id of table t because other objects depend on it"),
        "got: {err}"
    );
}
