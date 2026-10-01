mod common;

use rust_decimal::Decimal;
use typedpg::{copy_in, sql};
use typedpg_e2e::{PostStatus, UserPreferences};

struct NewUser {
    name: String,
    email: String,
    age: Option<i32>,
}

#[tokio::test]
async fn copy_in_loads_rows_and_counts_them() {
    let pool = common::setup().await;
    let tag = common::unique("copy");
    let users: Vec<NewUser> = (0..1000)
        .map(|i| NewUser {
            name: format!("copy-{i}"),
            email: format!("{tag}-{i}@example.com"),
            age: (i % 2 == 0).then_some(i),
        })
        .collect();

    let copied = copy_in!(
        &pool,
        "users (name, email, age)",
        users { name, email, age }
    )
    .await
    .expect("copy");
    assert_eq!(copied, 1000);

    let prefix = format!("{tag}-%@example.com");
    let stats = sql!(
        &pool,
        "SELECT count(*) AS n, count(age) AS with_age FROM users WHERE email LIKE $prefix"
    )
    .fetch_one()
    .await
    .expect("count");
    assert_eq!((stats.n, stats.with_age), (1000, 500));
}

#[tokio::test]
async fn copy_in_takes_any_iterator_lazily() {
    let pool = common::setup().await;
    let rows = (0..3).map(|i| NewUser {
        name: format!("lazy-{i}"),
        email: format!("lazy-{i}@example.com"),
        age: None,
    });
    let copied = copy_in!(&pool, "users (name, email)", rows { name, email })
        .await
        .expect("copy");
    assert_eq!(copied, 3);
}

struct Target {
    id: i32,
    status: Option<PostStatus>,
    statuses: Vec<PostStatus>,
    pref: Option<UserPreferences>,
    prefs: Option<Vec<UserPreferences>>,
    tags: Vec<String>,
    amount: Option<Decimal>,
}

#[tokio::test]
async fn enums_domains_and_their_arrays_round_trip() {
    let pool = common::setup().await;
    let pref = |theme: &str| UserPreferences {
        theme: theme.into(),
        newsletter: true,
        daily_digest_limit: 3,
    };
    let (id1, id2) = (
        common::unique_id(&pool).await,
        common::unique_id(&pool).await,
    );
    let rows = vec![
        Target {
            id: id1,
            status: Some(PostStatus::Published),
            statuses: vec![PostStatus::Draft, PostStatus::Archived],
            pref: Some(pref("dark")),
            prefs: Some(vec![pref("a"), pref("b")]),
            tags: vec!["x".into(), "y".into()],
            amount: Some(Decimal::new(1234, 2)),
        },
        Target {
            id: id2,
            status: None,
            statuses: vec![],
            pref: None,
            prefs: None,
            tags: vec![],
            amount: None,
        },
    ];
    let copied = copy_in!(
        &pool,
        "copy_targets (id, status, statuses, pref, prefs, tags, amount)",
        &rows {
            id,
            status,
            statuses,
            pref,
            prefs,
            tags,
            amount
        }
    )
    .await
    .expect("copy");
    assert_eq!(copied, 2);

    let back = sql!(
        &pool,
        "SELECT id, status, statuses, pref, prefs, tags, amount, doubled \
         FROM copy_targets WHERE id IN ($id1, $id2) ORDER BY id"
    )
    .fetch_all()
    .await
    .expect("read back");
    assert_eq!(back.len(), 2);
    let first = &back[0];
    assert_eq!(first.status, Some(PostStatus::Published));
    assert_eq!(first.statuses, [PostStatus::Draft, PostStatus::Archived]);
    assert_eq!(first.pref, Some(pref("dark")));
    assert_eq!(first.prefs, Some(vec![pref("a"), pref("b")]));
    assert_eq!(first.tags, ["x", "y"]);
    assert_eq!(first.amount, Some(Decimal::new(1234, 2)));
    assert_eq!(
        first.doubled,
        Some(id1 * 2),
        "generated columns are computed"
    );
    let second = &back[1];
    assert_eq!(
        (second.status, second.pref.clone(), second.prefs.clone()),
        (None, None, None)
    );
    assert!(second.statuses.is_empty() && second.tags.is_empty());
}

#[tokio::test]
async fn a_failing_row_aborts_the_whole_copy() {
    let pool = common::setup().await;
    let users = [
        NewUser {
            name: "dup-1".into(),
            email: "copy-dup@example.com".into(),
            age: None,
        },
        NewUser {
            name: "dup-2".into(),
            email: "copy-dup@example.com".into(),
            age: None,
        },
    ];
    let err = copy_in!(&pool, "users (name, email)", users { name, email })
        .await
        .expect_err("duplicate email");
    assert!(err.to_string().contains("users_email_key"), "{err}");
    let email = "copy-dup@example.com";
    let n = sql!(
        &pool,
        "SELECT count(*) AS n FROM users WHERE email = $email"
    )
    .fetch_value()
    .await
    .expect("count");
    assert_eq!(n, 0, "no row of the aborted COPY is kept");
}

#[tokio::test]
async fn copy_in_runs_inside_a_transaction() {
    let pool = common::setup().await;
    let mut client = pool.get().await.expect("client");
    let tx = client.transaction().await.expect("begin");
    let users = [NewUser {
        name: "tx".into(),
        email: "copy-tx@example.com".into(),
        age: None,
    }];
    let copied = copy_in!(&tx, "users (name, email)", users { name, email })
        .await
        .expect("copy");
    assert_eq!(copied, 1);
    tx.rollback().await.expect("rollback");

    let email = "copy-tx@example.com";
    let n = sql!(
        &pool,
        "SELECT count(*) AS n FROM users WHERE email = $email"
    )
    .fetch_value()
    .await
    .expect("count");
    assert_eq!(n, 0, "rolled back with the transaction");
}
