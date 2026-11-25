//! End-to-end coverage for anonymous records inside other values: arrays
//! of `ROW(…)`s (`array_agg`, `ARRAY[…]`, `ARRAY(SELECT …)`), scalar
//! subqueries, CASE / COALESCE, and rows nested in arrays nested in rows —
//! with fields of user types (an enum, a composite, a JSONB domain, a
//! composite mapped to a user struct, arrays and ranges of user types).
//!
//! PostgreSQL sends each as an anonymous record (or an array of them),
//! whose binary form carries every field's OID: these check the generated
//! structs decode it.

mod common;

use typedpg::sql;
use typedpg_e2e::{GeoPoint, PostStatus};

/// Insert two offices labelled `<prefix>-a` (with an org) and `<prefix>-b`
/// (without), and return `prefix` for the query to find them by.
async fn seed_offices(pool: &deadpool_postgres::Pool, prefix: &str) {
    let client = pool.get().await.expect("get client");
    client
        .execute(
            "INSERT INTO offices (label, addr, org) VALUES
                 ($1 || '-a', ROW('1 Rue', 'Paris', '75001'),
                  ROW('Acme', ROW('2 Ave', 'Lyon', NULL))),
                 ($1 || '-b', ROW('3 Blvd', 'Nice', '06000'), NULL)",
            &[&prefix],
        )
        .await
        .expect("seed offices");
}

#[tokio::test]
async fn array_agg_of_rows_with_composite_fields() {
    let pool = common::setup().await;
    let prefix = "nested-agg";
    seed_offices(&pool, prefix).await;
    let pattern = format!("{prefix}-%");

    let offices = sql!(
        &pool,
        "SELECT array_agg(ROW(label, addr, org) ORDER BY label) AS offices
         FROM offices WHERE label LIKE $pattern"
    )
    .fetch_value()
    .await
    .expect("array_agg of rows")
    .expect("two offices");

    assert_eq!(offices.len(), 2);
    assert_eq!(offices[0].f1, "nested-agg-a");
    assert_eq!(offices[0].f2.city.as_deref(), Some("Paris"));
    let org = offices[0].f3.as_ref().expect("org");
    assert_eq!(org.name.as_deref(), Some("Acme"));
    let hq = org.hq.as_ref().expect("hq");
    assert_eq!(hq.city.as_deref(), Some("Lyon"));
    assert_eq!(hq.zip, None);
    assert_eq!(offices[1].f1, "nested-agg-b");
    assert!(offices[1].f3.is_none());

    // No row: array_agg is NULL.
    let none = sql!(
        &pool,
        "SELECT array_agg((label, addr)) AS offices FROM offices WHERE label = 'no such office'"
    )
    .fetch_value()
    .await
    .expect("empty array_agg");
    assert!(none.is_none());
}

#[tokio::test]
async fn array_constructor_and_sublink_of_rows() {
    let pool = common::setup().await;

    let pairs = sql!(
        &pool,
        "SELECT ARRAY[ROW(1, 'x'::text), ROW(2, NULL::text)] AS pairs"
    )
    .fetch_value()
    .await
    .expect("ARRAY[ROW(…)]");
    assert_eq!(pairs.len(), 2);
    assert_eq!((pairs[0].f1, pairs[0].f2.as_deref()), (1, Some("x")));
    assert_eq!((pairs[1].f1, pairs[1].f2.as_deref()), (2, None));

    // A NULL element: `Vec<Option<_>>`.
    let with_null = sql!(&pool, "SELECT ARRAY[ROW(1, 'x'::text), NULL] AS pairs")
        .fetch_value()
        .await
        .expect("ARRAY[ROW(…), NULL]");
    assert_eq!(with_null.len(), 2);
    assert_eq!(with_null[0].as_ref().map(|p| p.f1), Some(1));
    assert!(with_null[1].is_none());

    let prefix = "nested-sublink";
    seed_offices(&pool, prefix).await;
    let pattern = format!("{prefix}-%");
    let cities = sql!(
        &pool,
        "SELECT ARRAY(
             SELECT ROW(label, (addr).city) FROM offices
             WHERE label LIKE $pattern ORDER BY label
         ) AS cities"
    )
    .fetch_value()
    .await
    .expect("ARRAY(SELECT ROW(…))");
    let cities: Vec<(String, Option<String>)> = cities.into_iter().map(|r| (r.f1, r.f2)).collect();
    assert_eq!(
        cities,
        vec![
            ("nested-sublink-a".to_owned(), Some("Paris".to_owned())),
            ("nested-sublink-b".to_owned(), Some("Nice".to_owned())),
        ]
    );
}

#[tokio::test]
async fn rows_with_enum_jsonb_domain_and_mapped_composite_fields() {
    let pool = common::setup().await;
    let client = pool.get().await.expect("get client");
    let label = "nested-tagged";
    client
        .execute(
            "INSERT INTO tagged_rows (t) VALUES
                 (ROW($1, 'archived',
                      '{\"theme\":\"x\",\"newsletter\":false,\"daily_digest_limit\":1}'))",
            &[&label],
        )
        .await
        .expect("seed tagged");
    let name = "nested-peak";
    client
        .execute(
            "INSERT INTO landmarks (name, location) VALUES ($1, ROW(1.5, -2.5))",
            &[&name],
        )
        .await
        .expect("seed landmark");

    let tagged = sql!(
        &pool,
        "SELECT array_agg(ROW((t).status, (t).prefs)) AS tagged
         FROM tagged_rows WHERE (t).label = $label"
    )
    .fetch_value()
    .await
    .expect("rows of an enum and a jsonb domain")
    .expect("one row");
    assert_eq!(tagged.len(), 1);
    assert_eq!(tagged[0].f1, Some(PostStatus::Archived));
    let prefs = tagged[0].f2.as_ref().expect("prefs");
    assert_eq!(prefs.daily_digest_limit, 1);

    let places = sql!(
        &pool,
        "SELECT ARRAY(SELECT ROW(name, location) FROM landmarks WHERE name = $name) AS places"
    )
    .fetch_value()
    .await
    .expect("rows of a mapped composite");
    assert_eq!(places.len(), 1);
    let location: &GeoPoint = &places[0].f2;
    assert_eq!((location.x, location.y), (Some(1.5), Some(-2.5)));
}

#[tokio::test]
async fn rows_of_arrays_and_ranges_of_user_types() {
    use std::ops::Bound;
    use typedpg::types::Range;

    let pool = common::setup().await;
    let rows = sql!(
        &pool,
        "SELECT ARRAY[ROW(
             ARRAY['draft', 'published']::post_status[],
             '[1.5,2.5)'::floatrange,
             ARRAY[ROW('1 Main St', 'Paris', NULL)::address],
             ARRAY[1, 2]::positive_int[]
         )] AS r"
    )
    .fetch_value()
    .await
    .expect("rows of user-typed arrays");
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.f1, vec![PostStatus::Draft, PostStatus::Published]);
    assert_eq!(r.f2, Range::new(Bound::Included(1.5), Bound::Excluded(2.5)));
    assert_eq!(r.f3[0].city.as_deref(), Some("Paris"));
    assert_eq!(r.f4, vec![1, 2]);
}

#[tokio::test]
async fn rows_nested_in_arrays_nested_in_rows() {
    let pool = common::setup().await;
    let r = sql!(
        &pool,
        "SELECT ROW(
             1,
             ARRAY[ROW('published'::post_status, ARRAY[ROW(2.5::float8, 'x'::text)])]
         ) AS r"
    )
    .fetch_value()
    .await
    .expect("nested rows and arrays");
    assert_eq!(r.f1, 1);
    assert_eq!(r.f2.len(), 1);
    assert_eq!(r.f2[0].f1, PostStatus::Published);
    assert_eq!(r.f2[0].f2.len(), 1);
    assert_eq!(r.f2[0].f2[0].f1, 2.5);
    assert_eq!(r.f2[0].f2[0].f2, "x");
}

#[tokio::test]
async fn scalar_subquery_case_and_coalesce_of_rows() {
    let pool = common::setup().await;
    let prefix = "nested-scalar";
    seed_offices(&pool, prefix).await;
    let label = format!("{prefix}-a");

    let office = sql!(
        &pool,
        "SELECT (SELECT ROW(label, addr) FROM offices WHERE label = $label) AS office",
        label = label.as_str()
    )
    .fetch_value()
    .await
    .expect("scalar subquery of a row")
    .expect("found");
    assert_eq!(office.f1, label);
    assert_eq!(office.f2.street.as_deref(), Some("1 Rue"));

    let missing = "no such office";
    let fallback = sql!(
        &pool,
        "SELECT COALESCE(
             (SELECT ROW(label, (addr).city) FROM offices WHERE label = $missing),
             ROW('none'::text, NULL::text)
         ) AS office"
    )
    .fetch_value()
    .await
    .expect("COALESCE of rows");
    assert_eq!((fallback.f1.as_str(), fallback.f2), ("none", None));

    let flag = true;
    let picked = sql!(
        &pool,
        "SELECT CASE WHEN $flag::bool THEN ROW(1, 'yes'::text) ELSE ROW(0, 'no'::text) END AS r"
    )
    .fetch_value()
    .await
    .expect("CASE of rows");
    assert_eq!((picked.f1, picked.f2.as_str()), (1, "yes"));

    // An element taken out of an array of rows.
    let first = sql!(&pool, "SELECT (ARRAY[ROW(7, 'seven'::text)])[1] AS r")
        .fetch_value()
        .await
        .expect("subscript of an array of rows")
        .expect("in range");
    assert_eq!((first.f1, first.f2.as_str()), (7, "seven"));
}
