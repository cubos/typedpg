//! UNION, UNION ALL, INTERSECT, EXCEPT: column unification, nullability
//! propagation across branches, type coercion of mixed branches.

use crate::common::*;

#[test]
fn union_with_incompatible_concrete_types_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, s TEXT NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT id FROM t UNION SELECT s FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "UNION types bigint and text cannot be matched (column `id`)\n",
            "  help: cast both sides to a common type, e.g. `id::bigint`\n",
        ),
    );
}

#[test]
fn union_with_incompatible_unknown_literal_rejected() {
    // `'text'` is an untyped string literal: PG keeps it `unknown` in a
    // set-operation arm and coerces it to the other arm's integer, which
    // fails in the input function.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT 1 UNION SELECT 'text'"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"text\"",
    );
}

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age  INT
         );
         CREATE TABLE posts (
            id      BIGINT PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            body    TEXT
         );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── UNION / UNION ALL / INTERSECT / EXCEPT type match ────────────────────────

#[test]
fn types_match_union_all() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users WHERE age > 20 \
             UNION ALL \
             SELECT id, name FROM users WHERE age <= 20",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn types_match_union_distinct() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             UNION \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_intersect() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             INTERSECT \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_except() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT name FROM users \
             EXCEPT \
             SELECT title FROM posts",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

#[test]
fn types_match_cte_union() {
    let db = setup();
    let s = db
        .analyze(
            "WITH all_names AS (\
               SELECT name FROM users \
               UNION ALL \
               SELECT author_name AS name FROM comments\
             ) \
             SELECT name FROM all_names",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text())]);
}

// ── Nullability propagation across branches ──────────────────────────────────

#[test]
fn union_nullable_branch() {
    let db = setup();
    // Nullable because body is nullable.
    let sql = "SELECT name as val FROM users UNION ALL SELECT body as val FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("val", text())]);
}

#[test]
fn union_all_not_null() {
    let db = setup();
    let sql = "SELECT name as val FROM users UNION ALL SELECT title as val FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("val", text())]);
}

// ── Stress / complex ─────────────────────────────────────────────────────────

#[test]
fn stress_union_with_null_literal_branch() {
    let db = setup();
    // One branch is a literal NULL → union should be nullable.
    let sql = "SELECT name as val FROM users \
               UNION ALL \
               SELECT NULL as val";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "val").nullable);
}

#[test]
fn stress_union_mixed_types() {
    let db = setup();
    // int + bigint → bigint (coercion).
    let sql = "SELECT age as num FROM users \
               UNION ALL \
               SELECT id as num FROM users";
    let info = db.analyze(sql).unwrap();
    // age is nullable → union is nullable (even though id is NOT NULL).
    assert_cols(&info, vec![cn("num", int8())]);
}

#[test]
fn complex_union_three_branches() {
    let db = setup();
    let sql = "SELECT name as label FROM users \
               UNION ALL \
               SELECT title as label FROM posts \
               UNION ALL \
               SELECT author_name as label FROM comments";
    let info = db.analyze(sql).unwrap();
    // All three NOT NULL → union NOT NULL.
    assert!(!col(&info, "label").nullable);
}

#[test]
fn union_varchar_first_branch_keeps_varchar() {
    // PG's `select_common_type` only switches the running candidate when the
    // implicit cast is one-way; varchar ↔ text casts exist in both
    // directions, so the *first* branch's type wins: varchar UNION text is
    // varchar (and text UNION varchar is text). Found by the differential
    // fuzzer — collapsing every string mix to text diverges from Describe.
    let db = setup();
    let s = db
        .analyze("SELECT name::varchar AS v FROM users UNION SELECT title FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("v", varchar())]);

    let s = db
        .analyze("SELECT name AS v FROM users UNION SELECT title::varchar FROM posts")
        .unwrap();
    assert_cols(&s, vec![c("v", text())]);
}

#[test]
fn set_op_column_count_mismatch_uses_pg_wording() {
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users UNION SELECT id, title FROM posts")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("each UNION query must have the same number of columns"),
        "got: {err}"
    );
    let err = db
        .analyze("SELECT id FROM users INTERSECT SELECT id, title FROM posts")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("each INTERSECT query must have the same number of columns"),
        "got: {err}"
    );
}

/// EXCEPT only emits left rows, and INTERSECT only left rows matched by a
/// right one (NULLs match each other): a column is NULL only when the left
/// arm's is — and for INTERSECT, the right arm's too.
#[test]
fn except_and_intersect_nullability_follows_the_left_arm() {
    let db = setup();
    let s = db
        .analyze("SELECT name AS a, age AS b FROM users EXCEPT SELECT NULL, 1")
        .unwrap();
    assert_cols(&s, vec![c("a", text()), cn("b", int4())]);
    let s = db
        .analyze("SELECT name AS a, age AS b, age AS c FROM users INTERSECT SELECT NULL, 1, age FROM users")
        .unwrap();
    assert_cols(&s, vec![c("a", text()), c("b", int4()), cn("c", int4())]);
    let s = db
        .analyze("SELECT name AS a FROM users UNION SELECT NULL")
        .unwrap();
    assert_cols(&s, vec![cn("a", text())]);
}
