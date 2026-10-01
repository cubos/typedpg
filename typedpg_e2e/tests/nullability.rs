//! End-to-end nullability: for each shape that can make a column NULL, the
//! data below makes it NULL, and the test pins the Rust type `sql!`
//! generated (`let _: Option<T>` / `let _: T`) and that the rows decode.
//! A NOT NULL inference that is wrong shows up here as a decode error.
//!
//! Every test of the run shares the database: rows are keyed by unique
//! emails / labels and queries filter on them.

mod common;

use typedpg::sql;

/// Insert a user with no age and return its id.
async fn user(pool: &deadpool_postgres::Pool, prefix: &str) -> i64 {
    let name = common::unique(prefix);
    let email = &format!("{name}@example.com");
    sql!(
        pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user")
    .id
}

/// Insert a post of `user_id` and return its id.
async fn post(pool: &deadpool_postgres::Pool, user_id: i64) -> i64 {
    let title = common::unique("post");
    sql!(
        pool,
        "INSERT INTO posts (user_id, title) VALUES ($user_id, $title) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert post")
    .id
}

#[tokio::test]
async fn right_join_nulls_the_left_side() {
    let pool = common::setup().await;
    let uid = user(&pool, "right-join").await;
    let pid = post(&pool, uid).await;
    // The join condition never holds: the post comes back without a user.
    let rows = sql!(
        &pool,
        "SELECT u.name, p.title
         FROM users u RIGHT JOIN posts p ON p.user_id = u.id AND u.id < 0
         WHERE p.id = $pid"
    )
    .fetch_all()
    .await
    .expect("right join");
    assert_eq!(rows.len(), 1);
    let name: &Option<String> = &rows[0].name;
    let _title: &String = &rows[0].title;
    assert_eq!(*name, None);
}

#[tokio::test]
async fn full_join_nulls_both_sides() {
    let pool = common::setup().await;
    let uid = user(&pool, "full-join").await;
    let pid = post(&pool, uid).await;
    let mut rows = sql!(
        &pool,
        "SELECT u.id AS user_id, p.id AS post_id
         FROM (SELECT id FROM users WHERE id = $uid) u
         FULL JOIN (SELECT id FROM posts WHERE id = $pid) p ON false"
    )
    .fetch_all()
    .await
    .expect("full join");
    rows.sort_by_key(|r| r.user_id.is_none());
    let user_id: Option<i64> = rows[0].user_id;
    let post_id: Option<i64> = rows[0].post_id;
    assert_eq!((user_id, post_id), (Some(uid), None));
    assert_eq!((rows[1].user_id, rows[1].post_id), (None, Some(pid)));
}

#[tokio::test]
async fn aggregates_over_an_empty_set() {
    let pool = common::setup().await;
    let email = common::unique("nobody");
    let row = sql!(
        &pool,
        "SELECT sum(age) AS total, max(id) AS latest, count(*) AS n, array_agg(id) AS ids
         FROM users WHERE email = $email"
    )
    .fetch_one()
    .await
    .expect("aggregates");
    let total: Option<i64> = row.total;
    let latest: Option<i64> = row.latest;
    let n: i64 = row.n;
    let ids: Option<Vec<i64>> = row.ids;
    assert_eq!((total, latest, n, ids), (None, None, 0, None));
}

#[tokio::test]
async fn case_without_else_and_scalar_subquery() {
    let pool = common::setup().await;
    let uid = user(&pool, "case").await;
    let row = sql!(
        &pool,
        "SELECT CASE WHEN id < 0 THEN name END AS never,
                (SELECT title FROM posts WHERE user_id = $uid LIMIT 1) AS first_title,
                CASE WHEN id < 0 THEN name ELSE 'x' END AS always
         FROM users WHERE id = $uid"
    )
    .fetch_one()
    .await
    .expect("case / subquery");
    let never: Option<String> = row.never;
    let first_title: Option<String> = row.first_title;
    let always: String = row.always;
    assert_eq!((never, first_title, always.as_str()), (None, None, "x"));
}

#[tokio::test]
async fn coalesce_is_not_null_only_with_a_not_null_argument() {
    let pool = common::setup().await;
    let uid = user(&pool, "coalesce").await;
    // A bare parameter in COALESCE is inferred nullable — and so, with
    // `age` NULL too, is the COALESCE; `$fallback!` keeps both NOT NULL.
    let fallback: Option<i32> = None;
    let row = sql!(
        &pool,
        "SELECT COALESCE(age, 0) AS zero, COALESCE(age, $fallback) AS maybe
         FROM users WHERE id = $uid"
    )
    .fetch_one()
    .await
    .expect("coalesce");
    let zero: i32 = row.zero;
    let maybe: Option<i32> = row.maybe;
    assert_eq!((zero, maybe), (0, None));

    let fallback = 7;
    let row = sql!(
        &pool,
        "SELECT COALESCE(age, $fallback!) AS seven FROM users WHERE id = $uid"
    )
    .fetch_one()
    .await
    .expect("coalesce with a NOT NULL parameter");
    let seven: i32 = row.seven;
    assert_eq!(seven, 7);
}

#[tokio::test]
async fn lag_and_lead_past_the_partition() {
    let pool = common::setup().await;
    let uid = user(&pool, "window").await;
    let row = sql!(
        &pool,
        "SELECT lag(id) OVER (ORDER BY id) AS prev, lead(id) OVER (ORDER BY id) AS next,
                lag(id, 1, 0::bigint) OVER (ORDER BY id) AS prev_or_zero,
                row_number() OVER () AS n
         FROM users WHERE id = $uid"
    )
    .fetch_one()
    .await
    .expect("window");
    let prev: Option<i64> = row.prev;
    let next: Option<i64> = row.next;
    let prev_or_zero: i64 = row.prev_or_zero;
    let n: i64 = row.n;
    assert_eq!((prev, next, prev_or_zero, n), (None, None, 0, 1));
}

#[tokio::test]
async fn union_with_a_null_branch() {
    let pool = common::setup().await;
    let uid = user(&pool, "union").await;
    let rows = sql!(
        &pool,
        "SELECT name FROM users WHERE id = $uid UNION ALL SELECT NULL"
    )
    .fetch_all()
    .await
    .expect("union");
    let names: Vec<Option<String>> = rows.into_iter().map(|r| r.name).collect();
    assert_eq!(names.len(), 2);
    assert!(names[0].is_some());
    assert_eq!(names[1], None);
}

#[tokio::test]
async fn view_over_an_outer_join() {
    let pool = common::setup().await;
    let uid = user(&pool, "view").await;
    let row = sql!(
        &pool,
        "SELECT name, title FROM user_post_titles WHERE user_id = $uid"
    )
    .fetch_one()
    .await
    .expect("view");
    let _name: String = row.name;
    let title: Option<String> = row.title;
    assert_eq!(title, None);
}

#[tokio::test]
async fn where_is_not_null_is_not_narrowed() {
    let pool = common::setup().await;
    let with_age = user(&pool, "refine").await;
    let without_age = user(&pool, "refine").await;
    sql!(&pool, "UPDATE users SET age = 30 WHERE id = $with_age")
        .execute()
        .await
        .expect("set age");
    let ids = [with_age, without_age];
    // The analyzer doesn't narrow a column through WHERE: `age` stays
    // `Option` (conservative), and only rows with an age come back.
    let ages = sql!(
        &pool,
        "SELECT age FROM users WHERE id = ANY($ids) AND age IS NOT NULL"
    )
    .fetch_all()
    .await
    .expect("filtered");
    let ages: Vec<Option<i32>> = ages.into_iter().map(|r| r.age).collect();
    assert_eq!(ages, vec![Some(30)]);
    let mut all = sql!(&pool, "SELECT age FROM users WHERE id = ANY($ids)")
        .fetch_all()
        .await
        .expect("unrefined");
    all.sort_by_key(|r| r.age.is_none());
    let all: Vec<Option<i32>> = all.into_iter().map(|r| r.age).collect();
    assert_eq!(all, vec![Some(30), None]);
}

#[tokio::test]
async fn on_conflict_returning() {
    let pool = common::setup().await;
    let name = &common::unique("conflict");
    let email = &format!("{name}@example.com");
    sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email)"
    )
    .execute()
    .await
    .expect("first insert");
    // DO UPDATE returns the existing row, whose age is NULL.
    let row = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email)
         ON CONFLICT (email) DO UPDATE SET name = excluded.name
         RETURNING id, age, old.age AS old_age, new.name AS new_name"
    )
    .fetch_one()
    .await
    .expect("upsert");
    let _id: i64 = row.id;
    let age: Option<i32> = row.age;
    // OLD is missing when the row is inserted; NEW is always there.
    let old_age: Option<i32> = row.old_age;
    let new_name: String = row.new_name;
    assert_eq!((age, old_age), (None, None));
    assert_eq!(&new_name, name);
    // DO NOTHING returns no row on a conflict.
    let none = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email)
         ON CONFLICT (email) DO NOTHING RETURNING id"
    )
    .fetch_optional()
    .await
    .expect("insert or nothing");
    assert!(none.is_none());
}

#[tokio::test]
async fn generated_columns_follow_their_inputs() {
    let pool = common::setup().await;
    let label = &common::unique("generated");
    sql!(
        &pool,
        "INSERT INTO generated_values (label) VALUES ($label)"
    )
    .execute()
    .await
    .expect("insert");
    let row = sql!(
        &pool,
        "SELECT base, doubled, tripled, label_length FROM generated_values WHERE label = $label"
    )
    .fetch_one()
    .await
    .expect("select generated");
    let base: Option<i32> = row.base;
    let doubled: Option<i32> = row.doubled;
    let tripled: Option<i32> = row.tripled;
    let label_length: Option<i32> = row.label_length;
    assert_eq!((base, doubled, tripled), (None, None, None));
    assert_eq!(label_length, Some(label.len() as i32));
}

#[tokio::test]
async fn recursive_cte_nulls_reach_later_iterations() {
    let pool = common::setup().await;
    // `b` turns NULL on the first step and `a` copies it on the next.
    let rows = sql!(
        &pool,
        "WITH RECURSIVE r(a, b, k) AS (
             SELECT 1, 1, 1 UNION ALL SELECT b, NULL::int, k + 1 FROM r WHERE k < 3
         ) SELECT a, k FROM r ORDER BY k"
    )
    .fetch_all()
    .await
    .expect("recursive");
    let a: Vec<Option<i32>> = rows.iter().map(|r| r.a).collect();
    let _k: i32 = rows[0].k;
    assert_eq!(a, vec![Some(1), Some(1), None]);
}

#[tokio::test]
async fn string_to_table_with_a_null_string() {
    let pool = common::setup().await;
    let rows = sql!(&pool, "SELECT string_to_table('a,,b', ',', '') AS part")
        .fetch_all()
        .await
        .expect("string_to_table");
    let parts: Vec<Option<String>> = rows.into_iter().map(|r| r.part).collect();
    assert_eq!(parts, vec![Some("a".into()), None, Some("b".into())]);
}

#[tokio::test]
async fn fields_of_a_row_type_value_are_nullable() {
    let pool = common::setup().await;
    let label = &common::unique("row-holder");
    // `row_parts.a` is NOT NULL in its table, not in a value of its type.
    sql!(
        &pool,
        "INSERT INTO row_holders (label, part) VALUES ($label, ROW(NULL, NULL)::row_parts)"
    )
    .execute()
    .await
    .expect("insert");
    let row = sql!(
        &pool,
        "SELECT (part).a AS a, part FROM row_holders WHERE label = $label"
    )
    .fetch_one()
    .await
    .expect("select");
    let a: Option<i32> = row.a;
    let part_a: Option<i32> = row.part.a;
    let part_b: Option<String> = row.part.b;
    assert_eq!((a, part_a, part_b), (None, None, None));
}
