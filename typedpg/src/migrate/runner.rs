use tokio_postgres::Client;

use super::MigrationsConfig;
use super::source::MigrationSource;
use super::source::has_no_transaction_marker;
use typedpg_core::migrations::machine::{
    Action, DbError, Failure, Input, MachineConfig, Migration as MachineMigration, Migrator,
    Outcome, Reply, Step,
};

/// Status of a single migration, indicating whether it has been applied.
///
/// Returned by [`status`] for each migration found in the [`MigrationSource`].
///
/// # Example
///
/// ```rust,no_run
/// # async fn example(client: &tokio_postgres::Client,
/// #     source: &typedpg::migrate::MigrationSource,
/// #     config: &typedpg::migrate::MigrationsConfig) -> Result<(), typedpg::Error> {
/// let statuses = typedpg::migrate::status(client, source, config).await?;
/// for s in &statuses {
///     if !s.applied {
///         println!("Pending: {}", s.name);
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    /// The migration name (file stem), e.g. `"0001_create_users"`.
    pub name: String,
    /// `true` if this migration has been applied to the database.
    pub applied: bool,
    /// Timestamp when the migration was applied, or `None` if it is still pending.
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `true` if the migration's statements on disk differ from what was
    /// originally applied. Comments, whitespace and unquoted case do not
    /// count.
    pub drifted: bool,
}

/// Applies all pending migrations in order.
///
/// Acquires a PostgreSQL advisory lock (using `config.lock_id`) to prevent
/// concurrent migration runs, then applies each pending migration in version order.
/// The lock is released when the function returns, even on failure.
///
/// By default each migration runs inside a transaction. This can be configured
/// globally via [`MigrationsConfig::use_transaction`],
/// or disabled per-migration with `-- no-transaction` on the first line of the SQL file.
///
/// Returns the list of migration names that were applied in this run. If all
/// migrations are already applied, returns an empty `Vec`.
///
/// # Errors
///
/// - [`Error::Migration`](crate::Error::Migration) if a migration's SQL fails to execute.
/// - [`Error::Database`](crate::Error::Database) on connection or lock errors.
///
/// # Example
///
/// ```rust,no_run
/// use typedpg::migrate::{MigrationSource, MigrationsConfig, run};
/// use std::path::Path;
///
/// # async fn example() -> Result<(), typedpg::Error> {
/// let (mut client, conn) =
///     tokio_postgres::connect("host=localhost dbname=mydb", tokio_postgres::NoTls).await?;
/// tokio::spawn(conn);
///
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let applied = run(&mut client, &source, &MigrationsConfig::default()).await?;
/// println!("Applied {} migrations", applied.len());
/// # Ok(())
/// # }
/// ```
pub async fn run(
    client: &mut Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<String>, crate::Error> {
    let input = machine_input(source, config, Action::Up);
    match drive(client, input).await? {
        Outcome::Applied(applied) => Ok(applied),
        outcome => Err(unexpected(&outcome)),
    }
}

/// Returns the status of all known migrations (applied and pending).
///
/// Queries the migrations tracking table and cross-references with the
/// [`MigrationSource`] to produce a [`MigrationStatus`] for each migration.
/// The result is ordered by migration version.
///
/// Unlike [`run`] and [`revert`], this function does **not** acquire an advisory
/// lock -- it is a read-only operation. It creates nothing either: without a
/// tracking table every migration is pending.
///
/// # Errors
///
/// - [`Error::Database`](crate::Error::Database) if the tracking table query fails.
///
/// # Example
///
/// ```rust,no_run
/// use typedpg::migrate::{MigrationSource, MigrationsConfig, status};
/// use std::path::Path;
///
/// # async fn example(client: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let statuses = status(client, &source, &MigrationsConfig::default()).await?;
/// for s in &statuses {
///     let mark = if s.applied { "+" } else { " " };
///     println!("[{}] {}", mark, s.name);
/// }
/// # Ok(())
/// # }
/// ```
pub async fn status(
    client: &Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<MigrationStatus>, crate::Error> {
    let input = machine_input(source, config, Action::Status);
    match drive(client, input).await? {
        Outcome::Status(rows) => rows
            .into_iter()
            .map(|row| {
                Ok(MigrationStatus {
                    name: row.name,
                    applied: row.applied,
                    applied_at: row
                        .applied_at
                        .as_deref()
                        .map(parse_applied_at)
                        .transpose()?,
                    drifted: row.drifted,
                })
            })
            .collect(),
        outcome => Err(unexpected(&outcome)),
    }
}

/// Reverts a single migration by name.
///
/// If the migration has a `.down.sql` file, executes the rollback SQL and removes the
/// record from the tracking table. If there is no down file, returns
/// [`Error::Migration`](crate::Error::Migration) -- unless `force` is `true`, in which
/// case it removes the tracking record without executing any SQL.
///
/// The down SQL runs in a transaction following `config.use_transaction` and
/// its own `-- no-transaction` marker.
///
/// Acquires an advisory lock for the duration of the operation.
///
/// # Errors
///
/// - [`Error::Migration`](crate::Error::Migration) if the migration is not currently
///   applied, not found in the source, has no down file (and `force` is `false`),
///   or if the down SQL fails.
/// - [`Error::Database`](crate::Error::Database) on connection or lock errors.
///
/// # Example
///
/// ```rust,no_run
/// use typedpg::migrate::{MigrationSource, MigrationsConfig, revert};
/// use std::path::Path;
///
/// # async fn example(client: &mut tokio_postgres::Client) -> Result<(), typedpg::Error> {
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let config = MigrationsConfig::default();
///
/// // Revert a specific migration (requires a .down.sql file)
/// revert(client, &source, "0002_add_email", false, &config).await?;
///
/// // Force-remove a migration record without running down SQL
/// revert(client, &source, "0003_create_index", true, &config).await?;
/// # Ok(())
/// # }
/// ```
pub async fn revert(
    client: &mut Client,
    source: &MigrationSource,
    name: &str,
    force: bool,
    config: &MigrationsConfig,
) -> Result<(), crate::Error> {
    let input = machine_input(
        source,
        config,
        Action::Revert {
            name: Some(name.to_string()),
            force,
        },
    );
    match drive(client, input).await? {
        Outcome::Reverted(_) => Ok(()),
        outcome => Err(unexpected(&outcome)),
    }
}

/// Records an applied migration's current file as applied, without running
/// it.
///
/// For an intentional edit of an applied migration that does not need to
/// run again: the file's text and normalized hash replace the stored ones,
/// so the run no longer reports drift. `applied_at` and the database are
/// untouched.
///
/// Acquires an advisory lock for the duration of the operation.
///
/// # Errors
///
/// - [`Error::Migration`](crate::Error::Migration) if `name` is not in the
///   source, or has not been applied.
/// - [`Error::Database`](crate::Error::Database) on connection or lock errors.
///
/// # Example
///
/// ```rust,no_run
/// use typedpg::migrate::{MigrationSource, MigrationsConfig, accept};
/// use std::path::Path;
///
/// # async fn example(client: &mut tokio_postgres::Client) -> Result<(), typedpg::Error> {
/// let source = MigrationSource::from_dir(Path::new("./migrations"))?;
/// let config = MigrationsConfig::default();
///
/// // The file's comment was fixed after it ran: record it, run nothing.
/// accept(client, &source, &config, "0002_add_email").await?;
/// # Ok(())
/// # }
/// ```
pub async fn accept(
    client: &mut Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
    name: &str,
) -> Result<(), crate::Error> {
    let input = machine_input(
        source,
        config,
        Action::Accept {
            name: name.to_string(),
        },
    );
    match drive(client, input).await? {
        Outcome::Accepted => Ok(()),
        outcome => Err(unexpected(&outcome)),
    }
}

/// The machine's input from the source and the runner's settings.
fn machine_input(source: &MigrationSource, config: &MigrationsConfig, action: Action) -> Input {
    Input {
        action,
        config: MachineConfig {
            table: config.table.clone(),
            lock_id: config.lock_id,
            use_transaction: config.use_transaction,
            fail_on_drift: config.fail_on_drift,
        },
        migrations: source
            .migrations()
            .iter()
            .map(|migration| MachineMigration {
                name: migration.name.clone(),
                sql: migration.sql.clone(),
                down_sql: migration.down_sql.clone(),
                no_transaction: migration.no_transaction,
                down_no_transaction: migration
                    .down_sql
                    .as_deref()
                    .is_some_and(has_no_transaction_marker),
            })
            .collect(),
    }
}

/// Perform each step the machine emits on `client`, until it finishes or
/// fails.
async fn drive(client: &Client, input: Input) -> Result<Outcome, crate::Error> {
    let mut migrator = Migrator::new(input);
    let mut reply = Reply::Start;
    // `Failure::Db` means the error the machine was answering when it
    // decided to fail, not necessarily the last reply's: cleaning up (a
    // ROLLBACK, the advisory unlock) happens in between. Keeping the first
    // unanswered driver error preserves the original one.
    let mut pending: Option<tokio_postgres::Error> = None;

    loop {
        match migrator.step(reply) {
            Step::Query { sql, params } => {
                let params: Vec<Option<&str>> =
                    params.iter().map(|param| param.as_deref()).collect();
                let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
                    .iter()
                    .map(|param| param as &(dyn tokio_postgres::types::ToSql + Sync))
                    .collect();
                match client.query(&sql, &params).await {
                    Ok(rows) => {
                        reply = Reply::Rows(
                            rows.iter()
                                .map(|row| {
                                    (0..row.len())
                                        .map(|column| row.get::<_, Option<String>>(column))
                                        .collect()
                                })
                                .collect(),
                        );
                    }
                    Err(error) => {
                        let db_error = describe_db_error(&error);
                        pending.get_or_insert(error);
                        reply = Reply::DbError(db_error);
                    }
                }
            }
            Step::Execute { sql } => match client.batch_execute(&sql).await {
                Ok(()) => reply = Reply::Done,
                Err(error) => {
                    let db_error = describe_db_error(&error);
                    pending.get_or_insert(error);
                    reply = Reply::DbError(db_error);
                }
            },
            Step::Sleep { ms } => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                reply = Reply::Done;
            }
            Step::Warn(message) => {
                eprintln!("{message}");
                reply = Reply::Done;
            }
            Step::Finish(outcome) => return Ok(outcome),
            Step::Fail(Failure::Message(message)) => {
                return Err(crate::Error::Migration(message));
            }
            Step::Fail(Failure::Db) => {
                return Err(pending.map(Into::into).unwrap_or_else(|| {
                    crate::Error::Migration(
                        "typedpg: the migration run failed without a database error".to_string(),
                    )
                }));
            }
        }
    }
}

/// A `tokio_postgres::Error` as the machine formats it.
fn describe_db_error(error: &tokio_postgres::Error) -> DbError {
    let Some(db) = error.as_db_error() else {
        return DbError {
            code: None,
            message: error.to_string(),
            severity: None,
            detail: None,
            hint: None,
            position: None,
            internal_position: None,
            internal_query: None,
        };
    };
    use tokio_postgres::error::ErrorPosition;
    let (position, internal_position, internal_query) = match db.position() {
        Some(ErrorPosition::Original(position)) => (Some(*position), None, None),
        Some(ErrorPosition::Internal { position, query }) => {
            (None, Some(*position), Some(query.clone()))
        }
        None => (None, None, None),
    };
    DbError {
        code: Some(db.code().code().to_string()),
        message: db.message().to_string(),
        severity: Some(db.severity().to_string()),
        detail: db.detail().map(str::to_string),
        hint: db.hint().map(str::to_string),
        position,
        internal_position,
        internal_query,
    }
}

fn parse_applied_at(text: &str) -> Result<chrono::DateTime<chrono::Utc>, crate::Error> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|e| crate::Error::Migration(format!("failed to read applied_at: {e}")))
}

/// The public functions know which outcome their action produces; any other
/// is a bug in the machine.
fn unexpected(outcome: &Outcome) -> crate::Error {
    crate::Error::Migration(format!(
        "typedpg: unexpected migration machine outcome: {outcome:?}"
    ))
}
