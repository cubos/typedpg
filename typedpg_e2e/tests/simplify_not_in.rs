//! `sql!` sends `x NOT IN (SELECT y …)` as `NOT EXISTS` when neither side
//! can be NULL: the rewritten query runs on PostgreSQL and returns the rows
//! the written one does, through every fetch method (`fetch_one` wraps it
//! in a `LIMIT 2` subquery).

mod common;

use typedpg::sql;

#[tokio::test]
async fn not_in_sent_as_not_exists_returns_the_same_rows() {
    let pool = common::setup().await;
    let tag = common::unique_id(&pool).await;
    let pattern = format!("simplify-{tag}-%");
    let mut ids = Vec::new();
    for who in ["poster", "lurker"] {
        let name = format!("{who}-{tag}");
        let email = format!("simplify-{tag}-{who}@example.com");
        let row = sql!(
            &pool,
            "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
        )
        .fetch_one()
        .await
        .expect("insert user");
        ids.push(row.id);
    }
    let (poster, lurker) = (ids[0], ids[1]);
    let title = "hello";
    sql!(
        &pool,
        "INSERT INTO posts (user_id, title) VALUES ($poster, $title)"
    )
    .execute()
    .await
    .expect("insert post");

    // Inlined: `NOT EXISTS (SELECT 1 FROM posts WHERE u.id = user_id)`.
    let rows = sql!(
        &pool,
        "SELECT u.id FROM users u WHERE u.email LIKE $pattern \
         AND u.id NOT IN (SELECT user_id FROM posts) ORDER BY u.id",
        pattern = pattern.clone()
    )
    .fetch_all()
    .await
    .expect("not in, inlined");
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![lurker]);

    // Wrapped whole: the DISTINCT stays the subquery's.
    let row = sql!(
        &pool,
        "SELECT u.id FROM users u WHERE u.email LIKE $pattern \
         AND u.id NOT IN (SELECT DISTINCT user_id FROM posts)",
        pattern = pattern.clone()
    )
    .fetch_one()
    .await
    .expect("not in, wrapped");
    assert_eq!(row.id, lurker);

    // Row-wise, in the target list.
    let rows = sql!(
        &pool,
        "SELECT u.id, (u.id, u.name) NOT IN (SELECT p.user_id, u2.name FROM posts p \
         JOIN users u2 ON u2.id = p.user_id) AS quiet \
         FROM users u WHERE u.email LIKE $pattern ORDER BY u.id"
    )
    .fetch_all()
    .await
    .expect("row-wise not in");
    let quiet: Vec<(i64, bool)> = rows.iter().map(|r| (r.id, r.quiet)).collect();
    assert_eq!(quiet, vec![(poster, false), (lurker, true)]);
}
