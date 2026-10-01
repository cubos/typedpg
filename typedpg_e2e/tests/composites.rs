//! End-to-end coverage for composite (record) types and the unified
//! `[package.metadata.typedpg.types]` resolution.
//!
//! Exercises:
//!
//! - a plain composite column decoded into a synthesized struct;
//! - a composite nested inside another composite;
//! - a nullable composite column;
//! - an anonymous `ROW(...)` value;
//! - a composite read through a subquery in `FROM`;
//! - a composite pointed at a user struct via `[types]` (`geo_point`),
//!   rebuilt field-by-field;
//! - a composite whose own fields are customised types (an enum and a JSONB
//!   domain);
//! - every base kind reached *through* a domain — and through a domain over a
//!   domain: composite, enum, and a JSONB domain.

mod common;

use typedpg::sql;
use typedpg_e2e::{GeoPoint, PostStatus};

/// Ids of the rows seeded into each table for a single test.
struct Ids {
    office: i64,
    warehouse: i64,
    landmark: i64,
    tagged: i64,
    domained: i64,
}

/// Seed every composite table with raw SQL so the tests can focus on the
/// *read* path. Every test of the run shares the database, so the inserts
/// accumulate: tests look their rows up by the ids returned here.
async fn seed(pool: &deadpool_postgres::Pool) -> Ids {
    let client = pool.get().await.expect("get client");

    let office: i64 = client
        .query_one(
            "INSERT INTO offices (label, addr, org) VALUES
                 ('main',
                  ROW('1 Rue', 'Paris', '75001'),
                  ROW('Acme', ROW('2 Ave', 'Lyon', '69001')))
             RETURNING id",
            &[],
        )
        .await
        .expect("seed office")
        .get(0);

    let warehouse: i64 = client
        .query_one(
            "INSERT INTO offices (label, addr, org) VALUES
                 ('warehouse', ROW('3 Blvd', 'Nice', '06000'), NULL)
             RETURNING id",
            &[],
        )
        .await
        .expect("seed warehouse")
        .get(0);

    let landmark: i64 = client
        .query_one(
            "INSERT INTO landmarks (name, location) VALUES ('peak', ROW(12.5, -7.25))
             RETURNING id",
            &[],
        )
        .await
        .expect("seed landmark")
        .get(0);

    let tagged: i64 = client
        .query_one(
            "INSERT INTO tagged_rows (t) VALUES
                 (ROW('vip', 'archived',
                      '{\"theme\":\"x\",\"newsletter\":false,\"daily_digest_limit\":1}'))
             RETURNING id",
            &[],
        )
        .await
        .expect("seed tagged")
        .get(0);

    let domained: i64 = client
        .query_one(
            "INSERT INTO domained (addr1, addr2, status, prefs) VALUES
                 (ROW('9 Lane', 'Tours', '37000'),
                  ROW('5 Way', 'Metz', '57000'),
                  'published',
                  '{\"theme\":\"dark\",\"newsletter\":true,\"daily_digest_limit\":9}')
             RETURNING id",
            &[],
        )
        .await
        .expect("seed domained")
        .get(0);

    Ids {
        office,
        warehouse,
        landmark,
        tagged,
        domained,
    }
}

#[tokio::test]
async fn select_plain_composite_column() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let office_id = ids.office;

    let row = sql!(
        &pool,
        "SELECT id, label, addr FROM offices WHERE id = $office_id"
    )
    .fetch_one()
    .await
    .expect("select composite column");

    // `addr` is decoded into a synthesized struct with one field per
    // composite attribute. PG composite attributes are always nullable.
    assert_eq!(row.label, "main");
    assert_eq!(row.addr.street.as_deref(), Some("1 Rue"));
    assert_eq!(row.addr.city.as_deref(), Some("Paris"));
    assert_eq!(row.addr.zip.as_deref(), Some("75001"));
}

#[tokio::test]
async fn select_composite_nested_in_composite() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let office_id = ids.office;

    let row = sql!(&pool, "SELECT org FROM offices WHERE id = $office_id")
        .fetch_one()
        .await
        .expect("select nested composite");

    // `org` is a nullable `company` composite; `company.hq` is itself an
    // `address` composite — the synthesized structs nest.
    let org = row.org.expect("org is populated");
    assert_eq!(org.name.as_deref(), Some("Acme"));
    let hq = org.hq.expect("hq is populated");
    assert_eq!(hq.street.as_deref(), Some("2 Ave"));
    assert_eq!(hq.city.as_deref(), Some("Lyon"));
    assert_eq!(hq.zip.as_deref(), Some("69001"));
}

#[tokio::test]
async fn nullable_composite_column_is_none() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;

    let warehouse_id = ids.warehouse;
    let row = sql!(&pool, "SELECT org FROM offices WHERE id = $warehouse_id")
        .fetch_one()
        .await
        .expect("select null composite");

    assert!(row.org.is_none(), "warehouse has no org");
}

#[tokio::test]
async fn select_anonymous_row_constructor() {
    let pool = common::setup().await;
    let _ = seed(&pool).await;

    let id: i64 = 42;
    let label = "answer";
    let row = sql!(&pool, "SELECT ROW($id::int8, $label::text) AS pair")
        .fetch_one()
        .await
        .expect("select ROW(...)");

    // An anonymous record surfaces as a synthesized struct with positional
    // field names `f1`, `f2`, ….
    assert_eq!(row.pair.f1, 42);
    assert_eq!(row.pair.f2, "answer");
}

#[tokio::test]
async fn select_composite_through_subquery_in_from() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let office_id = ids.office;

    let row = sql!(
        &pool,
        "SELECT sub.addr
         FROM (SELECT id, addr FROM offices) AS sub
         WHERE sub.id = $office_id"
    )
    .fetch_one()
    .await
    .expect("select composite via subquery");

    assert_eq!(row.addr.city.as_deref(), Some("Paris"));
}

#[tokio::test]
async fn composite_with_types_override_builds_user_struct() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let landmark_id = ids.landmark;

    let row = sql!(
        &pool,
        "SELECT name, location FROM landmarks WHERE id = $landmark_id"
    )
    .fetch_one()
    .await
    .expect("select overridden composite");

    // `geo_point` is pointed at `GeoPoint` via `[types]`; the macro rebuilds
    // it field-by-field from the decoded record.
    let location: GeoPoint = row.location;
    assert_eq!(
        location,
        GeoPoint {
            x: Some(12.5),
            y: Some(-7.25)
        }
    );
}

#[tokio::test]
async fn insert_with_row_constructor_then_read_back() {
    let pool = common::setup().await;
    let _ = seed(&pool).await;

    let unique_label = common::unique("annex");
    let label = unique_label.as_str();
    let street = "9 Lane";
    let city = "Tours";
    let zip = "37000";
    sql!(
        &pool,
        "INSERT INTO offices (label, addr)
         VALUES ($label, ROW($street::text, $city::text, $zip::text)::address)"
    )
    .execute()
    .await
    .expect("insert with ROW(...)");

    let row = sql!(&pool, "SELECT addr FROM offices WHERE label = $label")
        .fetch_one()
        .await
        .expect("read back inserted composite");

    assert_eq!(row.addr.street.as_deref(), Some("9 Lane"));
    assert_eq!(row.addr.city.as_deref(), Some("Tours"));
    assert_eq!(row.addr.zip.as_deref(), Some("37000"));
}

#[tokio::test]
async fn composite_with_customised_fields() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let tagged_id = ids.tagged;

    let row = sql!(&pool, "SELECT t FROM tagged_rows WHERE id = $tagged_id")
        .fetch_one()
        .await
        .expect("select composite with customised fields");

    // `tagged` has an enum field (`status`) and a JSONB-domain field
    // (`prefs`) — the synthesized struct decodes each through its own bridge.
    let t = row.t;
    assert_eq!(t.label.as_deref(), Some("vip"));
    assert_eq!(t.status, Some(PostStatus::Archived));
    let prefs = t.prefs.expect("prefs populated");
    assert_eq!(prefs.theme, "x");
    assert!(!prefs.newsletter);
    assert_eq!(prefs.daily_digest_limit, 1);
}

#[tokio::test]
async fn composite_through_domain() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let domained_id = ids.domained;

    // `addr1` is declared `address_dom` — a domain over the `address`
    // composite. The domain is transparent: it decodes to the same
    // synthesized struct a bare `address` column would.
    let row = sql!(&pool, "SELECT addr1 FROM domained WHERE id = $domained_id")
        .fetch_one()
        .await
        .expect("select composite through domain");

    assert_eq!(row.addr1.street.as_deref(), Some("9 Lane"));
    assert_eq!(row.addr1.city.as_deref(), Some("Tours"));
}

#[tokio::test]
async fn composite_through_domain_of_domain() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let domained_id = ids.domained;

    // `addr2` is `address_dom2` — a domain over `address_dom` over `address`.
    // The resolution walk peels both domains.
    let row = sql!(&pool, "SELECT addr2 FROM domained WHERE id = $domained_id")
        .fetch_one()
        .await
        .expect("select composite through domain-of-domain");

    assert_eq!(row.addr2.street.as_deref(), Some("5 Way"));
    assert_eq!(row.addr2.city.as_deref(), Some("Metz"));
}

#[tokio::test]
async fn enum_through_domain() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let domained_id = ids.domained;

    // `status` is `status_dom` — a domain over the `post_status` enum, which
    // `[types]` maps to `PostStatus`.
    let row = sql!(&pool, "SELECT status FROM domained WHERE id = $domained_id")
        .fetch_one()
        .await
        .expect("select enum through domain");

    assert_eq!(row.status, Some(PostStatus::Published));
}

#[tokio::test]
async fn jsonb_through_domain_of_domain() {
    let pool = common::setup().await;
    let ids = seed(&pool).await;
    let domained_id = ids.domained;

    // `prefs` is `prefs_dom` — a domain over `user_preferences`, which is
    // itself a domain over `jsonb`. The walk reaches `user_preferences` in
    // `[types]` and the innermost `jsonb` selects the serde bridge.
    let row = sql!(&pool, "SELECT prefs FROM domained WHERE id = $domained_id")
        .fetch_one()
        .await
        .expect("select jsonb through domain-of-domain");

    let prefs = row.prefs.expect("prefs populated");
    assert_eq!(prefs.theme, "dark");
    assert!(prefs.newsletter);
    assert_eq!(prefs.daily_digest_limit, 9);
}
