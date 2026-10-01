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

/// Another session on the same database.
async fn second_session(db: &str) -> Client {
    typedpg_test_support::server().connect(db).await
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

/// `status` is read-only: on a database that never ran a migration it
/// reports everything pending without creating the tracking table.
#[tokio::test]
async fn status_does_not_create_the_tracking_table() {
    let (client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();

    let statuses = migrate::status(&client, &source, &MigrationsConfig::default())
        .await
        .unwrap();
    assert!(statuses.iter().all(|s| !s.applied));
    assert!(!table_exists(&client, "public._migrations").await);
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

/// The advisory lock is released when a run fails, so the next runner
/// (or another session) can take it.
#[tokio::test]
async fn lock_is_released_after_a_failed_run() {
    let (mut client, db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "0001_bad.sql", "THIS IS NOT VALID SQL;");
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    migrate::run(&mut client, &source, &config)
        .await
        .unwrap_err();

    let other = second_session(&db).await;
    let got: bool = other
        .query_one("SELECT pg_try_advisory_lock($1)", &[&config.lock_id])
        .await
        .unwrap()
        .get(0);
    assert!(got, "the failed run kept the advisory lock");
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

/// Editing an applied migration is drift. With `fail_on_drift` (the
/// default) the run aborts before applying anything — including a pending
/// migration that sorts *before* the drifted one (merged out of order).
#[tokio::test]
async fn drift_aborts_the_run_before_applying_anything() {
    let (mut client, _db) = fresh_db().await;
    let config = MigrationsConfig::default();
    let first = MigrationSource::from_embedded([
        ("0001_a", "CREATE TABLE a (id INT);", None),
        ("0003_c", "CREATE TABLE c (id INT);", None),
    ])
    .unwrap();
    migrate::run(&mut client, &first, &config).await.unwrap();

    let edited = MigrationSource::from_embedded([
        ("0001_a", "CREATE TABLE a (id INT);", None),
        ("0002_b", "CREATE TABLE b (id INT);", None),
        ("0003_c", "CREATE TABLE c (id BIGINT);", None),
    ])
    .unwrap();
    let err = migrate::run(&mut client, &edited, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("migration '0003_c' has been modified since it was applied"),
        "unexpected error: {err}"
    );
    assert!(
        !table_exists(&client, "b").await,
        "0002_b ran despite the drift"
    );

    let statuses = migrate::status(&client, &edited, &config).await.unwrap();
    let drifted: Vec<_> = statuses
        .iter()
        .map(|s| (s.name.as_str(), s.drifted))
        .collect();
    assert_eq!(
        drifted,
        [("0001_a", false), ("0002_b", false), ("0003_c", true)]
    );
}

/// With `fail_on_drift = false` drift is only a warning: pending
/// migrations still apply, and `status` still flags the edited one.
#[tokio::test]
async fn drift_is_a_warning_without_fail_on_drift() {
    let (mut client, _db) = fresh_db().await;
    let config = MigrationsConfig {
        fail_on_drift: false,
        ..Default::default()
    };
    let first =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (id INT);", None)]).unwrap();
    migrate::run(&mut client, &first, &config).await.unwrap();

    let edited = MigrationSource::from_embedded([
        ("0001_a", "CREATE TABLE a (id INT); -- edited", None),
        ("0002_b", "CREATE TABLE b (id INT);", None),
    ])
    .unwrap();
    let applied = migrate::run(&mut client, &edited, &config).await.unwrap();
    assert_eq!(applied, ["0002_b"]);

    let statuses = migrate::status(&client, &edited, &config).await.unwrap();
    assert!(statuses[0].applied && statuses[0].drifted);
    assert!(statuses[1].applied && !statuses[1].drifted);
}

/// Two runners racing on a fresh database (two replicas booting at once):
/// both succeed and every migration applies exactly once. The tracking
/// table must be created under the advisory lock — two concurrent
/// `CREATE TABLE IF NOT EXISTS` can both miss the table and collide.
#[tokio::test]
async fn concurrent_runs_on_a_fresh_database() {
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    for _ in 0..10 {
        let (mut a, db) = fresh_db().await;
        let mut b = second_session(&db).await;
        let (ra, rb) = tokio::join!(
            migrate::run(&mut a, &source, &config),
            migrate::run(&mut b, &source, &config),
        );
        let (ra, rb) = (ra.expect("runner a"), rb.expect("runner b"));
        let mut all: Vec<_> = ra.into_iter().chain(rb).collect();
        all.sort();
        assert_eq!(all, [USERS, ORDERS], "each migration applied once");
    }
}

/// A source baked in with `embed_migrations!` runs like one read from disk.
#[tokio::test]
async fn embedded_source_runs() {
    let (mut client, _db) = fresh_db().await;
    let source = typedpg::embed_migrations!("tests/embedded_migrations");
    let names: Vec<_> = source
        .migrations()
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(names, ["0001_create_users", "0002_create_orders"]);

    let applied = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap();
    assert_eq!(applied, names);
    assert!(table_exists(&client, "orders").await);
}

/// A tracking table from before `sql_source` existed: its rows have no
/// stored text, so they can never drift, and the column is added on the
/// next run.
#[tokio::test]
async fn legacy_tracking_table_without_sql_source() {
    let (mut client, _db) = fresh_db().await;
    client
        .batch_execute(
            "CREATE TABLE public._migrations (
                 name       TEXT PRIMARY KEY,
                 applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
             );
             CREATE TABLE a (id INT);
             INSERT INTO public._migrations (name) VALUES ('0001_a');",
        )
        .await
        .unwrap();
    let source = MigrationSource::from_embedded([
        ("0001_a", "CREATE TABLE a (id INT); -- not what ran", None),
        ("0002_b", "CREATE TABLE b (id INT);", None),
    ])
    .unwrap();
    let config = MigrationsConfig::default();

    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(statuses[0].applied && !statuses[0].drifted);
    assert!(!statuses[1].applied);

    let applied = migrate::run(&mut client, &source, &config).await.unwrap();
    assert_eq!(applied, ["0002_b"]);
    let stored: Vec<Option<String>> = client
        .query(
            "SELECT sql_source FROM public._migrations ORDER BY name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(stored, [None, Some("CREATE TABLE b (id INT);".into())]);
}
