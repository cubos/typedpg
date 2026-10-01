mod common;

use typedpg::sql;
use typedpg_e2e::{PostStatus, UserPreferences};

struct Target {
    id: i32,
    status: PostStatus,
    maybe_status: Option<PostStatus>,
    statuses: Option<Vec<PostStatus>>,
    pref: UserPreferences,
    maybe_pref: Option<UserPreferences>,
    prefs: Option<Vec<UserPreferences>>,
    num: Option<i32>,
    nums: Option<Vec<i32>>,
}

fn pref(theme: &str) -> UserPreferences {
    UserPreferences {
        theme: theme.into(),
        newsletter: false,
        daily_digest_limit: 1,
    }
}

#[tokio::test]
async fn spread_binds_enum_domain_and_array_fields() {
    let pool = common::setup().await;
    let targets = [
        Target {
            id: common::unique_id(&pool).await,
            status: PostStatus::Published,
            maybe_status: Some(PostStatus::Archived),
            statuses: Some(vec![PostStatus::Draft, PostStatus::Published]),
            pref: pref("dark"),
            maybe_pref: Some(pref("light")),
            prefs: Some(vec![pref("a"), pref("b")]),
            num: Some(3),
            nums: Some(vec![1, 2]),
        },
        Target {
            id: common::unique_id(&pool).await,
            status: PostStatus::Draft,
            maybe_status: None,
            statuses: None,
            pref: pref("plain"),
            maybe_pref: None,
            prefs: None,
            num: None,
            nums: None,
        },
    ];
    // A regular enum parameter alongside the spread.
    let min_status = PostStatus::Draft;
    let rows = sql!(
        &pool,
        "INSERT INTO spread_targets \
         VALUES $..targets { id, status, maybe_status, statuses, pref, maybe_pref, prefs, num, nums } \
         RETURNING id, status, maybe_status, statuses, pref, maybe_pref, prefs, num, nums, \
             status > $min_status AS \"past_draft!\""
    )
    .fetch_all()
    .await
    .expect("spread insert");
    assert_eq!(rows.len(), 2);
    for (row, t) in rows.iter().zip(&targets) {
        assert_eq!(row.id, t.id);
        assert_eq!(row.past_draft, t.status != PostStatus::Draft);
        assert_eq!(row.status, t.status);
        assert_eq!(row.maybe_status, t.maybe_status);
        assert_eq!(row.statuses, t.statuses);
        assert_eq!(row.pref, t.pref);
        assert_eq!(row.maybe_pref, t.maybe_pref);
        assert_eq!(row.prefs, t.prefs);
        assert_eq!(row.num, t.num);
        assert_eq!(row.nums, t.nums);
    }

    // A domain's CHECK still applies to a spread value.
    let bad = [Target {
        id: common::unique_id(&pool).await,
        num: Some(0),
        ..targets_template()
    }];
    let err = sql!(
        &pool,
        "INSERT INTO spread_targets (id, status, pref, num) \
         VALUES $..bad { id, status, pref, num }"
    )
    .execute()
    .await
    .expect_err("positive_int rejects 0");
    assert!(err.to_string().contains("positive_int"), "{err}");
}

fn targets_template() -> Target {
    Target {
        id: 0,
        status: PostStatus::Draft,
        maybe_status: None,
        statuses: None,
        pref: pref("t"),
        maybe_pref: None,
        prefs: None,
        num: None,
        nums: None,
    }
}

#[tokio::test]
async fn regular_enum_array_params_bind() {
    let pool = common::setup().await;
    let statuses = vec![PostStatus::Draft, PostStatus::Archived];
    let row = sql!(
        &pool,
        "SELECT $statuses::post_status[] AS echoed, 'archived'::post_status = ANY($statuses) AS hit"
    )
    .fetch_one()
    .await
    .expect("enum array param");
    assert_eq!(row.echoed, [PostStatus::Draft, PostStatus::Archived]);
    assert_eq!(row.hit, Some(true));
}

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
