use tokio_postgres::Client;

use super::MigrationsConfig;
use super::source::MigrationSource;
use super::split::split_statements;

/// Computes the MD5 hex digest of a migration's SQL content.
///
/// Must match the hash the server computes for the stored text,
/// [`STORED_SQL_HASH`], so the two can be compared.
fn sql_hash(sql: &str) -> String {
    format!("{:x}", md5::compute(sql.as_bytes()))
}

/// The MD5 of a stored migration's UTF-8 bytes. `md5(text)` would hash
/// the text in the *database* encoding: on a LATIN1 database every
/// migration with a non-ASCII character would look drifted.
const STORED_SQL_HASH: &str = "md5(convert_to(sql_source, 'UTF8'))";

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
    /// `true` if the migration's SQL source on disk differs from what was originally applied.
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
    let applied = get_applied(client, config).await?;
    let mut newly_applied = Vec::new();

    // Drift is checked over every applied migration before anything new
    // runs: a pending migration can sort before an applied one (merged out
    // of order), and must not run when the run is going to abort.
    for migration in source.migrations() {
        let Some(Some(h)) = applied.get(&migration.name) else {
            continue;
        };
        if *h != sql_hash(&migration.sql) {
            if config.fail_on_drift {
                return Err(crate::Error::Migration(format!(
                    "migration '{}' has been modified since it was applied; \
                     set [package.metadata.typedpg.migrations] fail_on_drift = false \
                     to downgrade to a warning",
                    migration.name
                )));
            }
            eprintln!(
                "warning: migration '{}' has been modified since it was applied",
                migration.name
            );
        }
    }

    for migration in source.migrations() {
        if applied.contains_key(&migration.name) {
            continue;
        }

        let use_tx = config.use_transaction && !migration.no_transaction;

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
                    "INSERT INTO {} (name, sql_source) VALUES ($1, $2)",
                    config.table
                ),
                &[&migration.name, &migration.sql],
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
                        "INSERT INTO {} (name, sql_source) VALUES ($1, $2)",
                        config.table
                    ),
                    &[&migration.name, &migration.sql],
                )
                .await?;
        }

        newly_applied.push(migration.name.clone());
    }

    Ok(newly_applied)
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

    // Read-only, so neither create the table nor add the column `run`
    // would: a table from before `sql_source` existed reads with no stored
    // text, and no table at all as nothing applied.
    let (exists, has_sql_source): (bool, bool) = {
        let row = client
            .query_one(
                "SELECT c.oid IS NOT NULL,
                        EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                                WHERE a.attrelid = c.oid AND a.attname = 'sql_source'
                                  AND NOT a.attisdropped)
                 FROM (SELECT pg_catalog.to_regclass($1) AS oid) AS c",
                &[&config.table],
            )
            .await?;
        (row.get(0), row.get(1))
    };
    let rows = if !exists {
        Vec::new()
    } else {
        let source_column = if has_sql_source {
            STORED_SQL_HASH
        } else {
            "NULL::text"
        };
        client
            .query(
                &format!(
                    "SELECT name, applied_at, {source_column} FROM {} ORDER BY name",
                    config.table
                ),
                &[],
            )
            .await?
    };

    let mut applied: std::collections::HashMap<
        String,
        (chrono::DateTime<chrono::Utc>, Option<String>),
    > = std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let name: String = row
            .try_get(0)
            .map_err(|e| crate::Error::Migration(format!("failed to read migration name: {e}")))?;
        let applied_at: chrono::DateTime<chrono::Utc> = row
            .try_get(1)
            .map_err(|e| crate::Error::Migration(format!("failed to read applied_at: {e}")))?;
        let stored_hash: Option<String> = row
            .try_get(2)
            .map_err(|e| crate::Error::Migration(format!("failed to read sql_hash: {e}")))?;
        applied.insert(name, (applied_at, stored_hash));
    }

    let statuses = source
        .migrations()
        .iter()
        .map(|m| {
            let info = applied.get(&m.name);
            let drifted = match &info {
                Some((_, Some(stored))) => *stored != sql_hash(&m.sql),
                _ => false,
            };
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
/// The down SQL runs in a transaction following the same rules as the up migration
/// (`config.use_transaction` and the `-- no-transaction` marker).
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
            let use_tx = config.use_transaction && !migration.no_transaction;

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
            name       TEXT PRIMARY KEY,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            sql_source TEXT
        );
        ALTER TABLE {0} ADD COLUMN IF NOT EXISTS sql_source TEXT;",
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

/// Returns a map of applied migration names to their stored SQL hash (if any).
async fn get_applied(
    client: &Client,
    config: &MigrationsConfig,
) -> Result<std::collections::HashMap<String, Option<String>>, crate::Error> {
    let rows = client
        .query(
            &format!("SELECT name, {STORED_SQL_HASH} FROM {}", config.table),
            &[],
        )
        .await?;

    let mut map = std::collections::HashMap::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.get(0);
        let hash: Option<String> = row.get(1);
        map.insert(name, hash);
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
