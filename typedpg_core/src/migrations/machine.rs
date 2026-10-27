//! The migration runner as a sans-I/O state machine.
//!
//! `typedpg::migrate` and `@cubos/typedpg` must apply, inspect, revert and
//! accept migrations exactly alike. Rather than keep two orchestrations in
//! agreement by hand, both drive this machine: it decides every query, every
//! script and every message, and each side only performs the step on its own
//! client. The machine never touches a database, an async runtime or a file.
//!
//! A [`Migrator`] is created from the [`Input`] (the action, the settings and
//! the migrations the caller loaded) and stepped:
//!
//! ```
//! use typedpg_core::migrations::machine::{
//!     Action, Input, MachineConfig, Migrator, Reply, Step,
//! };
//!
//! let mut migrator = Migrator::new(Input {
//!     action: Action::Up,
//!     config: MachineConfig {
//!         table: "public._migrations".into(),
//!         lock_id: 713705,
//!         use_transaction: true,
//!         fail_on_drift: true,
//!     },
//!     migrations: vec![],
//! });
//! match migrator.step(Reply::Start) {
//!     Step::Query { sql, .. } => assert!(sql.contains("pg_try_advisory_lock")),
//!     other => panic!("unexpected first step: {other:?}"),
//! }
//! ```
//!
//! Every value crossing the boundary is text: [`Step::Query`] parameters are
//! sent as text (the SQL casts them further when needed) and every selected
//! column is cast to text. That is what lets the Rust driver bind
//! `Option<&str>`/read `Option<String>` and the npm driver reuse its
//! `typedpgQuery` (raw text values), with no driver-specific type mapping.
//! Timestamps are selected as UTC RFC 3339 text; the drivers parse them into
//! their public types.
//!
//! The machine owns the cleanup, too: a failure inside a transaction emits
//! `ROLLBACK`, and the advisory lock is always released before the machine
//! finishes or fails. A driver that gets [`Failure::Db`] must rethrow the
//! driver error it replied to the machine with (the original error object,
//! not a message).

use super::{HASH_VERSION, drift_message, hash, locate_position, split_statements};
use crate::config::validate_table_name;

/// What a run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Apply every pending migration, in order.
    Up,
    /// Report every migration's status, read-only.
    Status,
    /// Revert one migration: the named one, or the last applied when no
    /// name is given.
    Revert {
        /// The migration to revert, or `None` for the last applied one.
        name: Option<String>,
        /// Remove the record without a down file's SQL when it has none.
        force: bool,
    },
    /// Record an applied migration's current file without running it.
    Accept {
        /// The migration to accept.
        name: String,
    },
}

/// The runner settings the machine needs. A subset of
/// [`MigrationsConfig`](crate::config::MigrationsConfig), free of the config
/// file's serde types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    /// The tracking table: `name` or `schema.name`.
    pub table: String,
    /// The advisory lock's id.
    pub lock_id: i64,
    /// Run each migration in a transaction.
    pub use_transaction: bool,
    /// Abort a run when an applied migration's statements were edited.
    pub fail_on_drift: bool,
}

/// One migration as the machine sees it: what `MigrationSource` and
/// `loadMigrations` produce, without the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The file stem, e.g. `0001_create_users`.
    pub name: String,
    /// The up SQL.
    pub sql: String,
    /// The down SQL, when a down file exists.
    pub down_sql: Option<String>,
    /// The up file's `-- no-transaction` marker.
    pub no_transaction: bool,
    /// The down file's `-- no-transaction` marker.
    pub down_no_transaction: bool,
}

/// Everything one action needs to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// What to do.
    pub action: Action,
    /// The runner settings.
    pub config: MachineConfig,
    /// The migrations, in the order they must apply.
    pub migrations: Vec<Migration>,
}

/// A database error, as the machine needs it to format the same message both
/// runners report.
///
/// `position` is the 1-based character index into the text that was sent,
/// exactly as PostgreSQL reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    /// The SQLSTATE code, when the driver exposes it.
    pub code: Option<String>,
    /// The server's primary message.
    pub message: String,
    /// `ERROR`, `FATAL`, … — absent for a client-side failure.
    pub severity: Option<String>,
    /// The server's DETAIL field.
    pub detail: Option<String>,
    /// The server's HINT field.
    pub hint: Option<String>,
    /// The error cursor's position in the sent text.
    pub position: Option<u32>,
    /// The cursor's position in an internally generated query.
    pub internal_position: Option<u32>,
    /// That internally generated query.
    pub internal_query: Option<String>,
}

/// The driver's answer to the step the machine emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The first reply, to start the machine.
    Start,
    /// Every row of a [`Step::Query`], each value's text (or NULL).
    Rows(Vec<Vec<Option<String>>>),
    /// A [`Step::Execute`], [`Step::Sleep`] or [`Step::Warn`] succeeded.
    Done,
    /// The database rejected the step.
    DbError(DbError),
}

/// What the machine asks the driver to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run a query with text parameters; reply with its rows.
    Query {
        /// The SQL.
        sql: String,
        /// The parameters, in `$1`, `$2`, … order.
        params: Vec<Option<String>>,
    },
    /// Run a script with no parameters and no result, possibly several
    /// statements.
    Execute {
        /// The SQL.
        sql: String,
    },
    /// Wait, for the advisory lock's backoff.
    Sleep {
        /// Milliseconds.
        ms: u64,
    },
    /// Report a warning: a drift the settings downgrade, or a lock release
    /// that failed while another error is already on its way out.
    Warn(String),
    /// The action is done.
    Finish(Outcome),
    /// The action failed. [`Failure::Db`] means the driver must rethrow the
    /// original error it replied with (the machine's cleanup may have gone
    /// through other steps in between).
    Fail(Failure),
}

/// A finished action's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The migrations applied in this run, in order.
    Applied(Vec<String>),
    /// Every migration's status.
    Status(Vec<StatusRow>),
    /// The migration that was reverted.
    Reverted(String),
    /// The migration recorded as applied.
    Accepted,
}

/// One migration's status. `applied_at` is UTC RFC 3339 text, parsed by each
/// driver into its public type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    /// The migration name (file stem).
    pub name: String,
    /// Whether the tracking table records it.
    pub applied: bool,
    /// When it applied, UTC RFC 3339, or `None` while pending.
    pub applied_at: Option<String>,
    /// Applied, and its statements edited since.
    pub drifted: bool,
}

/// Why an action failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A complete message, as the runners report it today.
    Message(String),
    /// Rethrow the driver error of the reply that caused the failure.
    Db,
}

/// The state machine.
#[derive(Debug, Clone)]
pub struct Migrator {
    action: Action,
    config: MachineConfig,
    migrations: Vec<Migration>,
    stage: Stage,
    /// Rows of the tracking table, by name, as `Up`/`Revert`/`Accept` read
    /// them.
    applied: Vec<(String, AppliedRow)>,
    /// Migrations applied by this run.
    newly_applied: Vec<String>,
    /// How far the up flow's drift check got.
    drift_index: usize,
    /// How far the up flow's apply loop got.
    apply_index: usize,
    /// Status rows built so far.
    status_rows: Vec<StatusRow>,
    /// The tracking table's rows, as `Status` reads them.
    status_applied: Vec<(String, StatusInfo)>,
}

/// What a tracking-table row says about drift.
#[derive(Debug, Clone)]
struct AppliedRow {
    hash: Option<String>,
    hash_version: Option<i32>,
}

impl AppliedRow {
    /// Whether the stored hash differs from the file's, computed with the
    /// current algorithm. A row without a hash (no `sql_source` stored, from
    /// before that column existed) is not checked, and neither is one whose
    /// hash predates the current algorithm — `Up` re-hashes it first.
    fn drifted(&self, sql: &str) -> bool {
        self.hash_version == Some(HASH_VERSION)
            && self
                .hash
                .as_deref()
                .is_some_and(|stored| stored != hash(sql))
    }
}

/// What a status read keeps per applied migration.
#[derive(Debug, Clone)]
struct StatusInfo {
    applied_at: Option<String>,
    hash: Option<String>,
}

/// What to do after the lock and tracking table are ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    Up,
    Revert,
    Accept,
}

/// Where the machine is: the step it last emitted, and what that step was
/// for.
#[derive(Debug, Clone)]
enum Stage {
    Start,
    /// A `pg_try_advisory_lock` query; `delay_ms` is what a "not locked"
    /// answer sleeps before trying again.
    TryLock {
        delay_ms: u64,
        then: Then,
    },
    /// The backoff sleep; `next_delay_ms` is the delay of the next attempt.
    LockSleep {
        next_delay_ms: u64,
        then: Then,
    },
    /// The tracking-table script.
    EnsureTable {
        then: Then,
    },
    /// The backfill select.
    BackfillSelect {
        then: Then,
    },
    /// The backfill update of `rows[pos]`.
    BackfillUpdate {
        rows: Vec<(String, String)>,
        pos: usize,
        then: Then,
    },
    /// The applied-migrations select.
    AppliedSelect {
        then: Then,
    },
    /// The stored `sql_source` of the drifted migration at `index`.
    DriftStored {
        index: usize,
    },
    /// A drift warning was emitted; continue the check after `index`.
    DriftContinue {
        index: usize,
    },
    UpBegin {
        index: usize,
    },
    UpTransaction {
        index: usize,
    },
    UpInsert {
        index: usize,
    },
    UpCommit {
        index: usize,
    },
    /// Statement `stmt` of a `-- no-transaction` migration.
    UpStatement {
        index: usize,
        stmt: usize,
    },
    UpNoTransactionInsert {
        index: usize,
    },
    /// The rollback emitted before a failure; its own error is ignored.
    UpRollback {
        failure: Failure,
    },
    RevertBegin {
        index: usize,
    },
    RevertTransaction {
        index: usize,
    },
    RevertDelete {
        index: usize,
    },
    RevertCommit {
        index: usize,
    },
    /// Statement `stmt` of a `-- no-transaction` down file.
    RevertStatement {
        index: usize,
        stmt: usize,
    },
    RevertNoTransactionDelete {
        index: usize,
    },
    /// The delete of a forced revert without a down file.
    RevertForget {
        index: usize,
    },
    RevertRollback {
        failure: Failure,
    },
    AcceptUpdate,
    /// The advisory unlock; what to do with its reply.
    Unlock {
        pending: Option<Failure>,
        outcome: Option<Outcome>,
    },
    /// A failed unlock was reported as a warning; the original failure is
    /// still on its way out.
    UnlockWarn {
        pending: Failure,
    },
    StatusColumns,
    StatusRows {
        has_source: bool,
        has_hash_version: bool,
    },
    StatusStale,
    StatusWarn {
        next: usize,
    },
    Terminal,
}

impl Migrator {
    /// Create a machine for `input`, before its first step.
    pub fn new(input: Input) -> Self {
        Self {
            action: input.action,
            config: input.config,
            migrations: input.migrations,
            stage: Stage::Start,
            applied: Vec::new(),
            newly_applied: Vec::new(),
            drift_index: 0,
            apply_index: 0,
            status_rows: Vec::new(),
            status_applied: Vec::new(),
        }
    }

    /// Advance the machine: answer the step it emitted (starting with
    /// [`Reply::Start`]) and get the next one.
    pub fn step(&mut self, reply: Reply) -> Step {
        let stage = std::mem::replace(&mut self.stage, Stage::Terminal);
        match (stage, reply) {
            (Stage::Start, Reply::Start) => self.start(),

            (Stage::TryLock { delay_ms, then }, Reply::Rows(rows)) => {
                if is_true(first_value(&rows)) {
                    self.ensure_table(then)
                } else {
                    self.stage = Stage::LockSleep {
                        next_delay_ms: (delay_ms * 2).min(500),
                        then,
                    };
                    Step::Sleep { ms: delay_ms }
                }
            }
            (Stage::TryLock { .. }, _) => self.fail_now(Failure::Db),
            (
                Stage::LockSleep {
                    next_delay_ms,
                    then,
                },
                _,
            ) => self.try_lock(next_delay_ms, then),

            (Stage::EnsureTable { then }, Reply::Done) => match then {
                Then::Up => self.backfill_select(then),
                Then::Revert => self.read_applied(then),
                // An accept of a name no file has runs no query, as the
                // runners already do.
                Then::Accept => match &self.action {
                    Action::Accept { name } if !self.migrations.iter().any(|m| &m.name == name) => {
                        self.fail_locked(Failure::Message(format!(
                            "no migration file named '{name}'"
                        )))
                    }
                    _ => self.read_applied(then),
                },
            },
            (Stage::EnsureTable { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::BackfillSelect { then }, Reply::Rows(rows)) => {
                let updates: Vec<(String, String)> = rows
                    .iter()
                    .filter_map(|row| {
                        let name = row.first()?.clone()?;
                        let sql = row.get(1)?.clone()?;
                        Some((name, sql))
                    })
                    .collect();
                if updates.is_empty() {
                    self.read_applied(then)
                } else {
                    let (name, sql) = updates[0].clone();
                    self.stage = Stage::BackfillUpdate {
                        rows: updates,
                        pos: 0,
                        then,
                    };
                    self.backfill_update(&name, &sql)
                }
            }
            (Stage::BackfillSelect { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::BackfillUpdate { rows, pos, then }, Reply::Rows(_)) => {
                let next = pos + 1;
                if next < rows.len() {
                    let (name, sql) = rows[next].clone();
                    self.stage = Stage::BackfillUpdate {
                        rows,
                        pos: next,
                        then,
                    };
                    self.backfill_update(&name, &sql)
                } else {
                    self.read_applied(then)
                }
            }
            (Stage::BackfillUpdate { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::AppliedSelect { then }, Reply::Rows(rows)) => match self.parse_applied(&rows) {
                Ok(()) => match then {
                    Then::Up => {
                        self.drift_index = 0;
                        self.drift_step()
                    }
                    Then::Revert => self.revert_step(),
                    Then::Accept => self.accept_step(),
                },
                Err(failure) => self.fail_locked(failure),
            },
            (Stage::AppliedSelect { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::DriftStored { index }, Reply::Rows(rows)) => {
                let stored = first_value(&rows).map(str::to_owned);
                let migration = &self.migrations[index];
                let message = drift_message(
                    &migration.name,
                    stored.as_deref(),
                    &migration.sql,
                    self.config.fail_on_drift,
                );
                if self.config.fail_on_drift {
                    self.fail_locked(Failure::Message(message))
                } else {
                    self.stage = Stage::DriftContinue { index: index + 1 };
                    Step::Warn(format!("warning: {message}"))
                }
            }
            (Stage::DriftStored { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::DriftContinue { index }, _) => {
                self.drift_index = index;
                self.drift_step()
            }

            (Stage::UpBegin { index }, Reply::Done) => {
                let sql = self.migrations[index].sql.clone();
                self.stage = Stage::UpTransaction { index };
                Step::Execute { sql }
            }
            (Stage::UpBegin { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::UpTransaction { index }, Reply::Done) => self.up_insert(index),
            (Stage::UpTransaction { index }, Reply::DbError(error)) => {
                let migration = &self.migrations[index];
                let message = format!(
                    "failed to apply migration {}: {}",
                    migration.name,
                    format_db_error(&error, &migration.sql, 0)
                );
                self.up_rollback(Failure::Message(message))
            }
            (Stage::UpTransaction { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::UpInsert { index }, Reply::Rows(_)) => {
                self.stage = Stage::UpCommit { index };
                Step::Execute {
                    sql: "COMMIT".to_string(),
                }
            }
            (Stage::UpInsert { .. }, _) => self.up_rollback(Failure::Db),

            (Stage::UpCommit { index }, Reply::Done) => {
                self.newly_applied.push(self.migrations[index].name.clone());
                self.apply_index = index + 1;
                self.apply_step()
            }
            (Stage::UpCommit { .. }, _) => self.up_rollback(Failure::Db),

            (Stage::UpStatement { index, stmt }, Reply::Done) => {
                let statements = split_statements(&self.migrations[index].sql);
                if let Some(statement) = statements.get(stmt + 1) {
                    self.stage = Stage::UpStatement {
                        index,
                        stmt: stmt + 1,
                    };
                    Step::Execute {
                        sql: (*statement).to_string(),
                    }
                } else {
                    self.up_no_transaction_insert(index)
                }
            }
            (Stage::UpStatement { index, stmt }, Reply::DbError(error)) => {
                let migration = &self.migrations[index];
                let statements = split_statements(&migration.sql);
                let statement = statements.get(stmt).copied().unwrap_or_default();
                // `split_statements` returns slices of the migration.
                let base = statement.as_ptr() as usize - migration.sql.as_ptr() as usize;
                let message = format!(
                    "failed to apply migration {} (statement {}): {}",
                    migration.name,
                    stmt + 1,
                    format_db_error(&error, &migration.sql, base)
                );
                self.fail_locked(Failure::Message(message))
            }
            (Stage::UpStatement { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::UpNoTransactionInsert { index }, Reply::Rows(_)) => {
                self.newly_applied.push(self.migrations[index].name.clone());
                self.apply_index = index + 1;
                self.apply_step()
            }
            (Stage::UpNoTransactionInsert { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::UpRollback { failure }, _) => self.fail_locked(failure),

            (Stage::RevertBegin { index }, Reply::Done) => {
                let sql = self.migrations[index].down_sql.clone().unwrap_or_default();
                self.stage = Stage::RevertTransaction { index };
                Step::Execute { sql }
            }
            (Stage::RevertBegin { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::RevertTransaction { index }, Reply::Done) => {
                self.stage = Stage::RevertDelete { index };
                self.delete_query(index)
            }
            (Stage::RevertTransaction { index }, Reply::DbError(error)) => {
                let migration = &self.migrations[index];
                let down = migration.down_sql.clone().unwrap_or_default();
                let message = format!(
                    "failed to revert migration {}: {}",
                    migration.name,
                    format_db_error(&error, &down, 0)
                );
                self.revert_rollback(Failure::Message(message))
            }
            (Stage::RevertTransaction { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::RevertDelete { index }, Reply::Rows(_)) => {
                self.stage = Stage::RevertCommit { index };
                Step::Execute {
                    sql: "COMMIT".to_string(),
                }
            }
            (Stage::RevertDelete { .. }, _) => self.revert_rollback(Failure::Db),

            (Stage::RevertCommit { index }, Reply::Done) => self.release(
                None,
                Some(Outcome::Reverted(self.migrations[index].name.clone())),
            ),
            (Stage::RevertCommit { .. }, _) => self.revert_rollback(Failure::Db),

            (Stage::RevertStatement { index, stmt }, Reply::Done) => {
                let down = self.migrations[index].down_sql.clone().unwrap_or_default();
                let statements = split_statements(&down);
                if let Some(statement) = statements.get(stmt + 1) {
                    self.stage = Stage::RevertStatement {
                        index,
                        stmt: stmt + 1,
                    };
                    Step::Execute {
                        sql: (*statement).to_string(),
                    }
                } else {
                    self.stage = Stage::RevertNoTransactionDelete { index };
                    self.delete_query(index)
                }
            }
            (Stage::RevertStatement { index, stmt }, Reply::DbError(error)) => {
                let migration = &self.migrations[index];
                let down = migration.down_sql.clone().unwrap_or_default();
                let statements = split_statements(&down);
                let statement = statements.get(stmt).copied().unwrap_or_default();
                let base = statement.as_ptr() as usize - down.as_ptr() as usize;
                let message = format!(
                    "failed to revert migration {} (statement {}): {}",
                    migration.name,
                    stmt + 1,
                    format_db_error(&error, &down, base)
                );
                self.fail_locked(Failure::Message(message))
            }
            (Stage::RevertStatement { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::RevertNoTransactionDelete { index }, Reply::Rows(_)) => self.release(
                None,
                Some(Outcome::Reverted(self.migrations[index].name.clone())),
            ),
            (Stage::RevertNoTransactionDelete { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::RevertForget { index }, Reply::Rows(_)) => self.release(
                None,
                Some(Outcome::Reverted(self.migrations[index].name.clone())),
            ),
            (Stage::RevertForget { .. }, _) => self.fail_locked(Failure::Db),

            (Stage::RevertRollback { failure }, _) => self.fail_locked(failure),

            (Stage::AcceptUpdate, Reply::Rows(_)) => self.release(None, Some(Outcome::Accepted)),
            (Stage::AcceptUpdate, _) => self.fail_locked(Failure::Db),

            (Stage::Unlock { pending, outcome }, Reply::Rows(_) | Reply::Done) => match pending {
                Some(failure) => self.fail_now(failure),
                None => self.finish(outcome.unwrap_or(Outcome::Accepted)),
            },
            (Stage::Unlock { pending, .. }, Reply::DbError(error)) => match pending {
                Some(pending) => {
                    self.stage = Stage::UnlockWarn { pending };
                    Step::Warn(format!(
                        "typedpg: failed to release advisory lock: {}",
                        format_db_error(&error, &self.unlock_sql(), 0)
                    ))
                }
                // After a success the unlock error is the run's error, as
                // the runners already surface it.
                None => self.fail_now(Failure::Db),
            },
            (Stage::UnlockWarn { pending }, _) => self.fail_now(pending),

            (Stage::StatusColumns, Reply::Rows(rows)) => {
                let row = rows.first().cloned().unwrap_or_default();
                let exists = is_true(row.first().and_then(|v| v.as_deref()));
                if !exists {
                    self.stage = Stage::Terminal;
                    let statuses = self
                        .migrations
                        .iter()
                        .map(|m| StatusRow {
                            name: m.name.clone(),
                            applied: false,
                            applied_at: None,
                            drifted: false,
                        })
                        .collect();
                    Step::Finish(Outcome::Status(statuses))
                } else {
                    let has_source = is_true(row.get(1).and_then(|v| v.as_deref()));
                    let has_hash = is_true(row.get(2).and_then(|v| v.as_deref()));
                    let has_hash_version = is_true(row.get(3).and_then(|v| v.as_deref()));
                    self.stage = Stage::StatusRows {
                        has_source,
                        has_hash_version,
                    };
                    Step::Query {
                        sql: self.status_rows_sql(has_hash, has_hash_version),
                        params: Vec::new(),
                    }
                }
            }
            (Stage::StatusColumns, _) => self.fail_now(Failure::Db),

            (
                Stage::StatusRows {
                    has_source,
                    has_hash_version,
                },
                Reply::Rows(rows),
            ) => {
                for row in &rows {
                    let name = match row.first().cloned().flatten() {
                        Some(name) => name,
                        None => {
                            return self.fail_now(Failure::Message(
                                "failed to read migration name".to_string(),
                            ));
                        }
                    };
                    let applied_at = row.get(1).cloned().flatten();
                    if applied_at.is_none() {
                        return self
                            .fail_now(Failure::Message("failed to read applied_at".to_string()));
                    }
                    let stored_hash = row.get(2).cloned().flatten();
                    let version = match row.get(3).cloned().flatten() {
                        Some(text) => match text.parse::<i32>() {
                            Ok(version) => Some(version),
                            Err(_) => {
                                return self.fail_now(Failure::Message(format!(
                                    "failed to read hash_version: {text}"
                                )));
                            }
                        },
                        None => None,
                    };
                    self.status_applied.push((
                        name,
                        StatusInfo {
                            applied_at,
                            hash: if version == Some(HASH_VERSION) {
                                stored_hash
                            } else {
                                None
                            },
                        },
                    ));
                }
                if has_source {
                    self.stage = Stage::StatusStale;
                    Step::Query {
                        sql: self.status_stale_sql(has_hash_version),
                        params: if has_hash_version {
                            vec![Some(HASH_VERSION.to_string())]
                        } else {
                            Vec::new()
                        },
                    }
                } else {
                    self.build_status(0)
                }
            }
            (Stage::StatusRows { .. }, _) => self.fail_now(Failure::Db),

            (Stage::StatusStale, Reply::Rows(rows)) => {
                for row in &rows {
                    let name = row.first().cloned().flatten();
                    let sql = row.get(1).cloned().flatten();
                    if let (Some(name), Some(sql)) = (name, sql)
                        && let Some((_, info)) = self
                            .status_applied
                            .iter_mut()
                            .find(|(applied, _)| *applied == name)
                    {
                        info.hash = Some(hash(&sql));
                    }
                }
                self.build_status(0)
            }
            (Stage::StatusStale, _) => self.fail_now(Failure::Db),

            (Stage::StatusWarn { next }, _) => self.build_status(next),

            (Stage::Terminal, _) => Step::Fail(Failure::Message(
                "typedpg: the migration machine is already finished".to_string(),
            )),

            (_, _) => Step::Fail(Failure::Message(
                "typedpg: invalid reply to a migration machine step".to_string(),
            )),
        }
    }

    /// The first step: validate the settings, then start the action.
    fn start(&mut self) -> Step {
        if let Err(error) = validate_table_name(&self.config.table) {
            return self.fail_now(Failure::Message(error.to_string()));
        }
        match &self.action {
            Action::Status => {
                self.stage = Stage::StatusColumns;
                Step::Query {
                    sql: self.status_columns_sql(),
                    params: vec![Some(self.config.table.clone())],
                }
            }
            Action::Up => self.try_lock(20, Then::Up),
            Action::Revert { .. } => self.try_lock(20, Then::Revert),
            Action::Accept { .. } => self.try_lock(20, Then::Accept),
        }
    }

    /// Try the advisory lock. Polled rather than waited for in
    /// `pg_advisory_lock`: a session waiting inside a statement is a
    /// transaction the holder's `CREATE INDEX CONCURRENTLY` waits for in
    /// turn, which PG reports as a deadlock (and kills one of the two).
    fn try_lock(&mut self, delay_ms: u64, then: Then) -> Step {
        self.stage = Stage::TryLock { delay_ms, then };
        Step::Query {
            sql: self.try_lock_sql(),
            params: vec![Some(self.config.lock_id.to_string())],
        }
    }

    /// Create or upgrade the tracking table. Under the lock: two runners
    /// racing on a fresh database would otherwise both find it missing, and
    /// one `CREATE TABLE IF NOT EXISTS` fails on the other's catalog rows.
    fn ensure_table(&mut self, then: Then) -> Step {
        self.stage = Stage::EnsureTable { then };
        Step::Execute {
            sql: format!(
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
                self.config.table
            ),
        }
    }

    /// The one-time upgrade of the tracking table's stored hashes: every row
    /// whose `hash_version` is not the current one gets the normalized hash
    /// of its stored text, when it has one. Rows without `sql_source` stay
    /// unhashed — they can never drift.
    fn backfill_select(&mut self, then: Then) -> Step {
        self.stage = Stage::BackfillSelect { then };
        Step::Query {
            sql: format!(
                "SELECT name::pg_catalog.text, sql_source::pg_catalog.text FROM {} \
                 WHERE hash_version IS DISTINCT FROM ($1::pg_catalog.text)::pg_catalog.int4 \
                   AND sql_source IS NOT NULL",
                self.config.table
            ),
            params: vec![Some(HASH_VERSION.to_string())],
        }
    }

    fn backfill_update(&mut self, name: &str, sql: &str) -> Step {
        Step::Query {
            sql: format!(
                "UPDATE {} SET hash = $1::pg_catalog.text, \
                 hash_version = ($2::pg_catalog.text)::pg_catalog.int4 \
                 WHERE name = $3::pg_catalog.text",
                self.config.table
            ),
            params: vec![
                Some(hash(sql)),
                Some(HASH_VERSION.to_string()),
                Some(name.to_string()),
            ],
        }
    }

    fn read_applied(&mut self, then: Then) -> Step {
        self.stage = Stage::AppliedSelect { then };
        Step::Query {
            sql: format!(
                "SELECT name::pg_catalog.text, hash::pg_catalog.text, \
                 hash_version::pg_catalog.text FROM {}",
                self.config.table
            ),
            params: Vec::new(),
        }
    }

    fn parse_applied(&mut self, rows: &[Vec<Option<String>>]) -> Result<(), Failure> {
        let mut applied: Vec<(String, AppliedRow)> = Vec::new();
        for row in rows {
            let name = row
                .first()
                .cloned()
                .flatten()
                .ok_or_else(|| Failure::Message("failed to read migration name".to_string()))?;
            let stored_hash = row.get(1).cloned().flatten();
            let hash_version = match row.get(2).cloned().flatten() {
                Some(text) => Some(text.parse::<i32>().map_err(|_| {
                    Failure::Message(format!("failed to read hash_version: {text}"))
                })?),
                None => None,
            };
            applied.push((
                name,
                AppliedRow {
                    hash: stored_hash,
                    hash_version,
                },
            ));
        }
        self.applied = applied;
        Ok(())
    }

    /// Check every applied migration for drift before applying anything: a
    /// pending migration can sort before an applied one (merged out of
    /// order), and must not run when the run is going to abort.
    fn drift_step(&mut self) -> Step {
        loop {
            let Some(migration) = self.migrations.get(self.drift_index) else {
                return self.apply_step();
            };
            let drifted = self
                .applied
                .iter()
                .find(|(name, _)| *name == migration.name)
                .is_some_and(|(_, stored)| stored.drifted(&migration.sql));
            if drifted {
                self.stage = Stage::DriftStored {
                    index: self.drift_index,
                };
                return Step::Query {
                    sql: format!(
                        "SELECT sql_source::pg_catalog.text FROM {} \
                         WHERE name = $1::pg_catalog.text",
                        self.config.table
                    ),
                    params: vec![Some(migration.name.clone())],
                };
            }
            self.drift_index += 1;
        }
    }

    fn apply_step(&mut self) -> Step {
        let migration = loop {
            let Some(migration) = self.migrations.get(self.apply_index) else {
                return self.release(None, Some(Outcome::Applied(self.newly_applied.clone())));
            };
            if !self.applied.iter().any(|(name, _)| *name == migration.name) {
                break migration;
            }
            self.apply_index += 1;
        };

        let index = self.apply_index;
        if self.config.use_transaction && !migration.no_transaction {
            self.stage = Stage::UpBegin { index };
            return Step::Execute {
                sql: "BEGIN".to_string(),
            };
        }

        let sql = migration.sql.clone();
        let statements = split_statements(&sql);
        match statements.first() {
            Some(statement) => {
                self.stage = Stage::UpStatement { index, stmt: 0 };
                Step::Execute {
                    sql: (*statement).to_string(),
                }
            }
            None => self.up_no_transaction_insert(index),
        }
    }

    fn up_insert(&mut self, index: usize) -> Step {
        let migration = &self.migrations[index];
        let name = migration.name.clone();
        let sql = migration.sql.clone();
        self.stage = Stage::UpInsert { index };
        Step::Query {
            sql: format!(
                "INSERT INTO {} (name, sql_source, hash, hash_version) \
                 VALUES (($1::pg_catalog.text), ($2::pg_catalog.text), \
                 ($3::pg_catalog.text), (($4::pg_catalog.text)::pg_catalog.int4))",
                self.config.table
            ),
            params: vec![
                Some(name),
                Some(sql),
                Some(hash(&self.migrations[index].sql)),
                Some(HASH_VERSION.to_string()),
            ],
        }
    }

    fn up_no_transaction_insert(&mut self, index: usize) -> Step {
        self.stage = Stage::UpNoTransactionInsert { index };
        let migration = &self.migrations[index];
        let name = migration.name.clone();
        let sql = migration.sql.clone();
        Step::Query {
            sql: format!(
                "INSERT INTO {} (name, sql_source, hash, hash_version) \
                 VALUES (($1::pg_catalog.text), ($2::pg_catalog.text), \
                 ($3::pg_catalog.text), (($4::pg_catalog.text)::pg_catalog.int4))",
                self.config.table
            ),
            params: vec![
                Some(name),
                Some(sql),
                Some(hash(&self.migrations[index].sql)),
                Some(HASH_VERSION.to_string()),
            ],
        }
    }

    fn up_rollback(&mut self, failure: Failure) -> Step {
        self.stage = Stage::UpRollback { failure };
        Step::Execute {
            sql: "ROLLBACK".to_string(),
        }
    }

    fn revert_step(&mut self) -> Step {
        let (name, force) = match &self.action {
            Action::Revert { name, force } => (name.clone(), *force),
            _ => (None, false),
        };
        let target = match name {
            Some(name) => name,
            None => match self
                .migrations
                .iter()
                .rev()
                .find(|m| self.applied.iter().any(|(name, _)| *name == m.name))
            {
                Some(migration) => migration.name.clone(),
                None => {
                    return self.fail_locked(Failure::Message(
                        "no applied migrations to revert".to_string(),
                    ));
                }
            },
        };
        if !self.applied.iter().any(|(name, _)| *name == target) {
            return self.fail_locked(Failure::Message(format!(
                "migration '{target}' is not applied"
            )));
        }
        let Some(index) = self.migrations.iter().position(|m| m.name == target) else {
            return self.fail_locked(Failure::Message(format!(
                "migration '{target}' not found in source"
            )));
        };

        match self.migrations[index].down_sql.clone() {
            Some(down) => {
                // The down SQL's own marker decides: the up's marker speaks
                // about the up's statements, and the down is the other way
                // round. A `CREATE INDEX CONCURRENTLY` cannot run inside a
                // transaction.
                let use_transaction =
                    self.config.use_transaction && !self.migrations[index].down_no_transaction;
                if use_transaction {
                    self.stage = Stage::RevertBegin { index };
                    return Step::Execute {
                        sql: "BEGIN".to_string(),
                    };
                }
                match split_statements(&down).first() {
                    Some(statement) => {
                        self.stage = Stage::RevertStatement { index, stmt: 0 };
                        Step::Execute {
                            sql: (*statement).to_string(),
                        }
                    }
                    None => {
                        self.stage = Stage::RevertNoTransactionDelete { index };
                        self.delete_query(index)
                    }
                }
            }
            None if !force => self.fail_locked(Failure::Message(format!(
                "migration '{target}' has no down file ({target}.down.sql). \
                 Use force to remove the record without running SQL."
            ))),
            None => {
                self.stage = Stage::RevertForget { index };
                self.delete_query(index)
            }
        }
    }

    fn delete_query(&mut self, index: usize) -> Step {
        Step::Query {
            sql: format!(
                "DELETE FROM {} WHERE name = $1::pg_catalog.text",
                self.config.table
            ),
            params: vec![Some(self.migrations[index].name.clone())],
        }
    }

    fn revert_rollback(&mut self, failure: Failure) -> Step {
        self.stage = Stage::RevertRollback { failure };
        Step::Execute {
            sql: "ROLLBACK".to_string(),
        }
    }

    fn accept_step(&mut self) -> Step {
        let name = match &self.action {
            Action::Accept { name } => name.clone(),
            _ => String::new(),
        };
        if !self.applied.iter().any(|(applied, _)| *applied == name) {
            return self.fail_locked(Failure::Message(format!(
                "migration '{name}' has not been applied: nothing to accept"
            )));
        }
        let Some(index) = self.migrations.iter().position(|m| m.name == name) else {
            return self.fail_locked(Failure::Message(format!(
                "no migration file named '{name}'"
            )));
        };
        let migration = &self.migrations[index];
        let sql = migration.sql.clone();
        self.stage = Stage::AcceptUpdate;
        Step::Query {
            sql: format!(
                "UPDATE {} SET sql_source = ($1::pg_catalog.text), \
                 hash = ($2::pg_catalog.text), \
                 hash_version = (($3::pg_catalog.text)::pg_catalog.int4) \
                 WHERE name = ($4::pg_catalog.text)",
                self.config.table
            ),
            params: vec![
                Some(sql.clone()),
                Some(hash(&sql)),
                Some(HASH_VERSION.to_string()),
                Some(name),
            ],
        }
    }

    /// Build the status rows from `index` on, emitting each drifted
    /// migration's warning on the way.
    fn build_status(&mut self, mut index: usize) -> Step {
        while index < self.migrations.len() {
            let name = self.migrations[index].name.clone();
            let sql = self.migrations[index].sql.clone();
            let info = self
                .status_applied
                .iter()
                .find(|(applied, _)| *applied == name)
                .map(|(_, info)| info);
            let drifted = info
                .and_then(|info| info.hash.as_deref())
                .is_some_and(|stored| stored != hash(&sql));
            self.status_rows.push(StatusRow {
                name: name.clone(),
                applied: info.is_some(),
                applied_at: info.and_then(|info| info.applied_at.clone()),
                drifted,
            });
            if drifted {
                self.stage = Stage::StatusWarn { next: index + 1 };
                return Step::Warn(format!(
                    "warning: migration '{name}' has been modified since it was applied"
                ));
            }
            index += 1;
        }
        self.stage = Stage::Terminal;
        Step::Finish(Outcome::Status(std::mem::take(&mut self.status_rows)))
    }

    /// Release the advisory lock, then finish or fail with what was pending.
    fn release(&mut self, pending: Option<Failure>, outcome: Option<Outcome>) -> Step {
        self.stage = Stage::Unlock { pending, outcome };
        Step::Query {
            sql: self.unlock_sql(),
            params: vec![Some(self.config.lock_id.to_string())],
        }
    }

    fn finish(&mut self, outcome: Outcome) -> Step {
        self.stage = Stage::Terminal;
        Step::Finish(outcome)
    }

    fn fail_now(&mut self, failure: Failure) -> Step {
        self.stage = Stage::Terminal;
        Step::Fail(failure)
    }

    fn fail_locked(&mut self, failure: Failure) -> Step {
        self.release(Some(failure), None)
    }

    fn try_lock_sql(&self) -> String {
        "SELECT (pg_try_advisory_lock(($1::pg_catalog.text)::pg_catalog.int8))::pg_catalog.text"
            .to_string()
    }

    fn unlock_sql(&self) -> String {
        "SELECT (pg_advisory_unlock(($1::pg_catalog.text)::pg_catalog.int8))::pg_catalog.text"
            .to_string()
    }

    fn status_columns_sql(&self) -> String {
        "SELECT (c.oid IS NOT NULL)::pg_catalog.text,
                (EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                         WHERE a.attrelid = c.oid AND a.attname = 'sql_source'
                           AND NOT a.attisdropped))::pg_catalog.text,
                (EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                         WHERE a.attrelid = c.oid AND a.attname = 'hash'
                           AND NOT a.attisdropped))::pg_catalog.text,
                (EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                         WHERE a.attrelid = c.oid AND a.attname = 'hash_version'
                           AND NOT a.attisdropped))::pg_catalog.text
         FROM (SELECT pg_catalog.to_regclass(($1::pg_catalog.text)) AS oid) AS c"
            .to_string()
    }

    fn status_rows_sql(&self, has_hash: bool, has_hash_version: bool) -> String {
        let hash_column = if has_hash {
            "hash::pg_catalog.text"
        } else {
            "NULL::pg_catalog.text"
        };
        let version_column = if has_hash_version {
            "hash_version::pg_catalog.text"
        } else {
            "NULL::pg_catalog.text"
        };
        format!(
            "SELECT name::pg_catalog.text, \
             to_char(applied_at AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
             {hash_column}, {version_column} \
             FROM {} ORDER BY name",
            self.config.table
        )
    }

    fn status_stale_sql(&self, has_hash_version: bool) -> String {
        let stale_filter = if has_hash_version {
            " AND hash_version IS DISTINCT FROM ($1::pg_catalog.text)::pg_catalog.int4"
        } else {
            ""
        };
        format!(
            "SELECT name::pg_catalog.text, sql_source::pg_catalog.text FROM {} \
             WHERE sql_source IS NOT NULL{stale_filter}",
            self.config.table
        )
    }
}

/// The first cell of the first row.
fn first_value(rows: &[Vec<Option<String>>]) -> Option<&str> {
    rows.first()
        .and_then(|row| row.first())
        .and_then(|value| value.as_deref())
}

/// A `bool` selected as text: PostgreSQL renders `true`/`false`.
fn is_true(value: Option<&str>) -> bool {
    matches!(value, Some("true") | Some("t"))
}

/// Format a database error the way both runners report it: severity and
/// message, DETAIL and HINT, and PG's position as a line of the migration
/// with a caret. `sql` is the migration's text and `base` the byte offset in
/// it of the text that was sent (0 when the whole migration was).
fn format_db_error(error: &DbError, sql: &str, base: usize) -> String {
    let mut out = match &error.severity {
        Some(severity) => format!("{severity}: {}", error.message),
        None => error.message.clone(),
    };
    if let Some(detail) = &error.detail {
        out.push_str("\nDETAIL: ");
        out.push_str(detail);
    }
    if let Some(hint) = &error.hint {
        out.push_str("\nHINT: ");
        out.push_str(hint);
    }
    if let Some(position) = error.position {
        out.push_str(&locate_position(sql, base, position));
    } else if let Some(position) = error.internal_position {
        out.push_str(&format!(
            "\nINTERNAL POSITION: {position}\nQUERY: {}",
            error.internal_query.as_deref().unwrap_or_default()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration_marker::has_no_transaction_marker;

    fn config() -> MachineConfig {
        MachineConfig {
            table: "public._migrations".to_string(),
            lock_id: 713705,
            use_transaction: true,
            fail_on_drift: true,
        }
    }

    fn migration(name: &str, sql: &str, down: Option<&str>) -> Migration {
        Migration {
            name: name.to_string(),
            sql: sql.to_string(),
            down_sql: down.map(str::to_string),
            no_transaction: has_no_transaction_marker(sql),
            down_no_transaction: down.is_some_and(has_no_transaction_marker),
        }
    }

    fn input(action: Action, migrations: Vec<Migration>, config: MachineConfig) -> Input {
        Input {
            action,
            config,
            migrations,
        }
    }

    fn one(value: &str) -> Vec<Option<String>> {
        vec![Some(value.to_string())]
    }

    fn rows(rows: Vec<Vec<Option<String>>>) -> Reply {
        Reply::Rows(rows)
    }

    fn done() -> Reply {
        Reply::Done
    }

    fn locked() -> Reply {
        rows(vec![one("true")])
    }

    fn error(message: &str, position: Option<u32>) -> Reply {
        Reply::DbError(DbError {
            code: Some("42601".to_string()),
            message: message.to_string(),
            severity: Some("ERROR".to_string()),
            detail: None,
            hint: None,
            position,
            internal_position: None,
            internal_query: None,
        })
    }

    fn is_lock(sql: &str) -> bool {
        sql.contains("pg_try_advisory_lock")
    }

    fn is_unlock(sql: &str) -> bool {
        sql.contains("pg_advisory_unlock")
    }

    fn is_ensure(sql: &str) -> bool {
        sql.contains("CREATE TABLE IF NOT EXISTS") && sql.contains("ADD COLUMN IF NOT EXISTS")
    }

    fn is_backfill_select(sql: &str) -> bool {
        sql.starts_with("SELECT name::pg_catalog.text, sql_source::pg_catalog.text")
    }

    fn is_backfill_update(sql: &str) -> bool {
        sql.starts_with("UPDATE ") && sql.contains("SET hash")
    }

    fn is_applied_select(sql: &str) -> bool {
        sql.starts_with("SELECT name::pg_catalog.text, hash::pg_catalog.text")
    }

    fn is_stored_sql(sql: &str) -> bool {
        sql.starts_with("SELECT sql_source::pg_catalog.text FROM")
    }

    fn is_insert(sql: &str) -> bool {
        sql.starts_with("INSERT INTO")
    }

    fn is_delete(sql: &str) -> bool {
        sql.starts_with("DELETE FROM")
    }

    fn is_accept_update(sql: &str) -> bool {
        sql.starts_with("UPDATE ") && sql.contains("SET sql_source")
    }

    fn is_status_columns(sql: &str) -> bool {
        sql.contains("c.oid IS NOT NULL")
    }

    fn is_status_rows(sql: &str) -> bool {
        sql.contains("to_char(applied_at")
    }

    fn is_status_stale(sql: &str) -> bool {
        sql.starts_with("SELECT name::pg_catalog.text, sql_source::pg_catalog.text FROM")
            && sql.contains("WHERE sql_source IS NOT NULL")
    }

    /// Answers each step until the machine finishes or fails, recording the
    /// steps it emitted.
    fn drive(
        input: Input,
        mut answer: impl FnMut(&Step) -> Reply,
    ) -> (Vec<Step>, Result<Outcome, Failure>) {
        let mut migrator = Migrator::new(input);
        let mut reply = Reply::Start;
        let mut steps = Vec::new();
        loop {
            let step = migrator.step(reply);
            let terminal = match &step {
                Step::Fail(failure) => Some(Err(failure.clone())),
                Step::Finish(outcome) => Some(Ok(outcome.clone())),
                _ => None,
            };
            if let Some(result) = terminal {
                steps.push(step);
                return (steps, result);
            }
            reply = answer(&step);
            steps.push(step);
        }
    }

    fn executes(steps: &[Step], sql: &str) -> usize {
        steps
            .iter()
            .filter(|step| matches!(step, Step::Execute { sql: executed } if executed == sql))
            .count()
    }

    fn queries(steps: &[Step]) -> impl Iterator<Item = &str> {
        steps.iter().filter_map(|step| match step {
            Step::Query { sql, .. } => Some(sql.as_str()),
            _ => None,
        })
    }

    /// A fresh up: the lock, the table and the two migrations' transactions,
    /// then the unlock.
    #[test]
    fn up_applies_pending_migrations_in_transactions() {
        let migrations = vec![
            migration("0001_a", "CREATE TABLE a (id INT);", None),
            migration("0002_b", "CREATE TABLE b (id INT);", None),
        ];
        let (steps, result) = drive(input(Action::Up, migrations, config()), |step| match step {
            Step::Execute { sql } if is_ensure(sql) => done(),
            Step::Execute { sql } if sql == "BEGIN" || sql == "COMMIT" => done(),
            Step::Execute { sql } if sql.starts_with("CREATE TABLE") => done(),
            Step::Query { sql, .. } if is_lock(sql) => locked(),
            Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![]),
            Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![]),
            Step::Query { sql, .. } if is_insert(sql) => rows(vec![]),
            Step::Query { sql, .. } if is_unlock(sql) => locked(),
            other => panic!("unexpected step: {other:?}"),
        });

        assert_eq!(
            result,
            Ok(Outcome::Applied(vec![
                "0001_a".to_string(),
                "0002_b".to_string()
            ]))
        );
        assert!(
            queries(&steps).any(is_ensure)
                || steps
                    .iter()
                    .any(|s| matches!(s, Step::Execute { sql } if is_ensure(sql)))
        );
        assert_eq!(executes(&steps, "BEGIN"), 2);
        assert_eq!(executes(&steps, "COMMIT"), 2);
        assert_eq!(executes(&steps, "CREATE TABLE a (id INT);"), 1);
        assert_eq!(executes(&steps, "CREATE TABLE b (id INT);"), 1);
        let lock = queries(&steps).position(is_lock).expect("no lock attempt");
        let unlock = queries(&steps).position(is_unlock).expect("no unlock");
        assert!(
            lock < unlock,
            "the lock was not taken before it was released"
        );
        assert!(
            queries(&steps).take(lock).all(|sql| !is_unlock(sql)),
            "the lock was released without being taken"
        );
    }

    /// A contended lock: each "not locked" answer sleeps, 20 ms then 40 ms,
    /// and the run proceeds once the lock is taken.
    #[test]
    fn lock_contention_backs_off_before_trying_again() {
        let mut attempts = 0;
        let (steps, result) = drive(input(Action::Up, vec![], config()), |step| match step {
            Step::Execute { sql } if is_ensure(sql) => done(),
            Step::Query { sql, .. } if is_lock(sql) => {
                attempts += 1;
                if attempts <= 2 {
                    rows(vec![one("false")])
                } else {
                    locked()
                }
            }
            Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![]),
            Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![]),
            Step::Query { sql, .. } if is_unlock(sql) => locked(),
            Step::Sleep { .. } => done(),
            other => panic!("unexpected step: {other:?}"),
        });

        assert_eq!(result, Ok(Outcome::Applied(vec![])));
        let sleeps: Vec<u64> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Sleep { ms } => Some(*ms),
                _ => None,
            })
            .collect();
        assert_eq!(sleeps, [20, 40]);
        assert_eq!(attempts, 3);
    }

    /// Drift with `fail_on_drift`: the run fails with the diff before
    /// applying anything (the pending migration sorts before the drifted
    /// one), and still releases the lock.
    #[test]
    fn drift_aborts_before_applying_and_releases_the_lock() {
        let old = "CREATE TABLE a (id INT);";
        let new = "CREATE TABLE a (id BIGINT);";
        let migrations = vec![
            migration("0001_a", new, None),
            migration("0002_b", "CREATE TABLE b (id INT);", None),
        ];
        let stored_hash = hash(old);
        let stored_hash_for_reply = stored_hash.clone();
        let (steps, result) = drive(
            input(Action::Up, migrations, config()),
            move |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![vec![
                    Some("0001_a".to_string()),
                    Some(stored_hash_for_reply.clone()),
                    Some(HASH_VERSION.to_string()),
                ]]),
                Step::Query { sql, .. } if is_stored_sql(sql) => rows(vec![one(old)]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        let Err(Failure::Message(message)) = result else {
            panic!("expected a drift failure, got {result:?}");
        };
        assert!(
            message.starts_with("migration '0001_a' has been modified since it was applied:\n"),
            "{message}"
        );
        assert!(message.contains("-CREATE TABLE a (id INT);"), "{message}");
        assert!(
            message.contains("+CREATE TABLE a (id BIGINT);"),
            "{message}"
        );
        assert!(
            !queries(&steps).any(is_insert),
            "a migration was recorded despite the drift"
        );
        assert!(
            !steps.iter().any(
                |step| matches!(step, Step::Execute { sql } if sql.contains("CREATE TABLE b"))
            ),
            "a pending migration ran despite the drift"
        );
        let unlock = steps
            .iter()
            .position(|step| matches!(step, Step::Query { sql, .. } if is_unlock(sql)))
            .expect("no unlock");
        assert_eq!(
            unlock,
            steps.len() - 2,
            "the unlock comes right before the failure"
        );
    }

    /// With `fail_on_drift` off the drift is a warning, and pending
    /// migrations still apply.
    #[test]
    fn drift_without_fail_on_drift_is_a_warning() {
        let old = "CREATE TABLE a (id INT);";
        let new = "CREATE TABLE a (id BIGINT);";
        let mut config = config();
        config.fail_on_drift = false;
        let stored_hash = hash(old);
        let stored_hash_for_reply = stored_hash.clone();
        let (steps, result) = drive(
            input(
                Action::Up,
                vec![
                    migration("0001_a", new, None),
                    migration("0002_b", "CREATE TABLE b (id INT);", None),
                ],
                config,
            ),
            move |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Execute { .. } => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![vec![
                    Some("0001_a".to_string()),
                    Some(stored_hash_for_reply.clone()),
                    Some(HASH_VERSION.to_string()),
                ]]),
                Step::Query { sql, .. } if is_stored_sql(sql) => rows(vec![one(old)]),
                Step::Query { sql, .. } if is_insert(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                Step::Warn(_) => done(),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        assert_eq!(result, Ok(Outcome::Applied(vec!["0002_b".to_string()])));
        let warn = steps
            .iter()
            .position(|step| matches!(step, Step::Warn(message) if message.starts_with("warning: migration '0001_a' has been modified")))
            .expect("no drift warning");
        let begin = steps
            .iter()
            .position(|step| matches!(step, Step::Execute { sql } if sql == "BEGIN"))
            .expect("no transaction");
        assert!(warn < begin, "the pending migration ran before the check");
    }

    /// A failing transaction: the machine rolls back, then releases the lock,
    /// then reports PG's line and caret.
    #[test]
    fn a_failing_transactional_migration_rolls_back_then_unlocks() {
        let sql = "CREATE TABLE ok_t (a int);\nCREATE TABL bad (a int);\n";
        let (steps, result) = drive(
            input(Action::Up, vec![migration("0001_bad", sql, None)], config()),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Execute { sql } if sql == "BEGIN" => done(),
                Step::Execute { sql: executed } if executed == sql => {
                    error("syntax error at or near \"TABL\"", Some(35))
                }
                Step::Execute { sql } if sql == "ROLLBACK" => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        let Err(Failure::Message(message)) = result else {
            panic!("expected a migration failure, got {result:?}");
        };
        assert_eq!(
            message,
            "failed to apply migration 0001_bad: ERROR: syntax error at or near \"TABL\"\n\
             LINE 2: CREATE TABL bad (a int);\n               ^"
        );
        let rollback = steps
            .iter()
            .position(|step| matches!(step, Step::Execute { sql } if sql == "ROLLBACK"))
            .expect("no rollback");
        let unlock = steps
            .iter()
            .position(|step| matches!(step, Step::Query { sql, .. } if is_unlock(sql)))
            .expect("no unlock");
        assert!(rollback < unlock);
        assert_eq!(executes(&steps, "COMMIT"), 0);
        assert!(
            !queries(&steps).any(is_insert),
            "the failed migration was recorded"
        );
    }

    /// A `-- no-transaction` migration sends each statement on its own; a
    /// failure keeps the statements before it applied, rolls nothing back,
    /// and still releases the lock.
    #[test]
    fn a_failing_no_transaction_statement_keeps_what_ran() {
        let sql = "-- no-transaction\nCREATE TABLE ok2 (a int);\nCREATE TABL bad;\n";
        let (steps, result) = drive(
            input(
                Action::Up,
                vec![migration("0001_partial", sql, None)],
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Execute { sql } if sql.contains("CREATE TABLE ok2") => done(),
                Step::Execute { sql } if sql.contains("CREATE TABL bad") => {
                    error("syntax error at or near \"TABL\"", Some(9))
                }
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        let Err(Failure::Message(message)) = result else {
            panic!("expected a migration failure, got {result:?}");
        };
        assert!(
            message.starts_with(
                "failed to apply migration 0001_partial (statement 2): ERROR: syntax error at or near \"TABL\"\nLINE 3: CREATE TABL bad;\n               ^"
            ),
            "{message}"
        );
        assert_eq!(executes(&steps, "ROLLBACK"), 0);
        let unlock = steps
            .iter()
            .position(|step| matches!(step, Step::Query { sql, .. } if is_unlock(sql)))
            .expect("no unlock");
        assert!(
            unlock + 2 == steps.len(),
            "the unlock comes right before the failure"
        );
        assert!(!queries(&steps).any(is_insert));
    }

    /// A tracking table from before the hash columns: each stale row's text
    /// is hashed and written back before the applied rows are read.
    #[test]
    fn backfill_hashes_stale_rows() {
        let stored = "CREATE TABLE a (id INT);";
        let (steps, result) = drive(
            input(
                Action::Up,
                vec![migration("0001_a", stored, None)],
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_backfill_select(sql) => rows(vec![vec![
                    Some("0001_a".to_string()),
                    Some(stored.to_string()),
                ]]),
                Step::Query { sql, .. } if is_backfill_update(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![vec![
                    Some("0001_a".to_string()),
                    Some(hash(stored)),
                    Some(HASH_VERSION.to_string()),
                ]]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        assert_eq!(result, Ok(Outcome::Applied(vec![])));
        let update = steps
            .iter()
            .find_map(|step| match step {
                Step::Query { sql, params } if is_backfill_update(sql) => Some(params.clone()),
                _ => None,
            })
            .expect("no backfill update");
        assert_eq!(
            update,
            vec![
                Some(hash(stored)),
                Some(HASH_VERSION.to_string()),
                Some("0001_a".to_string())
            ]
        );
    }

    /// `status` on a table without the hash columns never writes: it reads
    /// the table's columns and rows, hashes stale text in memory, and warns
    /// about the edited migration.
    #[test]
    fn status_on_an_old_table_only_reads() {
        let original = "CREATE TABLE a (id INT);";
        let (steps, result) = drive(
            input(
                Action::Status,
                vec![
                    migration("0001_a", original, None),
                    migration("0002_b", "CREATE TABLE b (id BIGINT);", None),
                ],
                config(),
            ),
            |step| match step {
                Step::Query { sql, .. } if is_status_columns(sql) => rows(vec![vec![
                    Some("true".to_string()),
                    Some("true".to_string()),
                    Some("false".to_string()),
                    Some("false".to_string()),
                ]]),
                Step::Query { sql, .. } if is_status_rows(sql) => rows(vec![
                    vec![
                        Some("0001_a".to_string()),
                        Some("2026-01-02T03:04:05.000000Z".to_string()),
                        None,
                        None,
                    ],
                    vec![
                        Some("0002_b".to_string()),
                        Some("2026-01-02T03:04:06.000000Z".to_string()),
                        None,
                        None,
                    ],
                ]),
                Step::Query { sql, .. } if is_status_stale(sql) => rows(vec![
                    vec![Some("0001_a".to_string()), Some(original.to_string())],
                    vec![
                        Some("0002_b".to_string()),
                        Some("CREATE TABLE b (id BIGINT);".to_string()),
                    ],
                ]),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        let Ok(Outcome::Status(statuses)) = result else {
            panic!("expected statuses, got {result:?}");
        };
        assert_eq!(statuses.len(), 2);
        assert!(statuses[0].applied && !statuses[0].drifted);
        assert_eq!(
            statuses[0].applied_at.as_deref(),
            Some("2026-01-02T03:04:05.000000Z")
        );
        assert!(
            statuses[1].applied && !statuses[1].drifted,
            "the stored text was ignored"
        );
        assert!(
            !queries(&steps).any(|sql| sql.starts_with("INSERT")
                || sql.starts_with("UPDATE")
                || sql.starts_with("DELETE")
                || sql.starts_with("CREATE")
                || sql.starts_with("ALTER")),
            "status wrote: {steps:?}"
        );
        assert_eq!(executes(&steps, "BEGIN"), 0);
    }

    /// `status` flags a real edit of an old table's row as drift, warning
    /// first.
    #[test]
    fn status_warns_about_drift() {
        let (steps, result) = drive(
            input(
                Action::Status,
                vec![migration("0001_a", "CREATE TABLE a (id BIGINT);", None)],
                config(),
            ),
            |step| match step {
                Step::Query { sql, .. } if is_status_columns(sql) => rows(vec![vec![
                    Some("true".to_string()),
                    Some("true".to_string()),
                    Some("false".to_string()),
                    Some("false".to_string()),
                ]]),
                Step::Query { sql, .. } if is_status_rows(sql) => rows(vec![vec![
                    Some("0001_a".to_string()),
                    Some("2026-01-02T03:04:05.000000Z".to_string()),
                    None,
                    None,
                ]]),
                Step::Query { sql, .. } if is_status_stale(sql) => rows(vec![vec![
                    Some("0001_a".to_string()),
                    Some("CREATE TABLE a (id INT);".to_string()),
                ]]),
                Step::Warn(_) => done(),
                other => panic!("unexpected step: {other:?}"),
            },
        );

        let Ok(Outcome::Status(statuses)) = result else {
            panic!("expected statuses, got {result:?}");
        };
        assert!(statuses[0].drifted);
        assert!(matches!(
            &steps[steps.len() - 2],
            Step::Warn(message) if message == "warning: migration '0001_a' has been modified since it was applied"
        ));
    }

    /// `accept` rejects an unknown file without reading the table, and an
    /// unapplied one without writing.
    #[test]
    fn accept_rejects_unknown_and_unapplied() {
        let source = vec![migration("0001_a", "CREATE TABLE a (id INT);", None)];

        let (steps, result) = drive(
            input(
                Action::Accept {
                    name: "0009_nope".to_string(),
                },
                source.clone(),
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(
            result,
            Err(Failure::Message(
                "no migration file named '0009_nope'".to_string()
            ))
        );
        assert!(!queries(&steps).any(is_applied_select));

        let (steps, result) = drive(
            input(
                Action::Accept {
                    name: "0001_a".to_string(),
                },
                source,
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(
            result,
            Err(Failure::Message(
                "migration '0001_a' has not been applied: nothing to accept".to_string()
            ))
        );
        assert!(!queries(&steps).any(is_accept_update));
    }

    /// Revert's rules: a down file is required unless forced, and the down
    /// file's own `-- no-transaction` marker decides how it runs.
    #[test]
    fn revert_rules() {
        let source = vec![migration(
            "0001_a",
            "CREATE TABLE a (id INT);",
            Some("-- no-transaction\nDROP TABLE a;\nDROP TABLE b;"),
        )];

        // No down file, not forced: the record stays.
        let (steps, result) = drive(
            input(
                Action::Revert {
                    name: Some("0001_a".to_string()),
                    force: false,
                },
                vec![migration("0001_a", "CREATE TABLE a (id INT);", None)],
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => {
                    rows(vec![vec![one("0001_a")[0].clone(), None, None]])
                }
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(
            result,
            Err(Failure::Message(
                "migration '0001_a' has no down file (0001_a.down.sql). \
                 Use force to remove the record without running SQL."
                    .to_string()
            ))
        );
        assert!(!queries(&steps).any(is_delete));

        // Forced: only the record is removed.
        let (steps, result) = drive(
            input(
                Action::Revert {
                    name: Some("0001_a".to_string()),
                    force: true,
                },
                vec![migration("0001_a", "CREATE TABLE a (id INT);", None)],
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => {
                    rows(vec![vec![one("0001_a")[0].clone(), None, None]])
                }
                Step::Query { sql, .. } if is_delete(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(result, Ok(Outcome::Reverted("0001_a".to_string())));
        assert_eq!(queries(&steps).filter(|sql| is_delete(sql)).count(), 1);

        // A `-- no-transaction` down file: each statement on its own.
        let (steps, result) = drive(
            input(
                Action::Revert {
                    name: Some("0001_a".to_string()),
                    force: false,
                },
                source,
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Execute { .. } => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => {
                    rows(vec![vec![one("0001_a")[0].clone(), None, None]])
                }
                Step::Query { sql, .. } if is_delete(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(result, Ok(Outcome::Reverted("0001_a".to_string())));
        assert_eq!(executes(&steps, "BEGIN"), 0);
        assert_eq!(
            steps
                .iter()
                .filter(
                    |step| matches!(step, Step::Execute { sql } if sql.contains("DROP TABLE a;"))
                )
                .count(),
            1
        );
        assert_eq!(
            steps
                .iter()
                .filter(
                    |step| matches!(step, Step::Execute { sql } if sql.contains("DROP TABLE b;"))
                )
                .count(),
            1
        );
    }

    /// The down file's own marker decides; the up's says nothing about it.
    #[test]
    fn a_transactional_down_runs_in_a_transaction() {
        let source = vec![migration(
            "0001_a",
            "-- no-transaction\nCREATE TABLE a (id INT);",
            Some("DROP TABLE a;"),
        )];
        let (steps, result) = drive(
            input(
                Action::Revert {
                    name: Some("0001_a".to_string()),
                    force: false,
                },
                source,
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Execute { .. } => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => {
                    rows(vec![vec![one("0001_a")[0].clone(), None, None]])
                }
                Step::Query { sql, .. } if is_delete(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(result, Ok(Outcome::Reverted("0001_a".to_string())));
        assert_eq!(executes(&steps, "BEGIN"), 1);
        assert_eq!(executes(&steps, "COMMIT"), 1);
        assert_eq!(executes(&steps, "DROP TABLE a;"), 1);
    }

    /// With no name, the last applied migration is reverted — or, without
    /// any, the action fails before touching a down file.
    #[test]
    fn revert_without_a_name_takes_the_last_applied() {
        let source = vec![
            migration("0001_a", "CREATE TABLE a (id INT);", Some("DROP TABLE a;")),
            migration("0002_b", "CREATE TABLE b (id INT);", Some("DROP TABLE b;")),
        ];
        let (steps, result) = drive(
            input(
                Action::Revert {
                    name: None,
                    force: false,
                },
                source,
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Execute { .. } => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![
                    vec![one("0001_a")[0].clone(), None, None],
                    vec![one("0002_b")[0].clone(), None, None],
                ]),
                Step::Query { sql, .. } if is_delete(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(result, Ok(Outcome::Reverted("0002_b".to_string())));
        assert_eq!(executes(&steps, "DROP TABLE b;"), 1);

        let (_, result) = drive(
            input(
                Action::Revert {
                    name: None,
                    force: false,
                },
                vec![],
                config(),
            ),
            |step| match step {
                Step::Execute { sql } if is_ensure(sql) => done(),
                Step::Query { sql, .. } if is_lock(sql) => locked(),
                Step::Query { sql, .. } if is_applied_select(sql) => rows(vec![]),
                Step::Query { sql, .. } if is_unlock(sql) => locked(),
                other => panic!("unexpected step: {other:?}"),
            },
        );
        assert_eq!(
            result,
            Err(Failure::Message(
                "no applied migrations to revert".to_string()
            ))
        );
    }

    /// The settings are validated before anything touches the database.
    #[test]
    fn an_invalid_table_name_fails_before_the_lock() {
        let mut config = config();
        config.table = "x; DROP TABLE y".to_string();
        let (steps, result) = drive(input(Action::Up, vec![], config), |step| {
            panic!("unexpected step: {step:?}")
        });
        let Err(Failure::Message(message)) = result else {
            panic!("expected a validation failure");
        };
        assert_eq!(
            message,
            "invalid migration table name 'x; DROP TABLE y': \
             'x; DROP TABLE y' is not a valid PostgreSQL identifier"
        );
        assert!(matches!(steps.as_slice(), [Step::Fail(_)]));
    }
}
