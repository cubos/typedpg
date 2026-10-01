//! `fetch_one` / `fetch_optional` / `fetch_value*` row-count errors say
//! which query they are about.

mod common;

use typedpg::sql;

struct NewUser {
    name: String,
    email: String,
}

fn query_of(err: &typedpg::Error) -> typedpg::error::QueryContext {
    match err {
        typedpg::Error::NoRows { query, .. } | typedpg::Error::TooManyRows { query, .. } => *query,
        other => panic!("expected a row-count error, got {other:?}"),
    }
}

#[tokio::test]
async fn no_rows_names_the_query_and_its_call_site() {
    let pool = common::setup().await;
    let id = -1i64;
    let line = line!() + 1;
    let err = sql!(&pool, "SELECT name FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect_err("no user has id -1");
    assert!(matches!(err, typedpg::Error::NoRows { .. }), "{err:?}");
    let query = query_of(&err);
    assert_eq!(query.sql(), "SELECT name FROM users WHERE id = $1");
    assert_eq!(query.file(), file!());
    assert_eq!(query.line(), line);
    assert_eq!(
        err.to_string(),
        format!(
            "query returned no rows: `SELECT name FROM users WHERE id = $1` (sql! at {}:{}:{})",
            file!(),
            line,
            query.column()
        )
    );

    let err = sql!(&pool, "SELECT name FROM users WHERE id = $id")
        .fetch_value()
        .await
        .expect_err("fetch_value needs a row");
    assert!(matches!(err, typedpg::Error::NoRows { .. }), "{err:?}");
}

#[tokio::test]
async fn too_many_rows_names_the_query() {
    let pool = common::setup().await;
    for email in [common::unique("rows-a"), common::unique("rows-b")] {
        sql!(
            &pool,
            "INSERT INTO users (name, email) VALUES ('Rows', $email)"
        )
        .execute()
        .await
        .expect("insert");
    }
    let err = sql!(&pool, "SELECT email FROM users WHERE name = 'Rows'")
        .fetch_optional()
        .await
        .expect_err("two rows");
    assert!(matches!(err, typedpg::Error::TooManyRows { .. }), "{err:?}");
    assert_eq!(
        query_of(&err).sql(),
        "SELECT email FROM users WHERE name = 'Rows'"
    );

    let err = sql!(&pool, "SELECT email FROM users WHERE name = 'Rows'")
        .fetch_value_optional()
        .await
        .expect_err("two rows");
    assert!(matches!(err, typedpg::Error::TooManyRows { .. }), "{err:?}");
}

#[tokio::test]
async fn fetch_value_optional_reads_a_missing_row_as_none() {
    let pool = common::setup().await;
    let id = -1i64;
    let name = sql!(&pool, "SELECT name FROM users WHERE id = $id")
        .fetch_value_optional()
        .await
        .expect("no row is None");
    assert_eq!(name, None);
    // A nullable column flattens: NULL and no row are both None.
    let email = &common::unique("rows-null-age");
    sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('RowsAge', $email)"
    )
    .execute()
    .await
    .expect("insert");
    let age: Option<i32> = sql!(&pool, "SELECT age FROM users WHERE email = $email")
        .fetch_value_optional()
        .await
        .expect("a NULL age");
    assert_eq!(age, None);
}

#[tokio::test]
async fn spread_row_count_errors_name_the_query() {
    let pool = common::setup().await;
    let none: Vec<NewUser> = Vec::new();
    let err = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..none { name, email } RETURNING id"
    )
    .fetch_one()
    .await
    .expect_err("an empty spread inserts nothing");
    assert!(matches!(err, typedpg::Error::NoRows { .. }), "{err:?}");
    assert_eq!(
        query_of(&err).sql(),
        "INSERT INTO users (name, email) VALUES  RETURNING id"
    );

    let two = [
        NewUser {
            name: "Spread".into(),
            email: common::unique("rows-spread-a"),
        },
        NewUser {
            name: "Spread".into(),
            email: common::unique("rows-spread-b"),
        },
    ];
    let err = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..two { name, email } RETURNING id"
    )
    .fetch_one()
    .await
    .expect_err("two rows returned");
    assert!(matches!(err, typedpg::Error::TooManyRows { .. }), "{err:?}");
}
