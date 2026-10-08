mod common;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use typedpg::sql;
use uuid::Uuid;

#[tokio::test]
async fn uuid_primary_key_and_client_generated_id() {
    let pool = common::setup().await;

    let id = Uuid::new_v4();
    let name = "Gadget";
    let tags = ["new", "featured"];
    let price = Decimal::from_str("9.99").unwrap();

    sql!(
        &pool,
        "INSERT INTO items (id, name, tags, price) VALUES ($id, $name, $tags, $price)"
    )
    .execute()
    .await
    .expect("insert");

    let row = sql!(
        &pool,
        "SELECT id, name, tags, price, created_at FROM items WHERE id = $id"
    )
    .fetch_one()
    .await
    .expect("select");

    let _uuid_check: Uuid = row.id;
    let _tags_check: &Vec<String> = &row.tags;
    let _price_check: Decimal = row.price;
    let _created_check: DateTime<Utc> = row.created_at;

    assert_eq!(row.id, id);
    assert_eq!(row.name, "Gadget");
    assert_eq!(row.tags, vec!["new".to_string(), "featured".to_string()]);
    assert_eq!(row.price, Decimal::from_str("9.99").unwrap());
}

#[tokio::test]
async fn array_any_filter() {
    let pool = common::setup().await;

    for (name, tags) in [
        ("widget-a", vec!["sale".to_string(), "popular".to_string()]),
        ("widget-b", vec!["sale".to_string()]),
        ("widget-c", vec!["clearance".to_string()]),
    ] {
        let id = Uuid::new_v4();
        let price = Decimal::from_str("1.00").unwrap();
        sql!(
            &pool,
            "INSERT INTO items (id, name, tags, price) VALUES ($id, $name, $tags, $price)"
        )
        .execute()
        .await
        .expect("insert");
    }

    let wanted = "sale";
    let rows = sql!(
        &pool,
        "SELECT name, tags FROM items WHERE $wanted = ANY(tags) ORDER BY name"
    )
    .fetch_all()
    .await
    .expect("any filter");

    let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"widget-a"));
    assert!(names.contains(&"widget-b"));
    assert!(!names.contains(&"widget-c"));
}

#[tokio::test]
async fn timestamptz_roundtrip() {
    let pool = common::setup().await;

    let id = Uuid::new_v4();
    let name = "DatedItem";
    let tags: [&str; 0] = [];
    let price = Decimal::from_str("0.01").unwrap();
    sql!(
        &pool,
        "INSERT INTO items (id, name, tags, price) VALUES ($id, $name, $tags, $price)"
    )
    .execute()
    .await
    .expect("insert");

    let row = sql!(&pool, "SELECT created_at FROM items WHERE id = $id")
        .fetch_one()
        .await
        .expect("select");

    let now = Utc::now();
    let diff = now.signed_duration_since(row.created_at);
    assert!(
        diff.num_seconds().abs() < 10,
        "created_at too far from now: {diff}"
    );
}

#[tokio::test]
async fn a_date_or_timestamp_parameter_is_finite() {
    let pool = common::setup().await;

    // chrono's dates and timestamps can't be `infinity`: formatting or
    // extracting from one is never NULL.
    let at = DateTime::parse_from_rfc3339("2024-02-29T12:34:56Z")
        .unwrap()
        .with_timezone(&Utc);
    let day = at.date_naive();
    let local = at.naive_utc();
    let row = sql!(
        &pool,
        "SELECT to_char($at::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS a,
                extract(month FROM $day::date) AS b, to_char($local::timestamp, 'HH24:MI') AS c"
    )
    .fetch_one()
    .await
    .expect("select");
    let (a, b, c): (String, Decimal, String) = (row.a, row.b, row.c);
    assert_eq!(
        (a.as_str(), b, c.as_str()),
        ("2024-02-29", Decimal::from(2), "12:34")
    );
}
