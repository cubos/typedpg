//! `PgCatalog::query_view`: the text/tree lint rules read, with the query
//! simplifications skipped and offsets mapped back to what the user wrote.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE customers (id bigint PRIMARY KEY, name text);
         CREATE TABLE banned (customer_id bigint NOT NULL);",
    )
    .unwrap();
    db
}

#[test]
fn view_stays_as_written_and_maps_named_params() {
    let db = setup();
    let sql = "SELECT c.id FROM customers c WHERE c.name = $name \
               AND c.id NOT IN (SELECT customer_id FROM banned)";
    let view = db.query_view(sql).unwrap();

    // `analyze` rewrites `NOT IN (SELECT …)` into `NOT EXISTS`; the view
    // keeps it as written.
    assert!(!db.analyze(sql).unwrap().simplifications.is_empty());
    assert!(view.analyzed.simplifications.is_empty());
    assert!(
        view.sql.contains("NOT IN (SELECT customer_id FROM banned)"),
        "{}",
        view.sql
    );
    assert!(!view.sql.contains("NOT EXISTS"));
    // Named parameters became positional placeholders.
    assert!(view.sql.contains("c.name = $1"), "{}", view.sql);
    assert!(!view.sql.contains("$name"));

    // Offsets after the rewritten parameter map back to the original text.
    let in_view = view.sql.find("banned").unwrap();
    let in_original = sql.find("banned").unwrap();
    assert_ne!(in_view, in_original);
    assert_eq!(view.original_offset(in_view), in_original);
}

#[test]
fn view_materializes_spreads_and_maps_past_them() {
    let db = setup();
    let sql = "SELECT id FROM customers WHERE id IN $..ids AND name = 'x'";
    let view = db.query_view(sql).unwrap();
    assert!(view.sql.contains("IN ($1)"), "{}", view.sql);
    assert!(!view.sql.contains("$..ids"));

    let in_view = view.sql.find("name").unwrap();
    let in_original = sql.find("name").unwrap();
    assert_ne!(in_view, in_original);
    assert_eq!(view.original_offset(in_view), in_original);
}
