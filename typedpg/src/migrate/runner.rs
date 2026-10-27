use tokio_postgres::Client;

use super::MigrationsConfig;
use super::normalize::{HASH_VERSION, hash};
use super::source::MigrationSource;
use super::source::has_no_transaction_marker;
use super::split::split_statements;

/// An applied migration's stored drift hash, as `run` reads it: no
/// `sql_source` download, only the hash and the algorithm that made it.
#[derive(Debug, Clone)]
struct AppliedMigration {
    hash: Option<String>,
    hash_version: Option<i32>,
}

impl AppliedMigration {
    /// Whether the stored hash differs from the file's, computed with the
    /// current algorithm. A row without a hash (no `sql_source` stored, from
    /// before that column existed) is not checked, and neither is one whose
    /// hash predates the current algorithm — `run` re-hashes it first.
    fn drifted(&self, sql: &str) -> bool {
        self.hash_version == Some(HASH_VERSION)
            && self
                .hash
                .as_deref()
                .is_some_and(|stored| stored != hash(sql))
    }
}

/// Formats a `tokio_postgres::Error` including the underlying Postgres
/// `DbError` details (severity, message, detail, hint, position), which are
/// otherwise hidden behind the generic `"db error"` Display impl.
///
/// `sql` is the migration's text and `base` the byte offset in it of the
/// statement that was sent (0 when the whole migration was): PG's
/// position, a character index into what was sent, is reported as the
/// line and column of the migration file, with the line and a caret.
fn format_pg_error(e: &tokio_postgres::Error, sql: &str, base: usize) -> String {
    if let Some(db) = e.as_db_error() {
        let mut out = format!("{}: {}", db.severity(), db.message());
        if let Some(detail) = db.detail() {
            out.push_str("\nDETAIL: ");
            out.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            out.push_str("\nHINT: ");
            out.push_str(hint);
        }
        if let Some(pos) = db.position() {
            use tokio_postgres::error::ErrorPosition;
            match pos {
                ErrorPosition::Original(p) => out.push_str(&locate_position(sql, base, *p)),
                ErrorPosition::Internal { position, query } => out.push_str(&format!(
                    "\nINTERNAL POSITION: {}\nQUERY: {}",
                    position, query
                )),
            }
        }
        out
    } else {
        e.to_string()
    }
}

/// PG's error position `position` — a 1-based character index into
/// `sql[base..]`, the text that was sent — as psql shows it: `LINE n:` and
/// the line, then a caret under the column. `n` counts lines of the whole
/// `sql`.
fn locate_position(sql: &str, base: usize, position: u32) -> String {
    let sent = sql.get(base..).unwrap_or_default();
    let chars = usize::try_from(position).unwrap_or(0).saturating_sub(1);
    let at = base
        + sent
            .char_indices()
            .nth(chars)
            .map_or(sent.len(), |(i, _)| i);
    let line_start = sql[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = sql[at..].find('\n').map_or(sql.len(), |i| at + i);
    let line_no = sql[..at].matches('\n').count() + 1;
    let column = sql[line_start..at].chars().count();
    let prefix = format!("LINE {line_no}: ");
    format!(
        "\n{prefix}{}\n{}^",
        &sql[line_start..line_end],
        " ".repeat(prefix.len() + column)
    )
}

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
    validate(config)?;
    acquire_lock(client, config).await?;

    // The tracking table is created under the lock: two runners racing on
    // a fresh database would otherwise both find it missing, and one
    // `CREATE TABLE IF NOT EXISTS` fails on the other's catalog rows.
    let result = match ensure_table(client, config).await {
        Ok(()) => run_inner(client, source, config).await,
        Err(e) => Err(e),
    };

    // Always release lock, even if run_inner failed.
    let release = release_lock(client, config).await;
    match (&result, release) {
        (Ok(_), Ok(_)) => result,
        (Ok(_), Err(rel_err)) => Err(rel_err),
        (Err(_), Err(rel_err)) => {
            eprintln!("typedpg: failed to release advisory lock: {rel_err}");
            result
        }
        (Err(_), Ok(_)) => result,
    }
}

async fn run_inner(
    client: &mut Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
) -> Result<Vec<String>, crate::Error> {
    backfill_hashes(client, config).await?;
    let applied = get_applied(client, config).await?;
    let mut newly_applied = Vec::new();

    // Drift is checked over every applied migration before anything new
    // runs: a pending migration can sort before an applied one (merged out
    // of order), and must not run when the run is going to abort.
    for migration in source.migrations() {
        let Some(stored) = applied.get(&migration.name) else {
            continue;
        };
        if !stored.drifted(&migration.sql) {
            continue;
        }
        let stored_sql = stored_sql_source(client, config, &migration.name).await?;
        let message = drift_message(
            &migration.name,
            stored_sql.as_deref(),
            &migration.sql,
            config.fail_on_drift,
        );
        if config.fail_on_drift {
            return Err(crate::Error::Migration(message));
        }
        eprintln!("warning: {message}");
    }

    for migration in source.migrations() {
        if applied.contains_key(&migration.name) {
            continue;
        }

        let use_tx = config.use_transaction && !migration.no_transaction;
        let migration_hash = hash(&migration.sql);

        if use_tx {
            let tx = client.transaction().await?;

            tx.batch_execute(&migration.sql).await.map_err(|e| {
                crate::Error::Migration(format!(
                    "failed to apply migration {}: {}",
                    migration.name,
                    format_pg_error(&e, &migration.sql, 0)
                ))
            })?;

            tx.execute(
                &format!(
                    "INSERT INTO {} (name, sql_source, hash, hash_version) \
                     VALUES ($1, $2, $3, $4)",
                    config.table
                ),
                &[
                    &migration.name,
                    &migration.sql,
                    &migration_hash,
                    &HASH_VERSION,
                ],
            )
            .await?;

            tx.commit().await?;
        } else {
            execute_each(client, &migration.sql)
                .await
                .map_err(|(n, at, e)| {
                    crate::Error::Migration(format!(
                        "failed to apply migration {} (statement {n}): {}",
                        migration.name,
                        format_pg_error(&e, &migration.sql, at)
                    ))
                })?;

            client
                .execute(
                    &format!(
                        "INSERT INTO {} (name, sql_source, hash, hash_version) \
                         VALUES ($1, $2, $3, $4)",
                        config.table
                    ),
                    &[
                        &migration.name,
                        &migration.sql,
                        &migration_hash,
                        &HASH_VERSION,
                    ],
                )
                .await?;
        }

        newly_applied.push(migration.name.clone());
    }

    Ok(newly_applied)
}

/// The one-time upgrade of a tracking table's stored hashes: every row whose
/// `hash_version` is not the current one gets the normalized hash of its
/// stored text, when it has one. Rows without `sql_source` stay unhashed —
/// they can never drift.
async fn backfill_hashes(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    let rows = client
        .query(
            &format!(
                "SELECT name, sql_source FROM {} \
                 WHERE hash_version IS DISTINCT FROM $1::int4 AND sql_source IS NOT NULL",
                config.table
            ),
            &[&HASH_VERSION],
        )
        .await?;
    for row in &rows {
        let name: String = row.get(0);
        let sql: String = row.get(1);
        client
            .execute(
                &format!(
                    "UPDATE {} SET hash = $1, hash_version = $2 WHERE name = $3",
                    config.table
                ),
                &[&hash(&sql), &HASH_VERSION, &name],
            )
            .await?;
    }
    Ok(())
}

/// The stored `sql_source` of a drifted migration, for its diff.
async fn stored_sql_source(
    client: &Client,
    config: &MigrationsConfig,
    name: &str,
) -> Result<Option<String>, crate::Error> {
    let row = client
        .query_opt(
            &format!("SELECT sql_source FROM {} WHERE name = $1", config.table),
            &[&name],
        )
        .await?;
    Ok(row.and_then(|row| row.get(0)))
}

/// The drift message: the migration's name, a unified diff of what was
/// applied against the file (when its text is stored), and how to make an
/// intentional edit stop being drift. The error also says how to downgrade
/// drift to a warning.
fn drift_message(
    name: &str,
    stored_sql: Option<&str>,
    file_sql: &str,
    fail_on_drift: bool,
) -> String {
    let mut message = format!("migration '{name}' has been modified since it was applied:");
    if let Some(stored_sql) = stored_sql {
        let file_name = format!("{name}.sql");
        let diff = similar::TextDiff::from_lines(stored_sql, file_sql)
            .unified_diff()
            .context_radius(3)
            .header("applied", &file_name)
            .to_string();
        message.push('\n');
        message.push_str(diff.trim_end());
    }
    message.push_str(&format!(
        "\nIf the change is intentional (it does not need to run again), record the file as \
         applied with `typedpg migrate accept {name}`; otherwise restore the file and add a new \
         migration."
    ));
    if fail_on_drift {
        message.push_str(
            "\nSet fail_on_drift = false ([package.metadata.typedpg.migrations] in Cargo.toml) \
             or \"failOnDrift\": false (\"migrations\" in typedpg.config.json) to downgrade this \
             to a warning.",
        );
    }
    message
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
    validate(config)?;

    // Read-only, so neither create the table nor add the columns `run`
    // would: a table from before `sql_source` existed reads with no stored
    // text, and no table at all as nothing applied.
    let (exists, has_sql_source, has_hash, has_hash_version): (bool, bool, bool, bool) = {
        let row = client
            .query_one(
                "SELECT c.oid IS NOT NULL,
                        EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                                WHERE a.attrelid = c.oid AND a.attname = 'sql_source'
                                  AND NOT a.attisdropped),
                        EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                                WHERE a.attrelid = c.oid AND a.attname = 'hash'
                                  AND NOT a.attisdropped),
                        EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                                WHERE a.attrelid = c.oid AND a.attname = 'hash_version'
                                  AND NOT a.attisdropped)
                 FROM (SELECT pg_catalog.to_regclass($1) AS oid) AS c",
                &[&config.table],
            )
            .await?;
        (row.get(0), row.get(1), row.get(2), row.get(3))
    };

    let mut applied: std::collections::HashMap<
        String,
        (chrono::DateTime<chrono::Utc>, Option<String>),
    > = std::collections::HashMap::new();
    if exists {
        let hash_column = if has_hash { "hash" } else { "NULL::text" };
        let version_column = if has_hash_version {
            "hash_version"
        } else {
            "NULL::int4"
        };
        let rows = client
            .query(
                &format!(
                    "SELECT name, applied_at, {hash_column}, {version_column} \
                     FROM {} ORDER BY name",
                    config.table
                ),
                &[],
            )
            .await?;
        for row in &rows {
            let name: String = row.try_get(0).map_err(|e| {
                crate::Error::Migration(format!("failed to read migration name: {e}"))
            })?;
            let applied_at: chrono::DateTime<chrono::Utc> = row
                .try_get(1)
                .map_err(|e| crate::Error::Migration(format!("failed to read applied_at: {e}")))?;
            let stored_hash: Option<String> = row
                .try_get(2)
                .map_err(|e| crate::Error::Migration(format!("failed to read hash: {e}")))?;
            let hash_version: Option<i32> = row.try_get(3).map_err(|e| {
                crate::Error::Migration(format!("failed to read hash_version: {e}"))
            })?;
            // Only a current-algorithm hash is read; an older row's text is
            // hashed in memory below.
            let stored_hash = if hash_version == Some(HASH_VERSION) {
                stored_hash
            } else {
                None
            };
            applied.insert(name, (applied_at, stored_hash));
        }

        // The stale rows' text is fetched (and hashed) here, never written
        // back: `status` only reads.
        if has_sql_source {
            let stale_filter = if has_hash_version {
                " AND hash_version IS DISTINCT FROM $1::int4"
            } else {
                ""
            };
            let sql = format!(
                "SELECT name, sql_source FROM {} WHERE sql_source IS NOT NULL{stale_filter}",
                config.table
            );
            let rows = if has_hash_version {
                client.query(&sql, &[&HASH_VERSION]).await?
            } else {
                client.query(&sql, &[]).await?
            };
            for row in &rows {
                let name: String = row.try_get(0).map_err(|e| {
                    crate::Error::Migration(format!("failed to read migration name: {e}"))
                })?;
                let stored_sql: String = row.try_get(1).map_err(|e| {
                    crate::Error::Migration(format!("failed to read sql_source: {e}"))
                })?;
                if let Some((_, stored_hash)) = applied.get_mut(&name) {
                    *stored_hash = Some(hash(&stored_sql));
                }
            }
        }
    }

    let statuses = source
        .migrations()
        .iter()
        .map(|m| {
            let info = applied.get(&m.name);
            let drifted = info.is_some_and(|(_, stored)| {
                stored
                    .as_deref()
                    .is_some_and(|stored| stored != hash(&m.sql))
            });
            if drifted {
                eprintln!(
                    "warning: migration '{}' has been modified since it was applied",
                    m.name
                );
            }
            MigrationStatus {
                name: m.name.clone(),
                applied: info.is_some(),
                applied_at: info.map(|(at, _)| *at),
                drifted,
            }
        })
        .collect();

    Ok(statuses)
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
    validate(config)?;
    acquire_lock(client, config).await?;

    // Under the lock, as in `run`.
    let result = match ensure_table(client, config).await {
        Ok(()) => revert_inner(client, source, name, force, config).await,
        Err(e) => Err(e),
    };

    let release = release_lock(client, config).await;
    match (&result, release) {
        (Ok(_), Ok(_)) => result,
        (Ok(_), Err(rel_err)) => Err(rel_err),
        (Err(_), Err(rel_err)) => {
            eprintln!("typedpg: failed to release advisory lock: {rel_err}");
            result
        }
        (Err(_), Ok(_)) => result,
    }
}

async fn revert_inner(
    client: &mut Client,
    source: &MigrationSource,
    name: &str,
    force: bool,
    config: &MigrationsConfig,
) -> Result<(), crate::Error> {
    // Check if the migration is actually applied
    let applied = get_applied(client, config).await?;
    if !applied.contains_key(name) {
        return Err(crate::Error::Migration(format!(
            "migration '{}' is not applied",
            name
        )));
    }

    let migration = source.find(name).ok_or_else(|| {
        crate::Error::Migration(format!("migration '{}' not found in source", name))
    })?;

    match &migration.down_sql {
        Some(down_sql) => {
            // The down SQL's own marker decides: the up's marker speaks
            // about the up's statements, and the down is the other way
            // round. A `CREATE INDEX CONCURRENTLY` cannot run inside a
            // transaction.
            let use_tx = config.use_transaction && !has_no_transaction_marker(down_sql);

            if use_tx {
                let tx = client.transaction().await?;

                tx.batch_execute(down_sql).await.map_err(|e| {
                    crate::Error::Migration(format!(
                        "failed to revert migration {}: {}",
                        name,
                        format_pg_error(&e, down_sql, 0)
                    ))
                })?;

                tx.execute(
                    &format!("DELETE FROM {} WHERE name = $1", config.table),
                    &[&name.to_string()],
                )
                .await?;

                tx.commit().await?;
            } else {
                execute_each(client, down_sql).await.map_err(|(n, at, e)| {
                    crate::Error::Migration(format!(
                        "failed to revert migration {} (statement {n}): {}",
                        name,
                        format_pg_error(&e, down_sql, at)
                    ))
                })?;

                client
                    .execute(
                        &format!("DELETE FROM {} WHERE name = $1", config.table),
                        &[&name.to_string()],
                    )
                    .await?;
            }
        }
        None => {
            if !force {
                return Err(crate::Error::Migration(format!(
                    "migration '{}' has no down file ({}.down.sql). Use force to remove the record without running SQL.",
                    name, name
                )));
            }

            client
                .execute(
                    &format!("DELETE FROM {} WHERE name = $1", config.table),
                    &[&name.to_string()],
                )
                .await?;
        }
    }

    Ok(())
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
    validate(config)?;
    acquire_lock(client, config).await?;

    // Under the lock, as in `run`.
    let result = match ensure_table(client, config).await {
        Ok(()) => accept_inner(client, source, config, name).await,
        Err(e) => Err(e),
    };

    let release = release_lock(client, config).await;
    match (&result, release) {
        (Ok(_), Ok(_)) => result,
        (Ok(_), Err(rel_err)) => Err(rel_err),
        (Err(_), Err(rel_err)) => {
            eprintln!("typedpg: failed to release advisory lock: {rel_err}");
            result
        }
        (Err(_), Ok(_)) => result,
    }
}

async fn accept_inner(
    client: &Client,
    source: &MigrationSource,
    config: &MigrationsConfig,
    name: &str,
) -> Result<(), crate::Error> {
    let migration = source
        .find(name)
        .ok_or_else(|| crate::Error::Migration(format!("no migration file named '{name}'")))?;
    if !get_applied(client, config).await?.contains_key(name) {
        return Err(crate::Error::Migration(format!(
            "migration '{name}' has not been applied: nothing to accept"
        )));
    }
    client
        .execute(
            &format!(
                "UPDATE {} SET sql_source = $1, hash = $2, hash_version = $3 WHERE name = $4",
                config.table
            ),
            &[&migration.sql, &hash(&migration.sql), &HASH_VERSION, &name],
        )
        .await?;
    Ok(())
}

/// Run `sql` outside a transaction, one statement at a time: sent whole,
/// PG would run its statements in one implicit transaction block (see
/// [`split_statements`]). Fails with the 1-based number of the failing
/// statement and its byte offset in `sql`; the statements before it stay
/// applied.
async fn execute_each(
    client: &Client,
    sql: &str,
) -> Result<(), (usize, usize, tokio_postgres::Error)> {
    for (i, statement) in split_statements(sql).into_iter().enumerate() {
        // `split_statements` returns slices of `sql`.
        let at = statement.as_ptr() as usize - sql.as_ptr() as usize;
        client
            .batch_execute(statement)
            .await
            .map_err(|e| (i + 1, at, e))?;
    }
    Ok(())
}

/// Reject a tracking table name that is not a plain qualified identifier:
/// it is interpolated into every query.
fn validate(config: &MigrationsConfig) -> Result<(), crate::Error> {
    config
        .validate()
        .map_err(|e| crate::Error::Migration(e.to_string()))
}

async fn ensure_table(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    validate(config)?;
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS {} (
            name         TEXT PRIMARY KEY,
            applied_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
            sql_source   TEXT,
            hash         TEXT,
            hash_version INTEGER
        );
        ALTER TABLE {0} ADD COLUMN IF NOT EXISTS sql_source TEXT;
        ALTER TABLE {0} ADD COLUMN IF NOT EXISTS hash TEXT;
        ALTER TABLE {0} ADD COLUMN IF NOT EXISTS hash_version INTEGER;",
        config.table
    );
    client.batch_execute(&sql).await?;
    Ok(())
}

/// Take the runner's advisory lock, polling `pg_try_advisory_lock` rather
/// than waiting in `pg_advisory_lock`: a session waiting inside a statement
/// is a transaction the holder's `CREATE INDEX CONCURRENTLY` waits for in
/// turn, which PG reports as a deadlock (and kills one of the two).
async fn acquire_lock(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    let mut delay = std::time::Duration::from_millis(20);
    loop {
        let locked: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&config.lock_id])
            .await?
            .get(0);
        if locked {
            return Ok(());
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(std::time::Duration::from_millis(500));
    }
}

async fn release_lock(client: &Client, config: &MigrationsConfig) -> Result<(), crate::Error> {
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&config.lock_id])
        .await?;
    Ok(())
}

/// Returns a map of applied migration names to their stored drift hash (and
/// the algorithm that computed it).
async fn get_applied(
    client: &Client,
    config: &MigrationsConfig,
) -> Result<std::collections::HashMap<String, AppliedMigration>, crate::Error> {
    let rows = client
        .query(
            &format!("SELECT name, hash, hash_version FROM {}", config.table),
            &[],
        )
        .await?;

    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.get(0);
        let hash: Option<String> = row.get(1);
        let hash_version: Option<i32> = row.get(2);
        map.insert(name, AppliedMigration { hash, hash_version });
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::locate_position;

    #[test]
    fn positions_are_lines_and_columns_of_the_migration() {
        let sql = "CREATE TABLE a (id int);\nCREATE TABL b (id int);\n";
        // The whole migration sent: PG's position 33 is `TABL` on line 2.
        assert_eq!(
            locate_position(sql, 0, 33),
            "\nLINE 2: CREATE TABL b (id int);\n               ^"
        );
        // One statement sent (non-transactional run): the position is
        // relative to it, the line still the file's.
        let second = sql.find("CREATE TABL b").unwrap();
        assert_eq!(
            locate_position(sql, second, 8),
            "\nLINE 2: CREATE TABL b (id int);\n               ^"
        );
        // PG counts characters, not bytes.
        assert_eq!(
            locate_position("SELECT 'é' x y", 0, 14),
            "\nLINE 1: SELECT 'é' x y\n                     ^"
        );
    }
}
