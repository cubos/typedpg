mod common;

use typedpg::sql;
use typedpg_e2e::PostStatus;

#[tokio::test]
async fn enum_roundtrip_insert_and_read() {
    let pool = common::setup().await;

    let name = "EnumOwner";
    let email = "enum-owner@example.com";
    let user = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user");

    let user_id = user.id;
    let title = "Enum post";
    let status = PostStatus::Published;
    let inserted = sql!(
        &pool,
        "INSERT INTO posts (user_id, title, status) VALUES ($user_id, $title, $status) RETURNING id, status"
    )
    .fetch_one()
    .await
    .expect("insert post");

    assert_eq!(inserted.status, PostStatus::Published);

    let post_id = inserted.id;
    let fetched = sql!(&pool, "SELECT status FROM posts WHERE id = $post_id")
        .fetch_one()
        .await
        .expect("select status");
    assert_eq!(fetched.status, PostStatus::Published);
}

#[tokio::test]
async fn enum_update_changes_variant() {
    let pool = common::setup().await;

    let name = "EnumUpdater";
    let email = "enum-updater@example.com";
    let user = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user");

    let user_id = user.id;
    let title = "To be archived";
    let draft = sql!(
        &pool,
        "INSERT INTO posts (user_id, title) VALUES ($user_id, $title) RETURNING id, status"
    )
    .fetch_one()
    .await
    .expect("insert post");
    // Default status is 'draft'.
    assert_eq!(draft.status, PostStatus::Draft);

    let post_id = draft.id;
    let new_status = PostStatus::Archived;
    let updated = sql!(
        &pool,
        "UPDATE posts SET status = $new_status WHERE id = $post_id"
    )
    .execute()
    .await
    .expect("update status");
    assert_eq!(updated, 1);

    let after = sql!(&pool, "SELECT status FROM posts WHERE id = $post_id")
        .fetch_one()
        .await
        .expect("select");
    assert_eq!(after.status, PostStatus::Archived);
}

#[tokio::test]
async fn enum_filter_in_where_clause() {
    let pool = common::setup().await;

    let name = "EnumFilter";
    let email = "enum-filter@example.com";
    let user = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert user");

    for (t, s) in [
        ("a", PostStatus::Draft),
        ("b", PostStatus::Published),
        ("c", PostStatus::Published),
        ("d", PostStatus::Archived),
    ] {
        let user_id = user.id;
        let title = t.to_string();
        let status = s;
        sql!(
            &pool,
            "INSERT INTO posts (user_id, title, status) VALUES ($user_id, $title, $status)"
        )
        .execute()
        .await
        .expect("insert post");
    }

    let user_id = user.id;
    let status = PostStatus::Published;
    let published = sql!(
        &pool,
        "SELECT id, title, status FROM posts
         WHERE user_id = $user_id AND status = $status
         ORDER BY title"
    )
    .fetch_all()
    .await
    .expect("filter");

    assert_eq!(published.len(), 2);
    assert!(published.iter().all(|r| r.status == PostStatus::Published));
    let titles: Vec<_> = published.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["b", "c"]);
}

#[tokio::test]
async fn unmapped_enum_params_take_what_text_params_do() {
    let pool = common::setup().await;
    // `mood` has no [types] mapping: its labels are Strings, and its
    // parameters take &str / String / Option<_> like a text parameter.
    let id = common::unique_id(&pool).await;
    let row = sql!(
        &pool,
        "INSERT INTO moods (id, mood, fixed) VALUES ($id, $m, $f) RETURNING mood, fixed",
        m = "ok",
        f = "sad"
    )
    .fetch_one()
    .await
    .expect("&str");
    assert_eq!(
        (row.mood.as_deref(), row.fixed.as_str()),
        (Some("ok"), "sad")
    );

    let id = common::unique_id(&pool).await;
    let label = String::from("sad");
    let row = sql!(
        &pool,
        "INSERT INTO moods (id, mood, fixed) VALUES ($id, $m, $f) RETURNING mood",
        m = label.clone(),
        f = &label
    )
    .fetch_one()
    .await
    .expect("String");
    assert_eq!(row.mood.as_deref(), Some("sad"));

    let id = common::unique_id(&pool).await;
    let row = sql!(
        &pool,
        "INSERT INTO moods (id, mood) VALUES ($id, $m) RETURNING mood",
        m = None::<&str>
    )
    .fetch_one()
    .await
    .expect("None");
    assert_eq!(row.mood, None);

    let id = common::unique_id(&pool).await;
    let row = sql!(
        &pool,
        "INSERT INTO moods (id, mood) VALUES ($id, $m) RETURNING mood",
        m = Some("ok")
    )
    .fetch_one()
    .await
    .expect("Some(&str)");
    assert_eq!(row.mood.as_deref(), Some("ok"));
}
