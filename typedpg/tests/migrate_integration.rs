//! The migration runner against a real PostgreSQL: the server
//! `typedpg_test_support` shares across the test run, one fresh database
//! per test.

use std::fs;
use std::path::Path;

use tokio_postgres::Client;
use typedpg::migrate::{self, MigrationSource};
use typedpg_core::config::MigrationsConfig;

/// A connection to a fresh, empty database, and that database's name.
async fn fresh_db() -> (Client, String) {
    let server = typedpg_test_support::server();
    let db = server.create_database("migrate").await;
    (server.connect(&db).await, db)
}

async fn table_exists(client: &Client, name: &str) -> bool {
    client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&name])
        .await
        .unwrap()
        .get(0)
}

async fn recorded(client: &Client, table: &str) -> Vec<String> {
    client
        .query(&format!("SELECT name FROM {table} ORDER BY name"), &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

fn write(dir: &Path, file: &str, sql: &str) {
    fs::write(dir.join(file), sql).unwrap();
}

fn create_test_migrations(dir: &Path) {
    write(
        dir,
        "20260318120000_create_users.sql",
        "CREATE TABLE users (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
        );",
    );
    write(
        dir,
        "20260319120000_create_orders.sql",
        "CREATE TABLE orders (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id BIGINT NOT NULL REFERENCES users(id)
        );",
    );
}

fn create_test_migrations_with_down(dir: &Path) {
    create_test_migrations(dir);
    write(
        dir,
        "20260318120000_create_users.down.sql",
        "DROP TABLE users;",
    );
    write(
        dir,
        "20260319120000_create_orders.down.sql",
        "DROP TABLE orders;",
    );
}

const USERS: &str = "20260318120000_create_users";
const ORDERS: &str = "20260319120000_create_orders";

#[tokio::test]
async fn run_applies_all_pending_migrations() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();

    let applied = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap();

    assert_eq!(applied, [USERS, ORDERS]);
    assert!(table_exists(&client, "users").await);
    assert!(table_exists(&client, "orders").await);
}

#[tokio::test]
async fn run_is_idempotent() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let first = migrate::run(&mut client, &source, &config).await.unwrap();
    assert_eq!(first.len(), 2);
    let second = migrate::run(&mut client, &source, &config).await.unwrap();
    assert!(second.is_empty());
}

#[tokio::test]
async fn status_shows_applied_and_pending() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(!statuses[0].applied);
    assert!(!statuses[1].applied);

    let partial =
        MigrationSource::from_embedded([(USERS, source.migrations()[0].sql.as_str(), None)])
            .unwrap();
    migrate::run(&mut client, &partial, &config).await.unwrap();

    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(statuses[0].applied);
    assert!(statuses[0].applied_at.is_some());
    assert!(!statuses[0].drifted);
    assert!(!statuses[1].applied);
    assert!(statuses[1].applied_at.is_none());
}

#[tokio::test]
async fn revert_with_down_sql() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations_with_down(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();
    migrate::run(&mut client, &source, &config).await.unwrap();

    // Orders first: it references users.
    migrate::revert(&mut client, &source, ORDERS, false, &config)
        .await
        .unwrap();

    assert!(!table_exists(&client, "orders").await);
    assert!(table_exists(&client, "users").await);
    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn revert_without_down_sql_errors() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();
    migrate::run(&mut client, &source, &config).await.unwrap();

    let err = migrate::revert(&mut client, &source, ORDERS, false, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no down file"), "unexpected error: {err}");
}

#[tokio::test]
async fn revert_force_without_down_sql() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();
    migrate::run(&mut client, &source, &config).await.unwrap();

    // Removes the record only: no SQL runs.
    migrate::revert(&mut client, &source, ORDERS, true, &config)
        .await
        .unwrap();

    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);
    assert!(table_exists(&client, "orders").await);
}

#[tokio::test]
async fn revert_not_applied_errors() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();

    let err = migrate::revert(
        &mut client,
        &source,
        USERS,
        false,
        &MigrationsConfig::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not applied"), "unexpected error: {err}");
}

/// Every statement of a failing transactional migration rolls back, not
/// only the failing one, and the migration is not recorded.
#[tokio::test]
async fn failed_migration_rolls_back_every_statement() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "20260318120000_create_users.sql",
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY);",
    );
    write(
        dir.path(),
        "20260319120000_partial.sql",
        "CREATE TABLE partial (); SELECT 1/0;",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let err = migrate::run(&mut client, &source, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("failed to apply migration 20260319120000_partial")
            && err.contains("division by zero"),
        "unexpected error: {err}"
    );

    assert!(!table_exists(&client, "partial").await);
    assert_eq!(recorded(&client, "public._migrations").await, [USERS]);
}

#[tokio::test]
async fn no_transaction_migration() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "20260318120000_create_users.sql",
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT NOT NULL);",
    );
    write(
        dir.path(),
        "20260319120000_add_index.sql",
        "-- no-transaction\nCREATE INDEX CONCURRENTLY idx_users_name ON users(name);",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();

    let applied = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap();
    assert_eq!(applied.len(), 2);
    assert!(table_exists(&client, "idx_users_name").await);
}

#[tokio::test]
async fn custom_table_name() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "20260318120000_create_users.sql",
        "CREATE TABLE users (id SERIAL PRIMARY KEY);",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig {
        table: "public._my_custom_migrations".to_string(),
        ..Default::default()
    };

    migrate::run(&mut client, &source, &config).await.unwrap();

    assert_eq!(
        recorded(&client, "public._my_custom_migrations").await,
        [USERS]
    );
    assert!(!table_exists(&client, "public._migrations").await);
}
