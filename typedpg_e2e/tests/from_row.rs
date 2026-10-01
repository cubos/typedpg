//! `fetch_*_as::<T>()` with a `#[derive(FromRow)]` type: checked against
//! the query's columns at compile time, decoded like `sql!`'s own struct.

mod common;

use typedpg::sql;

#[derive(typedpg::FromRow, Debug, PartialEq)]
struct UserRow {
    id: i64,
    name: String,
    // A nullable column.
    age: Option<i32>,
}

/// A field may be an `Option` of a NOT NULL column.
#[derive(typedpg::FromRow, Debug, PartialEq)]
struct LooseName {
    name: Option<String>,
}

/// Columns named with a nullability annotation: PostgreSQL returns them as
/// `body!` / `age?`, the fields are `body` / `age`.
#[derive(typedpg::FromRow, Debug, PartialEq)]
struct Annotated {
    body: String,
    age: Option<i32>,
}

/// Generic structs derive too.
#[derive(typedpg::FromRow, Debug, PartialEq)]
struct Single<T> {
    value: T,
}

async fn insert_user(pool: &deadpool_postgres::Pool, email: &str, age: Option<i32>) -> i64 {
    sql!(
        pool,
        "INSERT INTO users (name, email, age) VALUES ('FromRow', $email, $age) RETURNING id"
    )
    .fetch_value()
    .await
    .expect("insert user")
}

#[tokio::test]
async fn fetch_as_builds_the_derived_type() {
    let pool = common::setup().await;
    let tag = common::unique("fromrow");
    let a = insert_user(&pool, &format!("{tag}-a"), Some(40)).await;
    let b = insert_user(&pool, &format!("{tag}-b"), None).await;
    let pattern = format!("{tag}-%");

    let all = sql!(
        &pool,
        "SELECT id, name, age FROM users WHERE email LIKE $pattern ORDER BY email"
    )
    .fetch_all_as::<UserRow>()
    .await
    .expect("fetch_all_as");
    assert_eq!(
        all,
        [
            UserRow {
                id: a,
                name: "FromRow".into(),
                age: Some(40)
            },
            UserRow {
                id: b,
                name: "FromRow".into(),
                age: None
            },
        ]
    );

    // Extra columns are ignored; a field may widen to Option.
    let one = sql!(&pool, "SELECT id, name, email FROM users WHERE id = $a")
        .fetch_one_as::<LooseName>()
        .await
        .expect("fetch_one_as");
    assert_eq!(
        one,
        LooseName {
            name: Some("FromRow".into())
        }
    );

    let none = sql!(&pool, "SELECT name FROM users WHERE id = -1")
        .fetch_optional_as::<LooseName>()
        .await
        .expect("fetch_optional_as");
    assert_eq!(none, None);

    let err = sql!(&pool, "SELECT name FROM users WHERE id = -1")
        .fetch_one_as::<LooseName>()
        .await
        .expect_err("no row");
    assert!(matches!(err, typedpg::Error::NoRows { .. }), "{err:?}");

    let value = sql!(&pool, "SELECT age AS value FROM users WHERE id = $a")
        .fetch_one_as::<Single<Option<i32>>>()
        .await
        .expect("generic");
    assert_eq!(value, Single { value: Some(40) });
}

#[tokio::test]
async fn annotated_column_names_map_to_their_fields() {
    let pool = common::setup().await;
    let user_id = insert_user(&pool, &common::unique("fromrow-annotated"), Some(7)).await;
    let title = &common::unique("annotated");
    sql!(
        &pool,
        "INSERT INTO posts (user_id, title, body) VALUES ($user_id, $title, 'text')"
    )
    .execute()
    .await
    .expect("insert post");
    let row = sql!(
        &pool,
        r#"SELECT p.body AS "body!", u.age AS "age?"
           FROM posts p JOIN users u ON u.id = p.user_id WHERE p.title = $title"#
    )
    .fetch_one_as::<Annotated>()
    .await
    .expect("annotated columns");
    assert_eq!(
        row,
        Annotated {
            body: "text".into(),
            age: Some(7)
        }
    );

    // FromRow::from_row (by name, on a raw row) strips the annotation too.
    let client = pool.get().await.expect("client");
    let raw = client
        .query_one(r#"SELECT 'x'::text AS "body!", NULL::int AS "age?""#, &[])
        .await
        .expect("raw query");
    let row = <Annotated as typedpg::FromRow>::from_row(&raw).expect("from_row");
    assert_eq!(
        row,
        Annotated {
            body: "x".into(),
            age: None
        }
    );
}

/// Fields of mapped enum and JSONB-domain types: decoded like `sql!`'s
/// own struct, though they have no `FromSql` impl.
#[derive(typedpg::FromRow, Debug, PartialEq)]
struct Mapped {
    status: typedpg_e2e::PostStatus,
    prefs: Option<typedpg_e2e::UserPreferences>,
}

#[tokio::test]
async fn mapped_enum_and_domain_fields() {
    let pool = common::setup().await;
    let row = sql!(
        &pool,
        r#"SELECT 'archived'::post_status AS status,
                  '{"theme": "x", "newsletter": false, "daily_digest_limit": 2}'::user_preferences
                      AS prefs"#
    )
    .fetch_one_as::<Mapped>()
    .await
    .expect("mapped fields");
    assert_eq!(
        row,
        Mapped {
            status: typedpg_e2e::PostStatus::Archived,
            prefs: Some(typedpg_e2e::UserPreferences {
                theme: "x".into(),
                newsletter: false,
                daily_digest_limit: 2,
            }),
        }
    );
}

#[tokio::test]
async fn spread_queries_fetch_as_too() {
    struct NewUser {
        email: String,
    }
    let pool = common::setup().await;
    let users = [NewUser {
        email: common::unique("fromrow-spread"),
    }];
    let rows = sql!(
        &pool,
        "INSERT INTO users (name, email) SELECT 'FromRow', e FROM (VALUES $..users { email }) v(e) \
         RETURNING name"
    )
    .fetch_all_as::<LooseName>()
    .await
    .expect("spread fetch_all_as");
    assert_eq!(
        rows,
        [LooseName {
            name: Some("FromRow".into())
        }]
    );
}
