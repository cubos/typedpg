//! The migration runner against a real PostgreSQL: the server
//! `typedpg_test_support` shares across the test run, one fresh database
//! per test.

use std::fs;
use std::path::Path;

use tokio_postgres::Client;
use typedpg::migrate::{self, MigrationSource};
use typedpg_core::config::MigrationsConfig;
use typedpg_core::migrations::{HASH_VERSION, hash, hash_with_version};

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

async fn column_exists(client: &Client, table: &str, column: &str) -> bool {
    client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                            WHERE a.attrelid = pg_catalog.to_regclass($1)
                              AND a.attname = $2 AND NOT a.attisdropped)",
            &[&table, &column],
        )
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

/// A down migration that fails reports the server's error and leaves the
/// migration applied, its effects untouched.
#[tokio::test]
async fn failing_down_migration_reports_the_server_error() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    create_test_migrations(dir.path());
    write(
        dir.path(),
        "20260319120000_create_orders.down.sql",
        "DROP TABLE orders; DROP TABLE no_such_table;",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();
    migrate::run(&mut client, &source, &config).await.unwrap();

    let err = migrate::revert(&mut client, &source, ORDERS, false, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(&format!("failed to revert migration {ORDERS}"))
            && err.contains("table \"no_such_table\" does not exist"),
        "unexpected error: {err}"
    );

    assert!(
        table_exists(&client, "orders").await,
        "the down SQL rolled back"
    );
    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(statuses[1].applied, "still recorded as applied");
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

/// The down SQL may ask for no transaction of its own: a `CREATE INDEX
/// CONCURRENTLY` cannot run inside one.
#[tokio::test]
async fn a_down_migration_may_ask_for_no_transaction() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "0001_create_users.sql",
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL);",
    );
    write(
        dir.path(),
        "0001_create_users.down.sql",
        "-- no-transaction\nDROP INDEX CONCURRENTLY IF EXISTS idx_users_name;",
    );
    write(
        dir.path(),
        "0002_add_index.sql",
        "CREATE INDEX idx_users_name ON users(name);",
    );
    write(
        dir.path(),
        "0002_add_index.down.sql",
        "-- no-transaction\nDROP INDEX CONCURRENTLY idx_users_name;",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();
    migrate::run(&mut client, &source, &config).await.unwrap();

    // The down SQL runs outside a transaction: `DROP INDEX CONCURRENTLY`
    // refuses to run inside one.
    migrate::revert(&mut client, &source, "0002_add_index", false, &config)
        .await
        .unwrap();
    assert!(!table_exists(&client, "idx_users_name").await);
}

/// The down SQL's own `-- no-transaction` marker decides how it runs: the
/// up's marker speaks about the up's statements, and the down is the other
/// way round.
#[tokio::test]
async fn the_down_marker_decides_how_the_down_runs() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "0001_create_users.sql",
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL);",
    );
    write(
        dir.path(),
        "0001_create_users.down.sql",
        "-- no-transaction\nDROP INDEX CONCURRENTLY IF EXISTS idx_users_name;",
    );
    write(
        dir.path(),
        "0002_add_index.sql",
        "CREATE INDEX idx_users_name ON users(name);",
    );
    write(
        dir.path(),
        "0002_add_index.down.sql",
        "-- no-transaction\nDROP INDEX CONCURRENTLY idx_users_name;",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();
    migrate::run(&mut client, &source, &config).await.unwrap();

    // The down SQL runs outside a transaction: `DROP INDEX CONCURRENTLY`
    // refuses to run inside one.
    migrate::revert(&mut client, &source, "0002_add_index", false, &config)
        .await
        .unwrap();
    assert!(!table_exists(&client, "idx_users_name").await);
}

/// A `-- no-transaction` migration may hold several statements that refuse
/// to run in a transaction block. Sent as one multi-statement query they
/// would all run in PG's implicit transaction block and fail.
#[tokio::test]
async fn no_transaction_migration_with_several_statements() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "0001_create_users.sql",
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL, email TEXT);",
    );
    write(
        dir.path(),
        "0002_add_indexes.sql",
        "-- no-transaction\n\
         CREATE INDEX CONCURRENTLY idx_users_name ON users(name);\n\
         -- a comment; with a semicolon\n\
         CREATE INDEX CONCURRENTLY \"idx;email\" ON users(email);\n",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();

    let applied = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap();
    assert_eq!(applied.len(), 2);
    assert!(table_exists(&client, "idx_users_name").await);
    assert!(table_exists(&client, "\"idx;email\"").await);
}

/// A role granted the tracking table's privileges, but not its owner, runs
/// migrations: a complete table gets no `ALTER TABLE`, which needs the
/// table's ownership even to `ADD COLUMN IF NOT EXISTS` an existing column
/// ("must be owner of table _migrations" failed every action before).
#[tokio::test]
async fn a_role_that_does_not_own_the_tracking_table_runs_migrations() {
    let (mut client, db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "0001_a.sql", "CREATE TABLE a (id INT);");
    let config = MigrationsConfig::default();
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    migrate::run(&mut client, &source, &config).await.unwrap();

    let role = format!("{db}_deployer");
    client
        .batch_execute(&format!(
            "CREATE ROLE \"{role}\";
             GRANT CREATE ON SCHEMA public TO \"{role}\";
             GRANT ALL ON public._migrations TO \"{role}\";
             SET ROLE \"{role}\";"
        ))
        .await
        .unwrap();
    write(dir.path(), "0002_b.sql", "CREATE TABLE b (id INT);");
    write(dir.path(), "0002_b.down.sql", "DROP TABLE b;");
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    assert_eq!(
        migrate::run(&mut client, &source, &config).await.unwrap(),
        ["0002_b"]
    );
    migrate::accept(&mut client, &source, &config, "0002_b")
        .await
        .unwrap();
    assert_eq!(
        migrate::revert_last(&mut client, &source, false, &config)
            .await
            .unwrap(),
        "0002_b"
    );
    assert_eq!(recorded(&client, "public._migrations").await, ["0001_a"]);
}

/// A failure inside a `DO` block (or a function, a trigger) says where, as
/// psql's CONTEXT line does: the message alone did not tell which line of
/// the body raised it.
#[tokio::test]
async fn a_failure_inside_a_do_block_reports_its_context() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "0001_do.sql",
        "DO $$\nBEGIN\n  PERFORM 1;\n  RAISE EXCEPTION 'boom';\nEND $$;",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let err = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.ends_with(
            "failed to apply migration 0001_do: ERROR:  boom\n\
             CONTEXT:  PL/pgSQL function inline_code_block line 4 at RAISE"
        ),
        "{err}"
    );
}

/// A failed migration reports PG's error exactly as psql 19 prints it at
/// its default verbosity: each expected text below is `psql -f` of the
/// same file, its `psql:<file>:<line>: ` prefix left out.
#[tokio::test]
async fn errors_are_reported_as_psql_reports_them() {
    let (mut client, _db) = fresh_db().await;
    let config = MigrationsConfig::default();
    let setup = "CREATE TABLE t (ab int);\n\
                 CREATE TABLE t2 (a int);\n\
                 CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$\n\
                 BEGIN\n  RETURN (SELECT nosuch FROM t2);\nEND\n$$;";
    migrate::run(
        &mut client,
        &MigrationSource::from_embedded([("0001_setup", setup, None)]).unwrap(),
        &config,
    )
    .await
    .unwrap();

    let cases = [
        // A position and a DETAIL.
        (
            "SELECT '{1,2'::int[];",
            "ERROR:  malformed array literal: \"{1,2\"\n\
             LINE 1: SELECT '{1,2'::int[];\n               ^\n\
             DETAIL:  Unexpected end of input.",
        ),
        // A position and a HINT.
        (
            "SELECT aa FROM t;",
            "ERROR:  column \"aa\" does not exist\n\
             LINE 1: SELECT aa FROM t;\n               ^\n\
             HINT:  Perhaps you meant to reference the column \"t.ab\".",
        ),
        // A syntax error.
        (
            "CREATE TABL b (id int);",
            "ERROR:  syntax error at or near \"TABL\"\n\
             LINE 1: CREATE TABL b (id int);\n               ^",
        ),
        // A position, a DETAIL and a CONTEXT.
        (
            "SELECT jsonb '{\"a\":}';",
            "ERROR:  invalid input syntax for type json\n\
             LINE 1: SELECT jsonb '{\"a\":}';\n                     ^\n\
             DETAIL:  Expected JSON value, but found \"}\".\n\
             CONTEXT:  JSON data, line 1: {\"a\":}",
        ),
        // An internal query: its caret, QUERY and CONTEXT.
        (
            "SELECT f();",
            "ERROR:  column \"nosuch\" does not exist\n\
             LINE 1: (SELECT nosuch FROM t2)\n                ^\n\
             QUERY:  (SELECT nosuch FROM t2)\n\
             CONTEXT:  PL/pgSQL function f() line 3 at RETURN",
        ),
        // RAISE with a DETAIL and a HINT.
        (
            "DO $$\nBEGIN\n  RAISE EXCEPTION 'boom %', 42\n    \
             USING DETAIL = 'the detail', HINT = 'the hint';\nEND\n$$;",
            "ERROR:  boom 42\n\
             DETAIL:  the detail\n\
             HINT:  the hint\n\
             CONTEXT:  PL/pgSQL function inline_code_block line 3 at RAISE",
        ),
    ];
    for (sql, psql) in cases {
        let source =
            MigrationSource::from_embedded([("0001_setup", setup, None), ("0002_bad", sql, None)])
                .unwrap();
        let err = migrate::run(&mut client, &source, &config)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.ends_with(&format!("failed to apply migration 0002_bad: {psql}")),
            "{sql}:\n{err}"
        );
    }
}

/// An unterminated dollar quote swallows the rest of the file, as psql
/// sends it: PG rejects that text whole, and the `DROP TABLE` inside it
/// never runs (split at its `;`, it ran before the error).
#[tokio::test]
async fn an_unterminated_dollar_quote_runs_nothing_after_it() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "0001_typo.sql",
        "-- no-transaction\n\
         CREATE TABLE keep (id INT);\n\
         SELECT $body$ oops;\n\
         DROP TABLE keep;\n",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();

    let err = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("(statement 2): ERROR:  unterminated dollar-quoted string"),
        "{err}"
    );
    assert!(table_exists(&client, "keep").await);
}

/// With `use_transaction = false` every migration runs outside a
/// transaction: statements PG refuses in a transaction block work, and a
/// failure keeps what the statements before it did.
#[tokio::test]
async fn global_use_transaction_false() {
    let (mut client, _db) = fresh_db().await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "0001_create_users.sql",
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL);\n\
         CREATE INDEX CONCURRENTLY idx_users_name ON users(name);",
    );
    write(
        dir.path(),
        "0002_partial.sql",
        "CREATE TABLE kept (); SELECT 1/0;",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig {
        use_transaction: false,
        ..Default::default()
    };

    let err = migrate::run(&mut client, &source, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("division by zero"), "unexpected error: {err}");
    assert!(table_exists(&client, "idx_users_name").await);
    assert!(
        table_exists(&client, "kept").await,
        "no transaction: the statement before the failure stays applied"
    );
    assert_eq!(
        recorded(&client, "public._migrations").await,
        ["0001_create_users"]
    );
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
        ("0001_a", "CREATE TABLE a (id BIGINT);", None),
        ("0002_b", "CREATE TABLE b (id INT);", None),
    ])
    .unwrap();
    let applied = migrate::run(&mut client, &edited, &config).await.unwrap();
    assert_eq!(applied, ["0002_b"]);

    let statuses = migrate::status(&client, &edited, &config).await.unwrap();
    assert!(statuses[0].applied && statuses[0].drifted);
    assert!(statuses[1].applied && !statuses[1].drifted);
}

/// A comment, whitespace or unquoted-case edit changes no statement, so it
/// is not drift: the run goes ahead and `status` does not flag the file.
#[tokio::test]
async fn comment_whitespace_and_case_edits_are_not_drift() {
    let (mut client, _db) = fresh_db().await;
    let config = MigrationsConfig::default();
    let original =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (id INT);", None)]).unwrap();
    migrate::run(&mut client, &original, &config).await.unwrap();

    let edited = MigrationSource::from_embedded([(
        "0001_a",
        "-- a note\ncreate table a\n(id int);\n/* trailing */",
        None,
    )])
    .unwrap();
    let applied = migrate::run(&mut client, &edited, &config).await.unwrap();
    assert!(applied.is_empty(), "the applied migration ran again");

    let statuses = migrate::status(&client, &edited, &config).await.unwrap();
    assert!(statuses[0].applied && !statuses[0].drifted);
}

/// A real edit is drift: the error carries a unified diff of the applied
/// text against the file, and the command that records an intentional edit.
#[tokio::test]
async fn a_real_edit_reports_the_diff_and_the_accept_hint() {
    let (mut client, _db) = fresh_db().await;
    let config = MigrationsConfig::default();
    let original = "CREATE TABLE a (\n    id INT\n);";
    let first = MigrationSource::from_embedded([("0001_a", original, None)]).unwrap();
    migrate::run(&mut client, &first, &config).await.unwrap();

    let edited =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (\n    id BIGINT\n);", None)])
            .unwrap();
    let err = migrate::run(&mut client, &edited, &config)
        .await
        .unwrap_err()
        .to_string();
    for needle in [
        "migration '0001_a' has been modified since it was applied:",
        "--- applied",
        "+++ 0001_a.sql",
        "@@ -1,3 +1,3 @@",
        "-    id INT",
        "+    id BIGINT",
        "typedpg migrate accept 0001_a",
        "fail_on_drift = false",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in:\n{err}");
    }

    let statuses = migrate::status(&client, &edited, &config).await.unwrap();
    assert!(statuses[0].drifted);
}

/// `accept` records the edited file as applied: nothing runs, `applied_at`
/// is unchanged, and the next run and `status` agree the migration is not
/// drifted.
#[tokio::test]
async fn accept_records_the_edited_file_without_running_it() {
    let (mut client, _db) = fresh_db().await;
    let config = MigrationsConfig::default();
    let first =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (id INT);", None)]).unwrap();
    migrate::run(&mut client, &first, &config).await.unwrap();
    let applied_at: chrono::DateTime<chrono::Utc> = client
        .query_one(
            "SELECT applied_at FROM public._migrations WHERE name = '0001_a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);

    let edited = MigrationSource::from_embedded([(
        "0001_a",
        "CREATE TABLE a (id BIGINT);\nCREATE TABLE b (id INT);",
        None,
    )])
    .unwrap();
    assert!(migrate::run(&mut client, &edited, &config).await.is_err());

    migrate::accept(&mut client, &edited, &config, "0001_a")
        .await
        .unwrap();

    // `accept` runs no SQL, and the recorded migration does not either.
    assert!(!table_exists(&client, "b").await, "accept ran the file");
    assert!(
        migrate::run(&mut client, &edited, &config)
            .await
            .unwrap()
            .is_empty()
    );
    let statuses = migrate::status(&client, &edited, &config).await.unwrap();
    assert!(statuses[0].applied && !statuses[0].drifted);

    let row = client
        .query_one(
            "SELECT applied_at, sql_source, hash, hash_version FROM public._migrations \
             WHERE name = '0001_a'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, chrono::DateTime<chrono::Utc>>(0), applied_at);
    assert_eq!(row.get::<_, String>(1), edited.migrations()[0].sql);
    let stored_hash: String = row.get(2);
    assert_eq!(stored_hash.len(), 32, "not an MD5 hex digest");
    assert_eq!(row.get::<_, i32>(3), HASH_VERSION);
}

/// A tracking table from before the hash columns: a run adds them, fills
/// each row's hash from its stored text, and a comment-only edit of such a
/// migration is not drift.
#[tokio::test]
async fn run_upgrades_a_table_from_before_the_hash_columns() {
    let (mut client, _db) = fresh_db().await;
    client
        .batch_execute(
            "CREATE TABLE public._migrations (
                 name       TEXT PRIMARY KEY,
                 applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                 sql_source TEXT
             );
             CREATE TABLE a (id INT);
             INSERT INTO public._migrations (name, sql_source)
             VALUES ('0001_a', 'CREATE TABLE a (id INT);');",
        )
        .await
        .unwrap();
    assert!(!column_exists(&client, "public._migrations", "hash").await);

    let source = MigrationSource::from_embedded([
        (
            "0001_a",
            "CREATE TABLE a (id INT); -- edited comment\n",
            None,
        ),
        ("0002_b", "CREATE TABLE b (id INT);", None),
    ])
    .unwrap();
    let config = MigrationsConfig::default();
    let applied = migrate::run(&mut client, &source, &config).await.unwrap();
    assert_eq!(applied, ["0002_b"]);

    assert!(column_exists(&client, "public._migrations", "hash").await);
    assert!(column_exists(&client, "public._migrations", "hash_version").await);
    let row = client
        .query_one(
            "SELECT hash, hash_version FROM public._migrations WHERE name = '0001_a'",
            &[],
        )
        .await
        .unwrap();
    assert!(
        row.get::<_, Option<String>>(0).is_some(),
        "the old row was not hashed"
    );
    assert_eq!(row.get::<_, i32>(1), HASH_VERSION);

    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(!statuses[0].drifted);
}

/// `status` on a table from before the hash columns writes nothing — the
/// columns are still absent afterwards — and still sees a real edit.
#[tokio::test]
async fn status_on_an_old_table_writes_nothing_and_sees_drift() {
    let (client, _db) = fresh_db().await;
    client
        .batch_execute(
            "CREATE TABLE public._migrations (
                 name       TEXT PRIMARY KEY,
                 applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                 sql_source TEXT
             );
             INSERT INTO public._migrations (name, sql_source)
             VALUES ('0001_a', 'CREATE TABLE a (id INT);');",
        )
        .await
        .unwrap();
    let config = MigrationsConfig::default();

    let same =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (id INT);", None)]).unwrap();
    let statuses = migrate::status(&client, &same, &config).await.unwrap();
    assert!(statuses[0].applied && !statuses[0].drifted);
    assert!(!column_exists(&client, "public._migrations", "hash").await);
    assert!(!column_exists(&client, "public._migrations", "hash_version").await);

    // A comment-only edit normalizes to the stored text: still no drift.
    let commented =
        MigrationSource::from_embedded([("0001_a", "-- a note\nCREATE TABLE a (id INT);", None)])
            .unwrap();
    assert!(!migrate::status(&client, &commented, &config).await.unwrap()[0].drifted);

    let edited =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (id BIGINT);", None)]).unwrap();
    assert!(migrate::status(&client, &edited, &config).await.unwrap()[0].drifted);
    assert!(
        !column_exists(&client, "public._migrations", "hash").await,
        "status wrote the hash column"
    );
    assert!(
        !column_exists(&client, "public._migrations", "hash_version").await,
        "status wrote the hash_version column"
    );
}

const LF_FUNCTION: &str =
    "CREATE FUNCTION f() RETURNS text AS $$\nBEGIN\n  RETURN 'a\nb';\nEND\n$$ LANGUAGE plpgsql;\n";

/// A tracking table as typedpg 0.6 to 0.8 wrote it (`hash_version` 1, the
/// hashes that release computed): `0001_f`, an LF function whose body and
/// string hold line breaks; `0002_b`, applied as `id INT`; `0003_c`, a row
/// without its text (its version-1 hash only).
async fn version_1_table(client: &Client) {
    client
        .batch_execute(&format!(
            "CREATE TABLE public._migrations (
                 name         TEXT PRIMARY KEY,
                 applied_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
                 sql_source   TEXT,
                 hash         TEXT,
                 hash_version INTEGER
             );
             {LF_FUNCTION}
             CREATE TABLE b (id INT);
             CREATE TABLE c (id INT);"
        ))
        .await
        .unwrap();
    let rows: [(&str, Option<&str>, String); 3] = [
        (
            "0001_f",
            Some(LF_FUNCTION),
            // What typedpg 0.8's `hash` returned for this text.
            "36008b21f609949ea9d09c40c9aad1d0".to_string(),
        ),
        (
            "0002_b",
            Some("CREATE TABLE b (id INT);"),
            hash_with_version("CREATE TABLE b (id INT);", 1).unwrap(),
        ),
        (
            "0003_c",
            None,
            hash_with_version("CREATE TABLE c (id INT);\n", 1).unwrap(),
        ),
    ];
    for (name, sql, stored) in rows {
        client
            .execute(
                "INSERT INTO public._migrations (name, sql_source, hash, hash_version) \
                 VALUES ($1, $2, $3, 1)",
                &[&name, &sql, &stored],
            )
            .await
            .unwrap();
    }
}

/// Upgrading typedpg reports no false drift on a table the previous hash
/// version wrote, even for a checkout whose line endings changed (git
/// `autocrlf`), and still reports a real edit under either version.
#[tokio::test]
async fn hashes_of_the_previous_version_report_no_false_drift() {
    let (mut client, _db) = fresh_db().await;
    version_1_table(&client).await;
    let config = MigrationsConfig::default();
    let crlf = |sql: &str| sql.replace('\n', "\r\n");
    let function = crlf(LF_FUNCTION);
    let checkout = |b: &str| {
        let b = crlf(b);
        MigrationSource::from_embedded([
            ("0001_f", function.as_str(), None),
            ("0002_b", b.as_str(), None),
            ("0003_c", "CREATE TABLE c (id INT);\n", None),
            ("0004_d", "CREATE TABLE d (id INT);", None),
        ])
        .unwrap()
    };

    // `status` reads the old rows without writing: the CRLF checkout is no
    // drift, the edit of `0002_b` is.
    let edited = checkout("CREATE TABLE b (id BIGINT);");
    let drifted: Vec<_> = migrate::status(&client, &edited, &config)
        .await
        .unwrap()
        .iter()
        .map(|s| (s.name.clone(), s.drifted))
        .collect();
    assert_eq!(
        drifted,
        [
            ("0001_f".to_string(), false),
            ("0002_b".to_string(), true),
            ("0003_c".to_string(), false),
            ("0004_d".to_string(), false),
        ]
    );
    let err = migrate::run(&mut client, &edited, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("migration '0002_b' has been modified since it was applied"),
        "unexpected error: {err}"
    );
    assert!(!err.contains("0001_f"), "false drift: {err}");
    assert!(!table_exists(&client, "d").await);

    // The file restored (with CRLF line endings): the run goes ahead, and
    // the rows with their text now carry the current version's hash.
    let restored = checkout("CREATE TABLE b (id INT);");
    let applied = migrate::run(&mut client, &restored, &config).await.unwrap();
    assert_eq!(applied, ["0004_d"]);
    let rows = client
        .query(
            "SELECT name, hash, hash_version FROM public._migrations ORDER BY name",
            &[],
        )
        .await
        .unwrap();
    let stored: Vec<(String, String, i32)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        stored,
        [
            ("0001_f".to_string(), hash(LF_FUNCTION), HASH_VERSION),
            (
                "0002_b".to_string(),
                hash("CREATE TABLE b (id INT);"),
                HASH_VERSION
            ),
            // No text to re-hash: it keeps its version-1 hash.
            (
                "0003_c".to_string(),
                hash_with_version("CREATE TABLE c (id INT);\n", 1).unwrap(),
                1
            ),
            (
                "0004_d".to_string(),
                hash("CREATE TABLE d (id INT);"),
                HASH_VERSION
            ),
        ]
    );
    assert!(
        migrate::status(&client, &restored, &config)
            .await
            .unwrap()
            .iter()
            .all(|s| !s.drifted)
    );

    // After the upgrade, real drift is still drift: an edit inside the
    // function's body, and an edit of the row only version 1 can check.
    let edited_function = crlf(&LF_FUNCTION.replace("'a", "'z"));
    let body_edit = MigrationSource::from_embedded([
        ("0001_f", edited_function.as_str(), None),
        ("0002_b", "CREATE TABLE b (id INT);", None),
        ("0003_c", "CREATE TABLE c (id BIGINT);\n", None),
        ("0004_d", "CREATE TABLE d (id INT);", None),
    ])
    .unwrap();
    let drifted: Vec<_> = migrate::status(&client, &body_edit, &config)
        .await
        .unwrap()
        .iter()
        .map(|s| s.drifted)
        .collect();
    assert_eq!(drifted, [true, false, true, false]);
}

/// `accept` only accepts a known, applied migration.
#[tokio::test]
async fn accept_rejects_unknown_and_unapplied_migrations() {
    let (mut client, _db) = fresh_db().await;
    let source =
        MigrationSource::from_embedded([("0001_a", "CREATE TABLE a (id INT);", None)]).unwrap();
    let config = MigrationsConfig::default();

    let err = migrate::accept(&mut client, &source, &config, "0009_nope")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no migration file named '0009_nope'"),
        "unexpected error: {err}"
    );

    let err = migrate::accept(&mut client, &source, &config, "0001_a")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("migration '0001_a' has not been applied: nothing to accept"),
        "unexpected error: {err}"
    );
}

/// Drift compares the migration text as applied, whatever the database
/// encoding: hashing it server-side (`md5(sql_source)`) would hash LATIN1
/// bytes and see drift in every migration with a non-ASCII character.
#[tokio::test]
async fn no_false_drift_on_a_non_utf8_database() {
    let server = typedpg_test_support::server();
    let admin = server.connect("postgres").await;
    let db = format!("migrate_latin1_{}", std::process::id());
    admin
        .batch_execute(&format!(
            "CREATE DATABASE {db} ENCODING 'LATIN1' LC_COLLATE 'C' LC_CTYPE 'C' \
             TEMPLATE template0"
        ))
        .await
        .unwrap();
    let mut client = server.connect(&db).await;
    let source = MigrationSource::from_embedded([(
        "0001_cafe",
        "CREATE TABLE cafe (name TEXT DEFAULT 'café');",
        None,
    )])
    .unwrap();
    let config = MigrationsConfig::default();

    migrate::run(&mut client, &source, &config).await.unwrap();
    let again = migrate::run(&mut client, &source, &config).await.unwrap();
    assert!(again.is_empty());
    let statuses = migrate::status(&client, &source, &config).await.unwrap();
    assert!(statuses[0].applied && !statuses[0].drifted);
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

/// Two runners racing past a `CREATE INDEX CONCURRENTLY`: the runner that
/// waits for the lock must not wait inside a statement, whose transaction
/// the index build would wait for in turn — PG then reports a deadlock and
/// kills one runner.
#[tokio::test]
async fn concurrent_runs_past_a_concurrent_index_build() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "0001_t.sql", "CREATE TABLE t (a int);");
    write(
        dir.path(),
        "0002_index.sql",
        "-- no-transaction\nCREATE INDEX CONCURRENTLY t_a ON t (a);",
    );
    let source = MigrationSource::from_dir(dir.path()).unwrap();
    let config = MigrationsConfig::default();

    for _ in 0..5 {
        let (mut a, db) = fresh_db().await;
        let mut b = second_session(&db).await;
        let (ra, rb) = tokio::join!(
            migrate::run(&mut a, &source, &config),
            migrate::run(&mut b, &source, &config),
        );
        let (ra, rb) = (ra.expect("runner a"), rb.expect("runner b"));
        let mut all: Vec<_> = ra.into_iter().chain(rb).collect();
        all.sort();
        assert_eq!(all, ["0001_t", "0002_index"], "each migration applied once");
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

/// Whether `client`'s session is outside any transaction block: a
/// savepoint is refused there (and only there).
async fn outside_a_block(client: &Client) -> bool {
    match client.batch_execute("SAVEPOINT typedpg_test_probe").await {
        Ok(()) => {
            client.batch_execute("ROLLBACK").await.unwrap();
            false
        }
        Err(error) => {
            assert_eq!(
                error.code(),
                Some(&tokio_postgres::error::SqlState::NO_ACTIVE_SQL_TRANSACTION),
                "{error}"
            );
            true
        }
    }
}

async fn lock_is_free(db: &str, lock_id: i64) -> bool {
    second_session(db)
        .await
        .query_one("SELECT pg_try_advisory_lock($1)", &[&lock_id])
        .await
        .unwrap()
        .get(0)
}

/// A migration the runner does not wrap — `-- no-transaction`, or
/// `use_transaction` off — whose `BEGIN` has no `COMMIT` would leave its
/// block open: the tracking row, the next migrations and the lock's release
/// would all run inside it and never commit. The runner rolls the block
/// back and fails instead: nothing is half-applied, for this session or
/// another, the lock is free, and the session is out of the block. With the
/// COMMIT added, the run applies both.
#[tokio::test]
async fn an_unwrapped_migration_left_open_fails_and_leaves_nothing() {
    for (marker, use_transaction) in [("-- no-transaction\n", true), ("", false)] {
        let (mut client, db) = fresh_db().await;
        let config = MigrationsConfig {
            use_transaction,
            ..Default::default()
        };
        let open = format!("{marker}BEGIN;\nCREATE TABLE a (id INT);\n");
        let source = MigrationSource::from_embedded([
            ("0001_a", open.as_str(), None),
            ("0002_b", "CREATE TABLE b (id INT);", None),
        ])
        .unwrap();

        let err = migrate::run(&mut client, &source, &config)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "migration error: {}",
                typedpg_core::migrations::machine::left_transaction_open_message("0001_a", false)
            ),
            "use_transaction = {use_transaction}"
        );
        assert!(outside_a_block(&client).await);
        let other = second_session(&db).await;
        for session in [&client, &other] {
            assert!(!table_exists(session, "a").await);
            assert!(!table_exists(session, "b").await);
            assert!(recorded(session, "public._migrations").await.is_empty());
        }
        assert!(lock_is_free(&db, config.lock_id).await);

        let closed = format!("{marker}BEGIN;\nCREATE TABLE a (id INT);\nCOMMIT;\n");
        let source = MigrationSource::from_embedded([
            ("0001_a", closed.as_str(), None),
            ("0002_b", "CREATE TABLE b (id INT);", None),
        ])
        .unwrap();
        let applied = migrate::run(&mut client, &source, &config).await.unwrap();
        assert_eq!(applied, ["0001_a", "0002_b"]);
        assert!(table_exists(&other, "a").await && table_exists(&other, "b").await);
        assert_eq!(
            recorded(&other, "public._migrations").await,
            ["0001_a", "0002_b"]
        );
        assert!(lock_is_free(&db, config.lock_id).await);
    }
}

/// The probe's setting set at session level — by a migration, here, or a
/// role or database default — is not mistaken for an open block: the probe
/// compares against a token of its own.
#[tokio::test]
async fn a_session_value_of_the_probe_setting_is_not_an_open_block() {
    let (mut client, db) = fresh_db().await;
    let config = MigrationsConfig {
        use_transaction: false,
        ..Default::default()
    };
    let source = MigrationSource::from_embedded([
        (
            "0001_a",
            "SET typedpg.transaction_probe = 'open';\nCREATE TABLE a (id INT);\n",
            None,
        ),
        (
            "0002_b",
            "BEGIN;\nCREATE TABLE b (id INT);\nCOMMIT;\n",
            None,
        ),
    ])
    .unwrap();
    let applied = migrate::run(&mut client, &source, &config).await.unwrap();
    assert_eq!(applied, ["0001_a", "0002_b"]);
    let other = second_session(&db).await;
    assert!(table_exists(&other, "a").await && table_exists(&other, "b").await);
    assert_eq!(
        recorded(&other, "public._migrations").await,
        ["0001_a", "0002_b"]
    );

    // With the value set, a block left open is still found.
    let source = MigrationSource::from_embedded([
        (
            "0001_a",
            "SET typedpg.transaction_probe = 'open';\nCREATE TABLE a (id INT);\n",
            None,
        ),
        (
            "0002_b",
            "BEGIN;\nCREATE TABLE b (id INT);\nCOMMIT;\n",
            None,
        ),
        ("0003_c", "BEGIN;\nCREATE TABLE c (id INT);\n", None),
    ])
    .unwrap();
    migrate::run(&mut client, &source, &config)
        .await
        .unwrap_err();
    assert!(!table_exists(&other, "c").await);
    assert!(outside_a_block(&client).await);
}

/// After `COMMIT AND CHAIN`, only the chained block is open: what the
/// commit committed stays, and the message says so.
#[tokio::test]
async fn a_chained_block_left_open_keeps_what_was_committed() {
    let (mut client, db) = fresh_db().await;
    let config = MigrationsConfig {
        use_transaction: false,
        ..Default::default()
    };
    let source = MigrationSource::from_embedded([(
        "0001_a",
        "BEGIN;\nCREATE TABLE a (id INT);\nCOMMIT AND CHAIN;\nCREATE TABLE b (id INT);\n",
        None,
    )])
    .unwrap();
    let err = migrate::run(&mut client, &source, &config)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        format!(
            "migration error: {}",
            typedpg_core::migrations::machine::left_transaction_open_message("0001_a", false)
        )
    );
    assert!(err.contains("statements committed before it"), "{err}");
    assert!(outside_a_block(&client).await);
    let other = second_session(&db).await;
    assert!(table_exists(&other, "a").await);
    assert!(!table_exists(&other, "b").await);
    assert!(recorded(&other, "public._migrations").await.is_empty());
}

/// A statement failing inside a block an unwrapped migration opened aborts
/// that block: the runner rolls it back, so the lock's release succeeds and
/// the session is usable again, and reports the statement's error.
#[tokio::test]
async fn a_failure_inside_an_unwrapped_block_rolls_it_back() {
    let (mut client, db) = fresh_db().await;
    let config = MigrationsConfig {
        use_transaction: false,
        ..Default::default()
    };
    let source = MigrationSource::from_embedded([(
        "0001_a",
        "CREATE TABLE kept (id INT);\nBEGIN;\nCREATE TABLE a (id INT);\nSELECT 1/0;\nCOMMIT;\n",
        None,
    )])
    .unwrap();

    let err = migrate::run(&mut client, &source, &config)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("failed to apply migration 0001_a (statement 4)")
            && err.contains("division by zero"),
        "unexpected error: {err}"
    );
    assert!(outside_a_block(&client).await);
    assert!(lock_is_free(&db, config.lock_id).await);
    let other = second_session(&db).await;
    assert!(table_exists(&other, "kept").await);
    assert!(!table_exists(&other, "a").await);
    assert!(recorded(&other, "public._migrations").await.is_empty());
}

/// A down file run without the runner's transaction is held to the same
/// rule: left open, its block is rolled back, and the migration stays
/// applied.
#[tokio::test]
async fn an_unwrapped_down_left_open_fails_and_keeps_the_record() {
    let (mut client, db) = fresh_db().await;
    let config = MigrationsConfig::default();
    let source = MigrationSource::from_embedded([(
        "0001_a",
        "CREATE TABLE a (id INT);",
        Some("-- no-transaction\nBEGIN;\nDROP TABLE a;\n"),
    )])
    .unwrap();
    migrate::run(&mut client, &source, &config).await.unwrap();

    let err = migrate::revert(&mut client, &source, "0001_a", false, &config)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        format!(
            "migration error: {}",
            typedpg_core::migrations::machine::left_transaction_open_message("0001_a", true)
        )
    );
    assert!(outside_a_block(&client).await);
    assert!(lock_is_free(&db, config.lock_id).await);
    let other = second_session(&db).await;
    assert!(table_exists(&other, "a").await);
    assert_eq!(recorded(&other, "public._migrations").await, ["0001_a"]);
}

/// In the default mode the runner's own transaction wraps the file: its
/// `BEGIN` is only a warning, and the runner's `COMMIT` ends the block.
#[tokio::test]
async fn a_wrapped_migration_without_commit_applies() {
    let (mut client, db) = fresh_db().await;
    let source =
        MigrationSource::from_embedded([("0001_a", "BEGIN;\nCREATE TABLE a (id INT);\n", None)])
            .unwrap();
    let applied = migrate::run(&mut client, &source, &MigrationsConfig::default())
        .await
        .unwrap();
    assert_eq!(applied, ["0001_a"]);
    let other = second_session(&db).await;
    assert!(table_exists(&other, "a").await);
    assert_eq!(recorded(&other, "public._migrations").await, ["0001_a"]);
}
