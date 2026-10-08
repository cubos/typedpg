//! Scalar expressions: literals, arithmetic, boolean, concat, CASE,
//! COALESCE, NULLIF, strict vs non-strict operators, IS [NOT] NULL,
//! BETWEEN.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            email TEXT NOT NULL,
            age   INT
         );
         CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            body    TEXT
         );",
    )
    .unwrap();
    db
}

// ── Literals ─────────────────────────────────────────────────────────────────

#[test]
fn types_match_integer_literal() {
    let db = setup();
    let s = db.analyze("SELECT 42 AS val").unwrap();
    assert_cols(&s, vec![c("val", int4())]);
}

#[test]
fn types_match_boolean_literal() {
    let db = setup();
    let s = db.analyze("SELECT true AS flag, false AS other").unwrap();
    assert_cols(&s, vec![c("flag", bool_ty()), c("other", bool_ty())]);
}

#[test]
fn literal_not_null() {
    let db = setup();
    let sql = "SELECT id, 'constant' as label FROM users";
    let info = db.analyze(sql).unwrap();
    // PG coerces any `unknown`-typed output column to `text` before sending
    // it to the client, so the bare string literal surfaces as `text`.
    assert_cols(&info, vec![c("id", int8()), c("label", text())]);
}

// ── Arithmetic / operators ───────────────────────────────────────────────────

#[test]
fn types_match_arithmetic() {
    let db = setup();
    let s = db
        .analyze("SELECT id + 1 AS next_id, age * 2 AS double_age FROM users")
        .unwrap();
    assert_cols(&s, vec![c("next_id", int8()), cn("double_age", int4())]);
}

#[test]
fn types_match_string_concat() {
    let db = setup();
    let s = db
        .analyze("SELECT name || ' <' || email || '>' AS display FROM users")
        .unwrap();
    assert_cols(&s, vec![c("display", text())]);
}

#[test]
fn complex_arithmetic_on_nullable() {
    let db = setup();
    // age is nullable → age + 1 nullable.
    let sql = "SELECT id, age + 1 as age_plus_one FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("id", int8()), cn("age_plus_one", int4())]);
}

#[test]
fn complex_arithmetic_on_not_null() {
    let db = setup();
    // id is NOT NULL → id + 1 also NOT NULL.
    let sql = "SELECT id + 1 as next_id FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("next_id", int8())]);
}

#[test]
fn numeric_plus_int_returns_numeric() {
    let db = setup();
    // `numeric + int4` must resolve to `numeric + numeric → numeric`
    // (PG §10.2 step 3c — most exact matches wins). The alternative
    // `float4 + float4` is reachable via implicit casts but scores lower
    // because neither side matches exactly, so it would silently narrow
    // money-style computations.
    let s = db
        .analyze("SELECT SUM(id)::numeric + 1 AS r FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("r", numeric())]);
}

#[test]
fn complex_coalesce_in_arithmetic() {
    let db = setup();
    // COALESCE(age, 0) is NOT NULL → adding 10 stays NOT NULL.
    let sql = "SELECT COALESCE(age, 0) + 10 as safe_age_plus FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("safe_age_plus", int4())]);
}

// ── Built-in strict / non-strict functions ───────────────────────────────────

#[test]
fn types_match_upper_lower() {
    let db = setup();
    let s = db
        .analyze("SELECT upper(name) AS up, lower(email) AS lo FROM users")
        .unwrap();
    assert_cols(&s, vec![c("up", text()), c("lo", text())]);
}

#[test]
fn types_match_length() {
    let db = setup();
    let s = db.analyze("SELECT length(name) AS len FROM users").unwrap();
    assert_cols(&s, vec![c("len", int4())]);
}

#[test]
fn types_match_coalesce_with_literal() {
    let db = setup();
    let s = db
        .analyze("SELECT COALESCE(age, 0) AS age_or_zero FROM users")
        .unwrap();
    assert_cols(&s, vec![c("age_or_zero", int4())]);
}

#[test]
fn types_match_now() {
    let db = setup();
    let s = db.analyze("SELECT now() AS ts").unwrap();
    assert_cols(&s, vec![c("ts", timestamptz())]);
}

#[test]
fn coalesce_not_null() {
    let db = setup();
    let sql = "SELECT COALESCE(age, 0) as safe_age FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("safe_age", int4())]);
}

// ── NULLIF ───────────────────────────────────────────────────────────────────

#[test]
fn nullif_returns_first_arg_type() {
    let db = setup();
    // Result type is the first arg's type (NOT bool — NULLIF wraps the `=`
    // operator but projects the first operand back on the non-match branch).
    // Always nullable (NULL when args are equal).
    let s = db
        .analyze("SELECT NULLIF(age, 0) AS maybe_age FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("maybe_age", int4())]);
}

#[test]
fn nullif_on_not_null_column_is_nullable() {
    let db = setup();
    // Even on a NOT NULL column, NULLIF can produce NULL (when args match).
    let s = db
        .analyze("SELECT NULLIF(name, 'admin') AS maybe_name FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("maybe_name", text())]);
}

#[test]
fn nullif_with_param_inherits_type() {
    let db = setup();
    // `$p1` gets typed from the first arg via the implicit goal.
    let s = db
        .analyze("SELECT NULLIF(age, $p1) AS maybe_age FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("maybe_age", int4())]);
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn nullif_incompatible_concrete_types_rejected() {
    // PG dispatches to the `=` operator resolver and errors with
    // `operator does not exist: integer = text`. The analyzer mirrors the
    // wording verbatim so the pg_sanity prefix check passes, then appends
    // the NULLIF-specific suffix the macro caller will see.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT NULLIF(age, 'x'::text) FROM users"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: integer = text (NULLIF types integer and text cannot be matched)\n",
            "  ╭────\n",
            "1 │ SELECT NULLIF(age, 'x'::text) FROM users\n",
            "  ·               ───\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn nullif_int_with_string_literal_rejected() {
    // PG resolves `=` for (integer, unknown) to integer = integer and runs
    // int4's input function on the literal at parse_analyze time. The
    // analyzer mirrors it via `literal_input`, message verbatim.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT NULLIF(age, 'x') FROM users"),
        AnalyzeError::InvalidLiteral(_),
        concat!(
            "invalid input syntax for type integer: \"x\"\n",
            "  ╭────\n",
            "1 │ SELECT NULLIF(age, 'x') FROM users\n",
            "  ·                    ─┬─\n",
            "  ·                     ╰─ this literal\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn nullif_int_with_numeric_string_literal_coerced() {
    // The flip side: `'42'` is valid int4 input, so PG accepts and the
    // result keeps the first argument's type.
    let db = setup();
    let s = db
        .analyze("SELECT NULLIF(age, '42') AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

// ── CASE ─────────────────────────────────────────────────────────────────────

#[test]
fn case_result_conversion_errors_name_the_branch_as_pg_does() {
    let db = setup();
    // Same type category, no implicit cast: the ELSE type wins and the
    // THEN result fails to convert to it (transformCaseExpr's `CASE/WHEN`).
    assert_err_prefix!(
        db.analyze(
            "SELECT CASE WHEN age IS NULL THEN '\\x00'::bytea ELSE gen_random_uuid() END FROM users"
        ),
        AnalyzeError::Invalid(_),
        "CASE/WHEN could not convert type bytea to uuid"
    );
}

#[test]
fn types_match_case_with_else() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END AS category FROM users")
        .unwrap();
    assert_cols(&s, vec![c("category", text())]);
}

#[test]
fn types_match_case_expression() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS safe_age FROM users")
        .unwrap();
    // The ELSE branch is only reached when `age IS NULL` is not TRUE, so
    // `age` is NOT NULL there.
    assert_cols(&s, vec![c("safe_age", int4())]);
}

#[test]
fn case_with_else_not_null() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' ELSE 'minor' END as category FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("category", text())]);
}

#[test]
fn case_without_else_is_nullable() {
    let db = setup();
    let sql = "SELECT CASE WHEN age > 18 THEN 'adult' END as category FROM users";
    let info = db.analyze(sql).unwrap();
    // CASE without ELSE is nullable because there's no ELSE branch.
    assert_cols(&info, vec![cn("category", text())]);
}

#[test]
fn case_when_condition_must_be_boolean() {
    let db = setup();
    // A non-boolean WHEN condition is rejected with PG's exact wording. The
    // analyzer previously discarded this check and accepted the query.
    assert_analyze_err!(
        db.analyze("SELECT CASE WHEN age THEN 1 ELSE 0 END FROM users"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of CASE/WHEN must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ SELECT CASE WHEN age THEN 1 ELSE 0 END FROM users\n",
            "  ·                  ─┬─\n",
            "  ·                   ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn simple_case_when_values_compare_against_test_expr() {
    let db = setup();
    // Simple CASE (`CASE arg WHEN val …`): PG rewrites each WHEN into
    // `arg = val`, so the values are comparands against the test
    // expression, NOT boolean conditions. A regression once coerced
    // 'adult' to boolean and rejected with `invalid input syntax for
    // type boolean: "adult"`.
    let s = db
        .analyze("SELECT CASE name WHEN 'adult' THEN 1 WHEN 'minor' THEN 2 END AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

#[test]
fn simple_case_in_check_constraint() {
    let mut db = PgCatalog::new().unwrap();
    // Real-world regression shape: simple CASE over a discriminator column
    // with boolean THEN results, inside a table-level CHECK.
    db.apply_sql(
        "CREATE TABLE conversation_events (
            id      BIGINT PRIMARY KEY,
            type    TEXT NOT NULL,
            content TEXT,
            CONSTRAINT conversation_events_shape CHECK (
                CASE type
                    WHEN 'user_message'  THEN content IS NOT NULL
                    WHEN 'agent_message' THEN content IS NOT NULL
                END
            )
        );",
    )
    .unwrap();
}

#[test]
fn simple_case_when_value_needs_equality_overload_not_coercion() {
    let db = setup();
    // PG resolves `int4 = numeric` per WHEN — no coercion of the value to
    // the test type is required.
    let s = db
        .analyze("SELECT CASE age WHEN 1.5 THEN 'x' END AS v FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("v", text())]);
}

#[test]
fn simple_case_when_value_without_equality_operator_rejected() {
    let db = setup();
    assert_err_prefix!(
        db.analyze("SELECT CASE age WHEN true THEN 'x' END FROM users"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: integer = boolean",
    );
}

#[test]
fn simple_case_unknown_when_value_validated_against_test_type() {
    let db = setup();
    // An UNKNOWN WHEN value is assumed to be the test expression's type;
    // its literal content is validated under that type, like PG.
    assert_analyze_err!(
        db.analyze("SELECT CASE age WHEN 'abc' THEN 'x' END FROM users"),
        AnalyzeError::InvalidLiteral(_),
        concat!(
            "invalid input syntax for type integer: \"abc\"\n",
            "  ╭────\n",
            "1 │ SELECT CASE age WHEN 'abc' THEN 'x' END FROM users\n",
            "  ·                      ──┬──\n",
            "  ·                        ╰─ this literal\n",
            "  ╰────\n",
        ),
    );
}

// ── Boolean / NULL tests ─────────────────────────────────────────────────────

#[test]
fn types_match_null_test() {
    let db = setup();
    let s = db
        .analyze("SELECT id, age IS NULL AS is_null, age IS NOT NULL AS is_not_null FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("is_null", bool_ty()),
            c("is_not_null", bool_ty()),
        ],
    );
}

#[test]
fn types_match_boolean_test() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT (age > 18) IS TRUE AS adult, (age > 18) IS NOT TRUE AS not_adult FROM users",
        )
        .unwrap();
    assert_cols(&s, vec![c("adult", bool_ty()), c("not_adult", bool_ty())]);
}

#[test]
fn complex_boolean_with_nullable_input() {
    let db = setup();
    // age IS NOT NULL → bool, NOT NULL. age > 18 → bool, nullable (age can be NULL).
    let sql = "SELECT age IS NOT NULL as has_age, age > 18 as is_adult FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("has_age", bool_ty()), cn("is_adult", bool_ty())],
    );
}

#[test]
fn not_operand_must_be_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE NOT age"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of NOT must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE NOT age\n",
            "  ·                                ─┬─\n",
            "  ·                                 ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn and_operand_must_be_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE age AND true"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of AND must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE age AND true\n",
            "  ·                            ─┬─\n",
            "  ·                             ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn or_operand_must_be_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name OR true"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of OR must be type boolean, not type text\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name OR true\n",
            "  ·                            ──┬─\n",
            "  ·                              ╰─ this is text, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn not_operand_error_propagates_through_case_when() {
    let db = setup();
    // The NOT operand error is specific enough that the enclosing CASE/WHEN
    // does not shadow it with its own boolean-condition wording.
    assert_analyze_err!(
        db.analyze("SELECT CASE WHEN NOT age THEN 1 ELSE 0 END FROM users"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of NOT must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ SELECT CASE WHEN NOT age THEN 1 ELSE 0 END FROM users\n",
            "  ·                      ─┬─\n",
            "  ·                       ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

// ── Operator with an UNKNOWN operand (NULL / untyped literal) ────────────────
// PG resolves the unknown operand to the *other* (concrete) operand's type, so
// these are all valid. The analyzer used to reject them with a spurious
// "operator does not exist: integer > unknown".

#[test]
fn comparison_with_null_resolves_to_column_type() {
    let db = setup();
    let s = db.analyze("SELECT age > NULL AS r FROM users").unwrap();
    assert_cols(&s, vec![cn("r", bool_ty())]);
}

#[test]
fn equality_with_null_resolves_to_column_type() {
    let db = setup();
    let s = db.analyze("SELECT age = NULL AS r FROM users").unwrap();
    assert_cols(&s, vec![cn("r", bool_ty())]);
}

#[test]
fn arithmetic_with_null_resolves_to_column_type() {
    let db = setup();
    let s = db.analyze("SELECT age + NULL AS r FROM users").unwrap();
    assert_cols(&s, vec![cn("r", int4())]);
}

#[test]
fn bigint_comparison_with_null_is_accepted() {
    let db = setup();
    let s = db.analyze("SELECT id >= NULL AS r FROM users").unwrap();
    assert_cols(&s, vec![cn("r", bool_ty())]);
}

// ── Stress: nested COALESCE / CASE / expressions ─────────────────────────────

#[test]
fn stress_nested_coalesce() {
    let db = setup();
    // COALESCE(COALESCE(nullable, nullable), literal) → NOT NULL.
    let sql = "SELECT COALESCE(COALESCE(age, age), 0) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("val", int4())]);
}

#[test]
fn stress_coalesce_all_nullable() {
    let db = setup();
    // COALESCE(nullable, nullable) → still nullable (no non-null fallback).
    let sql = "SELECT COALESCE(age, age) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("val", int4())]);
}

#[test]
fn stress_case_with_null_branch() {
    let db = setup();
    // CASE with one branch returning NULL explicitly.
    let sql = "SELECT CASE WHEN age > 18 THEN name ELSE NULL END as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("val", text())]);
}

#[test]
fn stress_case_mixing_nullable_branches() {
    let db = setup();
    // CASE with one NOT NULL branch and one nullable branch. Qualify `id`
    // explicitly — both `users.id` and `posts.id` exist, so a bare `id` is
    // ambiguous (PG SQLSTATE 42702). The point of this test is the
    // nullability merge, not column resolution.
    let sql = "SELECT CASE WHEN u.id > 0 THEN name ELSE body END as val \
               FROM users u INNER JOIN posts p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    // name is NOT NULL but body is nullable → result is nullable.
    assert_cols(&info, vec![cn("val", text())]);
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_nested_case_in_coalesce() {
    let db = setup();
    // COALESCE(CASE without ELSE, literal) → NOT NULL.
    let sql = "SELECT COALESCE( \
                   CASE WHEN age > 18 THEN age END, \
                   0 \
               ) as val FROM users";
    let info = db.analyze(sql).unwrap();
    // CASE without ELSE is nullable, but COALESCE with 0 fallback makes it NOT NULL.
    assert_cols(&info, vec![c("val", int4())]);
}

// ── Strict pg_catalog functions ──────────────────────────────────────────────

#[test]
fn strict_pg_catalog_function_not_null() {
    let db = setup();
    // length(text) is pg_catalog, strict, not in exceptions → NOT NULL with NOT NULL input.
    let sql = "SELECT length(name) as len FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("len", int4())]);
}

#[test]
fn strict_pg_catalog_function_nullable_with_nullable_arg() {
    let db = setup();
    // length(text) is strict: nullable input → nullable output.
    let sql = "SELECT length(body) as len FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("len", int4())]);
}

#[test]
fn strict_pg_catalog_upper_not_null() {
    let db = setup();
    // upper(text) is pg_catalog, strict → NOT NULL with NOT NULL input.
    let sql = "SELECT upper(name) as uname FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("uname", text())]);
}

// ── Operators: + / ‖ strictness ──────────────────────────────────────────────

#[test]
fn operator_plus_not_null() {
    let db = setup();
    // 1 + 1: both non-null, operator not in exceptions → NOT NULL.
    let sql = "SELECT 1 + 1 as result";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("result", int4())]);
}

#[test]
fn operator_plus_nullable_arg() {
    let db = setup();
    // age is nullable → result is nullable.
    let sql = "SELECT age + 1 as next_age FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("next_age", int4())]);
}

#[test]
fn operator_concat_not_null() {
    let db = setup();
    // || with two NOT NULL → NOT NULL.
    let sql = "SELECT name || ' <' || email || '>' as display FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("display", text())]);
}

#[test]
fn operator_concat_nullable_arg() {
    let db = setup();
    // body is nullable → concat is nullable.
    let sql = "SELECT title || body as combined FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("combined", text())]);
}

// ── NULL literals ───────────────────────────────────────────────────────────

#[test]
fn null_at_top_level_coerces_to_text() {
    let db = setup();
    // PG coerces unresolved UNKNOWN output columns to text before sending
    // them over the wire. A bare `NULL` surfaces as `text` nullable.
    let s = db.analyze("SELECT NULL AS x").unwrap();
    assert_cols(&s, vec![cn("x", text())]);
}

#[test]
fn null_eq_null_is_nullable_bool() {
    let db = setup();
    // `NULL = NULL` is NULL (not TRUE) — result is `bool` nullable.
    let s = db.analyze("SELECT NULL = NULL AS x").unwrap();
    assert_cols(&s, vec![cn("x", bool_ty())]);
}

#[test]
fn null_text_concat_propagates_null() {
    let db = setup();
    // Strict `||`: NULL on the left makes the whole concat nullable.
    let s = db.analyze("SELECT NULL::text || 'x' AS y").unwrap();
    assert_cols(&s, vec![cn("y", text())]);
}

// ── Schema-qualified function call ──────────────────────────────────────────

#[test]
fn schema_qualified_function_call() {
    let db = setup();
    // `pg_catalog.now()` should resolve the same as `now()`.
    let s = db.analyze("SELECT pg_catalog.now() AS ts").unwrap();
    assert_cols(&s, vec![c("ts", timestamptz())]);
}

// ── Nullability of strict comparisons ────────────────────────────────────────
//
// Comparison operators (`=`, `<`, `<>`, …) are strict — any NULL operand
// makes the result NULL. The analyzer tracks this through the usual
// `any_arg_nullable` path.

#[test]
fn comparison_both_sides_not_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id = user_id AS same FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("same", bool_ty())]);
}

#[test]
fn comparison_with_nullable_column() {
    let db = setup();
    // `age` (nullable) `= 18` → bool but nullable.
    let s = db.analyze("SELECT age = 18 AS adult FROM users").unwrap();
    assert_cols(&s, vec![cn("adult", bool_ty())]);
}

#[test]
fn comparison_with_nullable_both_sides() {
    let db = setup();
    let s = db
        .analyze("SELECT p.body = u.name AS match FROM posts p JOIN users u ON u.id = p.user_id")
        .unwrap();
    // `body` is nullable, `name` is NOT NULL — result nullable (any-nullable).
    assert_cols(&s, vec![cn("match", bool_ty())]);
}

// ── CASE / COALESCE branch-type validation ──────────────────────────────────

#[test]
fn case_with_incompatible_concrete_arms_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT CASE WHEN true THEN 1 ELSE 'x'::text END"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "CASE types text and integer cannot be matched\n",
            "  ╭────\n",
            "1 │ SELECT CASE WHEN true THEN 1 ELSE 'x'::text END\n",
            "  ·                            ┬\n",
            "  ·                            ╰─ this is integer\n",
            "  ╰────\n",
            "  help: add an explicit cast so the branches share a type, e.g. `expr::integer`\n",
        ),
    );
}

#[test]
fn case_with_incompatible_unknown_literal_rejected() {
    // The branches resolve to int4 (the only concrete type); PG then runs
    // int4's input function on the literal at parse_analyze time. The
    // analyzer mirrors it via `literal_input`, message verbatim.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT CASE WHEN true THEN 1 ELSE 'x' END"),
        AnalyzeError::InvalidLiteral(_),
        concat!(
            "invalid input syntax for type integer: \"x\"\n",
            "  ╭────\n",
            "1 │ SELECT CASE WHEN true THEN 1 ELSE 'x' END\n",
            "  ·                                   ─┬─\n",
            "  ·                                    ╰─ this literal\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn case_with_valid_unknown_literal_coerced() {
    // `'2'` is valid int4 input, so the CASE lands on integer — like PG.
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN true THEN 1 ELSE '2' END AS v")
        .unwrap();
    assert_cols(&s, vec![c("v", int4())]);
}

#[test]
fn coalesce_with_incompatible_concrete_arms_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT COALESCE(1, 'x'::text)"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "COALESCE types integer and text cannot be matched\n",
            "  ╭────\n",
            "1 │ SELECT COALESCE(1, 'x'::text)\n",
            "  ·                    ─┬─\n",
            "  ·                     ╰─ this is text\n",
            "  ╰────\n",
            "  help: add an explicit cast so the branches share a type, e.g. `expr::text`\n",
        ),
    );
}

#[test]
fn coalesce_with_incompatible_unknown_literal_rejected() {
    // The args resolve to int4 (the only concrete type); PG then runs
    // int4's input function on the literal at parse_analyze time. The
    // analyzer mirrors it via `literal_input`, message verbatim.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT COALESCE(1, 'x')"),
        AnalyzeError::InvalidLiteral(_),
        concat!(
            "invalid input syntax for type integer: \"x\"\n",
            "  ╭────\n",
            "1 │ SELECT COALESCE(1, 'x')\n",
            "  ·                    ─┬─\n",
            "  ·                     ╰─ this literal\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn coalesce_with_valid_unknown_literal_coerced() {
    // `'42'` is valid int4 input, so COALESCE lands on integer — like PG.
    let db = setup();
    let s = db.analyze("SELECT COALESCE(1, '42') AS v").unwrap();
    assert_cols(&s, vec![c("v", int4())]);
}

// ── GREATEST / LEAST (non-strict minmax) ────────────────────────────────────

#[test]
fn greatest_of_all_null_typed_args() {
    let db = setup();
    // `GREATEST(NULL::int4, NULL::int4)` returns NULL typed int4 in PG —
    // the analyzer resolves the common arg type and marks the result
    // nullable because every arg is nullable.
    let s = db
        .analyze("SELECT GREATEST(NULL::int4, NULL::int4) AS g")
        .unwrap();
    assert_cols(&s, vec![cn("g", int4())]);
}

#[test]
fn least_of_mixed_nullable_args_keeps_not_null() {
    let db = setup();
    // `LEAST(nullable, non-null)` — GREATEST/LEAST skip NULLs at runtime,
    // so with at least one NOT NULL arg the result is NOT NULL (stricter
    // than PG's statement-level nullability, which just tracks types).
    let s = db.analyze("SELECT LEAST(age, id) AS m FROM users").unwrap();
    assert_cols(&s, vec![c("m", int8())]);
}

#[test]
fn greatest_over_int4_and_int8_promotes_to_int8() {
    let db = setup();
    // Common-type resolution promotes int4 + int8 → int8. `age` is nullable
    // but `id` is NOT NULL, so GREATEST's "skip NULLs" semantics guarantee
    // a non-null result.
    let s = db
        .analyze("SELECT GREATEST(age, id) AS g FROM users")
        .unwrap();
    assert_cols(&s, vec![c("g", int8())]);
}

#[test]
fn greatest_all_not_null_args() {
    let db = setup();
    let s = db
        .analyze("SELECT GREATEST(id, 1::int8) AS g FROM users")
        .unwrap();
    assert_cols(&s, vec![c("g", int8())]);
}

// ── ROW constructor ─────────────────────────────────────────────────────────

#[test]
fn row_comparison_returns_bool() {
    let db = setup();
    // `ROW(...)` builds an anonymous composite; the `record = record`
    // operator compares element-wise and returns bool.
    let s = db.analyze("SELECT ROW(1, 2) = ROW(1, 2) AS e").unwrap();
    assert_cols(&s, vec![c("e", bool_ty())]);
}

#[test]
fn row_from_columns_comparison() {
    let db = setup();
    // ROW wrappers are never NULL themselves, and `record = record` is
    // strict — since `id`/`name` are NOT NULL and the RHS uses literals,
    // the result is NOT NULL.
    let s = db
        .analyze("SELECT ROW(id, name) = ROW(1::int8, 'x') AS e FROM users")
        .unwrap();
    assert_cols(&s, vec![c("e", bool_ty())]);
}

// ── Interval / date arithmetic ───────────────────────────────────────────────

#[test]
fn timestamptz_plus_interval() {
    let db = setup();
    // `now()` is NOT NULL and `INTERVAL 'n'` is a constant, so the sum
    // stays NOT NULL.
    let s = db
        .analyze("SELECT now() + INTERVAL '1 day' AS later")
        .unwrap();
    assert_cols(&s, vec![c("later", timestamptz())]);
}

#[test]
fn age_between_two_timestamps() {
    let db = setup();
    let s = db.analyze("SELECT age(now(), now()) AS delta").unwrap();
    assert_cols(&s, vec![c("delta", interval())]);
}

#[test]
fn extract_year_from_now() {
    let db = setup();
    // EXTRACT returns numeric (PG14+) — independent of whether the source
    // field is nullable.
    let s = db.analyze("SELECT EXTRACT(YEAR FROM now()) AS y").unwrap();
    assert_cols(&s, vec![c("y", numeric())]);
}

#[test]
fn date_trunc_on_timestamptz() {
    let db = setup();
    let s = db.analyze("SELECT date_trunc('day', now()) AS d").unwrap();
    assert_cols(&s, vec![c("d", timestamptz())]);
}

// ── Non-strict pg_catalog functions that never return NULL ───────────────────

#[test]
fn nonstrict_concat_never_null() {
    let db = setup();
    // concat is non-strict but never returns NULL (treats NULLs as '').
    let sql = "SELECT concat(p.title, ' ', p.body) as full_text FROM posts p";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("full_text", text())]);
}

#[test]
fn nonstrict_concat_ws_never_null() {
    let db = setup();
    let sql = "SELECT concat_ws(', '::text, name, email) as combined FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("combined", text())]);
}

#[test]
fn concat_ws_with_null_separator_is_nullable() {
    let db = setup();
    // `concat_ws(sep, …)` skips NULL items, but a NULL separator makes the
    // entire result NULL — the variadic part is non-strict, the separator
    // arg is not.
    let sql = "SELECT concat_ws(NULL::text, 'a', 'b') AS c";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("c", text())]);
}

#[test]
fn nonstrict_now_never_null() {
    let db = setup();
    let sql = "SELECT now() as ts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("ts", timestamptz())]);
}

#[test]
fn nonstrict_random_never_null() {
    let db = setup();
    let sql = "SELECT random() as r";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("r", float8())]);
}

// ── Function overload resolution: preferred-type tie-break ───────────────────

#[test]
fn floor_of_integer_resolves_to_double_precision() {
    // `floor` has only `floor(numeric)` and `floor(double precision)`. For an
    // integer argument PG picks the preferred numeric type (double precision /
    // float8), not numeric. The analyzer used to return numeric.
    let db = setup();
    let s = db
        .analyze("SELECT floor(id) AS fb, floor(age) AS fa FROM users")
        .unwrap();
    assert_cols(&s, vec![c("fb", float8()), cn("fa", float8())]);
}

#[test]
fn single_overload_function_with_non_coercible_arg_rejected() {
    // `jsonb_typeof` has one overload, `jsonb_typeof(jsonb)`. An integer
    // argument has no implicit cast to jsonb, so PG rejects it — the analyzer
    // used to accept any single-overload function whose argument *count* lined
    // up, regardless of type.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT jsonb_typeof(42)"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function jsonb_typeof(integer) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT jsonb_typeof(42)\n",
            "  ·        ──────┬─────\n",
            "  ·              ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: the only candidate is:\n",
            "          jsonb_typeof(jsonb)\n",
        ),
    );
}

// ── Concatenation of a non-text value with a string literal ──────────────────

#[test]
fn concat_int_with_unknown_literal_resolves_to_text() {
    // `int || 'x'` resolves via PG's polymorphic `anynonarray || text`, with
    // the unknown literal taken as text → text. The analyzer used to reject it
    // with "operator does not exist: integer || unknown".
    let db = setup();
    let s = db.analyze("SELECT age || '!' AS c FROM users").unwrap();
    assert_cols(&s, vec![cn("c", text())]);
}

#[test]
fn concat_unknown_literal_with_int_resolves_to_text() {
    let db = setup();
    let s = db.analyze("SELECT 'n=' || age AS c FROM users").unwrap();
    assert_cols(&s, vec![cn("c", text())]);
}

#[test]
fn concat_function_result_int_with_unknown_literal_resolves_to_text() {
    // Like `age || '!'`, but the left side is an int-returning *function*
    // result (`length(name)`) rather than a column. Both are `int4`, so the
    // `anynonarray || text` resolution must fire identically — guards against
    // an `operator does not exist: integer || unknown` regression.
    let db = setup();
    // `name` is NOT NULL → `length(name)` and the strict `||` stay NOT NULL.
    let s = db
        .analyze("SELECT length(name) || '!' AS c FROM users")
        .unwrap();
    assert_cols(&s, vec![c("c", text())]);
}

#[test]
fn variadic_concat_ws_rejects_non_text_separator() {
    // `concat_ws(sep text, VARIADIC "any")` — the *fixed* separator must be
    // text. A lone `integer` arg binds to it and doesn't coerce, so the call
    // doesn't resolve. The variadic short-circuit used to accept any args.
    // PG: `function concat_ws(integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT concat_ws(age) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function concat_ws(integer) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT concat_ws(age) FROM users\n",
            "  ·        ────┬────\n",
            "  ·            ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: the only candidate is:\n",
            "          concat_ws(text, VARIADIC \"any\")\n",
        ),
    );
}

#[test]
fn variadic_concat_ws_with_text_separator_accepted() {
    // A text separator with a non-text variadic tail is valid — the `"any"`
    // variadic element accepts the `int`. Guards against over-rejection.
    let db = setup();
    let s = db
        .analyze("SELECT concat_ws(name, age) AS c FROM users")
        .unwrap();
    assert_cols(&s, vec![c("c", text())]);
}

// ── Integer literal magnitude → int4 / int8 / numeric (PG make_const) ────────

#[test]
fn large_integer_literal_is_bigint() {
    // libpg_query stores an integer too large for int4 as a `Float` token;
    // PG re-types it by magnitude. `9999999999` fits int8 → bigint (not numeric).
    let db = setup();
    let s = db.analyze("SELECT 9999999999 AS big").unwrap();
    assert_cols(&s, vec![c("big", int8())]);
}

#[test]
fn oversize_integer_literal_is_numeric() {
    // Beyond int8 range → numeric.
    let db = setup();
    let s = db
        .analyze("SELECT 99999999999999999999999 AS huge")
        .unwrap();
    assert_cols(&s, vec![c("huge", numeric())]);
}

#[test]
fn small_integer_literal_stays_int4() {
    let db = setup();
    let s = db.analyze("SELECT 42 AS small").unwrap();
    assert_cols(&s, vec![c("small", int4())]);
}

#[test]
fn radix_and_underscore_integer_literals_typed_by_magnitude() {
    // libpg_query hands non-int4 hex/octal/binary/underscore integers over
    // as `Float` text; PG's make_const re-parses them with pg_strtoint64,
    // which understands the prefixes and underscores (verified on PG 18).
    let db = setup();
    let s = db
        .analyze(
            "SELECT 0x80000000 AS a, 0xFFFFFFFFFF AS b, 0x7FFFFFFFFFFFFFFF AS c, \
             10_000_000_000 AS d, 0o77777777777 AS e, -0x80000000 AS f, \
             -0x8000000000000000 AS g, 0x8000000000000000 AS h, 1_000 AS i, \
             0b101 AS j, 1_000.5 AS k",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", int8()),
            c("b", int8()),
            c("c", int8()),
            c("d", int8()),
            c("e", int8()),
            c("f", int4()),
            c("g", int8()),
            c("h", numeric()),
            c("i", int4()),
            c("j", int4()),
            c("k", numeric()),
        ],
    );
}

// ── Bit-string literals (B'…' / X'…') → bit ─────────────────────────────────

fn bit() -> Type {
    basic("pg_catalog", "bit")
}

#[test]
fn bit_string_literals_are_bit() {
    // PG's make_const types a T_BitString as `bit` (typmod -1), not bytea.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE m (bits bit(8));").unwrap();
    let s = db
        .analyze(
            "SELECT B'101' AS a, X'1F' AS b, B'101' | B'011' AS c, \
             B'101'::varbit AS e",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", bit()),
            c("b", bit()),
            c("c", bit()),
            c("e", basic("pg_catalog", "varbit")),
        ],
    );
    let s = db.analyze("SELECT bits & B'00000001' AS a FROM m").unwrap();
    assert_cols(&s, vec![cn("a", bit())]);
}

#[test]
fn bit_string_literal_contents_validated() {
    // PG runs bit_in on the literal in make_const (22P02).
    let db = setup();
    for (sql, msg) in [
        ("SELECT B'102'", "\"2\" is not a valid binary digit"),
        ("SELECT X'1G'", "\"G\" is not a valid hexadecimal digit"),
    ] {
        let err = db.analyze(sql).expect_err(sql);
        assert!(
            matches!(err, AnalyzeError::InvalidLiteral(_)),
            "{sql}: {err:?}"
        );
        assert!(err.to_string().starts_with(msg), "{sql}: {err}");
    }
}

#[test]
fn variadic_any_requires_at_least_one_variadic_arg() {
    // `VARIADIC "any"` functions need ≥1 arg in the variadic slot, so
    // `concat_ws(text)` and `concat()` do not exist — only the fixed params
    // isn't enough. PG: `function concat_ws(text) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT concat_ws(name) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function concat_ws(text) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT concat_ws(name) FROM users\n",
            "  ·        ────┬────\n",
            "  ·            ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: the only candidate is:\n",
            "          concat_ws(text, VARIADIC \"any\")\n",
        ),
    );
    assert_analyze_err!(
        db.analyze("SELECT concat() FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function concat() does not exist\n",
            "  ╭────\n",
            "1 │ SELECT concat() FROM users\n",
            "  ·        ───┬──\n",
            "  ·           ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: the only candidate is:\n",
            "          concat(VARIADIC \"any\")\n",
        ),
    );
}

#[test]
fn variadic_any_with_one_variadic_arg_accepted() {
    // Guard against over-rejection: one arg in the variadic slot is enough.
    // `concat(name)` resolves; `format(name)` resolves via the non-variadic
    // `format(text)` overload.
    let db = setup();
    assert_eq!(
        col(
            &db.analyze("SELECT concat(name) AS c FROM users").unwrap(),
            "c"
        )
        .pg_type,
        text()
    );
    assert_eq!(
        col(
            &db.analyze("SELECT format(name) AS c FROM users").unwrap(),
            "c"
        )
        .pg_type,
        text()
    );
}

/// User-defined variadic overloads shared by the expansion tests below.
fn setup_variadic() -> PgCatalog {
    let mut db = setup();
    db.apply_sql(
        "CREATE FUNCTION f_var(VARIADIC xs INT[]) RETURNS INT
             AS $$ SELECT array_length(xs, 1) $$ LANGUAGE sql;
         CREATE FUNCTION f_var_any(VARIADIC xs anyarray) RETURNS anyelement
             AS $$ SELECT xs[1] $$ LANGUAGE sql;
         CREATE FUNCTION ov(a INT) RETURNS TEXT AS $$ SELECT 'plain' $$ LANGUAGE sql;
         CREATE FUNCTION ov(VARIADIC a INT[]) RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql;
         CREATE FUNCTION ov2(a INT, VARIADIC b INT[]) RETURNS TEXT
             AS $$ SELECT 'x' $$ LANGUAGE sql;
         CREATE FUNCTION ov2(VARIADIC a INT[]) RETURNS INT AS $$ SELECT 1 $$ LANGUAGE sql;",
    )
    .unwrap();
    db
}

#[test]
fn variadic_tail_takes_the_element_type() {
    // Unknown literals and params in the variadic tail are coerced to the
    // element type (`text`), not the declared `text[]`.
    let db = setup();
    let s = db
        .analyze(
            "SELECT jsonb_extract_path('{}'::jsonb, 'a', 'b') AS p,
                    jsonb_extract_path_text('{}'::jsonb, $x, $y) AS t",
        )
        .unwrap();
    assert_eq!(col(&s, "p").pg_type, jsonb());
    assert_eq!(col(&s, "t").pg_type, text());
    assert_eq!(
        s.params
            .iter()
            .map(|p| p.pg_type.clone())
            .collect::<Vec<_>>(),
        vec![text(), text()]
    );
    assert_err_prefix(
        &db,
        "SELECT jsonb_extract_path('{}'::jsonb)",
        "function jsonb_extract_path(jsonb) does not exist",
    );
}

#[test]
fn user_variadic_functions_expand_to_the_call() {
    let db = setup_variadic();
    let s = db
        .analyze(
            "SELECT f_var(1, 2, 3) AS a, f_var_any(1, 2) AS b,
                    f_var(VARIADIC ARRAY[1, 2]) AS c,
                    f_var_any(VARIADIC ARRAY['a'::text]) AS d",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", int4()),
            cn("b", int4()),
            cn("c", int4()),
            cn("d", text()),
        ],
    );
    // No variadic element, or the array passed without the keyword.
    assert_err_prefix(&db, "SELECT f_var()", "function f_var() does not exist");
    assert_err_prefix(
        &db,
        "SELECT f_var(ARRAY[1, 2])",
        "function f_var(integer[]) does not exist",
    );
    assert_err_prefix(
        &db,
        "SELECT f_var(VARIADIC 1)",
        "function f_var(integer) does not exist",
    );
}

#[test]
fn variadic_keyword_on_a_plain_function_is_a_positional_call() {
    let db = setup();
    let s = db.analyze("SELECT upper(VARIADIC 'a') AS u").unwrap();
    assert_eq!(col(&s, "u").pg_type, text());
    assert_err_prefix(
        &db,
        "SELECT upper(VARIADIC ARRAY['a'])",
        "function upper(text[]) does not exist",
    );
}

#[test]
fn non_variadic_overload_beats_an_equal_variadic_expansion() {
    let db = setup_variadic();
    let s = db
        .analyze("SELECT ov(1) AS plain, ov(1, 2) AS spread, ov2(1) AS one")
        .unwrap();
    // `ov(1)` is its body, `'plain'`; a variadic call isn't read as one.
    assert_cols(
        &s,
        vec![c("plain", text()), cn("spread", int4()), cn("one", int4())],
    );
    // Two variadic expansions with the same signature stay tied.
    assert_err_prefix(
        &db,
        "SELECT ov2(1, 2)",
        "function ov2(integer, integer) is not unique",
    );
}

#[track_caller]
fn assert_err_prefix(db: &PgCatalog, sql: &str, expected: &str) {
    let err = db.analyze(sql).unwrap_err();
    assert!(
        err.to_string().starts_with(expected),
        "expected `{expected}` for `{sql}`, got: {err}"
    );
}

// ── Ambiguous overload resolution (SQLSTATE 42725) ──────────────────────────

#[test]
fn unknown_args_with_tied_candidates_is_not_unique() {
    // `mod` has int2/int4/int8/numeric variants — all Numeric category,
    // none carrying the category's preferred type (float8) — so unknown
    // inputs can't be resolved and PG refuses rather than guessing.
    let db = setup();
    let err = db.analyze("SELECT mod('5', '2')").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("function mod(unknown, unknown) is not unique"),
        "got: {err}"
    );
    let err = db.analyze("SELECT gcd('4', '6')").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("function gcd(unknown, unknown) is not unique"),
        "got: {err}"
    );
}

#[test]
fn unknown_args_resolved_by_preferred_type() {
    // `round` *does* have a float8 (preferred) variant, so the same shape
    // resolves — to double precision, exactly like PG.
    let db = setup();
    let s = db.analyze("SELECT round('1.5') AS v").unwrap();
    assert_cols(&s, vec![c("v", float8())]);
    let s = db.analyze("SELECT power('2', '3') AS v").unwrap();
    assert_cols(&s, vec![c("v", float8())]);
}

#[test]
fn both_unknown_operator_with_many_overloads_is_not_unique() {
    // `+` has no text overload and its candidates span several categories,
    // so two untyped operands are ambiguous (PG: 42725). `=` resolves via
    // the text fallback and stays accepted.
    let db = setup();
    let err = db.analyze("SELECT $p0 + $p1").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("operator is not unique: unknown + unknown"),
        "got: {err}"
    );
    db.analyze("SELECT NULL = NULL").unwrap();
}

// ── typmod through CASE / COALESCE / GREATEST / ARRAY / sublinks / casts ────

#[test]
fn typmod_follows_pg_expr_typmod_rules() {
    // atttypmod of the equivalent view columns on PG 18.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            vc varchar(10) NOT NULL, nm numeric(10,2) NOT NULL, c char(5) NOT NULL,
            ts timestamp(3) NOT NULL, b bool NOT NULL, tarr text[] NOT NULL,
            vc2 varchar(20)
         );",
    )
    .unwrap();
    let s = db
        .analyze(
            "SELECT (SELECT vc FROM t LIMIT 1) AS a, COALESCE(vc, vc) AS b, \
             CASE WHEN b THEN vc ELSE vc END AS c, GREATEST(vc, vc) AS d, ARRAY[vc] AS e, \
             vc::varchar AS f, nm::numeric AS g, c::bpchar AS h, ts::timestamp AS i, \
             tarr::varchar(2)[] AS j, (SELECT nm FROM t LIMIT 1) AS k, \
             COALESCE(vc, NULL) AS l, CASE WHEN b THEN vc END AS m, COALESCE(vc, vc2) AS n, \
             ARRAY(SELECT vc FROM t) AS x \
             FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", varchar_n(10)),
            c("b", varchar_n(10)),
            c("c", varchar_n(10)),
            c("d", varchar_n(10)),
            c("e", array_of(varchar_n(10))),
            c("f", varchar()),
            c("g", numeric()),
            c("h", bpchar()),
            c("i", timestamp()),
            c("j", array_of(varchar_n(2))),
            cn("k", numeric_ps(10, 2)),
            c("l", varchar()),
            cn("m", varchar()),
            c("n", varchar()),
            c("x", array_of(varchar_n(10))),
        ],
    );
}

#[test]
fn nullif_result_is_the_operators_coerced_left_input() {
    // transformAExprNullIf: the result type is the left input of the
    // resolved `=` — varchar has no `=`, so `text = text` wins and the
    // result is text; numeric's own `=` keeps numeric(10,2).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (vc varchar(10) NOT NULL, nm numeric(10,2) NOT NULL, b bool NOT NULL);",
    )
    .unwrap();
    let s = db
        .analyze(
            "SELECT NULLIF(vc, 'x') AS a, NULLIF(vc, vc) AS b, NULLIF(nm, 0) AS c, \
             NULLIF(nm, 1.5) AS d FROM t",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", text()),
            cn("b", text()),
            cn("c", numeric_ps(10, 2)),
            cn("d", numeric_ps(10, 2)),
        ],
    );
    assert_err_prefix!(
        db.analyze("SELECT NULLIF(b, 1) FROM t"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: boolean = integer"
    );
    assert_err_prefix!(
        db.analyze("SELECT NULLIF(1, 'x')"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"x\""
    );
}

#[test]
fn case_resolves_the_else_branch_first() {
    // transformCaseExpr puts the ELSE result first in select_common_type.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (b bool NOT NULL, vc varchar(10) NOT NULL, c char(5) NOT NULL);")
        .unwrap();
    let s = db
        .analyze("SELECT CASE WHEN b THEN vc ELSE c END AS a FROM t")
        .unwrap();
    assert_cols(&s, vec![c("a", bpchar())]);
    assert_err_prefix!(
        db.analyze("SELECT CASE WHEN b THEN 1 ELSE 'x'::text END FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "CASE types text and integer cannot be matched"
    );
    assert_err_prefix!(
        db.analyze("SELECT CASE WHEN b THEN 1 WHEN b THEN 2.5 ELSE true END FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "CASE types boolean and integer cannot be matched"
    );
    assert_err_prefix!(
        db.analyze("SELECT COALESCE(1, true)"),
        AnalyzeError::DatatypeMismatch(_),
        "COALESCE types integer and boolean cannot be matched"
    );
    assert_err_prefix!(
        db.analyze("SELECT GREATEST(1, 'a'::text)"),
        AnalyzeError::DatatypeMismatch(_),
        "GREATEST types integer and text cannot be matched"
    );
}

#[test]
fn simple_case_untyped_test_is_text_and_distinct_from_null_is_a_null_test() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (vc varchar(10) NOT NULL);")
        .unwrap();
    // An untyped CASE test expression is forced to text.
    assert_err_prefix!(
        db.analyze("SELECT CASE $p WHEN 1 THEN 'a' END AS a"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: text = integer"
    );
    // `x IS DISTINCT FROM NULL` is `x IS NOT NULL`: nothing types $p.
    assert_err_prefix!(
        db.analyze("SELECT $p IS DISTINCT FROM NULL AS a"),
        AnalyzeError::IndeterminateType(_),
        "could not determine data type of parameter $1"
    );
    // WHEN values are typed by the `=` PG resolves against the test.
    let s = db
        .analyze("SELECT CASE 1 WHEN $p THEN 'a' END AS a")
        .unwrap();
    assert_params(&s, vec![p(int4())]);
    let s = db
        .analyze("SELECT CASE vc WHEN $p THEN 'a' END AS a FROM t")
        .unwrap();
    assert_params(&s, vec![p(text())]);
}

#[test]
fn string_category_operators_with_an_untyped_side_resolve_like_pg() {
    // func_select_candidate keeps the candidates taking the preferred type
    // of the known input's category (text for varchar/bpchar), and picks
    // the string category at an unknown position.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (vc varchar(10) NOT NULL, c char(3) NOT NULL);")
        .unwrap();
    let s = db
        .analyze("SELECT vc = 'x' AS a, c = 'x' AS b, c || 'x' AS d, vc || 'y' AS e FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", bool_ty()),
            c("b", bool_ty()),
            c("d", text()),
            c("e", text()),
        ],
    );
    let s = db.analyze("SELECT vc FROM t WHERE vc = $p").unwrap();
    assert_params(&s, vec![p(text())]);
}

// ── Qualified-name lookups: schema first, PG wording ────────────────────────

#[test]
fn qualified_names_report_missing_schemas_and_collations_like_pg() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tq (s text NOT NULL); CREATE TYPE comp AS (a int, b text);")
        .unwrap();
    for (sql, msg) in [
        ("SELECT '(1,x)'::comp.a", "schema \"comp\" does not exist"),
        (
            "SELECT 1 OPERATOR(nope.+) 2",
            "schema \"nope\" does not exist",
        ),
        (
            "SELECT s COLLATE nope.nosuch FROM tq",
            "schema \"nope\" does not exist",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::UndefinedSchema(_), msg);
    }
    for (sql, msg) in [
        (
            "SELECT s COLLATE \"nosuch\" FROM tq",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
        ),
        (
            "SELECT s COLLATE pg_catalog.nosuch FROM tq",
            "collation \"pg_catalog.nosuch\" for encoding \"UTF8\" does not exist",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::UndefinedObject(_), msg);
    }
    // A schema-qualified operator resolves within that schema.
    let s = db
        .analyze("SELECT 1 OPERATOR(pg_catalog.+) 2 AS a")
        .unwrap();
    assert_cols(&s, vec![c("a", int4())]);
    assert_err_prefix!(
        db.analyze("SELECT 1 OPERATOR(pg_catalog.+) true"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: integer pg_catalog.+ boolean"
    );
    assert_err_prefix!(
        db.analyze("SELECT 1 OPERATOR(public.+) 2"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: integer public.+ integer"
    );
}
