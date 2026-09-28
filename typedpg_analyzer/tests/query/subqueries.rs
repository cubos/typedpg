//! Subqueries: in FROM, scalar, IN / NOT IN, EXISTS / NOT EXISTS,
//! ARRAY(SELECT …).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            name  TEXT NOT NULL,
            age   INT
         );
         CREATE TABLE posts (
            id           BIGINT PRIMARY KEY,
            user_id      BIGINT NOT NULL,
            title        TEXT NOT NULL,
            body         TEXT,
            published_at TIMESTAMPTZ
         );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL,
            rating      INT
         );",
    )
    .unwrap();
    db
}

// ── Subquery in FROM ─────────────────────────────────────────────────────────

#[test]
fn subquery_in_from() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT sub.name, sub.age \
             FROM (SELECT name, age FROM users WHERE age IS NOT NULL) sub",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), cn("age", int4())]);
}

#[test]
fn complex_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sub.user_name, sub.post_count \
               FROM ( \
                   SELECT u.name as user_name, COUNT(*) as post_count \
                   FROM users u \
                   INNER JOIN posts p ON p.user_id = u.id \
                   GROUP BY u.name \
               ) sub";
    let info = db.analyze(sql).unwrap();
    // user_name from NOT NULL column → NOT NULL through subquery.
    assert!(!col(&info, "user_name").nullable);
    // post_count is COUNT(*) → NOT NULL.
    assert!(!col(&info, "post_count").nullable);
}

// ── IN subquery ──────────────────────────────────────────────────────────────

#[test]
fn in_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users \
             WHERE id IN (SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn in_subquery_with_wrong_arity_rejected() {
    let db = setup();
    // PG: `subquery has too many columns`. A single-column LHS can't match
    // a multi-column subquery.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE id IN (SELECT id, name FROM users)"),
        AnalyzeError::Invalid(_),
        "subquery has too many columns (subquery has 2, lhs has 1)",
    );
}

#[test]
fn not_in_subquery() {
    let db = setup();
    // NOT IN is a semi-anti-join — doesn't affect the outer row shape.
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE id NOT IN (SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── ANY / ALL (subquery) ─────────────────────────────────────────────────────

#[test]
fn any_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE id = ANY(SELECT user_id FROM posts)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn all_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM users \
             WHERE age < ALL(SELECT rating FROM comments)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Correlated scalar subquery ───────────────────────────────────────────────

#[test]
fn correlated_scalar_subquery_in_select_list() {
    let db = setup();
    // Inner subquery references outer `t.id` — the analyzer must thread the
    // outer scope into the subselect.
    let s = db
        .analyze(
            "SELECT id, (SELECT title FROM posts p WHERE p.user_id = u.id LIMIT 1) AS first_title \
             FROM users u",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("first_title", text())]);
}

// ── EXISTS with SELECT * ─────────────────────────────────────────────────────

#[test]
fn exists_with_select_star_accepts() {
    let db = setup();
    // EXISTS ignores the projected columns, so `SELECT *` inside is fine.
    let s = db
        .analyze("SELECT id FROM users WHERE EXISTS(SELECT * FROM posts)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── NOT EXISTS ───────────────────────────────────────────────────────────────

#[test]
fn not_exists_subquery() {
    let db = setup();
    // NOT EXISTS, like EXISTS, returns a definite bool.
    let s = db
        .analyze(
            "SELECT u.name, \
                    NOT EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id) AS orphan \
             FROM users u",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("orphan", bool_ty())]);
}

// ── EXISTS subquery ──────────────────────────────────────────────────────────

#[test]
fn exists_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id, name FROM users u \
             WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn complex_exists_subquery() {
    let db = setup();
    let sql = "SELECT u.name, EXISTS(SELECT 1 FROM posts p WHERE p.user_id = u.id) as has_posts \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // EXISTS always returns bool, never NULL.
    assert_cols(&info, vec![c("name", text()), c("has_posts", bool_ty())]);
}

// ── Scalar subqueries (always nullable unless aggregate without GROUP BY) ────

#[test]
fn complex_scalar_subquery_always_nullable() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_post \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Scalar subquery is always nullable (zero rows → NULL).
    assert!(col(&info, "first_post").nullable);
}

#[test]
fn subquery_count_star_not_null() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Aggregate without GROUP BY → guaranteed 1 row, COUNT is NOT NULL.
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_plus_one_not_null() {
    let db = setup();
    // COUNT(*) + 1 wraps the aggregate in an AExpr — must still detect it.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) + 1 FROM posts p WHERE p.user_id = u.id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_count_cast_not_null() {
    let db = setup();
    // COUNT(*)::int wraps aggregate in TypeCast.
    let sql = "SELECT (SELECT COUNT(*)::int FROM posts) as cnt FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

#[test]
fn subquery_coalesce_sum_not_null() {
    let db = setup();
    // COALESCE(SUM(rating), 0) — aggregate detected through COALESCE.
    // SUM is nullable (empty group), but COALESCE with literal → NOT NULL.
    // Also: aggregate without GROUP BY → guaranteed 1 row.
    let sql = "SELECT (SELECT COALESCE(SUM(rating), 0) FROM comments) as total";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "total").nullable);
}

#[test]
fn subquery_sum_nullable() {
    let db = setup();
    // SUM without COALESCE: aggregate != COUNT → nullable result.
    // Even though guaranteed 1 row, SUM itself returns NULL for empty input.
    let sql = "SELECT (SELECT SUM(rating) FROM comments) as total";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "total").nullable);
}

#[test]
fn subquery_with_group_by_still_nullable() {
    let db = setup();
    // COUNT(*) with GROUP BY: subquery may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id GROUP BY p.user_id) as cnt \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "cnt").nullable);
}

#[test]
fn subquery_non_aggregate_still_nullable() {
    let db = setup();
    // Non-aggregate scalar subquery: may return 0 rows → nullable.
    let sql = "SELECT u.name, \
                      (SELECT p.title FROM posts p WHERE p.user_id = u.id LIMIT 1) as first_title \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    assert!(col(&info, "first_title").nullable);
}

#[test]
fn subquery_case_wrapping_count_not_null() {
    let db = setup();
    // CASE WHEN ... THEN COUNT(*) ELSE 0 END — aggregate inside CASE with ELSE.
    let sql = "SELECT (SELECT CASE WHEN true THEN COUNT(*) ELSE 0 END FROM posts) as cnt";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_deeply_nested_subquery() {
    let db = setup();
    let sql = "SELECT * FROM ( \
                   SELECT * FROM ( \
                       SELECT id, name, age FROM users \
                   ) inner_sq \
               ) outer_sq";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn stress_subquery_with_left_join_inside() {
    let db = setup();
    // Subquery does LEFT JOIN, outer SELECT sees nullable cols.
    let sql = "SELECT sq.name, sq.title FROM ( \
                   SELECT u.name, p.title \
                   FROM users u \
                   LEFT JOIN posts p ON p.user_id = u.id \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // title is nullable because of LEFT JOIN inside subquery; name stays NOT NULL.
    assert_cols(&info, vec![c("name", text()), cn("title", text())]);
}

#[test]
fn stress_subquery_computed_columns() {
    let db = setup();
    let sql = "SELECT sq.cnt, sq.max_age FROM ( \
                   SELECT COUNT(*) as cnt, MAX(age) as max_age FROM users \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // COUNT is NOT NULL, MAX is nullable.
    assert_cols(&info, vec![c("cnt", int8()), cn("max_age", int4())]);
}

#[test]
fn stress_aggregate_subquery_in_select() {
    let db = setup();
    let sql = "SELECT u.name, \
                      (SELECT COUNT(*) FROM posts p WHERE p.user_id = u.id) as post_count \
               FROM users u";
    let info = db.analyze(sql).unwrap();
    // Aggregate without GROUP BY → exactly 1 row, and COUNT is NOT NULL →
    // scalar subquery result is NOT NULL.
    assert_cols(&info, vec![c("name", text()), c("post_count", int8())]);
}

// ── Torture ──────────────────────────────────────────────────────────────────

// ── Multi-column IN (a, b) IN (SELECT …) ─────────────────────────────────────

#[test]
fn multi_column_in_subquery() {
    let db = setup();
    // PG: `(user_id, post_id) IN (SELECT a, b FROM …)` — the LHS row must
    // align with the subquery's column count, and the analyzer must not
    // collapse it to a single-column comparison.
    let s = db
        .analyze(
            "SELECT id FROM comments \
             WHERE (post_id, author_name) IN ( \
                 SELECT id, title FROM posts \
             )",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn multi_column_in_subquery_arity_mismatch_rejected() {
    let db = setup();
    // PG: `subquery has too few columns`. LHS has 2, subquery emits 1.
    assert_analyze_err!(
        db.analyze(
            "SELECT id FROM comments \
             WHERE (post_id, author_name) IN (SELECT id FROM posts)"
        ),
        AnalyzeError::Invalid(_),
        "subquery has too few columns (subquery has 1, lhs has 2)",
    );
}

#[test]
fn multi_column_not_in_subquery() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT id FROM comments \
             WHERE (post_id, author_name) NOT IN ( \
                 SELECT id, title FROM posts \
             )",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn torture_union_in_subquery_in_from() {
    let db = setup();
    let sql = "SELECT sq.val FROM ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) sq";
    let info = db.analyze(sql).unwrap();
    // Both NOT NULL → union NOT NULL → subquery NOT NULL.
    assert_cols(&info, vec![c("val", text())]);
}

// ── Implicit column name of a scalar subquery (PG FigureColname) ─────────────

#[test]
fn scalar_subquery_named_after_its_output_column() {
    // PG names an unaliased `(SELECT count(*) …)` after the subquery's single
    // output column (`count`), not `?column?`.
    let db = setup();
    let s = db.analyze("SELECT (SELECT count(*) FROM users)").unwrap();
    assert_eq!(col(&s, "count").name, "count");
}

#[test]
fn scalar_subquery_without_named_output_is_question_column() {
    // An unnamed output expression leaves the subquery as `?column?`.
    let db = setup();
    let s = db
        .analyze("SELECT (SELECT id + 1 FROM users LIMIT 1)")
        .unwrap();
    assert_eq!(s.columns[0].name, "?column?");
}

// ── IN/ANY/ALL subquery: comparison operator must resolve ────────────────────

#[test]
fn in_subquery_with_incompatible_types_rejected() {
    // `bigint IN (SELECT text …)` has no `=` operator. PG resolves the IN
    // comparison the same way as a plain `a = b` and rejects it.
    // PG: `operator does not exist: bigint = text`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM posts WHERE user_id IN (SELECT title FROM posts)"),
        AnalyzeError::UndefinedOperator(_),
        "operator does not exist: bigint = text",
    );
}

#[test]
fn in_subquery_with_castable_types_accepted() {
    // `integer IN (SELECT bigint …)` is fine — PG's cross-type `int4 = int8`
    // operator resolves it. Guards against over-rejecting valid IN-subqueries.
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age IN (SELECT id FROM users)")
        .unwrap();
    assert_eq!(col(&s, "id").pg_type, int8());
}

// ── LATERAL scope semantics (resolution precedence, ambiguity, `*`) ─────────

#[test]
fn lateral_star_excludes_lateral_sources() {
    // Inside a LATERAL subquery, `SELECT *` expands only the subquery's own
    // FROM — the laterally-visible outer aliases are reachable by name but
    // are not part of the star.
    let db = setup();
    let s = db
        .analyze("SELECT l.* FROM users, LATERAL (SELECT * FROM posts) AS l")
        .unwrap();
    assert_eq!(
        s.columns.len(),
        db.analyze("SELECT * FROM posts").unwrap().columns.len(),
        "lateral star must expand only the inner FROM"
    );
}

#[test]
fn lateral_inner_from_shadows_lateral_ref() {
    // `id` exists in both the inner FROM (posts) and the lateral outer
    // (users) — PG resolves to the inner one with no ambiguity.
    let db = setup();
    db.analyze("SELECT 1 FROM users, LATERAL (SELECT id + 1 AS q FROM posts) AS l")
        .unwrap();
}

#[test]
fn lateral_two_same_level_sources_are_ambiguous() {
    // Two lateral sources at the same level sharing a column name *are*
    // ambiguous when referenced from the subquery.
    let db = setup();
    let err = db
        .analyze(
            "SELECT 1 FROM users u1, users u2, LATERAL (SELECT name || 'x' AS q FROM posts) AS l",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("column reference \"name\" is ambiguous"),
        "got: {err}"
    );
}

#[test]
fn name_repeated_inside_one_from_entry_is_ambiguous() {
    // PG's scanRTEForColumn: two columns of one entry sharing the name make
    // a reference to it ambiguous, qualified or not; `*` still lists both.
    let db = setup();
    for sql in [
        "SELECT s.a FROM (SELECT 1 a, 2 a) s",
        "SELECT a FROM (SELECT 1 a, 2 a) s",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            matches!(err, AnalyzeError::AmbiguousColumn(_)),
            "{sql}: {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("column reference \"a\" is ambiguous"),
            "{sql}: {err}"
        );
    }
    let s = db.analyze("SELECT * FROM (SELECT 1 a, 2 a) s").unwrap();
    assert_eq!(s.columns.len(), 2);
}
