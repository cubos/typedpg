//! Nullability narrowed by the quals a value is read under: what WHERE,
//! HAVING, an inner join's ON, an aggregate's FILTER or a CASE branch's
//! WHEN proves non-NULL (PG's `find_nonnullable_vars`), and the outer joins
//! PG's `reduce_outer_joins` turns into inner ones. Every NOT NULL below is
//! checked against PostgreSQL 18 by the pg_sanity soundness oracle; every
//! nullable one pins a qual that proves nothing (a non-strict operator, an
//! OR whose arms disagree, a negated comparison...).

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE pair AS (a int, b int);
         CREATE TABLE users (
             id int PRIMARY KEY, name text NOT NULL, email text, age int,
             verified_at timestamptz, p pair, tags text[], flag boolean
         );
         CREATE TABLE posts (
             id int PRIMARY KEY, author_id int NOT NULL REFERENCES users,
             title text NOT NULL, body text, status text
         );
         CREATE TABLE labels (post_id int NOT NULL, label text);
         CREATE TABLE ka (k int, x int);
         CREATE TABLE kb (k int, y int);
         CREATE FUNCTION is_pos(int) RETURNS boolean
             LANGUAGE sql STRICT IMMUTABLE AS 'SELECT $1 > 0';
         CREATE FUNCTION is_pos_lax(int) RETURNS boolean
             LANGUAGE sql IMMUTABLE AS 'SELECT coalesce($1, 1) > 0';
         CREATE FUNCTION any_pos(VARIADIC int[]) RETURNS boolean
             LANGUAGE sql STRICT IMMUTABLE AS 'SELECT 0 < ANY($1)';
         CREATE FUNCTION lax_eq(int, int) RETURNS boolean
             LANGUAGE sql IMMUTABLE AS 'SELECT $1 IS NOT DISTINCT FROM $2';
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = lax_eq);",
    )
    .unwrap();
    db
}

/// Each output column's nullability (`true` = nullable).
#[track_caller]
fn assert_nullable(db: &PgCatalog, sql: &str, expected: &[(&str, bool)]) {
    let s = db.analyze(sql).unwrap();
    let actual: Vec<(&str, bool)> = s
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    assert_eq!(actual, expected, "nullability mismatch for `{sql}`");
}

/// The element nullability of array column `name`.
#[track_caller]
fn elements_nullable(db: &PgCatalog, sql: &str, name: &str) -> Option<bool> {
    let s = db.analyze(sql).unwrap();
    match &col(&s, name).pg_type {
        Type::Array {
            element_nullable, ..
        } => *element_nullable,
        other => panic!("column {name} is not an array: {other:?}"),
    }
}

// ── WHERE: what a TRUE qual proves ───────────────────────────────────────────

#[test]
fn is_not_null_narrows_and_is_null_does_not() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT email, age FROM users WHERE email IS NOT NULL",
        &[("email", false), ("age", true)],
    );
    assert_nullable(
        &db,
        "SELECT email FROM users WHERE email IS NULL",
        &[("email", true)],
    );
    // `NOT (x IS NULL)` is `x IS NOT NULL`; `NOT (x IS NOT NULL)` is not.
    assert_nullable(
        &db,
        "SELECT email FROM users WHERE NOT (email IS NULL)",
        &[("email", false)],
    );
    assert_nullable(
        &db,
        "SELECT email FROM users WHERE NOT (email IS NOT NULL)",
        &[("email", true)],
    );
}

#[test]
fn strict_comparisons_narrow_their_operands() {
    let db = setup();
    for qual in [
        "age > 0",
        "age = $1",
        "1 <= age",
        "-age < 0",
        "age BETWEEN 1 AND 5",
        "age NOT BETWEEN 1 AND 5",
        "age BETWEEN SYMMETRIC 5 AND 1",
        "age IN (1, 2, 3)",
        "age NOT IN (1, 2)",
        "age IN (1, id)",
        "age = ANY(ARRAY[1, 2])",
        "age = ANY($1::int[])",
        "age::text = '1'",
        "is_pos(age)",
        "is_pos(age) AND name <> ''",
        "(age > 0) IS TRUE",
        "(age > 0) IS FALSE",
        "(age > 0) IS NOT UNKNOWN",
        "NOT (age > 0)",
        "age > 0 OR age < -1",
        "num_nulls(age) = 0",
        "num_nonnulls(age) > 0",
        "(age > 0 AND email = 'a') OR age < -1",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT age FROM users WHERE {qual}"),
            &[("age", false)],
        );
    }
    for qual in [
        "email LIKE 'a%'",
        "email NOT LIKE 'x%'",
        "email ILIKE 'A%'",
        "email ~ 'a'",
        "email SIMILAR TO 'a%'",
        "lower(email) = 'a'",
        "length(email) > 0",
        "email || name = 'aa'",
        "upper(lower(email)) COLLATE \"C\" = 'A'",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT email FROM users WHERE {qual}"),
            &[("email", false)],
        );
    }
}

#[test]
fn quals_that_hold_for_null_prove_nothing() {
    let db = setup();
    for qual in [
        // Not strict.
        "coalesce(age, 0) > 0",
        "age IS DISTINCT FROM 5",
        "is_pos_lax(age)",
        "age === 1",
        "CASE WHEN age > 0 THEN true ELSE true END",
        "concat(age, '') = '1'",
        "greatest(age, 1) > 0",
        "nullif(age, 0) IS NULL",
        // Arguments packed into a VARIADIC array: the function gets a
        // non-NULL array with NULL elements.
        "any_pos(age, 1)",
        // ALL over an empty array is TRUE whatever the operand.
        "age = ALL($1::int[])",
        // NULL or TRUE / FALSE.
        "(age > 0) IS NOT TRUE",
        "(age > 0) IS NOT FALSE",
        "(age > 0) IS UNKNOWN",
        // The arms prove different columns.
        "age > 1 OR email = 'x'",
        "age > 1 OR id > 1",
        // Below the top level an AND may be FALSE with a NULL operand.
        "NOT (age > 1 AND id > 0)",
        "(age > 1 AND id > 0) IS NOT TRUE",
        // A parameter or a subquery says nothing about the column.
        "$1::int IS NOT NULL",
        "EXISTS (SELECT 1 FROM posts WHERE posts.id = users.age)",
        "id IN (SELECT age FROM users)",
    ] {
        assert_nullable(
            &db,
            &format!("SELECT age FROM users WHERE {qual}"),
            &[("age", true)],
        );
    }
}

#[test]
fn row_and_whole_row_tests() {
    let db = setup();
    // Every field of a ROW() that IS NOT NULL is.
    assert_nullable(
        &db,
        "SELECT age, email, verified_at FROM users WHERE ROW(age, email) IS NOT NULL",
        &[("age", false), ("email", false), ("verified_at", true)],
    );
    // A whole-row reference IS NOT NULL when every column is.
    assert_nullable(
        &db,
        "SELECT u.age, u.email FROM users u WHERE u IS NOT NULL",
        &[("age", false), ("email", false)],
    );
    assert_nullable(
        &db,
        "SELECT age FROM users u WHERE u.* IS NOT NULL",
        &[("age", false)],
    );
    // A composite column IS NOT NULL is itself non-NULL; reading a field
    // still depends on the field.
    assert_nullable(
        &db,
        "SELECT p, (p).a AS a FROM users WHERE p IS NOT NULL",
        &[("p", false), ("a", true)],
    );
    // A field of a NULL composite is NULL: a strict test on it proves the
    // composite.
    assert_nullable(&db, "SELECT p FROM users WHERE (p).a > 0", &[("p", false)]);
    // So does an element of a NULL array.
    assert_nullable(
        &db,
        "SELECT tags FROM users WHERE tags[1] = 'a'",
        &[("tags", false)],
    );
    // A row comparison can be FALSE with NULL fields: `(1, NULL) < (2, NULL)`.
    assert_nullable(
        &db,
        "SELECT age, email FROM users WHERE (age, email) < (1, 'x')",
        &[("age", true), ("email", true)],
    );
}

#[test]
fn boolean_column_quals() {
    let db = setup();
    assert_nullable(&db, "SELECT flag FROM users WHERE flag", &[("flag", false)]);
    assert_nullable(
        &db,
        "SELECT flag FROM users WHERE NOT flag",
        &[("flag", false)],
    );
    assert_nullable(
        &db,
        "SELECT flag FROM users WHERE flag IS NOT TRUE",
        &[("flag", true)],
    );
}

#[test]
fn where_facts_reach_every_later_clause() {
    let db = setup();
    // The select list, through expressions.
    assert_nullable(
        &db,
        "SELECT age + 1 AS next, lower(email) AS e FROM users
         WHERE age > 0 AND email <> ''",
        &[("next", false), ("e", false)],
    );
    // Aggregates over the rows past WHERE.
    assert_nullable(
        &db,
        "SELECT age, max(email) AS m FROM users WHERE email IS NOT NULL GROUP BY age",
        &[("age", true), ("m", false)],
    );
    assert_eq!(
        elements_nullable(
            &db,
            "SELECT array_agg(age) AS a FROM users WHERE age IS NOT NULL",
            "a"
        ),
        Some(false)
    );
    assert_eq!(
        elements_nullable(&db, "SELECT array_agg(age) AS a FROM users", "a"),
        Some(true)
    );
    // A window function still has its own NULLs (`lag` past the frame).
    assert_nullable(
        &db,
        "SELECT age, lag(age) OVER (ORDER BY id) AS prev FROM users WHERE age IS NOT NULL",
        &[("age", false), ("prev", true)],
    );
}

#[test]
fn grouping_sets_still_null_out_omitted_columns() {
    let db = setup();
    // ROLLUP's grand-total row has a NULL age whatever WHERE said.
    assert_nullable(
        &db,
        "SELECT age, count(*) AS n FROM users WHERE age IS NOT NULL GROUP BY ROLLUP (age)",
        &[("age", true), ("n", false)],
    );
    // HAVING drops that row.
    assert_nullable(
        &db,
        "SELECT age, count(*) AS n FROM users GROUP BY ROLLUP (age) HAVING age IS NOT NULL",
        &[("age", false), ("n", false)],
    );
    assert_nullable(
        &db,
        "SELECT age FROM users GROUP BY age HAVING age > 0",
        &[("age", false)],
    );
    // An aggregate in HAVING says nothing about its argument.
    assert_nullable(
        &db,
        "SELECT age FROM users GROUP BY age HAVING max(age) > 0",
        &[("age", true)],
    );
}

#[test]
fn subqueries_export_their_narrowed_columns() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT v.email FROM (SELECT email FROM users WHERE email IS NOT NULL) v",
        &[("email", false)],
    );
    assert_nullable(
        &db,
        "WITH verified AS (SELECT id, verified_at FROM users WHERE verified_at IS NOT NULL)
         SELECT verified_at FROM verified",
        &[("verified_at", false)],
    );
    assert_eq!(
        elements_nullable(
            &db,
            "SELECT ARRAY(SELECT email FROM users WHERE email LIKE 'a%') AS a",
            "a"
        ),
        Some(false)
    );
    // A view's columns too.
    let mut db = setup();
    db.apply_sql(
        "CREATE VIEW verified_users AS
             SELECT id, email, verified_at FROM users WHERE verified_at IS NOT NULL;",
    )
    .unwrap();
    assert_nullable(
        &db,
        "SELECT email, verified_at FROM verified_users",
        &[("email", true), ("verified_at", false)],
    );
}

#[test]
fn correlated_references_see_the_enclosing_quals() {
    let db = setup();
    // A sublink in the select list runs on rows past the WHERE.
    assert_nullable(
        &db,
        "SELECT (SELECT u.age) AS a FROM users u WHERE u.age IS NOT NULL",
        &[("a", false)],
    );
    // Not one in the WHERE itself.
    assert_nullable(
        &db,
        "SELECT age FROM users u WHERE u.age IS NOT NULL AND (SELECT u.age) > 0",
        &[("age", false)],
    );
}

// ── Outer references keep the enclosing level's join nullability ─────────────

#[test]
fn outer_and_lateral_references_to_a_null_extended_side() {
    let db = setup();
    // `p` is NULL-extended for users without posts: so is its `id` seen
    // from a sublink or a LATERAL subquery.
    assert_nullable(
        &db,
        "SELECT (SELECT p.id) AS v FROM users u LEFT JOIN posts p ON p.author_id = u.id",
        &[("v", true)],
    );
    assert_nullable(
        &db,
        "SELECT s.v FROM users u LEFT JOIN posts p ON p.author_id = u.id,
                LATERAL (SELECT p.id AS v) s",
        &[("v", true)],
    );
    assert_nullable(
        &db,
        "SELECT s.v FROM users u LEFT JOIN LATERAL (SELECT u.id AS v) s ON true",
        &[("v", true)],
    );
    assert_nullable(
        &db,
        "SELECT (SELECT p.title) AS t FROM users u LEFT JOIN posts p ON p.author_id = u.id",
        &[("t", true)],
    );
    // An inner join's entries stay as they are.
    assert_nullable(
        &db,
        "SELECT (SELECT p.id) AS v FROM users u JOIN posts p ON p.author_id = u.id",
        &[("v", false)],
    );
}

// ── Outer-join reduction ─────────────────────────────────────────────────────

#[test]
fn a_strict_where_qual_on_the_nullable_side_makes_the_join_inner() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT p.title, p.body, p.status FROM users u LEFT JOIN posts p ON p.author_id = u.id
         WHERE p.status = 'a'",
        &[("title", false), ("body", true), ("status", false)],
    );
    // The mirror image.
    assert_nullable(
        &db,
        "SELECT p.title FROM posts p RIGHT JOIN users u ON p.author_id = u.id WHERE p.id > 0",
        &[("title", false)],
    );
    // A whole-row test on the nullable side (`users` has a column `p`).
    assert_nullable(
        &db,
        "SELECT q.title FROM users u LEFT JOIN posts q ON q.author_id = u.id
         WHERE q IS NOT NULL",
        &[("title", false)],
    );
    assert_nullable(
        &db,
        "SELECT p.title FROM users u LEFT JOIN posts p ON p.author_id = u.id
         WHERE p IS NOT NULL",
        &[("title", true)],
    );
    // A strict function of the whole row: NULL for a NULL-extended one.
    assert_nullable(
        &db,
        "SELECT q.title FROM users u LEFT JOIN posts q ON q.author_id = u.id
         WHERE row_to_json(q) IS NOT NULL",
        &[("title", false)],
    );
}

#[test]
fn quals_that_keep_null_extended_rows_keep_the_join_outer() {
    let db = setup();
    for qual in [
        // The anti-join.
        "p.id IS NULL",
        "coalesce(p.status, 'x') = 'x'",
        "p.status IS DISTINCT FROM 'x'",
        "p.status = 'x' OR u.age > 0",
        "u.age > 0",
        "NOT (p.status = 'x' AND u.age > 0)",
    ] {
        assert_nullable(
            &db,
            &format!(
                "SELECT p.title FROM users u LEFT JOIN posts p ON p.author_id = u.id WHERE {qual}"
            ),
            &[("title", true)],
        );
    }
}

#[test]
fn full_join_reduction_per_side() {
    let db = setup();
    let sql = |qual: &str| {
        format!(
            "SELECT ka.x, kb.y, ka.k AS ak, kb.k AS bk FROM ka FULL JOIN kb ON ka.k = kb.k WHERE {qual}"
        )
    };
    // FULL → LEFT: ka is no longer NULL-extended, but its unmatched rows
    // (a NULL `ka.k` among them) stay.
    assert_nullable(
        &db,
        &sql("ka.x > 0"),
        &[("x", false), ("y", true), ("ak", true), ("bk", true)],
    );
    // FULL → RIGHT.
    assert_nullable(
        &db,
        &sql("kb.k > 0"),
        &[("x", true), ("y", true), ("ak", true), ("bk", false)],
    );
    // FULL → INNER: the ON clause holds, `ka.k = kb.k` proves both keys.
    assert_nullable(
        &db,
        &sql("ka.x > 0 AND kb.y > 0"),
        &[("x", false), ("y", false), ("ak", false), ("bk", false)],
    );
    assert_nullable(
        &db,
        &sql("ka.k IS NULL"),
        &[("x", true), ("y", true), ("ak", true), ("bk", true)],
    );
}

#[test]
fn reduction_reaches_nested_joins() {
    let db = setup();
    // The label forces `l`; the now-inner join's ON forces `p`, which
    // reduces the join below it.
    assert_nullable(
        &db,
        "SELECT p.title, l.label FROM users u
             LEFT JOIN posts p ON p.author_id = u.id
             LEFT JOIN labels l ON l.post_id = p.id
         WHERE l.label = 'a'",
        &[("title", false), ("label", false)],
    );
    // A LEFT join inside the nullable side of another.
    assert_nullable(
        &db,
        "SELECT p.title, l.post_id FROM users u
             LEFT JOIN (posts p LEFT JOIN labels l ON l.post_id = p.id) ON p.author_id = u.id
         WHERE l.label = 'a'",
        &[("title", false), ("post_id", false)],
    );
    // Forcing the outer side says nothing of the inner one.
    assert_nullable(
        &db,
        "SELECT p.title, l.post_id FROM users u
             LEFT JOIN (posts p LEFT JOIN labels l ON l.post_id = p.id) ON p.author_id = u.id
         WHERE p.id > 0",
        &[("title", false), ("post_id", true)],
    );
    // The outer join's ON forces an entry of an outer join below it.
    assert_nullable(
        &db,
        "SELECT p.title, l.post_id FROM users u
             LEFT JOIN (posts p LEFT JOIN labels l ON l.post_id = p.id)
                 ON p.author_id = u.id AND l.label = 'a'",
        &[("title", true), ("post_id", true)],
    );
    assert_nullable(
        &db,
        "SELECT p.title, l.post_id FROM users u
             LEFT JOIN (posts p LEFT JOIN labels l ON l.post_id = p.id)
                 ON p.author_id = u.id AND l.label = 'a'
         WHERE p.id > 0",
        &[("title", false), ("post_id", false)],
    );
}

#[test]
fn on_clauses_prove_their_side_when_its_row_is_there() {
    let db = setup();
    // An inner join's ON holds for every joined row.
    assert_nullable(
        &db,
        "SELECT u.email, p.body FROM users u JOIN posts p ON p.body = u.email",
        &[("email", false), ("body", false)],
    );
    // A LEFT join's ON holds for the nullable side when it matched — the
    // side's columns are still NULL for unmatched rows...
    assert_nullable(
        &db,
        "SELECT p.status FROM users u LEFT JOIN posts p
             ON p.author_id = u.id AND p.status = 'a'",
        &[("status", true)],
    );
    // ...but not once the join is reduced.
    assert_nullable(
        &db,
        "SELECT p.status FROM users u LEFT JOIN posts p
             ON p.author_id = u.id AND p.status = 'a'
         WHERE p.id IS NOT NULL",
        &[("status", false)],
    );
    // The preserved side's rows stay whatever the ON says.
    assert_nullable(
        &db,
        "SELECT u.age FROM users u LEFT JOIN posts p ON p.author_id = u.id AND u.age > 0",
        &[("age", true)],
    );
    // A FULL join's ON proves nothing on its own.
    assert_nullable(
        &db,
        "SELECT ka.x FROM ka FULL JOIN kb ON ka.k = kb.k AND ka.x > 0 WHERE ka.k > 0",
        &[("x", true)],
    );
    // An inner join on the nullable side of an outer one.
    assert_nullable(
        &db,
        "SELECT p.body, l.label FROM users u
             LEFT JOIN (posts p JOIN labels l ON l.label = p.body) ON p.author_id = u.id",
        &[("body", true), ("label", true)],
    );
    assert_nullable(
        &db,
        "SELECT p.body, l.label FROM users u
             LEFT JOIN (posts p JOIN labels l ON l.label = p.body) ON p.author_id = u.id
         WHERE p.id > 0",
        &[("body", false), ("label", false)],
    );
    // A later join's ON sees an earlier inner join's facts.
    assert_nullable(
        &db,
        "SELECT s.v FROM users u JOIN posts p ON p.body = u.email,
                LATERAL (SELECT p.body AS v) s",
        &[("v", false)],
    );
}

#[test]
fn using_joins() {
    let db = setup();
    // `ka.k = kb.k` holds for every row of the inner join.
    assert_nullable(
        &db,
        "SELECT k, ka.k AS ak, kb.k AS bk FROM ka JOIN kb USING (k)",
        &[("k", false), ("ak", false), ("bk", false)],
    );
    assert_nullable(
        &db,
        "SELECT k, kb.k AS bk FROM ka LEFT JOIN kb USING (k)",
        &[("k", true), ("bk", true)],
    );
    assert_nullable(
        &db,
        "SELECT k, ka.k AS ak, kb.k AS bk FROM ka LEFT JOIN kb USING (k) WHERE kb.y > 0",
        &[("k", false), ("ak", false), ("bk", false)],
    );
    assert_nullable(
        &db,
        "SELECT k FROM ka FULL JOIN kb USING (k) WHERE k > 0",
        &[("k", false)],
    );
    assert_nullable(
        &db,
        "SELECT k, ka.x FROM ka NATURAL JOIN kb",
        &[("k", false), ("x", true)],
    );
}

#[test]
fn aliased_joins() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT j.title, j.body FROM (users u LEFT JOIN posts p ON p.author_id = u.id) AS j
         WHERE j.title IS NOT NULL",
        &[("title", false), ("body", true)],
    );
}

// ── CASE ─────────────────────────────────────────────────────────────────────

#[test]
fn case_branches_know_their_conditions() {
    let db = setup();
    let check = |expr: &str, nullable: bool| {
        assert_nullable(
            &db,
            &format!("SELECT {expr} AS v FROM users"),
            &[("v", nullable)],
        );
    };
    // ELSE after `x IS NULL`: a hand-written COALESCE.
    check("CASE WHEN age IS NULL THEN 0 ELSE age END", false);
    // THEN under a strict condition.
    check("CASE WHEN age > 0 THEN age ELSE 0 END", false);
    check("CASE WHEN age IS NOT NULL THEN age ELSE -1 END", false);
    check("CASE WHEN is_pos(age) THEN age ELSE 0 END", false);
    // `age > 0` not TRUE may be `age` NULL.
    check("CASE WHEN age > 0 THEN 1 ELSE age END", true);
    check("CASE WHEN is_pos_lax(age) THEN age ELSE 0 END", true);
    // Every earlier WHEN not being TRUE accumulates.
    check(
        "CASE WHEN age IS NULL THEN 0 WHEN email IS NULL THEN age ELSE age + length(email) END",
        false,
    );
    check(
        "CASE WHEN age IS NULL OR email IS NULL THEN '' ELSE email || age END",
        false,
    );
    check(
        "CASE WHEN age IS NULL AND email IS NULL THEN '' ELSE email END",
        true,
    );
    // Not TRUE for a test that is never NULL: the test is FALSE.
    check("CASE WHEN NOT (age IS NOT NULL) THEN 0 ELSE age END", false);
    check("CASE WHEN (age > 0) IS NOT TRUE THEN 0 ELSE age END", false);
    check("CASE WHEN (age > 0) IS UNKNOWN THEN 0 ELSE age END", false);
    check(
        "CASE WHEN (age > 0) IS NOT FALSE THEN 0 ELSE age END",
        false,
    );
    check("CASE WHEN (age > 0) IS FALSE THEN 0 ELSE age END", true);
    // Some field of a ROW() that is not NULL is not, but which?
    check(
        "CASE WHEN ROW(age, email) IS NULL THEN '' ELSE email END",
        true,
    );
    // The condition only narrows its own branch.
    check("CASE WHEN age > 0 THEN 0 ELSE age END", true);
    check("CASE WHEN email IS NULL THEN age ELSE 0 END", true);
    // A branch nested in another.
    check(
        "CASE WHEN age IS NULL THEN 0 ELSE CASE WHEN email IS NULL THEN age ELSE age END END",
        false,
    );
}

#[test]
fn simple_case_compares_with_strict_equality() {
    let db = setup();
    let check = |expr: &str, nullable: bool| {
        assert_nullable(
            &db,
            &format!("SELECT {expr} AS v FROM users"),
            &[("v", nullable)],
        );
    };
    check("CASE age WHEN 1 THEN age ELSE 0 END", false);
    check("CASE age WHEN id THEN id + age ELSE 0 END", false);
    check("CASE 1 WHEN age THEN age ELSE 0 END", false);
    // No WHEN matching proves nothing.
    check("CASE age WHEN 1 THEN 0 ELSE age END", true);
}

#[test]
fn case_sees_whether_an_outer_join_matched() {
    let db = setup();
    assert_nullable(
        &db,
        "SELECT CASE WHEN p.id IS NULL THEN 'none' ELSE p.title END AS t,
                CASE WHEN p.id IS NOT NULL THEN p.title ELSE '' END AS t2,
                CASE WHEN p.id IS NULL THEN '' ELSE p.body END AS b
         FROM users u LEFT JOIN posts p ON p.author_id = u.id",
        &[("t", false), ("t2", false), ("b", true)],
    );
    // A preserved-side column says nothing of the match.
    assert_nullable(
        &db,
        "SELECT CASE WHEN u.age IS NULL THEN '' ELSE p.title END AS t
         FROM users u LEFT JOIN posts p ON p.author_id = u.id",
        &[("t", true)],
    );
}

#[test]
fn case_over_grouped_columns() {
    let db = setup();
    // The branch reads the grouped (possibly rolled-up) value.
    assert_nullable(
        &db,
        "SELECT CASE WHEN age IS NULL THEN -1 ELSE age END AS a
         FROM users GROUP BY ROLLUP (age)",
        &[("a", false)],
    );
    // Inside an aggregate, the row's value.
    assert_nullable(
        &db,
        "SELECT age, sum(CASE WHEN email IS NULL THEN 0 ELSE length(email) END) AS s
         FROM users GROUP BY age",
        &[("age", true), ("s", false)],
    );
}

// ── FILTER ───────────────────────────────────────────────────────────────────

#[test]
fn an_aggregate_reads_only_the_rows_its_filter_passes() {
    let db = setup();
    let elems = |sql: &str| elements_nullable(&db, sql, "a");
    assert_eq!(
        elems("SELECT array_agg(age) FILTER (WHERE age IS NOT NULL) AS a FROM users"),
        Some(false)
    );
    assert_eq!(
        elems("SELECT array_agg(age) FILTER (WHERE age > 0) AS a FROM users"),
        Some(false)
    );
    assert_eq!(
        elems("SELECT array_agg(age) FILTER (WHERE email IS NOT NULL) AS a FROM users"),
        Some(true)
    );
    assert_eq!(
        elems("SELECT array_agg(age) FILTER (WHERE age IS NULL) AS a FROM users"),
        Some(true)
    );
    // The classic: aggregate the matched side of a LEFT JOIN.
    assert_eq!(
        elems(
            "SELECT array_agg(p.title) FILTER (WHERE p.id IS NOT NULL) AS a
             FROM users u LEFT JOIN posts p ON p.author_id = u.id GROUP BY u.id"
        ),
        Some(false)
    );
    assert_eq!(
        elems(
            "SELECT array_agg(p.title) AS a
             FROM users u LEFT JOIN posts p ON p.author_id = u.id GROUP BY u.id"
        ),
        Some(true)
    );
    // The aggregate's own result can still be NULL: no row passed.
    assert_nullable(
        &db,
        "SELECT max(age) FILTER (WHERE age > 0) AS m FROM users GROUP BY email",
        &[("m", true)],
    );
    assert_nullable(
        &db,
        "SELECT string_agg(email, ',') FILTER (WHERE email IS NOT NULL) AS s FROM users",
        &[("s", true)],
    );
}

// ── Facts stay where they were proven ────────────────────────────────────────

#[test]
fn facts_do_not_leak_across_levels() {
    let db = setup();
    // The subquery's WHERE narrows its own `age`, not the outer one.
    assert_nullable(
        &db,
        "SELECT u.age FROM users u
         WHERE u.id IN (SELECT v.id FROM users v WHERE v.age IS NOT NULL)",
        &[("age", true)],
    );
    // Nor does a qual on another alias of the same table.
    assert_nullable(
        &db,
        "SELECT a.age AS a_age, b.age AS b_age FROM users a JOIN users b ON a.id = b.id
         WHERE b.age > 0",
        &[("a_age", true), ("b_age", false)],
    );
    // Set operations take each arm as it is.
    assert_nullable(
        &db,
        "SELECT age FROM users WHERE age IS NOT NULL UNION ALL SELECT age FROM users",
        &[("age", true)],
    );
    assert_nullable(
        &db,
        "SELECT age FROM users WHERE age IS NOT NULL UNION ALL
         SELECT age FROM users WHERE age > 1",
        &[("age", false)],
    );
}

// ── UPDATE / DELETE RETURNING ────────────────────────────────────────────────

fn dml_setup() -> PgCatalog {
    let mut db = setup();
    db.apply_sql(
        "CREATE TABLE gen (id int PRIMARY KEY, a int, g int GENERATED ALWAYS AS (a + 1) STORED);
         CREATE TABLE trg_users (id int PRIMARY KEY, email text, age int);
         CREATE FUNCTION wipe_email() RETURNS trigger LANGUAGE plpgsql AS
             $$BEGIN NEW.email := NULL; RETURN NEW; END$$;
         CREATE TRIGGER wipe BEFORE UPDATE ON trg_users
             FOR EACH ROW EXECUTE FUNCTION wipe_email();
         CREATE TABLE parent_t (id int, email text);
         CREATE TABLE child_t (extra int) INHERITS (parent_t);
         CREATE TABLE part_t (id int, email text) PARTITION BY LIST (id);
         CREATE TABLE part_t1 PARTITION OF part_t FOR VALUES IN (1);
         CREATE VIEW users_v AS SELECT id, email, age FROM users;",
    )
    .unwrap();
    db
}

#[test]
fn delete_returns_the_rows_its_where_saw() {
    let db = dml_setup();
    assert_nullable(
        &db,
        "DELETE FROM users WHERE email IS NOT NULL RETURNING email, age, old.email AS o",
        &[("email", false), ("age", true), ("o", false)],
    );
    assert_nullable(
        &db,
        "DELETE FROM part_t WHERE email LIKE 'x%' RETURNING email",
        &[("email", false)],
    );
    assert_nullable(
        &db,
        "DELETE FROM parent_t WHERE email LIKE 'x%' RETURNING email",
        &[("email", false)],
    );
    // USING entries, and their joins.
    assert_nullable(
        &db,
        "DELETE FROM users u USING posts p WHERE p.author_id = u.id AND p.body <> ''
         RETURNING p.body",
        &[("body", false)],
    );
    // An automatically updatable view's DELETE deletes the base rows the
    // WHERE saw.
    assert_nullable(
        &db,
        "DELETE FROM users_v WHERE email IS NOT NULL RETURNING email, old.email AS o",
        &[("email", false), ("o", false)],
    );
    // NEW of a deleted row is NULL.
    assert_nullable(
        &db,
        "DELETE FROM users WHERE email IS NOT NULL RETURNING new.email AS n",
        &[("n", true)],
    );
}

#[test]
fn update_returns_the_where_values_of_the_columns_it_keeps() {
    let db = dml_setup();
    // `age` is SET (to NULL here): RETURNING reads the new value. OLD
    // reads what WHERE saw.
    assert_nullable(
        &db,
        "UPDATE users SET age = NULL WHERE email IS NOT NULL AND age > 0
         RETURNING email, age, old.age AS old_age, new.email AS new_email",
        &[
            ("email", false),
            ("age", true),
            ("old_age", false),
            ("new_email", false),
        ],
    );
    assert_nullable(
        &db,
        "UPDATE users SET (email, age) = (NULL, 1) WHERE email IS NOT NULL RETURNING email",
        &[("email", true)],
    );
    // A stored generated column is recomputed from the new row.
    assert_nullable(
        &db,
        "UPDATE gen SET a = NULL WHERE g IS NOT NULL RETURNING g, old.g AS old_g",
        &[("g", true), ("old_g", false)],
    );
    // A BEFORE ROW trigger may rewrite any column of NEW.
    assert_nullable(
        &db,
        "UPDATE trg_users SET age = 1 WHERE email IS NOT NULL RETURNING email, old.email AS o",
        &[("email", true), ("o", false)],
    );
    // Rows of children and partitions: moved rows run their own triggers.
    assert_nullable(
        &db,
        "UPDATE parent_t SET id = 1 WHERE email IS NOT NULL RETURNING email",
        &[("email", true)],
    );
    assert_nullable(
        &db,
        "UPDATE part_t SET id = 1 WHERE email IS NOT NULL RETURNING email",
        &[("email", true)],
    );
    // Through an automatically updatable view: the base row's.
    assert_nullable(
        &db,
        "UPDATE users_v SET age = 1 WHERE email IS NOT NULL RETURNING email, old.email AS o",
        &[("email", false), ("o", false)],
    );
    // FROM entries keep what WHERE saw.
    assert_nullable(
        &db,
        "UPDATE users u SET age = 1 FROM posts p
         WHERE p.author_id = u.id AND p.body IS NOT NULL RETURNING p.body",
        &[("body", false)],
    );
}

#[test]
fn rows_written_through_a_view_are_not_the_views_rows() {
    let mut db = dml_setup();
    db.apply_sql(
        "CREATE VIEW verified AS SELECT id, name, verified_at FROM users
             WHERE verified_at IS NOT NULL;
         CREATE TABLE names (id int PRIMARY KEY, name text NOT NULL);
         CREATE VIEW names_v AS SELECT id, name FROM names;
         CREATE VIEW names_t AS SELECT id, name FROM names;
         CREATE FUNCTION names_ins() RETURNS trigger LANGUAGE plpgsql AS
             $$BEGIN RETURN NEW; END$$;
         CREATE TRIGGER ins INSTEAD OF INSERT ON names_t
             FOR EACH ROW EXECUTE FUNCTION names_ins();",
    )
    .unwrap();
    // Reading the view: its WHERE holds.
    assert_nullable(
        &db,
        "SELECT verified_at FROM verified",
        &[("verified_at", false)],
    );
    // A deleted row is one of its rows.
    assert_nullable(
        &db,
        "DELETE FROM verified WHERE id = 1 RETURNING verified_at",
        &[("verified_at", false)],
    );
    // A row written through it needn't pass its WHERE.
    assert_nullable(
        &db,
        "INSERT INTO verified (id, name, verified_at) VALUES (1, 'a', NULL) RETURNING verified_at, name",
        &[("verified_at", true), ("name", false)],
    );
    assert_nullable(
        &db,
        "UPDATE verified SET verified_at = NULL RETURNING verified_at, name",
        &[("verified_at", true), ("name", false)],
    );
    assert_nullable(
        &db,
        "MERGE INTO verified v USING (SELECT 1 AS id) s ON v.id = s.id
         WHEN MATCHED THEN UPDATE SET verified_at = NULL
         RETURNING v.verified_at",
        &[("verified_at", true)],
    );
    // An auto-updatable view keeps its base columns' NOT NULL...
    assert_nullable(
        &db,
        "INSERT INTO names_v VALUES (1, 'a') RETURNING name",
        &[("name", false)],
    );
    // ...but an INSTEAD OF trigger returns whatever row it likes.
    assert_nullable(
        &db,
        "INSERT INTO names_t VALUES (1, NULL) RETURNING name",
        &[("name", true)],
    );
}
