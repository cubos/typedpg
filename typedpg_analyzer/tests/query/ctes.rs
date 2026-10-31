//! Common Table Expressions (WITH): basic CTEs, multi-CTE, DML in CTE,
//! CTE referenced in joins/unions/subqueries.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name  TEXT NOT NULL,
            email TEXT NOT NULL,
            age   INT
         );
         CREATE TABLE posts (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id BIGINT NOT NULL,
            title   TEXT NOT NULL,
            body    TEXT
         );
         CREATE TABLE comments (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            post_id BIGINT NOT NULL,
            content TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Basic CTEs ───────────────────────────────────────────────────────────────

#[test]
fn cte_simple() {
    let db = setup();
    let s = db
        .analyze(
            "WITH active AS (SELECT id, name FROM users WHERE age > 18) \
             SELECT * FROM active",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn cte_multiple() {
    let db = setup();
    let s = db
        .analyze(
            "WITH \
               u AS (SELECT id, name FROM users), \
               p AS (SELECT user_id, title FROM posts) \
             SELECT u.name, p.title \
             FROM u INNER JOIN p ON p.user_id = u.id",
        )
        .unwrap();
    assert_cols(&s, vec![c("name", text()), c("title", text())]);
}

#[test]
fn cte_with_insert_returning() {
    let db = setup();
    let s = db
        .analyze(
            "WITH new_user AS (\
               INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name\
             ) SELECT * FROM new_user",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

// ── CTE with DML and LEFT JOIN nullability ───────────────────────────────────

#[test]
fn cte_dml_left_join() {
    let db = setup();
    let sql = "WITH ins AS (\
        INSERT INTO posts (user_id, title) VALUES ($p1, $p2) RETURNING id, user_id\
    ) \
    SELECT ins.id, u.name \
    FROM ins \
    LEFT JOIN users u ON u.id = ins.user_id";
    let info = db.analyze(sql).unwrap();
    // LEFT JOIN in DML CTE makes right side nullable.
    assert!(col(&info, "name").nullable);
}

#[test]
fn complex_cte_chain() {
    let db = setup();
    let sql = "WITH \
                   active_users AS (SELECT id, name FROM users), \
                   user_posts AS ( \
                       SELECT au.name, p.title \
                       FROM active_users au \
                       INNER JOIN posts p ON p.user_id = au.id \
                   ) \
               SELECT name, title FROM user_posts";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "name").nullable);
    assert!(!col(&info, "title").nullable);
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_cte_used_in_union() {
    let db = setup();
    let sql = "WITH active AS (SELECT name FROM users) \
               SELECT name FROM active \
               UNION ALL \
               SELECT title as name FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("name", text())]);
}

#[test]
fn stress_cte_insert_returning_coalesce() {
    let db = setup();
    let sql = "WITH ins AS ( \
                   INSERT INTO users (name, email) VALUES ($p1, $p2) \
                   RETURNING id, age \
               ) \
               SELECT id, COALESCE(age, 0) as safe_age FROM ins";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("id", int8()), c("safe_age", int4())]);
}

#[test]
fn stress_cte_dml_chain() {
    let db = setup();
    let sql = "WITH \
                   new_user AS ( \
                       INSERT INTO users (name, email) VALUES ($p1, $p2) RETURNING id, name \
                   ), \
                   new_post AS ( \
                       INSERT INTO posts (user_id, title) \
                       SELECT id, $p3 FROM new_user \
                       RETURNING id as post_id, user_id, title \
                   ) \
               SELECT nu.name, np.title, np.post_id \
               FROM new_user nu \
               INNER JOIN new_post np ON np.user_id = nu.id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("name", text()), c("title", text()), c("post_id", int8())],
    );
}

// ── Torture ──────────────────────────────────────────────────────────────────

#[test]
fn torture_cte_with_union_and_left_join() {
    let db = setup();
    let sql = "WITH all_names AS ( \
                   SELECT name FROM users \
                   UNION ALL \
                   SELECT title as name FROM posts \
               ) \
               SELECT u.id, an.name as other_name \
               FROM users u \
               LEFT JOIN all_names an ON true";
    let info = db.analyze(sql).unwrap();
    // LEFT JOIN on CTE → other_name nullable.
    assert_cols(&info, vec![c("id", int8()), cn("other_name", text())]);
}

#[test]
fn torture_multiple_ctes_cross_reference() {
    let db = setup();
    let sql = "WITH \
                   a AS (SELECT id, name FROM users), \
                   b AS (SELECT a.name, p.title FROM a INNER JOIN posts p ON p.user_id = a.id), \
                   c AS (SELECT b.name, b.title, cm.content \
                         FROM b LEFT JOIN comments cm ON true) \
               SELECT name, title, content FROM c";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![c("name", text()), c("title", text()), cn("content", text())],
    );
}

#[test]
fn torture_select_from_cte_left_join_cte() {
    let db = setup();
    let sql = "WITH \
                   u AS (SELECT id, name FROM users), \
                   p AS (SELECT user_id, title FROM posts) \
               SELECT u.name, p.title \
               FROM u LEFT JOIN p ON p.user_id = u.id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("name", text()), cn("title", text())]);
}

#[test]
fn torture_deeply_nested_cte_union_join() {
    let db = setup();
    // CTE → UNION → subquery → LEFT JOIN.
    let sql = "WITH names AS ( \
                   SELECT name as val FROM users \
                   UNION ALL \
                   SELECT title as val FROM posts \
               ) \
               SELECT n.val, u.age \
               FROM names n \
               LEFT JOIN users u ON u.name = n.val";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("val", text()), cn("age", int4())]);
}

// ── CTEs planned on their own ────────────────────────────────────────────────
//
// A CTE the planner inlines but can't pull up (LIMIT, ORDER BY, GROUP BY,
// ...) is a subquery it plans on its own once it plans the relation the
// reference is — folding its WHERE, and the outputs read — unless the
// reference's level proves that relation empty, or removes it from a join.
// One it doesn't inline (MATERIALIZED, referenced twice) is planned whole
// wherever it is referenced at all.

fn planned_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, x int, s text);
         CREATE TABLE u (id int PRIMARY KEY, x int);",
    )
    .unwrap();
    db
}

#[test]
fn an_inlined_cte_planned_on_its_own_fails_where_its_reference_is_planned() {
    let db = planned_setup();
    for sql in [
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) SELECT 1 FROM c",
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 ORDER BY x) SELECT 1 FROM c",
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 GROUP BY x) SELECT 1 FROM c",
        "WITH c AS NOT MATERIALIZED (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) \
         SELECT 1 FROM c a, c b",
        // A constant qual of several relations is a gating qual.
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) SELECT 1 FROM c, u WHERE false",
        // On a join's nullable side, removed only when it may be proven
        // distinct.
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) SELECT 1 FROM u LEFT JOIN c ON true",
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) \
         SELECT 1 FROM u LEFT JOIN c ON c.x = u.id",
        // Not inlined: planned whole, its unread outputs too.
        "WITH c AS (SELECT 1 / 0 AS x) SELECT 1 FROM c a, c b",
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1) SELECT 1 FROM c a, c b",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
    assert_err_prefix!(
        db.analyze("WITH c AS (SELECT x FROM t WHERE s ~ '(' ORDER BY x) SELECT 1 FROM c"),
        AnalyzeError::Invalid(_),
        "invalid regular expression: parentheses () not balanced"
    );
    assert_err_prefix!(
        db.analyze("WITH c AS (SELECT 40000::int2 AS y, x FROM t LIMIT 1) SELECT y FROM c"),
        AnalyzeError::NumericValueOutOfRange(_),
        "smallint out of range"
    );
    for sql in [
        // A single relation: the FALSE qual proves it empty.
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) SELECT 1 FROM c WHERE false",
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) \
         SELECT 1 FROM c, (SELECT 1) z WHERE false",
        // Removed: distinct on the join's column.
        "WITH c AS (SELECT DISTINCT x FROM t WHERE 1 / 0 = 1) \
         SELECT 1 FROM u LEFT JOIN c ON c.x = u.id",
        // An unread output is never planned.
        "WITH c AS (SELECT 40000::int2 AS y, x FROM t LIMIT 1) SELECT x FROM c",
        // Never referenced.
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) SELECT 1",
        // Folded away around the reference.
        "WITH c AS (SELECT x FROM t WHERE 1 / 0 = 1 LIMIT 1) \
         SELECT CASE WHEN true THEN 1 ELSE (SELECT 1 FROM c) END",
    ] {
        db.analyze(sql)
            .unwrap_or_else(|e| panic!("{sql}: unexpected error {e}"));
    }
}
