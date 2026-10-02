//! WHERE clause: AND/OR, IN, LIKE, IS NULL, NOT, comparison operators.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id         BIGINT PRIMARY KEY,
            name       TEXT NOT NULL,
            email      TEXT NOT NULL,
            age        INT,
            created_at TIMESTAMPTZ NOT NULL
        );",
    )
    .unwrap();
    db
}

// ── Non-boolean expression in WHERE is a type mismatch ───────────────────────

#[test]
fn where_int4_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE 42"),
        AnalyzeError::DatatypeMismatch(_),
        "argument of WHERE must be type boolean, not type integer",
    );
}

#[test]
fn where_text_column_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type text\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name\n",
            "  ·                            ──┬─\n",
            "  ·                              ╰─ this is text, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_int8_column_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT name FROM users WHERE id"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type bigint\n",
            "  ╭────\n",
            "1 │ SELECT name FROM users WHERE id\n",
            "  ·                              ─┬\n",
            "  ·                               ╰─ this is bigint, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_timestamptz_column_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE created_at"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type timestamp with time zone\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE created_at\n",
            "  ·                            ─────┬────\n",
            "  ·                                 ╰─ this is timestamp with time zone, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

// ── IS NULL / IS NOT NULL ────────────────────────────────────────────────────

#[test]
fn where_is_not_null() {
    let db = setup();
    // The rows past the WHERE have a non-NULL age.
    let s = db
        .analyze("SELECT id, age FROM users WHERE age IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("age", int4())]);
}

#[test]
fn where_is_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age IS NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn where_is_null_unknown_column_rejected() {
    // `IS NULL` / `IS NOT NULL` / `IS [NOT] TRUE|FALSE` must still resolve
    // their argument against the scope — historically the analyzer treated
    // these nodes as opaque booleans and silently accepted typo'd columns.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE ghost IS NULL"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE ghost IS NULL\n",
            "  ·                            ──┬──\n",
            "  ·                              ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_is_not_false_unknown_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE ghost IS NOT FALSE"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE ghost IS NOT FALSE\n",
            "  ·                            ──┬──\n",
            "  ·                              ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_is_not_false_unknown_column_in_subquery_rejected() {
    // The original report: a typo'd column inside a scalar subquery's
    // `IS NOT FALSE` was silently accepted because the BooleanTest arm
    // never descended into its arg.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "SELECT id FROM users u \
             WHERE EXISTS (SELECT 1 FROM users x WHERE x.ghost IS NOT FALSE)",
        ),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column x.ghost does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM users x WHERE x.ghost IS NOT FALSE)\n",
            "  ·                                                                  ───┬───\n",
            "  ·                                                                     ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── AND / OR / NOT ───────────────────────────────────────────────────────────

#[test]
fn where_and() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 AND email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn where_or() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 OR email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn where_not() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE NOT (age > $p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

// ── IN / LIKE / comparison ───────────────────────────────────────────────────

#[test]
fn where_in_list() {
    let db = setup();
    // Literal form: PG promotes list elements to the column's type (int4), but
    // since they are constants nothing surfaces in the param list.
    let s = db
        .analyze("SELECT id FROM users WHERE age IN (1, 2, 3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn where_in_list_with_params() {
    let db = setup();
    // Param form: each `$pN` inside the IN list must be inferred with the
    // left-hand column's type as the goal, so all three params surface as int4.
    let s = db
        .analyze("SELECT id FROM users WHERE age IN ($p1, $p2, $p3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(int4())]);
}

#[test]
fn where_like() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name LIKE $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn where_comparison_operators() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age >= $p1 AND age <= $p2 AND name <> $p3")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(text())]);
}

// ── BETWEEN ──────────────────────────────────────────────────────────────────

#[test]
fn where_between_literals() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN 18 AND 65")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn where_between_params() {
    let db = setup();
    // Both bounds are walked with the lhs column type as the inference goal,
    // so `$p1`/`$p2` resolve to `int4` (the type of `age`).
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN $p1 AND $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn where_not_between_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age NOT BETWEEN $p1 AND $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn where_between_symmetric_with_params() {
    let db = setup();
    // BETWEEN SYMMETRIC should route through the same walker as plain BETWEEN.
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN SYMMETRIC $p1 AND $p2")
        .unwrap();
    assert_params(&s, vec![p(int4()), p(int4())]);
}

// ── IS [NOT] DISTINCT FROM ──────────────────────────────────────────────────

#[test]
fn where_is_distinct_from_literal() {
    let db = setup();
    // PG defines `IS DISTINCT FROM` to be NULL-aware and to always produce
    // a definite boolean, so `d` is NOT NULL even though `age` is nullable.
    let s = db
        .analyze("SELECT id, age IS DISTINCT FROM 10 AS d FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("d", bool_ty())]);
}

#[test]
fn where_is_not_distinct_from_null() {
    let db = setup();
    // `col IS NOT DISTINCT FROM NULL` is effectively `col IS NULL` — still
    // always-definite, so the result column is NOT NULL.
    let s = db
        .analyze("SELECT id, age IS NOT DISTINCT FROM NULL AS d FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("d", bool_ty())]);
}

#[test]
fn where_is_distinct_from_with_param() {
    let db = setup();
    // The rhs param is walked and inferred from the lhs column type.
    // Matches the convention used by other WHERE-clause param tests: params
    // default to NOT NULL unless the caller annotates them with `$p1?`.
    let s = db
        .analyze("SELECT id FROM users WHERE age IS DISTINCT FROM $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

// ── POSIX regex + SIMILAR TO ─────────────────────────────────────────────────

#[test]
fn where_regex_match() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name ~ '^foo'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn where_regex_imatch_with_param() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name ~* $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn where_regex_not_match() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name !~ 'bar'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn where_similar_to() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name SIMILAR TO 'ab%'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Cross-type operators rejected ───────────────────────────────────────────

#[test]
fn text_eq_int_rejected() {
    let db = setup();
    // PG: `operator does not exist: text = bigint`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = id"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = bigint\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name = id\n",
            "  ·                                 ┬\n",
            "  ·                                 ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

#[test]
fn any_operator_mismatch_points_at_the_operator() {
    // PG positions it at the operator, like a plain comparison.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = ANY(ARRAY[1, 2])"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name = ANY(ARRAY[1, 2])\n",
            "  ·                                 ┬\n",
            "  ·                                 ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

#[test]
fn join_using_operator_mismatch_gets_the_hint() {
    // PG gives this one no position; the hint and the cast still help.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT 1 FROM (SELECT id FROM users) a JOIN (SELECT name AS id FROM users) b USING (id)"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: bigint = text\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `bigint = bigint` exists: cast the right operand to bigint (`expr::bigint`)\n",
        ),
    );
}

#[test]
fn text_eq_int_literal_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = 42"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name = 42\n",
            "  ·                                 ┬\n",
            "  ·                                 ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

#[test]
fn timestamptz_lt_int_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE created_at < 42"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: timestamp with time zone < integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE created_at < 42\n",
            "  ·                                       ┬\n",
            "  ·                                       ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
        ),
    );
}

#[test]
fn int_like_text_rejected() {
    let db = setup();
    // PG: `operator does not exist: bigint ~~ text`. Catches typos where a
    // developer writes `id LIKE 'foo'` instead of `id::text LIKE 'foo'`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE id LIKE '%1%'"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: bigint ~~ unknown\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE id LIKE '%1%'\n",
            "  ·                               ─┬\n",
            "  ·                                ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
        ),
    );
}

#[test]
fn same_param_with_conflicting_types_rejected() {
    let db = setup();
    // First use (`age = $p1`) pins `$p1` to int4. Second use (`name = $p1`)
    // would then be `text = int4`, which has no operator in PG — the
    // cross-type check above catches this uniformly.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE age = $p1 AND name = $p1"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE age = $p1 AND name = $p1\n",
            "  ·                                               ┬\n",
            "  ·                                               ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_complex_where_params() {
    let db = setup();
    let sql = "SELECT id FROM users \
               WHERE (name = $p1 OR email = $p2) AND age > $p3";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("id", int8())]);
    assert_params(&info, vec![p(text()), p(text()), p(int4())]);
}

// ── Error ordering: resolution before placement before boolean coercion ─────

#[test]
fn where_unresolvable_aggregate_reports_resolution_error() {
    // PG transforms bottom-up: `min(text, text)` has no overload, so the
    // `function … does not exist` error fires before the aggregate-placement
    // rule gets a chance.
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users WHERE min(name, name)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("function min(text, text) does not exist"),
        "got: {err}"
    );
}

#[test]
fn where_valid_aggregate_reports_placement_error() {
    // A resolvable aggregate in WHERE keeps the placement error — and it
    // outranks the boolean-coercion complaint (`min(id)` is bigint).
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users WHERE min(id)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in WHERE"),
        "got: {err}"
    );
}

#[test]
fn join_on_non_boolean_uses_pg_wording() {
    let db = setup();
    let err = db
        .analyze("SELECT u.id FROM users u JOIN users v ON u.age + v.age")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("argument of JOIN/ON must be type boolean, not type integer"),
        "got: {err}"
    );
}

#[test]
fn aggregate_in_limit_and_offset_rejected() {
    // PG forbids aggregates/window calls in LIMIT/OFFSET — after the
    // expression resolves, before the bigint-coercion complaint.
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users LIMIT count(*)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in LIMIT"),
        "got: {err}"
    );
    let err = db
        .analyze("SELECT id FROM users OFFSET sum(age)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in OFFSET"),
        "got: {err}"
    );
    let err = db
        .analyze("SELECT id FROM users LIMIT row_number() OVER ()")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("window functions are not allowed in LIMIT"),
        "got: {err}"
    );
}

/// PostgreSQL compiles a constant regex pattern while *planning*: a match of
/// two constants is folded (eval_const_expressions) and a WHERE / JOIN ON
/// qual `expr ~ 'pattern'` over one relation has its selectivity estimated
/// from the pattern (patternsel). An invalid pattern there fails every
/// execution, while elsewhere it only fails per evaluated row, and DDL
/// that merely stores the expression is accepted.
#[test]
fn constant_regex_patterns_compiled_by_the_planner_are_validated() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE rt (id INT PRIMARY KEY, s TEXT NOT NULL, n NAME);
         CREATE TABLE rt2 (id INT PRIMARY KEY, s TEXT);
         CREATE VIEW rv AS SELECT id FROM rt WHERE s ~ '(';",
    )
    .unwrap();
    for sql in [
        "SELECT 'a' ~ '(' AS m",
        "SELECT 'a' ~* ('(' || '') AS m",
        "SELECT 'a' SIMILAR TO '(' AS m",
        "SELECT id FROM rt WHERE s ~ '('",
        "SELECT id FROM rt WHERE NOT (s !~ '[z-a]') OR id = 1",
        "SELECT id FROM rt WHERE lower(s) ~* '('",
        "SELECT id FROM rt WHERE n ~ '('",
        "SELECT id FROM rt WHERE s SIMILAR TO '('",
        "SELECT rt.id FROM rt JOIN rt2 ON rt2.s ~ '(' AND rt2.id = rt.id",
        "DELETE FROM rt WHERE s ~ '('",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression:"
        );
    }
    for sql in [
        "SELECT s ~ '(' AS m FROM rt",
        "SELECT id FROM rt WHERE '(' ~ s",
        "SELECT rt.id FROM rt JOIN rt2 ON rt.s || rt2.s ~ '('",
        "SELECT id FROM rt WHERE s ~ '(?q)('",
        "SELECT id FROM rt WHERE s SIMILAR TO '\\('",
        "SELECT id FROM rv",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
