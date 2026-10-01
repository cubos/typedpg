mod common;

use typedpg::sql;
use typedpg_e2e::UserPreferences;

#[tokio::test]
async fn domain_jsonb_roundtrip_on_insert_and_select() {
    let pool = common::setup().await;

    let name = "Domain Owner";
    let email = "domain-owner@example.com";
    let preferences = UserPreferences {
        theme: "dark".into(),
        newsletter: true,
        daily_digest_limit: 5,
    };

    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, preferences) VALUES ($name, $email, $preferences) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let row = sql!(&pool, "SELECT id, preferences FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");

    let got: UserPreferences = row.preferences.expect("preferences set");
    assert_eq!(got.theme, "dark");
    assert!(got.newsletter);
    assert_eq!(got.daily_digest_limit, 5);
}

#[tokio::test]
async fn domain_jsonb_is_nullable_when_not_set() {
    let pool = common::setup().await;

    let name = "No Prefs";
    let email = "no-prefs@example.com";
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let row = sql!(&pool, "SELECT preferences FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");
    assert!(row.preferences.is_none());
}

#[tokio::test]
async fn domain_jsonb_update_replaces_value() {
    let pool = common::setup().await;

    let name = "Pref Updater";
    let email = "pref-updater@example.com";
    let preferences = UserPreferences {
        theme: "light".into(),
        newsletter: false,
        daily_digest_limit: 1,
    };
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email, preferences) VALUES ($name, $email, $preferences) RETURNING id"
    )
    .fetch_one()
    .await
    .expect("insert");

    let id = inserted.id;
    let preferences = UserPreferences {
        theme: "solarized".into(),
        newsletter: true,
        daily_digest_limit: 20,
    };
    let updated = sql!(
        &pool,
        "UPDATE users SET preferences = $preferences WHERE id = $id"
    )
    .execute()
    .await
    .expect("update");
    assert_eq!(updated, 1);

    let after = sql!(&pool, "SELECT preferences FROM users WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");
    let got = after.preferences.expect("preferences set");
    assert_eq!(got.theme, "solarized");
    assert_eq!(got.daily_digest_limit, 20);
}

#[tokio::test]
async fn arrays_of_domains_are_read_as_arrays_of_their_base_type() {
    let pool = common::setup().await;
    let client = pool.get().await.expect("client");
    client
        .batch_execute(
            "INSERT INTO domain_arrays (id, nums, prefs) VALUES \
             (1, '{1,2,3}', ARRAY['{\"theme\": \"dark\", \"newsletter\": false, \
             \"daily_digest_limit\": 2}'::user_preferences]), \
             (2, '{}', NULL)",
        )
        .await
        .expect("insert");

    let rows = sql!(
        &pool,
        "SELECT id, nums, prefs FROM domain_arrays ORDER BY id"
    )
    .fetch_all()
    .await
    .expect("select");
    assert_eq!(rows[0].nums, [1, 2, 3]);
    assert_eq!(
        rows[0].prefs,
        Some(vec![UserPreferences {
            theme: "dark".into(),
            newsletter: false,
            daily_digest_limit: 2,
        }])
    );
    assert!(rows[1].nums.is_empty());
    assert_eq!(rows[1].prefs, None);
}
