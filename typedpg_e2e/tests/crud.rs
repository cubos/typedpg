mod common;

use typedpg::sql;

#[tokio::test]
async fn insert_returning_and_select() {
    let pool = common::setup().await;

    let name = "Alice";
    let email = "alice-crud@example.com";
    let age = 30;
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, age) VALUES ($name, $email, $age) RETURNING id, created_at"
    )
    .fetch_one()
    .await
    .expect("insert");

    assert!(inserted.id > 0);

    let id = inserted.id;
    let found = sql!(
        &pool,
        "SELECT id, name, email, age FROM users WHERE id = $id"
    )
    .fetch_optional()
    .await
    .expect("select")
    .expect("row present");

    assert_eq!(found.name, "Alice");
    assert_eq!(found.email, "alice-crud@example.com");
    assert_eq!(found.age, Some(30));
}

#[tokio::test]
async fn update_and_delete_return_affected_rows() {
    let pool = common::setup().await;

    let email = "bob-crud@example.com";
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('Bob', $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let new_age = 42;
    let updated = sql!(&pool, "UPDATE users SET age = $new_age WHERE id = $id")
        .execute()
        .await
        .expect("update");
    assert_eq!(updated, 1);

    let after = sql!(&pool, "SELECT age FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select after update");
    assert_eq!(after.age, Some(42));

    let deleted = sql!(&pool, "DELETE FROM users WHERE id = $id")
        .execute()
        .await
        .expect("delete");
    assert_eq!(deleted, 1);

    let gone = sql!(&pool, "SELECT id FROM users WHERE id = $id")
        .fetch_optional()
        .await
        .expect("select after delete");
    assert!(gone.is_none());
}

#[tokio::test]
async fn cte_dml_insert_with_fetch_one() {
    // Regression: a top-level `WITH … (UPDATE … RETURNING) INSERT … RETURNING`
    // is valid PostgreSQL but cannot legally appear as the body of a
    // `SELECT * FROM (…)` subquery. `fetch_one`/`fetch_optional` used to wrap
    // every query starting with `WITH` in such a subquery, producing a syntax
    // error at the inner `INSERT`. The codegen now consults the analyzed
    // top-level kind and only wraps real `SELECT`s.
    let pool = common::setup().await;

    let owner_email = "ellie-crud@example.com";
    let owner = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('Ellie', $owner_email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user");

    let owner_id = owner.id;
    let post = sql!(
        &pool,
        "INSERT INTO posts (user_id, title, body) VALUES ($owner_id, 'Hello', 'old body') RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert post");

    let post_id = post.id;
    let new_body = "fresh body";
    let comment_body = "first comment";
    let inserted = sql!(
        &pool,
        "WITH bump AS ( \
             UPDATE posts SET body = $new_body WHERE id = $post_id RETURNING id, user_id \
         ) \
         INSERT INTO comments (post_id, user_id, body) \
         SELECT bump.id, bump.user_id, $comment_body FROM bump \
         RETURNING id, post_id, user_id, body"
    )
    .fetch_one()
    .await
    .expect("cte-dml insert");

    assert!(inserted.id > 0);
    assert_eq!(inserted.post_id, post_id);
    assert_eq!(inserted.user_id, owner_id);
    assert_eq!(inserted.body, "first comment");
}

#[tokio::test]
async fn cte_dml_select_with_fetch_optional() {
    // Regression: top-level `WITH (UPDATE … RETURNING) SELECT … JOIN …`. The
    // outer node is a SelectStmt, so a previous version of the codegen treated
    // it as wrappable and emitted `SELECT * FROM (<query>) LIMIT 2` for
    // `fetch_optional` — which PG rejects with `E0A000: WITH clause containing
    // a data-modifying statement must be at the top level`. The analyzer's
    // `can_run_as_subquery` flag now catches the DML-CTE and the codegen sends
    // the query unwrapped.
    let pool = common::setup().await;

    let owner_email = "fred-crud@example.com";
    let owner = sql!(
        &pool,
        "INSERT INTO users (name, email, age) VALUES ('Fred', $owner_email, 25) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user");

    let owner_id = owner.id;
    let post = sql!(
        &pool,
        "INSERT INTO posts (user_id, title, body) VALUES ($owner_id, 'Hi', 'before') RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert post");

    let post_id = post.id;
    let new_body = "after";
    let row = sql!(
        &pool,
        "WITH bumped AS ( \
             UPDATE posts SET body = $new_body WHERE id = $post_id RETURNING id, user_id, body \
         ) \
         SELECT b.id, b.body, u.email \
         FROM bumped b JOIN users u ON u.id = b.user_id"
    )
    .fetch_optional()
    .await
    .expect("cte-dml select");

    let row = row.expect("row present");
    assert_eq!(row.id, post_id);
    assert_eq!(row.body, Some("after".to_string()));
    assert_eq!(row.email, "fred-crud@example.com");
}

#[tokio::test]
async fn fetch_all_returns_multiple_rows() {
    let pool = common::setup().await;

    for (name, email) in [
        ("Carol", "carol-crud@example.com"),
        ("Dave", "dave-crud@example.com"),
    ] {
        sql!(
            &pool,
            "INSERT INTO users (name, email) VALUES ($name, $email)"
        )
        .execute()
        .await
        .expect("insert");
    }

    let pattern = "%-crud@example.com";
    let rows = sql!(
        &pool,
        "SELECT id, name, email FROM users WHERE email LIKE $pattern ORDER BY email"
    )
    .fetch_all()
    .await
    .expect("select");

    assert!(rows.len() >= 2);
    let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"Carol"));
    assert!(names.contains(&"Dave"));
}

#[tokio::test]
async fn database_errors_display_the_server_message() {
    let pool = common::setup().await;
    let email = "dup-display@example.com";
    sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('A', $email)"
    )
    .execute()
    .await
    .expect("first insert");
    let err = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('B', $email)"
    )
    .execute()
    .await
    .expect_err("duplicate email");
    assert_eq!(
        err.to_string(),
        "database error: duplicate key value violates unique constraint \"users_email_key\" \
         (SQLSTATE 23505) DETAIL: Key (email)=(dup-display@example.com) already exists."
    );
}
