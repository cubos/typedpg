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
    let statuses = [PostStatus::Draft, PostStatus::Archived];
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

/// `x IN $..ids`: a placeholder per item. An empty list still runs the
/// query: `IN` it is false, `NOT IN` it true.
#[tokio::test]
async fn list_spread_expands_an_in_list() {
    let pool = common::setup().await;
    let tag = common::unique("in-list");
    let mut ids = Vec::new();
    for n in 0..3 {
        let name = format!("u{n}");
        let email = format!("{tag}-{n}@example.com");
        let id = sql!(
            &pool,
            "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id"
        )
        .fetch_value()
        .await
        .expect("insert user");
        ids.push(id);
    }
    let ours = format!("{tag}-%");

    let picked = [ids[0], ids[2]];
    let rows = sql!(
        &pool,
        "SELECT name FROM users WHERE id IN $..picked ORDER BY id"
    )
    .fetch_all()
    .await
    .expect("IN");
    assert_eq!(
        rows.into_iter().map(|r| r.name).collect::<Vec<_>>(),
        ["u0", "u2"]
    );
    let rows = sql!(
        &pool,
        "SELECT name FROM users WHERE email LIKE $ours AND id NOT IN $..picked ORDER BY id",
        ours = ours.clone()
    )
    .fetch_all()
    .await
    .expect("NOT IN");
    assert_eq!(rows.into_iter().map(|r| r.name).collect::<Vec<_>>(), ["u1"]);

    let none: Vec<i64> = Vec::new();
    let rows = sql!(&pool, "SELECT name FROM users WHERE id IN $..none")
        .fetch_all()
        .await
        .expect("IN an empty list");
    assert!(rows.is_empty());
    let rows = sql!(
        &pool,
        "SELECT name FROM users WHERE email LIKE $ours AND id NOT IN $..none ORDER BY id"
    )
    .fetch_all()
    .await
    .expect("NOT IN an empty list");
    assert_eq!(
        rows.into_iter().map(|r| r.name).collect::<Vec<_>>(),
        ["u0", "u1", "u2"]
    );

    // Text elements from an array of `String`s, and a regular parameter
    // numbered before the list's placeholders.
    let emails = [
        format!("{tag}-1@example.com"),
        format!("{tag}-2@example.com"),
    ];
    let min = ids[2];
    let rows = sql!(
        &pool,
        "SELECT name FROM users WHERE email IN $..emails AND id >= $min"
    )
    .fetch_all()
    .await
    .expect("text IN");
    assert_eq!(rows.into_iter().map(|r| r.name).collect::<Vec<_>>(), ["u2"]);
}

/// The elements are bound as the left side's type requires: a mapped enum
/// as its label.
#[tokio::test]
async fn list_spread_binds_enum_elements() {
    let pool = common::setup().await;
    let statuses = [PostStatus::Draft, PostStatus::Archived];
    let row = sql!(
        &pool,
        "SELECT 'archived'::post_status IN $..statuses AS \"hit!\", \
                'published'::post_status IN $..statuses AS \"miss!\""
    )
    .fetch_one()
    .await
    .expect("enum IN");
    assert!(row.hit);
    assert!(!row.miss);
}

struct Person {
    name: String,
    email: String,
}

struct Email {
    email: String,
}

/// A rows spread with no item still runs the query, as SQL would with no
/// row: an empty spread next to other rows is left out, and a VALUES list
/// of nothing but empty spreads is a SELECT of no row.
#[tokio::test]
async fn empty_rows_spreads_run_the_query() {
    let pool = common::setup().await;
    let tag = common::unique("empty-rows");
    let ours = format!("{tag}-%");
    let none: Vec<Person> = Vec::new();
    let two: Vec<Person> = (0..2)
        .map(|n| Person {
            name: format!("p{n}"),
            email: format!("{tag}-{n}@example.com"),
        })
        .collect();

    // Two inserts in CTEs, the first one empty: the second still inserts.
    let row = sql!(
        &pool,
        "WITH a AS (INSERT INTO users (name, email) VALUES $..none { name, email } RETURNING id), \
              b AS (INSERT INTO users (name, email) VALUES $..two { name, email } RETURNING id) \
         SELECT (SELECT count(*) FROM a) AS \"a!\", (SELECT count(*) FROM b) AS \"b!\""
    )
    .fetch_one()
    .await
    .expect("CTE inserts");
    assert_eq!((row.a, row.b), (0, 2));

    // A written row and an empty spread: the written row is inserted.
    let email = format!("{tag}-fixed@example.com");
    let rows = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES ('fixed', $email), $..none { name, email } RETURNING name"
    )
    .fetch_all()
    .await
    .expect("fixed row");
    assert_eq!(
        rows.into_iter().map(|r| r.name).collect::<Vec<_>>(),
        ["fixed"]
    );

    // Read from an empty VALUES: an aggregate still has its row, and
    // `NOT IN` it keeps every row.
    let emails: Vec<Email> = Vec::new();
    let n = sql!(
        &pool,
        "WITH v (email) AS (VALUES $..emails { email }) SELECT count(*) AS \"n!\" FROM v"
    )
    .fetch_value()
    .await
    .expect("count");
    assert_eq!(n, 0);
    let n = sql!(
        &pool,
        "WITH v (email) AS (VALUES $..emails { email }) \
         SELECT count(*) AS \"n!\" FROM users WHERE email LIKE $ours AND email NOT IN (SELECT email FROM v)",
        ours = ours.clone()
    )
    .fetch_value()
    .await
    .expect("NOT IN");
    assert_eq!(n, 3);

    // A plain INSERT of nothing: no row, 0 affected.
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..none { name, email }"
    )
    .execute()
    .await
    .expect("empty insert");
    assert_eq!(inserted, 0);
    let returned = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..none { name, email } \
         ON CONFLICT (email) DO NOTHING RETURNING id"
    )
    .fetch_optional()
    .await
    .expect("empty insert returning");
    assert!(returned.is_none());
}

/// Above 1000 items, a spread whose array form the analysis proved the
/// same query is bound as arrays: `= ANY($1)` for a list, `unnest` for
/// rows. So more values than PG's 65535 parameters fit, and the results
/// are those of the written form.
#[tokio::test]
async fn large_spreads_are_bound_as_arrays() {
    let pool = common::setup().await;
    let tag = common::unique("large");

    // 30 000 rows × 2 fields: 60 000 values, one `unnest` parameter each.
    let rows: Vec<Person> = (0..30_000)
        .map(|n| Person {
            name: format!("n{n}"),
            email: format!("{tag}-{n}@example.com"),
        })
        .collect();
    let inserted = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..rows { name, email } RETURNING id, name"
    )
    .fetch_all()
    .await
    .expect("unnest insert");
    // In the items' order, as VALUES would.
    assert!(
        inserted
            .iter()
            .map(|r| &r.name)
            .eq(rows.iter().map(|r| &r.name))
    );

    // 70 000 ids, more than the parameters a statement can have.
    let ids: Vec<i64> = inserted
        .iter()
        .map(|r| r.id)
        .chain((1..=40_000).map(|n| -n))
        .collect();
    let ours = format!("{tag}-%");
    let n = sql!(
        &pool,
        "SELECT count(*) AS \"n!\" FROM users WHERE id IN $..ids"
    )
    .fetch_value()
    .await
    .expect("= ANY");
    assert_eq!(n, 30_000);
    // NOT IN, with the IN the operand of `=`: `true = (id <> ALL(…))`.
    let some: Vec<i64> = inserted.iter().take(29_000).map(|r| r.id).collect();
    let n = sql!(
        &pool,
        "SELECT count(*) AS \"n!\" FROM users WHERE email LIKE $ours AND true = id NOT IN $..some",
        ours = ours.clone()
    )
    .fetch_value()
    .await
    .expect("<> ALL");
    assert_eq!(n, 1_000);

    // Enum elements, bound as an array of labels.
    let statuses: Vec<PostStatus> = (0..1_500)
        .map(|n| {
            if n % 2 == 0 {
                PostStatus::Draft
            } else {
                PostStatus::Archived
            }
        })
        .collect();
    let n = sql!(
        &pool,
        "SELECT count(*) AS \"n!\" FROM unnest(ARRAY['draft', 'published', 'archived']::post_status[]) s \
         WHERE s IN $..statuses"
    )
    .fetch_value()
    .await
    .expect("enum = ANY");
    assert_eq!(n, 2);

    // No array form where it would change the query (here, the column's
    // nullability): 2 000 placeholders, as written.
    let values: Vec<i32> = (0..2_000).collect();
    let hit = sql!(&pool, "SELECT 1999 IN $..values AS hit")
        .fetch_value()
        .await
        .expect("written list");
    assert!(hit);
}
