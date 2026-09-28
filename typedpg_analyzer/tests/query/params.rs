//! Parameter inference: `$name`, `$name!`, `$name?`, goal-type-driven
//! inference (LIMIT/OFFSET/WHERE/INSERT/UPDATE), preferred-type tiebreaks,
//! `$..spread`.

use crate::common::*;

fn empty_db() -> PgCatalog {
    PgCatalog::new().unwrap()
}

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id         BIGINT PRIMARY KEY,
            name       TEXT NOT NULL,
            email      TEXT NOT NULL,
            age        INT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
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

/// Assert that the analyzer produces the expected param types.
fn assert_param_types(db: &PgCatalog, sql: &str, expected: &[Type]) {
    let info = db.analyze(sql).unwrap();
    assert_eq!(
        info.params.len(),
        expected.len(),
        "param count mismatch for: {sql}"
    );
    for (i, (p, exp)) in info.params.iter().zip(expected.iter()).enumerate() {
        assert_eq!(&p.pg_type, exp, "param ${} type mismatch for: {sql}", i + 1);
    }
}

/// Assert the single param of `sql` resolves to the expected PG type.
fn assert_single_param_ty(db: &PgCatalog, sql: &str, expected: Type) {
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params.len(), 1, "expected exactly one param in: {sql}");
    assert_eq!(
        info.params[0].pg_type, expected,
        "param PG type mismatch for: {sql}"
    );
}

// ── Untyped params fall back to PG's preferred type: `text` ──────────────────

#[test]
fn untyped_param_in_select_defaults_to_text() {
    let db = empty_db();
    let info = db.analyze("SELECT $p1").unwrap();
    assert_eq!(info.params[0].pg_type, text());
}

#[test]
fn untyped_params_in_comparison_default_to_text() {
    let db = empty_db();
    let info = db.analyze("SELECT $p1 > $p2").unwrap();
    assert_eq!(info.params[0].pg_type, text());
    assert_eq!(info.params[1].pg_type, text());
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: parameter nullability annotations
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_not_null_by_default() {
    let db = setup();
    // $p1 has no nullable annotation → NOT NULL.
    // COALESCE(nullable_age, not_null_param) → NOT NULL.
    let sql = "SELECT COALESCE(age, $p1) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "val").nullable);
}

#[test]
fn param_nullable_annotation() {
    let db = setup();
    // $p? has nullable annotation → nullable.
    // COALESCE(nullable_age, nullable_param) → nullable.
    let sql = "SELECT COALESCE(age, $p?) as val FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "val").nullable);
}

#[test]
fn param_nullable_propagates_to_param_info() {
    let db = setup();
    // $id not nullable (default), $age nullable (?).
    let sql = "SELECT * FROM users WHERE id = $id AND age = $age?";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable);
    assert!(info.params[1].nullable);
}

#[test]
fn param_not_null_in_where_comparison() {
    let db = setup();
    // $p1 not null by default → comparison `id = $p1` has both sides NOT NULL → NOT NULL.
    let sql = "SELECT id = $p1 as is_match FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "is_match").nullable);
}

#[test]
fn param_nullable_in_where_comparison() {
    let db = setup();
    // $p? is nullable → comparison `id = $p?` has one nullable side → nullable.
    let sql = "SELECT id = $p? as is_match FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "is_match").nullable);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: auto-inferred param nullability from INSERT/UPDATE target columns
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_nullable_inferred_from_update_nullable_column() {
    let db = setup();
    // age is nullable → $p1 should be auto-inferred as nullable.
    let sql = "UPDATE users SET age = $p1 WHERE id = $p2";
    let info = db.analyze(sql).unwrap();
    assert!(
        info.params[0].nullable,
        "age is nullable → $p1 inferred nullable"
    );
    assert!(
        !info.params[1].nullable,
        "id is NOT NULL → $p2 stays non-nullable"
    );
}

#[test]
fn param_not_null_inferred_from_update_not_null_column() {
    let db = setup();
    // name is NOT NULL → $p1 should stay non-nullable.
    let sql = "UPDATE users SET name = $p1 WHERE id = $p2";
    let info = db.analyze(sql).unwrap();
    assert!(
        !info.params[0].nullable,
        "name is NOT NULL → $p1 stays non-nullable"
    );
    assert!(
        !info.params[1].nullable,
        "id is NOT NULL → $p2 stays non-nullable"
    );
}

#[test]
fn param_nullable_inferred_from_update_multiple_columns() {
    let db = setup();
    // SET name (NOT NULL), age (nullable), email (NOT NULL)
    let sql = "UPDATE users SET name = $p1, age = $p2, email = $p3 WHERE id = $p4";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name NOT NULL");
    assert!(info.params[1].nullable, "age nullable");
    assert!(!info.params[2].nullable, "email NOT NULL");
    assert!(!info.params[3].nullable, "id NOT NULL (WHERE)");
}

#[test]
fn param_nullable_inferred_from_insert_values() {
    let db = setup();
    // body is nullable, title is NOT NULL, user_id is NOT NULL
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL");
    assert!(!info.params[1].nullable, "title NOT NULL");
    assert!(info.params[2].nullable, "body nullable");
}

#[test]
fn param_nullable_inferred_from_insert_all_nullable() {
    let db = setup();
    // age is nullable
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name NOT NULL");
    assert!(!info.params[1].nullable, "email NOT NULL");
    assert!(info.params[2].nullable, "age nullable");
}

#[test]
fn param_explicit_annotation_overrides_inferred_nullable() {
    let db = setup();
    // age is nullable → auto-inferred as nullable.
    // But explicit annotation `$age!` (not nullable) should override.
    let sql = "UPDATE users SET age = $age! WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(
        !info.params[0].nullable,
        "explicit non-nullable overrides inferred"
    );
    assert!(!info.params[1].nullable);
}

#[test]
fn param_explicit_nullable_annotation_on_not_null_column() {
    let db = setup();
    // name is NOT NULL, but explicit $name? annotation forces nullable.
    let sql = "UPDATE users SET name = $name? WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(
        info.params[0].nullable,
        "explicit nullable overrides column NOT NULL"
    );
}

#[test]
fn param_nullable_inferred_from_insert_select() {
    let db = setup();
    // INSERT ... SELECT with params mapped to columns.
    // body is nullable, title is NOT NULL.
    let sql =
        "INSERT INTO posts (user_id, title, body) SELECT $p1, $p2, $p3 FROM users WHERE id = $p4";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL");
    assert!(!info.params[1].nullable, "title NOT NULL");
    assert!(info.params[2].nullable, "body nullable");
}

#[test]
fn param_nullable_inferred_update_with_returning() {
    let db = setup();
    // Verify that param inference works alongside RETURNING.
    let sql = "UPDATE posts SET body = $p1, title = $p2 WHERE id = $p3 RETURNING id, body, title";
    let info = db.analyze(sql).unwrap();
    assert!(
        info.params[0].nullable,
        "body nullable → $p1 inferred nullable"
    );
    assert!(
        !info.params[1].nullable,
        "title NOT NULL → $p2 stays non-nullable"
    );
    assert!(!info.params[2].nullable, "id NOT NULL (WHERE)");
    // RETURNING columns
    assert!(!col(&info, "id").nullable);
    assert!(col(&info, "body").nullable);
    assert!(!col(&info, "title").nullable);
}

#[test]
fn param_bang_overrides_inferred_nullable_on_update() {
    let db = setup();
    // age is nullable → auto-inferred as nullable.
    // But $age! (force non-null) should override the inference.
    let sql = "UPDATE users SET age = $age! WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(
        !info.params[0].nullable,
        "$age! forces non-nullable even though column is nullable"
    );
    assert!(!info.params[1].nullable);
}

#[test]
fn param_bang_overrides_inferred_nullable_on_insert() {
    let db = setup();
    // body is nullable → auto-inferred as nullable.
    // But $body! should override.
    let sql =
        "INSERT INTO posts (user_id, title, body) VALUES ($user_id, $title, $body!) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL, no annotation");
    assert!(!info.params[1].nullable, "title NOT NULL, no annotation");
    assert!(
        !info.params[2].nullable,
        "$body! forces non-nullable even though column is nullable"
    );
}

#[test]
fn param_bang_and_question_mixed_in_update() {
    let db = setup();
    // name is NOT NULL, age is nullable, email is NOT NULL.
    // $name? forces nullable on NOT NULL col, $age! forces non-null on nullable col, $email auto.
    let sql = "UPDATE users SET name = $name?, age = $age!, email = $email WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(info.params[0].nullable, "$name? forces nullable");
    assert!(!info.params[1].nullable, "$age! forces non-nullable");
    assert!(
        !info.params[2].nullable,
        "$email auto → NOT NULL from column"
    );
    assert!(
        !info.params[3].nullable,
        "id NOT NULL (WHERE, no annotation)"
    );
}

#[test]
fn param_bang_on_insert_select() {
    let db = setup();
    // body is nullable, but $body! forces non-null.
    let sql = "INSERT INTO posts (user_id, title, body) \
               SELECT $user_id, $title, $body! FROM users WHERE id = $id";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "user_id NOT NULL, auto");
    assert!(!info.params[1].nullable, "title NOT NULL, auto");
    assert!(!info.params[2].nullable, "$body! forces non-nullable");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: goal-type-driven parameter inference
// ──────────────────────────────────────────────────────────────────────────────
// These tests validate that parameter types are inferred exactly as PostgreSQL
// infers them (via PREPARE), plus nullability inference from column definitions.

// ── LIMIT with bare param ─────────────────────────────────────────────

#[test]
fn goal_limit_bare_param() {
    // LIMIT $p1  →  $p1 must be int8 (bigint), matching PG's transformLimitClause.
    let db = setup();
    let sql = "SELECT id FROM users LIMIT $p1";
    assert_param_types(&db, sql, &[int8()]);
}

#[test]
fn goal_offset_bare_param() {
    // OFFSET $p1  →  $p1 must be int8.
    let db = setup();
    let sql = "SELECT id FROM users OFFSET $p1";
    assert_param_types(&db, sql, &[int8()]);
}

#[test]
fn goal_limit_and_offset_params() {
    // Both LIMIT and OFFSET as params.
    let db = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT $p1 OFFSET $p2";
    assert_param_types(&db, sql, &[int8(), int8()]);
}

// ── LIMIT with expression containing param ────────────────────────────

#[test]
fn goal_limit_param_plus_literal() {
    // LIMIT $p1 + 1  →  operator +(unknown, int4) resolves $p1 as int4.
    // The overall result int4 is coerced to int8 (assignment cast exists).
    let db = setup();
    let sql = "SELECT id FROM users LIMIT $p1 + 1";
    assert_param_types(&db, sql, &[int4()]);
}

#[test]
fn goal_limit_function_of_param() {
    // LIMIT length($p1)  →  $p1 must be text (from length(text) signature).
    // length() returns int4, which is coerced to int8 for LIMIT.
    // This is the key test: $p1 should NOT be int8.
    let db = setup();
    let sql = "SELECT id FROM users LIMIT length($p1)";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_limit_cast_param() {
    // LIMIT $p1::int4  →  $p1 receives int4 from the cast target.
    let db = setup();
    let sql = "SELECT id FROM users LIMIT $p1::int4";
    assert_param_types(&db, sql, &[int4()]);
}

// ── WHERE with bare param ─────────────────────────────────────────────

#[test]
fn goal_where_bool_param() {
    // WHERE $p1  →  $p1 must be bool (PG's coerce_to_boolean with COERCION_ASSIGNMENT).
    let db = setup();
    let sql = "SELECT id FROM users WHERE $p1";
    assert_param_types(&db, sql, &[bool_ty()]);
}

#[test]
fn goal_where_comparison_infers_column_type() {
    // WHERE id = $p1  →  $p1 gets type of id (int8).
    // WHERE name = $p2  →  $p2 gets type of name (text).
    // WHERE age > $p3  →  $p3 gets type of age (int4).
    let db = setup();
    let sql = "SELECT id FROM users WHERE id = $p1 AND name = $p2 AND age > $p3";
    assert_param_types(&db, sql, &[int8(), text(), int4()]);
}

#[test]
fn goal_where_function_of_param() {
    // WHERE length($p1) > 5  →  $p1 gets text from length(text), not bool from WHERE.
    let db = setup();
    let sql = "SELECT id FROM users WHERE length($p1) > 5";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_where_and_or_bool_propagation() {
    // WHERE $p1 AND $p2 OR $p3  →  all must be bool.
    let db = setup();
    let sql = "SELECT id FROM users WHERE $p1 AND $p2 OR $p3";
    assert_param_types(&db, sql, &[bool_ty(), bool_ty(), bool_ty()]);
}

// ── INSERT VALUES with param ──────────────────────────────────────────

#[test]
fn goal_insert_values_params() {
    // INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3)
    // $p1 → text (name), $p2 → text (email), $p3 → int4 (age).
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3)";
    assert_param_types(&db, sql, &[text(), text(), int4()]);
}

#[test]
fn goal_insert_values_expression() {
    // INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3 + 1)
    // $p3 is inferred from operator +(unknown, int4) as int4.
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3 + 1)";
    assert_param_types(&db, sql, &[text(), text(), int4()]);
}

#[test]
fn goal_insert_nullable_inference() {
    // age is nullable → $p3 should be nullable.
    // name/email are NOT NULL → $p1, $p2 should not be nullable.
    let db = setup();
    let sql = "INSERT INTO users (name, email, age) VALUES ($p1, $p2, $p3)";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name is NOT NULL");
    assert!(!info.params[1].nullable, "email is NOT NULL");
    assert!(info.params[2].nullable, "age is nullable");
}

// ── UPDATE SET with param ─────────────────────────────────────────────

#[test]
fn goal_update_set_params() {
    // UPDATE users SET name = $p1, age = $p2 WHERE id = $p3
    // $p1 → text, $p2 → int4, $p3 → int8.
    let db = setup();
    let sql = "UPDATE users SET name = $p1, age = $p2 WHERE id = $p3";
    assert_param_types(&db, sql, &[text(), int4(), int8()]);
}

#[test]
fn goal_update_set_nullable_inference() {
    // age is nullable → $p2 should be nullable.
    let db = setup();
    let sql = "UPDATE users SET name = $p1, age = $p2 WHERE id = $p3";
    let info = db.analyze(sql).unwrap();
    assert!(!info.params[0].nullable, "name is NOT NULL");
    assert!(info.params[1].nullable, "age is nullable");
    assert!(!info.params[2].nullable, "WHERE id = $p3, not nullable");
}

// ── TypeCast propagation ──────────────────────────────────────────────

#[test]
fn goal_cast_types_param() {
    // $p1::text → $p1 gets text.
    // $p2::int4 → $p2 gets int4.
    let db = setup();
    let sql = "SELECT $p1::text, $p2::int4";
    assert_param_types(&db, sql, &[text(), int4()]);
}

// ── COALESCE param inference ──────────────────────────────────────────

#[test]
fn goal_coalesce_param_with_typed_arg() {
    // COALESCE(age, $p1) → common type is int4, $p1 gets int4.
    let db = setup();
    let sql = "SELECT COALESCE(age, $p1) FROM users";
    assert_param_types(&db, sql, &[int4()]);
}

#[test]
fn goal_coalesce_all_params() {
    // COALESCE($p1, $p2) → both unknown → resolve as text (PG chapter 10.5 rule 3).
    let db = setup();
    let sql = "SELECT COALESCE($p1, $p2)";
    assert_param_types(&db, sql, &[text(), text()]);
}

// ── CASE param inference ──────────────────────────────────────────────

#[test]
fn goal_case_result_param() {
    // CASE WHEN true THEN age ELSE $p1 END → $p1 gets int4 (common type with age).
    let db = setup();
    let sql = "SELECT CASE WHEN true THEN age ELSE $p1 END FROM users";
    assert_param_types(&db, sql, &[int4()]);
}

#[test]
fn goal_case_condition_bool() {
    // CASE WHEN $p1 THEN 1 ELSE 0 END → $p1 must be bool.
    let db = setup();
    let sql = "SELECT CASE WHEN $p1 THEN 1 ELSE 0 END";
    assert_param_types(&db, sql, &[bool_ty()]);
}

// ── Mixed contexts: param used in multiple clauses ────────────────────

#[test]
fn goal_param_in_where_and_limit() {
    // $p1 used in WHERE (via comparison with int8 id), $p2 in LIMIT.
    let db = setup();
    let sql = "SELECT id FROM users WHERE id > $p1 LIMIT $p2";
    assert_param_types(&db, sql, &[int8(), int8()]);
}

#[test]
fn goal_param_insert_with_returning() {
    // Full INSERT ... RETURNING with various param contexts.
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id, title";
    assert_param_types(&db, sql, &[int8(), text(), text()]);
}

// ── Operator two-pass: concrete on one side ───────────────────────────

#[test]
fn goal_operator_param_equals_column() {
    // $p1 = name → $p1 gets text.
    let db = setup();
    let sql = "SELECT id FROM users WHERE $p1 = name";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_operator_param_both_sides() {
    // $p1 > $p2 with no context — PG defaults both to text.
    let db = setup();
    assert_param_types(&db, "SELECT $p1 > $p2", &[text(), text()]);
}

// ── Nested function/operator param inference ──────────────────────────

#[test]
fn goal_nested_function_in_where() {
    // WHERE upper($p1) = name → $p1 gets text from upper(text).
    let db = setup();
    let sql = "SELECT id FROM users WHERE upper($p1) = name";
    assert_param_types(&db, sql, &[text()]);
}

#[test]
fn goal_concat_param_in_where() {
    // WHERE name = $p1 || $p2 → both get text from || operator.
    let db = setup();
    let sql = "SELECT id FROM users WHERE name = $p1 || $p2";
    assert_param_types(&db, sql, &[text(), text()]);
}

// ── DELETE with params ────────────────────────────────────────────────

#[test]
fn goal_delete_where_param() {
    let db = setup();
    let sql = "DELETE FROM users WHERE id = $p1 RETURNING name";
    assert_param_types(&db, sql, &[int8()]);
}

// ── Multiple value rows in INSERT ─────────────────────────────────────

#[test]
fn goal_insert_multiple_value_rows() {
    // INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4)
    let db = setup();
    let sql = "INSERT INTO users (name, email) VALUES ($p1, $p2), ($p3, $p4)";
    assert_param_types(&db, sql, &[text(), text(), text(), text()]);
}

// ── Subquery param inference ──────────────────────────────────────────

#[test]
fn goal_subquery_param_in_where() {
    // WHERE id = (SELECT user_id FROM posts WHERE title = $p1 LIMIT 1)
    // $p1 gets text from title column.
    let db = setup();
    let sql =
        "SELECT name FROM users WHERE id = (SELECT user_id FROM posts WHERE title = $p1 LIMIT 1)";
    assert_param_types(&db, sql, &[text()]);
}

// ── Numeric promotion in operator context ─────────────────────────────

#[test]
fn goal_numeric_promotion_int4_with_int8() {
    // WHERE id > $p1 → id is int8, $p1 gets int8.
    // WHERE age > $p2 → age is int4, $p2 gets int4.
    let db = setup();
    let sql = "SELECT id FROM users WHERE id > $p1 AND age > $p2";
    assert_param_types(&db, sql, &[int8(), int4()]);
}

// ── Literal LIMIT/OFFSET (no params, just ensure no error) ───────────

#[test]
fn goal_limit_offset_literals_no_error() {
    let db = setup();
    let sql = "SELECT id FROM users ORDER BY id LIMIT 10 OFFSET 5";
    let info = db.analyze(sql).unwrap();
    assert!(!info.columns.is_empty(), "should have columns");
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: preferred-type tie-break when resolving functions with UNKNOWN args
// ──────────────────────────────────────────────────────────────────────────────
//
// When a call like `fn($param)` has an untyped parameter, PG §10.3 step 4e
// disambiguates overloads by:
//   1. Keeping candidates whose parameter at the UNKNOWN position is in the
//      string category ('S') — untyped literals are assumed to be strings.
//   2. Among those, preferring candidates whose parameter has
//      `typispreferred = true` (e.g. `text` over `bpchar`/`varchar`).
//
// Without this logic the analyzer would depend on the order of overloads in
// the `Vec` — which can shift when pg_proc is re-exported, extensions are
// installed, or the user defines functions in a different order. Each test
// below is constructed to fail if the tie-break is removed: they either
// register overloads in an order that favors the wrong candidate, or assert
// on `pg_type` to distinguish types that share the same Rust mapping.

#[test]
fn preferred_type_text_wins_over_bytea_in_reverse_registration_order() {
    // bytea is category 'U' and text is the preferred type of category 'S'.
    // Registering bytea first puts it at Vec[0]; without the tie-break,
    // `pick($p)` would choose bytea and `$p` would become `Vec<u8>`.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION public.pick(bytea) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(text) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    assert_single_param_ty(&db, "SELECT pick($p)", text());
}

#[test]
fn preferred_type_text_wins_over_bpchar_in_reverse_registration_order() {
    // Both `bpchar` and `text` are in category 'S', but only `text` has
    // `typispreferred = true`. Registering bpchar first puts it at Vec[0];
    // without the tie-break we'd return bpchar (OID 1042) — even though
    // both map to `String` in Rust, the chosen PG OID differs and drives
    // downstream behavior (e.g. generated `::text` casts).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION public.pick(bpchar) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(text) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    assert_single_param_ty(&db, "SELECT pick($p)", text());
}

#[test]
fn preferred_type_text_wins_over_varchar_in_reverse_registration_order() {
    // `varchar` and `text` share category 'S' and Rust type `String`; only
    // `text` is preferred. Without the tie-break, varchar would win since
    // it is registered first.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION public.pick(varchar) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(text) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    assert_single_param_ty(&db, "SELECT pick($p)", text());
}

#[test]
fn preferred_type_ignores_non_string_category_when_only_non_preferred_in_string() {
    // When bytea and bpchar are the only candidates, bpchar wins because
    // it is the only string-category (even though it is NOT preferred).
    // This exercises step 1 of the tie-break (category filter) independent
    // of step 2 (preferred-type filter).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION public.pick(bytea) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;
         CREATE FUNCTION public.pick(bpchar) RETURNS int
             AS 'SELECT 1' LANGUAGE sql;",
    )
    .unwrap();
    // With bytea at Vec[0] and no string-category filtering, a naive pick
    // would return bytea. The filter keeps only bpchar.
    assert_single_param_ty(&db, "SELECT pick($p)", bpchar());
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: inferência de tipo em ANY/ALL($param)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn param_as_array_in_any_rhs() {
    // id = ANY($ids): $ids deve ser inferido como int8[]
    let db = setup();
    let info = db
        .analyze("SELECT * FROM users WHERE id = ANY($ids)")
        .unwrap();
    assert_params(&info, vec![p(array_of(int8()))]);
}

#[test]
fn param_as_array_in_all_rhs() {
    // id = ALL($ids): mesmo comportamento que ANY
    let db = setup();
    let info = db
        .analyze("SELECT * FROM users WHERE id = ALL($ids)")
        .unwrap();
    assert_params(&info, vec![p(array_of(int8()))]);
}

#[test]
fn param_as_element_in_any_lhs() {
    // $tag = ANY(tags): $tag deve ser inferido como text (elemento do array)
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE items (id BIGINT PRIMARY KEY, tags TEXT[] NOT NULL)")
        .unwrap();
    let info = db
        .analyze("SELECT * FROM items WHERE $tag = ANY(tags)")
        .unwrap();
    assert_params(&info, vec![p(text())]);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests: mixed param types across the query (migrated from identical/nullability)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn params_all_types() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE name = $p1 AND age = $p2 AND id > $p3 AND created_at > $p4",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(int4()), p(int8()), p(timestamptz())]);
}

#[test]
fn params_with_cast() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1::text AND age > $p2::int4")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(int4())]);
}

#[test]
fn complex_multiple_params_from_different_contexts() {
    let db = setup();
    // $p1 from WHERE, $p2 via comparison, $p3 from another comparison.
    let sql = "SELECT id, name FROM users WHERE id = $p1 AND age > $p2 AND email = $p3";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params.len(), 3);
    assert_eq!(info.params[0].pg_type, int8());
    assert_eq!(info.params[1].pg_type, int4());
    assert_eq!(info.params[2].pg_type, text());
}

#[test]
fn stress_param_from_insert_values() {
    let db = setup();
    let sql = "INSERT INTO posts (user_id, title, body) VALUES ($p1, $p2, $p3) RETURNING id";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params.len(), 3);
    assert_eq!(info.params[0].pg_type, int8());
    assert_eq!(info.params[1].pg_type, text());
    assert_eq!(info.params[2].pg_type, text());
}

#[test]
fn stress_param_with_cast() {
    let db = setup();
    let sql = "SELECT id FROM users WHERE id = $p1::bigint";
    let info = db.analyze(sql).unwrap();
    assert_eq!(info.params[0].pg_type, int8());
}

#[test]
fn torture_param_in_coalesce() {
    let db = setup();
    let sql = "SELECT COALESCE(age, $p1) as val FROM users";
    let info = db.analyze(sql).unwrap();
    // $p1 is NOT NULL by default → COALESCE has a NOT NULL arg → NOT NULL.
    assert!(!col(&info, "val").nullable);
}

#[test]
fn param_pinned_by_values_list_common_type() {
    // A `$param` cell in a derived VALUES table adopts the column's common
    // type resolved from its concrete sibling rows — PG's Describe reports
    // `(VALUES (42), ($1))` with $1 as int4, not text. Found by the
    // differential fuzzer (the VALUES reconciliation skipped the back-fill).
    let db = setup();
    let info = db
        .analyze("SELECT a0 FROM (VALUES (42), ($p1)) AS v(a0)")
        .unwrap();
    assert_cols(&info, vec![c("a0", int4())]);
    assert_params(&info, vec![p(int4())]);

    let info = db
        .analyze("SELECT a0 FROM (VALUES (3.14), ($p1)) AS v(a0)")
        .unwrap();
    assert_params(&info, vec![p(numeric())]);
}

#[test]
fn values_list_literal_content_validated_under_common_type() {
    // The same back-fill validates string-literal contents: the second
    // row's 'x' must parse as the column's int4 common type.
    let db = setup();
    let err = db
        .analyze("SELECT a0 FROM (VALUES (42), ('x')) AS v(a0)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("invalid input syntax for type integer: \"x\""),
        "got: {err}"
    );
}

// ── Param inference in operator / set-op / null-test contexts ───────────────

#[test]
fn param_in_json_operator_gets_declared_arg_type() {
    // `prefs -> $p1` resolves `jsonb -> text` (string-category rule); the
    // param adopts the operator's *declared* right type. The old behavior
    // pre-pinned the param to the left side's type (jsonb) and then failed
    // resolution entirely.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE j (id BIGINT PRIMARY KEY, prefs JSONB);")
        .unwrap();
    let s = db.analyze("SELECT prefs -> $p1 FROM j").unwrap();
    assert_params(&s, vec![p(text())]);
    let s = db.analyze("SELECT prefs #> $p1 FROM j").unwrap();
    assert_params(&s, vec![p(array_of(text()))]);
}

#[test]
fn param_in_set_op_branch_adopts_peer_type() {
    let db = setup();
    let s = db
        .analyze("SELECT $p1 UNION ALL SELECT age FROM users")
        .unwrap();
    assert_params(&s, vec![p(int4())]);
    let s = db
        .analyze("SELECT age FROM users UNION ALL SELECT $p1")
        .unwrap();
    assert_params(&s, vec![p(int4())]);
}

#[test]
fn bare_param_in_null_test_is_indeterminate() {
    // IS [NOT] NULL accepts any type and pins nothing — PG rejects with
    // `could not determine data type of parameter $1`.
    let db = setup();
    let err = db.analyze("SELECT $p1 IS NULL").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("could not determine data type of parameter $1"),
        "got: {err}"
    );
    // PG locks the type at first use: a later concrete use does NOT fix it…
    let err = db.analyze("SELECT $p1 IS NULL, $p1 = 1").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("could not determine data type of parameter $1"),
        "got: {err}"
    );
    // …but a use typed *before* the null test is fine.
    db.analyze("SELECT $p1 = 1, $p1 IS NULL").unwrap();
}

#[test]
fn ambiguous_unknown_operand_is_not_unique() {
    // `date + unknown` keeps date+int4 / date+interval / date+time alive
    // through every tiebreak — PG: `operator is not unique` (42725).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE d (id BIGINT PRIMARY KEY, bday DATE);")
        .unwrap();
    let err = db.analyze("SELECT bday + $p1 FROM d").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("operator is not unique: date + unknown"),
        "got: {err}"
    );
}

#[test]
fn concat_with_param_resolves_homogeneous_overload() {
    // `text || $1` must pick `text || text` (param = text), not the
    // polymorphic `text || anynonarray`.
    let db = setup();
    let s = db.analyze("SELECT name || $p1 FROM users").unwrap();
    assert_params(&s, vec![p(text())]);
}

#[test]
fn nullif_and_array_default_column_names() {
    let db = setup();
    let s = db.analyze("SELECT NULLIF($p1, age) FROM users").unwrap();
    assert_eq!(s.columns[0].name, "nullif");
    let s = db.analyze("SELECT ARRAY[age, $p1] FROM users").unwrap();
    assert_eq!(s.columns[0].name, "array");
}

#[test]
fn param_in_distinct_on_is_registered_and_typed() {
    // DISTINCT ON expressions are walked like ORDER BY items — a `$N` seen
    // only there used to die on the param-count invariant.
    let db = setup();
    let s = db
        .analyze("SELECT DISTINCT ON (id = $p1) name FROM users")
        .unwrap();
    assert_params(&s, vec![p(int8())]);
    // Select-list aliases stay referencable, like ORDER BY.
    db.analyze("SELECT id AS x FROM users ORDER BY x").unwrap();
}

#[test]
fn array_concat_param_adopts_array_type() {
    // `tags || $1` resolves the polymorphic anycompatiblearray ||
    // anycompatiblearray (most specific homogeneous match), so PG's
    // Describe — and the analyzer — type the param as text[].
    let db = setup();
    let mut db2 = PgCatalog::new().unwrap();
    db2.apply_sql("CREATE TABLE a (id BIGINT PRIMARY KEY, tags TEXT[], nums INT[] NOT NULL);")
        .unwrap();
    let _ = db;
    let s = db2.analyze("SELECT tags || $p1 FROM a").unwrap();
    assert_params(&s, vec![p(array_of(text()))]);
    let s = db2.analyze("SELECT $p1 || nums FROM a").unwrap();
    assert_params(&s, vec![p(array_of(int4()))]);
}

#[test]
fn any_param_against_domain_column_pins_base_array() {
    // Operators resolve over the domain's base type — PG describes
    // `addr = ANY($1)` (addr being a text domain) as text[], not email[].
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE DOMAIN email AS TEXT; CREATE TABLE t3 (addr email);")
        .unwrap();
    let s = db
        .analyze("SELECT 1 FROM t3 WHERE addr = ANY($p1)")
        .unwrap();
    assert_params(&s, vec![p(array_of(text()))]);
}

#[test]
fn bare_param_in_variadic_any_is_indeterminate() {
    // `concat` takes VARIADIC "any" — PG cannot infer a bare $N there and
    // fails prepare with `could not determine data type of parameter $1`.
    let db = setup();
    let err = db
        .analyze("SELECT concat(name, $p1) FROM users")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("could not determine data type of parameter $1"),
        "got: {err}"
    );
    // A pinned param is fine.
    db.analyze("SELECT concat(name, $p1::text) FROM users")
        .unwrap();
}

#[test]
fn rows_frame_offset_param_is_bigint() {
    // ROWS/GROUPS frame offsets are int8 in PG; a bare $N there must
    // describe as bigint, not default to text.
    let db = setup();
    let s = db
        .analyze(
            "SELECT sum(age) OVER (ORDER BY id ROWS BETWEEN $p1 PRECEDING AND CURRENT ROW) \
             FROM users",
        )
        .unwrap();
    assert_params(&s, vec![p(int8())]);
}

#[test]
fn any_param_against_array_column_rejected_like_pg() {
    // `nums = ANY($1)` would need an array-of-array to type the param —
    // PG fails the lookup at prepare time.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t4 (nums INT[] NOT NULL);")
        .unwrap();
    let err = db
        .analyze("SELECT 1 FROM t4 WHERE nums = ANY($p1)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("could not find array type for data type integer[]"),
        "got: {err}"
    );
}

#[test]
fn escape_string_does_not_swallow_params() {
    // `E'it\'s'` is one string (the backslash escapes the quote); the
    // param after it must still be lexed and typed. The lexer used to
    // treat `\'` as the string end and eat `$p1` inside a phantom string.
    let db = setup();
    let s = db.analyze("SELECT E'it\\'s' || $p1 FROM users").unwrap();
    assert_params(&s, vec![p(text())]);
}

// ── BETWEEN / IN / ARRAY[…]::T[] context typing (transformAExprBetween,
//    transformAExprIn, transformArrayExpr) ────────────────────────────────

#[test]
fn param_typed_by_between_in_list_and_array_cast() {
    // Parameter types as reported by PG 18's pg_prepared_statements.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (n int NOT NULL, s text NOT NULL, ni int);")
        .unwrap();
    for (sql, expected) in [
        ("SELECT $p BETWEEN 1 AND 5 AS a", vec![int4()]),
        ("SELECT $p IN (1, 2) AS a", vec![int4()]),
        ("SELECT $p IN (n, 2) AS a FROM t", vec![int4()]),
        ("SELECT $p NOT IN (n, ni) AS a FROM t", vec![int4()]),
        ("SELECT ARRAY[$p]::int[] AS a", vec![int4()]),
        ("SELECT ARRAY[$p, $q]::int8[] AS a", vec![int8(), int8()]),
        ("SELECT ARRAY[ARRAY[$p]]::int[] AS a", vec![int4()]),
        ("SELECT $p IN (1, 2.5) AS a", vec![numeric()]),
        ("SELECT $p BETWEEN SYMMETRIC 1 AND 2.5 AS a", vec![int4()]),
        (
            "SELECT n BETWEEN $a AND $b AS a FROM t",
            vec![int4(), int4()],
        ),
        ("SELECT n IN ($a, $b) AS a FROM t", vec![int4(), int4()]),
        ("SELECT ARRAY[$p, 1] AS a", vec![int4()]),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let got: Vec<_> = s.params.iter().map(|p| p.pg_type.clone()).collect();
        assert_eq!(got, expected, "{sql}");
    }
    let s = db
        .analyze("SELECT ARRAY[]::int[] AS a, ARRAY[1.5]::int[] AS b, ARRAY[true]::int[] AS c")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", array_of(int4())),
            c("b", array_of(int4())),
            c("c", array_of(int4())),
        ],
    );
}

#[test]
fn between_and_in_list_errors_come_from_the_rewritten_comparisons() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (n int NOT NULL, s text NOT NULL);")
        .unwrap();
    assert_err_prefix!(
        db.analyze("SELECT s BETWEEN 1 AND 2 FROM t"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: text >= integer"
    );
    assert_err_prefix!(
        db.analyze("SELECT s IN (1, 2) FROM t"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: text = integer"
    );
    assert_err_prefix!(
        db.analyze("SELECT n IN ('a', 'b') FROM t"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"a\""
    );
}

#[test]
fn array_constructor_type_errors_match_pg() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (n int NOT NULL, s text NOT NULL);")
        .unwrap();
    assert_err_prefix!(
        db.analyze("SELECT ARRAY[]"),
        AnalyzeError::IndeterminateType(_),
        "cannot determine type of empty array"
    );
    assert_err_prefix!(
        db.analyze("SELECT ARRAY[ARRAY[1], ARRAY['2']]"),
        AnalyzeError::Invalid(_),
        "ARRAY could not convert type text[] to integer[]"
    );
    assert_err_prefix!(
        db.analyze("SELECT ARRAY[n, s] FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "ARRAY types integer and text cannot be matched"
    );
    assert_err_prefix!(
        db.analyze("SELECT ARRAY[1, 'x']"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"x\""
    );
}
