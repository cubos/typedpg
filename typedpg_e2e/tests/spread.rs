mod common;

use typedpg::sql;

struct NewUser {
    name: String,
    email: String,
    age: Option<i32>,
}

#[tokio::test]
async fn spread_inserts_every_row_with_its_fields() {
    let pool = common::setup().await;
    let tag = common::unique("spread");
    let users = [
        NewUser {
            name: "Ann".into(),
            email: format!("{tag}-ann@example.com"),
            age: Some(31),
        },
        NewUser {
            name: "Ben".into(),
            email: format!("{tag}-ben@example.com"),
            age: None,
        },
    ];
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, age) VALUES $..users { name, email, age }"
    )
    .execute()
    .await
    .expect("insert");
    assert_eq!(inserted, 2);

    let prefix = format!("{tag}-%");
    let rows = sql!(
        &pool,
        "SELECT name, age FROM users WHERE email LIKE $prefix ORDER BY name"
    )
    .fetch_all()
    .await
    .expect("select");
    let got: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.age)).collect();
    assert_eq!(got, [("Ann", Some(31)), ("Ben", None)]);
}

#[tokio::test]
async fn spread_with_regular_params_and_returning() {
    let pool = common::setup().await;
    let users = [NewUser {
        name: "Cid".into(),
        email: format!("{}@example.com", common::unique("spread-cid")),
        age: None,
    }];
    let age = 40;
    let rows = sql!(
        &pool,
        "INSERT INTO users (name, email, age) \
         SELECT name, email, $age FROM (VALUES $..users { name, email }) AS v(name, email) \
         RETURNING name, age"
    )
    .fetch_all()
    .await
    .expect("insert");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].name.as_str(), rows[0].age), ("Cid", Some(40)));

    let none: Vec<NewUser> = Vec::new();
    let affected = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..none { name, email }"
    )
    .execute()
    .await
    .expect("an empty spread runs nothing");
    assert_eq!(affected, 0);
}
