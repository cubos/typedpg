//! Shared setup for end-to-end tests.
//!
//! Every test of a run — whichever binary or process it lives in — talks
//! to the one PostgreSQL server `typedpg_test_support` shares across the
//! run, in one database (`typedpg_e2e`) this crate's migrations are applied
//! to once. Tests therefore see each other's rows: each must keep its data
//! apart (unique emails, labels, ids) and never assert on whole-table
//! counts.
//!
//! Every test creates its own `deadpool_postgres::Pool` — a pool built in
//! one test's runtime would leave behind connection tasks that die when
//! that runtime shuts down, surfacing as `kind: Closed` errors in sibling
//! tests.

#![allow(dead_code)]

use std::path::PathBuf;

use deadpool_postgres::{Config, Pool, Runtime};
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use typedpg::migrate::MigrationSource;
use typedpg_core::config::MigrationsConfig;
use typedpg_test_support::PgServer;

/// The database every e2e test shares.
pub const DATABASE: &str = "typedpg_e2e";

static MIGRATED: OnceCell<()> = OnceCell::const_new();

/// The shared server, with [`DATABASE`] created and migrated.
pub async fn server() -> &'static PgServer {
    let server = typedpg_test_support::server();
    MIGRATED
        .get_or_init(|| {
            typedpg_test_support::once_per_run("e2e-database", || async {
                server.ensure_database(DATABASE).await;
                let migrations_dir: PathBuf =
                    [env!("CARGO_MANIFEST_DIR"), "migrations"].iter().collect();
                let source = MigrationSource::from_dir(&migrations_dir).expect("load migrations");
                let mut client = server.connect(DATABASE).await;
                typedpg::migrate::run(&mut client, &source, &MigrationsConfig::default())
                    .await
                    .expect("run migrations");
                // Not a migration: the schema the macros see stays the app's.
                client
                    .batch_execute("CREATE SEQUENCE IF NOT EXISTS e2e_unique_ids")
                    .await
                    .expect("create the unique id sequence");
            })
        })
        .await;
    server
}

/// Connection settings for the migrated e2e database.
pub async fn database_config() -> tokio_postgres::Config {
    server().await.config(DATABASE)
}

/// A fresh deadpool pool on the migrated e2e database.
pub async fn setup() -> Pool {
    let server = server().await;
    let mut cfg = Config::new();
    cfg.host = Some(server.host().to_string());
    cfg.port = Some(server.port());
    cfg.user = Some("postgres".into());
    cfg.password = Some("postgres".into());
    cfg.dbname = Some(DATABASE.into());
    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .expect("build pool")
}

/// A string unique to this call across the whole run, for keys (emails,
/// labels) tests must not share: the process id, the process's start time
/// (pids get reused) and a per-process counter.
pub fn unique(prefix: &str) -> String {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    static START: OnceLock<u128> = OnceLock::new();
    let start = START.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos()
    });
    format!(
        "{prefix}-{}-{start}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// An integer key no other call of the run returns (from a sequence in the
/// shared database), for tables keyed by an explicit `INT` id.
pub async fn unique_id(pool: &Pool) -> i32 {
    let client = pool.get().await.expect("client");
    let id: i64 = client
        .query_one("SELECT nextval('e2e_unique_ids')", &[])
        .await
        .expect("nextval")
        .get(0);
    i32::try_from(id).expect("id fits an INT")
}
