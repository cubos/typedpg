//! Database migration system with advisory-lock protection.
//!
//! This module provides functions to apply, inspect, and revert SQL migrations
//! against a PostgreSQL database. Migrations are plain `.sql` files stored in a
//! directory, sorted by a numeric prefix.
//!
//! # File layout
//!
//! ```text
//! migrations/
//!   0001_create_users.sql          # "up" migration (required)
//!   0001_create_users.down.sql     # "down" migration (optional)
//!   0002_add_email.sql
//!   0003_create_index.sql
//! ```
//!
//! Each up migration file must be named `NNNN_description.sql` where `NNNN` is a
//! numeric prefix (used for ordering). An optional down migration uses the same
//! base name with a `.down.sql` suffix.
//!
//! # Transaction behavior
//!
//! By default, each migration runs inside a transaction. You can disable
//! transactions globally via [`MigrationsConfig::use_transaction`],
//! or per-migration by adding `-- no-transaction` as the first line of the SQL file
//! (useful for `CREATE INDEX CONCURRENTLY` and similar statements). The down
//! migration may carry the marker too: it decides how the down runs.
//!
//! Outside a transaction the statements of a migration run one at a time,
//! each committing on its own: a failure leaves the statements before it
//! applied (and the migration unrecorded).
//!
//! # One implementation, two drivers
//!
//! The orchestration — the lock and its backoff, the tracking table, drift
//! checks, the apply/revert/accept flows, every message — is a sans-I/O
//! state machine in `typedpg_core::migrations::machine`. It knows which
//! query, script or sleep comes next, never runs one, and the functions
//! below perform each step on the `tokio_postgres::Client`. The npm runner
//! (`@cubos/typedpg`) drives the same machine, compiled to WebAssembly, so
//! the two cannot disagree about what a migration is or how it runs.
//!
//! # Concurrency safety
//!
//! The [`run`], [`revert`] and [`accept`] functions acquire a PostgreSQL advisory lock
//! before applying changes, preventing concurrent migration runs from conflicting.
//!
//! # Drift
//!
//! An applied migration edited since is drift: [`run`] aborts (or warns,
//! with [`MigrationsConfig::fail_on_drift`] off) and prints a diff of the
//! text that was applied against the file. Comments, whitespace and the
//! case of unquoted words do not count. [`accept`] records an intentional
//! edit as applied without running it.
//!
//! # Usage
//!
//! ```rust,no_run
//! use typedpg::migrate::{MigrationSource, MigrationsConfig, run, status};
//! use std::path::Path;
//!
//! # async fn example() -> Result<(), typedpg::Error> {
//! let (mut client, conn) =
//!     tokio_postgres::connect("host=localhost dbname=mydb user=postgres", tokio_postgres::NoTls).await?;
//! tokio::spawn(conn);
//!
//! let source = MigrationSource::from_dir(Path::new("./migrations"))?;
//! let config = MigrationsConfig::default();
//!
//! // Apply all pending migrations
//! let applied = run(&mut client, &source, &config).await?;
//!
//! // Check the status of all migrations
//! let statuses = status(&client, &source, &config).await?;
//! for s in &statuses {
//!     println!("{}: {}", s.name, if s.applied { "applied" } else { "pending" });
//! }
//! # Ok(())
//! # }
//! ```

mod runner;
mod source;

pub use runner::{MigrationStatus, accept, revert, run, status};
pub use source::{Migration, MigrationSource};

/// Migration runner settings (tracking table, advisory lock id, transaction
/// behavior). Re-exported from `typedpg_core` so consumers of the runtime
/// crate don't need to depend on `typedpg_core` directly.
pub use typedpg_core::config::{ConfigError, MigrationsConfig};
