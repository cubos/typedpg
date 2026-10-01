use std::fs;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use testcontainers::core::Mount;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio_postgres::NoTls;
use typedpg::migrate::MigrationSource;
use typedpg_core::config::MigrationsConfig;

/// Container IDs that an `atexit` hook must force-remove before the test
/// process exits. testcontainers-rs relies on `Drop` to remove containers,
/// but `Drop` is never invoked for values held in `static` slots (used by
/// the e2e harness) and can be skipped on abnormal exit paths. Shelling out
/// to `docker rm -f` from `libc::atexit` is a last-resort reaper that works
/// even when Ryuk isn't around.
static TRACKED_CONTAINERS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

extern "C" fn reap_containers() {
    let Some(ids) = TRACKED_CONTAINERS.get() else {
        return;
    };
    let ids = match ids.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
    for id in ids {
        let _ = Command::new("docker")
            .args(["rm", "-f", &id])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn track_container(id: String) {
    let slot = TRACKED_CONTAINERS.get_or_init(|| {
        // SAFETY: `reap_containers` has the required C ABI and only touches
        // the static `Mutex`; registering it once is safe on every POSIX
        // target we support.
        unsafe {
            libc::atexit(reap_containers);
        }
        Mutex::new(Vec::new())
    });
    if let Ok(mut ids) = slot.lock() {
        ids.push(id);
    }
}

/// Start a throwaway PostgreSQL 18 container with Docker's
/// `--rm` flag set, so the container is deleted as soon as it exits — even
/// when the Ryuk reaper is unavailable or a test panics before Drop runs.
///
/// PGDATA is mounted as tmpfs so Postgres runs entirely in RAM: no anonymous
/// volume is created, so nothing is left behind on OrbStack/Docker after
/// teardown. Also speeds up the test cycle (initdb + fsync go through memory).
/// The official `postgres` image, as testcontainers-modules' `Postgres`
/// configured it: user / password / database `postgres`, fsync off, ready
/// once the server logs that it accepts connections.
fn postgres_image(name: &str, tag: &str) -> GenericImage {
    GenericImage::new(name, tag)
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_wait_for(WaitFor::message_on_stdout(
            "database system is ready to accept connections",
        ))
}

async fn start_postgres() -> ContainerAsync<GenericImage> {
    let container = postgres_image("postgres", "18")
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_DB", "postgres")
        .with_cmd(["-c", "fsync=off"])
        .with_mount(Mount::tmpfs_mount("/var/lib/postgresql"))
        .with_host_config_modifier(|cfg| cfg.auto_remove = Some(true))
        .start()
        .await
        .unwrap();
    track_container(container.id().to_string());
    container
}

async fn connect_to_container(
    container: &testcontainers::ContainerAsync<GenericImage>,
) -> tokio_postgres::Client {
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
    let (client, conn) = tokio_postgres::connect(&conn_str, NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("connection error: {e}");
        }
    });
    client
}

fn create_test_migrations(dir: &std::path::Path) {
    fs::write(
        dir.join("20260318120000_create_users.sql"),
        "CREATE TABLE users (
            id   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();

    fs::write(
        dir.join("20260319120000_create_orders.sql"),
        "CREATE TABLE orders (
            id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            user_id BIGINT NOT NULL REFERENCES users(id)
        );",
    )
    .unwrap();
}

fn create_test_migrations_with_down(dir: &std::path::Path) {
    create_test_migrations(dir);

    fs::write(
        dir.join("20260318120000_create_users.down.sql"),
        "DROP TABLE users;",
    )
    .unwrap();

    fs::write(
        dir.join("20260319120000_create_orders.down.sql"),
        "DROP TABLE orders;",
    )
    .unwrap();
}

#[tokio::test]
async fn run_applies_all_pending_migrations() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let applied = typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0], "20260318120000_create_users");
    assert_eq!(applied[1], "20260319120000_create_orders");

    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name IN ('users', 'orders') ORDER BY table_name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn run_is_idempotent() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let first = typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();
    assert_eq!(first.len(), 2);

    let second = typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();
    assert!(second.is_empty());
}

#[tokio::test]
async fn status_shows_applied_and_pending() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let statuses = typedpg::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(!statuses[0].applied);
    assert!(!statuses[1].applied);

    let partial_dir = tempfile::tempdir().unwrap();
    fs::write(
        partial_dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    let partial_source = MigrationSource::from_dir(partial_dir.path()).unwrap();
    typedpg::migrate::run(&mut client, &partial_source, &config)
        .await
        .unwrap();

    let statuses = typedpg::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(statuses[0].applied_at.is_some());
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn revert_with_down_sql() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations_with_down(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    // Revert orders (must revert before users due to FK)
    typedpg::migrate::revert(
        &mut client,
        &source,
        "20260319120000_create_orders",
        false,
        &config,
    )
    .await
    .unwrap();

    // orders table should be gone
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'orders'",
            &[],
        )
        .await
        .unwrap();
    assert!(rows.is_empty());

    // users should still exist
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'users'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    // Status should reflect the revert
    let statuses = typedpg::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn revert_without_down_sql_errors() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path()); // no down files

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    let result = typedpg::migrate::revert(
        &mut client,
        &source,
        "20260319120000_create_orders",
        false,
        &config,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("no down file"), "unexpected error: {err}");
}

#[tokio::test]
async fn revert_force_without_down_sql() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path()); // no down files

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    // force=true should succeed, just removing the record
    typedpg::migrate::revert(
        &mut client,
        &source,
        "20260319120000_create_orders",
        true,
        &config,
    )
    .await
    .unwrap();

    // Record removed but table still exists (no SQL ran)
    let statuses = typedpg::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);

    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'orders'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1); // table still exists
}

#[tokio::test]
async fn revert_not_applied_errors() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    // Don't run migrations — try to revert directly
    let result = typedpg::migrate::revert(
        &mut client,
        &source,
        "20260318120000_create_users",
        false,
        &config,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not applied"), "unexpected error: {err}");
}

#[tokio::test]
async fn failed_migration_rolls_back() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY);",
    )
    .unwrap();
    fs::write(
        dir.path().join("20260319120000_bad_migration.sql"),
        "THIS IS NOT VALID SQL;",
    )
    .unwrap();

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let result = typedpg::migrate::run(&mut client, &source, &config).await;
    assert!(result.is_err());

    let statuses = typedpg::migrate::status(&client, &source, &config)
        .await
        .unwrap();
    assert!(statuses[0].applied);
    assert!(!statuses[1].applied);
}

#[tokio::test]
async fn no_transaction_migration() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    fs::write(
        dir.path().join("20260319120000_add_index.sql"),
        "-- no-transaction\nCREATE INDEX CONCURRENTLY idx_users_name ON users(name);",
    )
    .unwrap();

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    let applied = typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();
    assert_eq!(applied.len(), 2);

    let rows = client
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = 'users' AND indexname = 'idx_users_name'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn custom_table_name() {
    let container = start_postgres().await;
    let mut client = connect_to_container(&container).await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("20260318120000_create_users.sql"),
        "CREATE TABLE users (id SERIAL PRIMARY KEY);",
    )
    .unwrap();

    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig {
        table: "public._my_custom_migrations".to_string(),
        ..Default::default()
    };

    typedpg::migrate::run(&mut client, &source, &config)
        .await
        .unwrap();

    let rows = client
        .query("SELECT * FROM public._my_custom_migrations", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "20260318120000_create_users");
}
