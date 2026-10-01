//! A value a column's Rust type can't hold is an `Error::Deserialize`
//! naming the column, never a panic.

mod common;

use typedpg::sql;

fn deserialize_message(err: typedpg::Error) -> String {
    match err {
        typedpg::Error::Deserialize(msg) => msg,
        other => panic!("expected a deserialization error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_value_the_rust_type_cant_hold_names_the_column() {
    let pool = common::setup().await;
    let err = sql!(&pool, "SELECT 'NaN'::numeric AS ratio")
        .fetch_one()
        .await
        .expect_err("NaN has no rust_decimal::Decimal");
    let msg = deserialize_message(err);
    assert!(msg.starts_with("column \"ratio\" (numeric): "), "{msg}");
}

#[tokio::test]
async fn a_null_in_a_column_forced_non_null_is_an_error() {
    let pool = common::setup().await;
    let err = sql!(&pool, r#"SELECT NULL::int4 AS "n!""#)
        .fetch_one()
        .await
        .expect_err("NULL can't be an i32");
    let msg = deserialize_message(err);
    assert!(
        msg.starts_with("column \"n\" (int4) is NULL, but its Rust type is not an Option"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_null_element_of_a_table_array_column_is_an_error() {
    let pool = common::setup().await;
    let name = &common::unique("decode-null-tag");
    sql!(
        &pool,
        "INSERT INTO items (name, tags, price) VALUES ($name, ARRAY['a', NULL], 1)"
    )
    .execute()
    .await
    .expect("insert");
    let err = sql!(&pool, "SELECT tags FROM items WHERE name = $name")
        .fetch_one()
        .await
        .expect_err("a table's text[] column reads as Vec<String>");
    let msg = deserialize_message(err);
    assert!(
        msg.starts_with("column \"tags\" (text[]) holds a NULL array element"),
        "{msg}"
    );
}

#[tokio::test]
async fn a_jsonb_domain_value_that_doesnt_deserialize_names_the_column() {
    let pool = common::setup().await;
    let err = sql!(
        &pool,
        r#"SELECT '{"theme": 1}'::user_preferences AS "prefs!""#
    )
    .fetch_one()
    .await
    .expect_err("theme must be a string");
    let msg = deserialize_message(err);
    assert!(
        msg.starts_with(
            "column \"prefs\" (public.user_preferences): failed to deserialize ::typedpg_e2e::UserPreferences: "
        ),
        "{msg}"
    );
}

#[tokio::test]
async fn from_row_errors_name_the_field() {
    #[derive(typedpg::FromRow, Debug)]
    #[allow(dead_code)]
    struct Ratio {
        ratio: i32,
    }

    let pool = common::setup().await;
    let client = pool.get().await.expect("client");
    let row = client
        .query_one("SELECT 'x'::text AS ratio", &[])
        .await
        .expect("query");
    let err = <Ratio as typedpg::FromRow>::from_row(&row).expect_err("text is no i32");
    let msg = deserialize_message(err);
    assert_eq!(
        msg,
        "column \"ratio\" (text): cannot convert between the Rust type `i32` and the Postgres \
         type `text`"
    );
}
