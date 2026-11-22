//! Real-PostgreSQL-backed sanity check for [`PgCatalog`](crate::PgCatalog).
//!
//! When the `pg_sanity` feature is on, every catalog produced by
//! [`PgCatalog::new`](crate::PgCatalog::new) carves out its own scratch
//! database inside the cluster pointed to by `POSTGRES_URL`, runs every
//! `apply_sql` / `analyze` against it, and drops the database on Drop.
//!
//! The cluster itself isn't managed by Rust — `scripts/run-pg-sanity.sh`
//! spins up a real PostgreSQL Docker container, exports `POSTGRES_URL`,
//! invokes `cargo nextest`, and tears the container down on exit. That
//! split keeps Rust dependencies thin (no async runtime, no embedded
//! binaries) and lets the same code run unchanged against any PG (CI,
//! local Docker, a remote dev cluster).
//!
//! `apply_sql` and `analyze` mirror their work onto the live server and
//! assert that:
//!
//! - the *outcome* matches (both succeed or both fail), and
//! - on success, every output column's name + qualified type name and every
//!   input parameter's qualified type name match exactly between our static
//!   analyzer and PG's wire-protocol Describe.
//!
//! Error message contract: when both fail, our analyzer's message must
//! *start with* PG's server-side message verbatim (the `DbError.message`
//! field). Extra detail / hints after that prefix are fine; missing the
//! prefix or diverging early is a hard panic. Additionally, when the
//! analyzer states an expected SQLSTATE for its error
//! ([`AnalyzeError::sqlstate`]), it must match the code PG
//! attached; errors with no stated code are compared on wording only.
//!
//! Every accepted query is also executed over adversarial data to check
//! the analyzer's nullability: see [`soundness`].

mod soundness;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use postgres::config::Config;
use postgres::types::{IsNull, ToSql, Type as PgType};
use postgres::{Client, NoTls};

use crate::error::AnalyzeError;
use crate::qualified_name::QualifiedName;
use crate::resolve::AnalyzedQuery;
use crate::types::Type;

/// A single way in which the static analyzer disagreed with the live
/// PostgreSQL server. Produced by the non-panicking `compare_*` methods and
/// consumed both by the panic-on-divergence `assert_*` wrappers (which the
/// regular `pg_sanity` test path uses) and by the differential fuzzer, which
/// collects many of these instead of aborting on the first.
///
/// [`Divergence::message`] is the exact human-readable text the `assert_*`
/// path panics with, so failure output is byte-identical whether a test or
/// the fuzzer surfaced it. [`Divergence::kind`] is a coarse discriminant used
/// to bucket / dedup findings by root cause.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub kind: DivergenceKind,
    pub message: String,
}

/// Coarse classification of a [`Divergence`], used by the fuzzer to group
/// findings without parsing the free-text message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DivergenceKind {
    /// DDL: analyzer accepted but PG rejected.
    ApplyPgRejected,
    /// DDL: analyzer rejected but PG accepted.
    ApplyAnalyzerRejected,
    /// Query: differing number of output columns.
    ColumnCount,
    /// Query: an output column's name differs.
    ColumnName,
    /// Query: an output column's type differs.
    ColumnType,
    /// Query: differing number of input parameters.
    ParamCount,
    /// Query: an input parameter's type differs.
    ParamType,
    /// Query: analyzer accepted but PG rejected (at prepare).
    AnalyzePgRejected,
    /// Query: analyzer rejected but PG accepted and executed.
    AnalyzeAcceptedExecuted,
    /// Both rejected, but the analyzer's message doesn't start with PG's.
    ErrorPrefix,
    /// Both rejected with the same message prefix, but the SQLSTATE the
    /// analyzer's error variant carries (`AnalyzeError::sqlstate`)
    /// differs from the one PG attached — the analyzer classified the
    /// error under the wrong variant.
    SqlState,
    /// Query: executed over adversarial data, a value the analyzer
    /// inferred NOT NULL (a column, an array element, a record field)
    /// came back NULL.
    Nullability,
    /// Query: the simplified SQL the analyzer sends instead of the one
    /// written (see `crate::simplify`) doesn't prepare to the same
    /// columns and parameters, or returns other rows (or fails where the
    /// original doesn't) over the adversarial data.
    Simplification,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// `ToSql` adapter that always serializes as SQL NULL regardless of the
/// requested PG type. Used by the `(analyzer rejected, PG accepted at
/// prepare)` execute fallback so we can fire the prepared statement
/// without inventing a typed value for every parameter slot. PG defers the
/// checks we actually want to observe (planner-level ON CONFLICT,
/// row-level NOT NULL via literal NULL, numeric/varchar overflow on
/// literals, identity-column rules) to execute time, and they all fire
/// regardless of parameter values — so passing NULLs is enough.
#[derive(Debug)]
struct NullParam;

impl ToSql for NullParam {
    fn to_sql(
        &self,
        _ty: &PgType,
        _out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        Ok(IsNull::Yes)
    }

    fn accepts(_ty: &PgType) -> bool {
        true
    }

    // Skip the standard `to_sql_checked!()` macro because it dispatches
    // through `accepts` — we want the value to claim compatibility with
    // any type, even ones the macro's WrongType guard would normally
    // reject.
    fn to_sql_checked(
        &self,
        _ty: &PgType,
        _out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        Ok(IsNull::Yes)
    }
}

pub(crate) struct PgSanityServer {
    /// The migrations' session, on the per-instance scratch database.
    client: Client,
    /// A second session on it, where queries are prepared: the
    /// application's queries run outside the migrations' session, so its
    /// temporary relations and session settings are not theirs.
    query_client: Client,
    /// Admin connection string (kept verbatim from `POSTGRES_URL`) so the
    /// `Drop` impl can connect back to a different database to run
    /// `DROP DATABASE` against the scratch one.
    admin_conn_str: String,
    /// Name of the scratch database; dropped from `admin_conn_str` on Drop.
    db_name: String,
    type_name_cache: HashMap<u32, String>,
    soundness: soundness::Soundness,
    /// Every role the catalog saw created by a migration, so `Drop` can drop
    /// them from the shared cluster after the scratch database goes.
    created_roles: Vec<String>,
}

/// Serializes the migrations writing a shared (cluster-wide) catalog
/// across every test process sharing the cluster: `pg_authid`,
/// `pg_auth_members`, `pg_parameter_acl`, `pg_db_role_setting`,
/// `pg_database`, `pg_tablespace`, `pg_shdescription`, `pg_shseclabel`.
/// Each test has its own database, but these rows are shared: two tests
/// updating the same one at once — `GRANT SET ON PARAMETER
/// log_min_duration_statement` rewrites that parameter's `pg_parameter_acl`
/// row — fail one of them with `tuple concurrently updated` (XX000), which
/// PG raises when a catalog row it updates in place changed under it (or
/// with a duplicate key when both insert it).
///
/// The lock is an exclusive lock on a file in the temporary directory,
/// named after `POSTGRES_URL`: the test processes run on one host (nextest
/// starts them all, as `scripts/run-pg-sanity.sh` does). A PostgreSQL
/// advisory lock would do, but those are per database, so it would take a
/// third session on the admin database per test — enough, with every
/// test waiting on it, to run the cluster out of connections. The lock is
/// released when the guard drops, or the process dies.
struct SharedCatalogLock(std::fs::File);

impl SharedCatalogLock {
    fn acquire(admin_conn_str: &str) -> SharedCatalogLock {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        admin_conn_str.hash(&mut hasher);
        let path = std::env::temp_dir().join(format!(
            "typedpg-pg-sanity-shared-catalogs-{:016x}.lock",
            hasher.finish()
        ));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("pg_sanity: opening {} failed: {e}", path.display()));
        file.lock()
            .unwrap_or_else(|e| panic!("pg_sanity: locking {} failed: {e}", path.display()));
        SharedCatalogLock(file)
    }
}

impl Drop for SharedCatalogLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Whether migration `sql` may write a shared catalog (see
/// [`SharedCatalogLock`]): a role statement, a grant on a parameter,
/// database or tablespace, a database or role setting, `ALTER SYSTEM`, a
/// comment or label on a shared object, `DROP` / `REASSIGN OWNED`, a
/// tablespace statement, or a `DO` block that names one of those. Erring
/// towards true only serializes more. A migration that doesn't parse is
/// taken as one.
fn writes_shared_catalog(sql: &str) -> bool {
    use typedpg_pg_query::ast::{Node, ObjectType};
    let Ok(parsed) = typedpg_pg_query::parse(sql) else {
        return true;
    };
    let shared = |objtype: ObjectType| {
        matches!(
            objtype,
            ObjectType::ObjectParameterAcl
                | ObjectType::ObjectDatabase
                | ObjectType::ObjectTablespace
                | ObjectType::ObjectRole
        )
    };
    parsed.tree.stmts.iter().any(|raw| match raw.stmt.as_ref() {
        Some(Node::GrantStmt(g)) => shared(g.objtype),
        Some(Node::CommentStmt(c)) => shared(c.objtype),
        Some(Node::SecLabelStmt(l)) => shared(l.objtype),
        Some(
            Node::CreateRoleStmt(_)
            | Node::AlterRoleStmt(_)
            | Node::AlterRoleSetStmt(_)
            | Node::DropRoleStmt(_)
            | Node::GrantRoleStmt(_)
            | Node::DropOwnedStmt(_)
            | Node::ReassignOwnedStmt(_)
            | Node::AlterSystemStmt(_)
            | Node::AlterDatabaseStmt(_)
            | Node::AlterDatabaseSetStmt(_)
            | Node::AlterDatabaseRefreshCollStmt(_)
            | Node::CreateTableSpaceStmt(_)
            | Node::DropTableSpaceStmt(_)
            | Node::AlterTableSpaceOptionsStmt(_),
        ) => true,
        // Its body runs whatever it builds: a word naming a shared object
        // is enough.
        Some(Node::DoStmt(_)) => {
            let upper = sql.to_ascii_uppercase();
            [
                "ROLE",
                "USER",
                "PARAMETER",
                "DATABASE",
                "TABLESPACE",
                "OWNED",
                "SYSTEM",
            ]
            .iter()
            .any(|word| upper.contains(word))
        }
        _ => false,
    })
}

impl PgSanityServer {
    /// Read `POSTGRES_URL`, create a per-instance scratch database with a
    /// random name, and return a client connected to it.
    pub(crate) fn spawn() -> Result<Self, String> {
        let admin_conn_str = std::env::var("POSTGRES_URL").map_err(|_| {
            "pg_sanity: POSTGRES_URL is not set. Run tests via `scripts/run-pg-sanity.sh`, \
             which spins up a Docker PostgreSQL and exports the URL."
                .to_string()
        })?;

        // Build the scratch DB name. Process pid + monotonic counter +
        // wall-clock nanos gives us enough entropy that concurrent test
        // binaries don't collide; underscore-prefix keeps it valid as an
        // unquoted SQL identifier.
        let db_name = scratch_db_name();

        // Connect to the admin database (whatever `POSTGRES_URL` points at)
        // and CREATE DATABASE. Drop the admin client immediately — we keep
        // the admin URL string around for the symmetric DROP at Drop time.
        {
            let mut admin = Client::connect(&admin_conn_str, NoTls).map_err(|e| {
                format!(
                    "pg_sanity: connect to admin DB failed (POSTGRES_URL={admin_conn_str:?}): {e}"
                )
            })?;
            admin
                .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
                .map_err(|e| format!("pg_sanity: CREATE DATABASE \"{db_name}\" failed: {e}"))?;
        }

        // Reconnect with the same parameters but pointing at the scratch DB.
        let scratch_conn_str = with_dbname(&admin_conn_str, &db_name)?;
        let connect = || {
            Client::connect(&scratch_conn_str, NoTls)
                .map_err(|e| format!("pg_sanity: connect to scratch DB \"{db_name}\" failed: {e}"))
        };
        let client = connect()?;
        let mut query_client = connect()?;
        // A query waiting on a lock the migrations' session holds (an open
        // transaction) is a harness problem, not a hang.
        query_client
            .batch_execute("SET lock_timeout = '10s'")
            .map_err(|e| format!("pg_sanity: SET lock_timeout failed: {e}"))?;

        Ok(Self {
            client,
            query_client,
            admin_conn_str,
            db_name,
            type_name_cache: HashMap::new(),
            soundness: soundness::Soundness::new(),
            created_roles: Vec::new(),
        })
    }

    /// Run `sql` on PG and panic if its outcome diverges from `our_result`.
    /// Thin wrapper over [`Self::compare_apply_matches`] for the `pg_sanity`
    /// test path, which wants a hard abort on the first divergence.
    pub(crate) fn assert_apply_matches<E: std::fmt::Display>(
        &mut self,
        sql: &str,
        our_result: &Result<(), E>,
    ) {
        if let Some(div) = self.compare_apply_matches(sql, our_result) {
            panic!("{}", div.message);
        }
    }

    /// Run `sql` on PG and return a [`Divergence`] if its outcome disagrees
    /// with `our_result`, or `None` if they're consistent. Non-panicking
    /// counterpart of [`Self::assert_apply_matches`].
    pub(crate) fn compare_apply_matches<E: std::fmt::Display>(
        &mut self,
        sql: &str,
        our_result: &Result<(), E>,
    ) -> Option<Divergence> {
        let pg_result = self.run_migration(sql);
        compare_apply_outcome(sql, our_result, &pg_result)
    }

    /// Send migration `sql` to the migrations' session as one simple query,
    /// and end the transaction it leaves open the way the analyzer does.
    ///
    /// A migration that succeeds commits whatever transaction is still open
    /// (`txblock::Transaction::finish`: an unterminated `BEGIN` is taken as
    /// committed). The server leaves such a block open past the query, out
    /// of the query session's sight and around the next migration, so the
    /// migration is followed by a `COMMIT`. That `COMMIT` belongs to the
    /// migration's outcome: when it fails (a deferred constraint), the
    /// migration failed.
    ///
    /// A migration that fails leaves the analyzer's catalog as the failed
    /// transaction found it (`txblock::Transaction::abort` rolls back to
    /// the transaction's start, session settings included — PG reverts a
    /// session-level `SET` made in a transaction that aborts, too). The
    /// server only gets there by itself when the failure hit an implicit
    /// transaction block: after an explicit `BEGIN` it stays in the aborted
    /// block (`TBLOCK_ABORT`), refusing every later statement (`current
    /// transaction is aborted`, 25P02). So a failure is followed by a
    /// `ROLLBACK`, which ends exactly the transaction the analyzer rolled
    /// back — what an earlier `COMMIT` in the migration made durable stays
    /// on both sides.
    ///
    /// Outside a block (the migration ended its last one) either statement
    /// only draws a warning. The caller compares the migration's outcome,
    /// which this returns.
    ///
    /// A migration that may write a shared catalog runs under
    /// [`SharedCatalogLock`], held until its transaction ends.
    fn run_migration(&mut self, sql: &str) -> Result<(), postgres::Error> {
        let lock =
            writes_shared_catalog(sql).then(|| SharedCatalogLock::acquire(&self.admin_conn_str));
        let pg_result = self
            .client
            .batch_execute(sql)
            .and_then(|()| self.client.batch_execute("COMMIT"));
        if pg_result.is_err() {
            self.client.batch_execute("ROLLBACK").unwrap_or_else(|e| {
                panic!(
                    "pg_sanity: ROLLBACK after a failed migration failed: {}",
                    render_pg_error(&e)
                )
            });
        }
        drop(lock);
        // A type renamed or moved to another schema keeps its OID: the
        // names cached for the old catalog may be stale.
        self.type_name_cache.clear();
        self.soundness.schema_changed();
        pg_result
    }

    /// [`Self::compare_apply_matches`] for a migration that creates, alters
    /// or drops an extension. `None` when the server doesn't ship an
    /// extension the analyzer bundles (pgvector, the PL transforms: see
    /// `outside_postgres_image`) and so rejects the migration with
    /// `extension "…" is not available`: nothing after it can be mirrored.
    pub(crate) fn compare_extension_apply_matches<E: std::fmt::Display>(
        &mut self,
        sql: &str,
        our_result: &Result<(), E>,
    ) -> Option<Option<Divergence>> {
        let pg_result = self.run_migration(sql);
        // extension.c's `extension "%s" is not available`, raised when the
        // control file is missing — for one of the bundled extensions the
        // image doesn't ship. Any other is a divergence.
        let unavailable = pg_result
            .as_ref()
            .err()
            .and_then(|e| e.as_db_error())
            .filter(|db| {
                *db.code() == postgres::error::SqlState::FEATURE_NOT_SUPPORTED
                    && db
                        .message()
                        .strip_prefix("extension \"")
                        .and_then(|m| m.strip_suffix("\" is not available"))
                        .is_some_and(crate::ddl::extensions::outside_postgres_image)
            });
        // The analyzer reports the same error for an extension it doesn't
        // bundle either. Otherwise it installed one the server lacks — and
        // went on to accept the migration, or to fail a later statement.
        if let Some(db) = unavailable
            && !our_result
                .as_ref()
                .is_err_and(|e| e.to_string().starts_with(db.message()))
        {
            return None;
        }
        Some(compare_apply_outcome(sql, our_result, &pg_result))
    }

    /// Compare `analysis_sql` (the rewritten SQL with `$N` placeholders that
    /// the analyzer fed to its static pass) against PG's Parse+Describe.
    /// Panics if outcomes diverge or any column's name or qualified type
    /// name disagrees.
    pub(crate) fn assert_analyze_matches(
        &mut self,
        analysis_sql: &str,
        our_result: &Result<AnalyzedQuery, AnalyzeError>,
        finite_params: &std::collections::HashSet<u32>,
    ) {
        if let Some(div) = self.compare_analyze_matches(analysis_sql, our_result, finite_params) {
            panic!("{}", div.message);
        }
    }

    /// The simplification oracle: the SQL the analyzer sends (`ours.sql`)
    /// instead of the one written (`original_sql`) must prepare to the same
    /// columns and parameters, and return the same rows over the
    /// adversarial data (see [`soundness::Soundness::equivalence`]).
    /// [`Self::check_simplification`] of `rewritten` for `written`, for the
    /// oracle's own tests: a deliberately wrong rewrite must be caught.
    pub(crate) fn check_rewrite(
        &mut self,
        written: &str,
        ours: &AnalyzedQuery,
        finite_params: &std::collections::HashSet<u32>,
    ) -> Option<Divergence> {
        let original = self
            .query_client
            .prepare(written)
            .unwrap_or_else(|e| panic!("pg_sanity: the written query must prepare: {e}"));
        self.check_simplification(written, ours, &original, finite_params)
    }

    fn check_simplification(
        &mut self,
        original_sql: &str,
        ours: &AnalyzedQuery,
        original: &postgres::Statement,
        finite_params: &std::collections::HashSet<u32>,
    ) -> Option<Divergence> {
        let simplified = match self.query_client.prepare(&ours.sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                return Some(Divergence {
                    kind: DivergenceKind::Simplification,
                    message: format!(
                        "pg_sanity: PG rejects the simplified query: {}\n\
                         written:\n---\n{original_sql}\n---\nsimplified:\n---\n{}\n---",
                        render_pg_error(&e),
                        ours.sql
                    ),
                });
            }
        };
        let shape = |s: &postgres::Statement| {
            (
                s.columns()
                    .iter()
                    .map(|c| (c.name().to_owned(), c.type_().oid()))
                    .collect::<Vec<_>>(),
                s.params().iter().map(PgType::oid).collect::<Vec<_>>(),
            )
        };
        if shape(original) != shape(&simplified) {
            return Some(Divergence {
                kind: DivergenceKind::Simplification,
                message: format!(
                    "pg_sanity: the simplified query prepares to other columns or parameters.\n\
                     written:\n---\n{original_sql}\n---\n{:?}\nsimplified:\n---\n{}\n---\n{:?}",
                    shape(original),
                    ours.sql,
                    shape(&simplified)
                ),
            });
        }
        self.soundness.equivalence(
            &mut self.query_client,
            (original_sql, original),
            (&ours.sql, &simplified),
            ours,
            finite_params,
        )
    }

    /// Non-panicking counterpart of [`Self::assert_analyze_matches`]: returns
    /// a [`Divergence`] describing the first disagreement found between our
    /// analyzer and PG's Parse+Describe (or execute-time fallback), or `None`
    /// when they're consistent.
    ///
    /// Harness-internal failures (a type OID PG handed us that we can't
    /// resolve, a failed `BEGIN` in the execute fallback) still panic — they
    /// indicate the mirror itself is broken, not an analyzer bug.
    pub(crate) fn compare_analyze_matches(
        &mut self,
        analysis_sql: &str,
        our_result: &Result<AnalyzedQuery, AnalyzeError>,
        finite_params: &std::collections::HashSet<u32>,
    ) -> Option<Divergence> {
        let pg_result = self.query_client.prepare(analysis_sql);

        match (our_result, &pg_result) {
            (Ok(ours), Ok(stmt)) => {
                let pg_columns: Vec<(String, u32)> = stmt
                    .columns()
                    .iter()
                    .map(|c| (c.name().to_string(), c.type_().oid()))
                    .collect();

                if ours.columns.len() != pg_columns.len() {
                    return Some(Divergence {
                        kind: DivergenceKind::ColumnCount,
                        message: format!(
                            "pg_sanity: column count mismatch.\n\
                             SQL:\n---\n{analysis_sql}\n---\n\
                             analyzer: {} columns ({:?})\n\
                             PG:       {} columns ({:?})",
                            ours.columns.len(),
                            ours.columns.iter().map(|c| &c.name).collect::<Vec<_>>(),
                            pg_columns.len(),
                            pg_columns.iter().map(|c| &c.0).collect::<Vec<_>>(),
                        ),
                    });
                }

                for (i, (our_col, (pg_name, pg_oid))) in
                    ours.columns.iter().zip(pg_columns.iter()).enumerate()
                {
                    // Trailing `!` / `?` on PG-side column names come from
                    // the analyzer's nullability-annotation syntax (e.g.
                    // `SELECT col AS "title!"`). The analyzer strips them
                    // when parsing the alias; PG preserves them verbatim.
                    // Strip on the comparison side too — but leave the
                    // literal `?column?` placeholder alone (PG's name for
                    // unnamed expressions, not an annotation).
                    let pg_name_stripped = if pg_name.as_str() == "?column?" {
                        pg_name.as_str()
                    } else {
                        pg_name
                            .strip_suffix('!')
                            .or_else(|| pg_name.strip_suffix('?'))
                            .unwrap_or(pg_name)
                    };
                    if our_col.name != pg_name_stripped {
                        return Some(Divergence {
                            kind: DivergenceKind::ColumnName,
                            message: format!(
                                "pg_sanity: column {i} name mismatch.\n\
                                 SQL:\n---\n{analysis_sql}\n---\n\
                                 analyzer: {:?}\nPG:       {:?}",
                                our_col.name, pg_name,
                            ),
                        });
                    }
                    let pg_qname = self.qualified_type_name(*pg_oid).unwrap_or_else(|e| {
                        panic!(
                            "pg_sanity: failed to resolve type oid {pg_oid} for column \
                             '{}': {e}",
                            our_col.name
                        )
                    });
                    let our_qname = qualified_type_name_for_compare(&our_col.pg_type);
                    if our_qname != pg_qname {
                        return Some(Divergence {
                            kind: DivergenceKind::ColumnType,
                            message: format!(
                                "pg_sanity: column '{}' type mismatch.\n\
                                 SQL:\n---\n{analysis_sql}\n---\n\
                                 analyzer: {our_qname} (Type::{:?})\nPG:       {pg_qname} (oid {pg_oid})",
                                our_col.name, our_col.pg_type,
                            ),
                        });
                    }
                }

                // --- Input parameter types ---
                // PG's Describe also reports the inferred type of each
                // positional placeholder ($1, $2, …). The analyzer's
                // positional ordering is regular params first, then each
                // spread's fields in order — exactly how `analysis_sql` was
                // built — so the two lists line up index-for-index. Compare
                // them the same way we compared output columns above.
                let our_params: Vec<(&str, &Type)> = ours
                    .params
                    .iter()
                    .map(|p| (p.name.as_str(), &p.pg_type))
                    .chain(
                        ours.spreads
                            .iter()
                            .flat_map(|s| s.fields.iter().map(|f| (f.name.as_str(), &f.pg_type))),
                    )
                    .collect();
                let pg_params = stmt.params();

                if our_params.len() != pg_params.len() {
                    return Some(Divergence {
                        kind: DivergenceKind::ParamCount,
                        message: format!(
                            "pg_sanity: parameter count mismatch.\n\
                             SQL:\n---\n{analysis_sql}\n---\n\
                             analyzer: {} params ({:?})\n\
                             PG:       {} params",
                            our_params.len(),
                            our_params.iter().map(|(n, _)| n).collect::<Vec<_>>(),
                            pg_params.len(),
                        ),
                    });
                }

                for (i, ((our_name, our_ty), pg_ty)) in
                    our_params.iter().zip(pg_params.iter()).enumerate()
                {
                    let pg_oid = pg_ty.oid();
                    // PG reports oid 0 ("unknown") when it genuinely couldn't
                    // pin down a parameter's type during Describe. There's
                    // nothing meaningful to compare against there, so skip.
                    if pg_oid == 0 {
                        continue;
                    }
                    let pg_qname = self.qualified_type_name(pg_oid).unwrap_or_else(|e| {
                        panic!(
                            "pg_sanity: failed to resolve type oid {pg_oid} for parameter \
                             ${} ('{our_name}'): {e}",
                            i + 1,
                        )
                    });
                    let our_qname = qualified_param_type_name_for_compare(our_ty);
                    if our_qname != pg_qname {
                        return Some(Divergence {
                            kind: DivergenceKind::ParamType,
                            message: format!(
                                "pg_sanity: parameter ${} ('{our_name}') type mismatch.\n\
                                 SQL:\n---\n{analysis_sql}\n---\n\
                                 analyzer: {our_qname} (Type::{our_ty:?})\nPG:       {pg_qname} (oid {pg_oid})",
                                i + 1,
                            ),
                        });
                    }
                }
                if let Some(d) = self.soundness.check(
                    &mut self.query_client,
                    analysis_sql,
                    ours,
                    stmt,
                    finite_params,
                ) {
                    return Some(d);
                }
                if ours.simplifications.is_empty() {
                    return None;
                }
                self.check_simplification(analysis_sql, ours, stmt, finite_params)
            }
            (Err(e), Err(_)) => {
                let our_msg = format!("{e}");
                check_error_prefix(
                    &our_msg,
                    e.sqlstate(),
                    pg_result.as_ref().unwrap_err(),
                    analysis_sql,
                    "analyze",
                )
            }
            (Ok(ours), Err(e)) => {
                e.as_db_error()?;
                Some(Divergence {
                    kind: DivergenceKind::AnalyzePgRejected,
                    message: format!(
                        "pg_sanity: analyzer accepted query but PG rejected it.\n\
                         SQL:\n---\n{analysis_sql}\n---\n\
                         analyzer columns: {:?}\nPG error: {}",
                        ours.columns
                            .iter()
                            .map(|c| (c.name.clone(), c.pg_type.clone()))
                            .collect::<Vec<_>>(),
                        render_pg_error(e),
                    ),
                })
            }
            (Err(_), Ok(stmt)) => {
                // PG accepted at prepare/parse time, but several checks
                // (planner ON CONFLICT validation, row-level NOT NULL via
                // literal NULL, numeric/varchar overflow on literals,
                // identity-column rules in MERGE/UPDATE) only fire at
                // execute time. Re-run the prepared statement inside a
                // BEGIN/ROLLBACK transaction with all-NULL parameters so
                // we observe the real runtime error before declaring a
                // divergence; if it does fire and matches our analyzer's
                // wording, the test is consistent.
                let n_params = stmt.params().len();
                let null_params: Vec<NullParam> = (0..n_params).map(|_| NullParam).collect();
                let null_param_refs: Vec<&(dyn ToSql + Sync)> =
                    null_params.iter().map(|p| p as _).collect();

                let exec_result = match self.query_client.transaction() {
                    Ok(mut tx) => {
                        let r = tx.execute(stmt, &null_param_refs);
                        // `tx` drops here → automatic rollback so the
                        // scratch DB stays clean across tests.
                        r
                    }
                    Err(e) => panic!(
                        "pg_sanity: BEGIN failed in execute-fallback for {analysis_sql}: {e}"
                    ),
                };

                match exec_result {
                    Ok(_rows) => Some(Divergence {
                        kind: DivergenceKind::AnalyzeAcceptedExecuted,
                        message: format!(
                            "pg_sanity: analyzer rejected query but PG accepted AND \
                             executed it (with all-NULL params).\n\
                             SQL:\n---\n{analysis_sql}\n---\n\
                             analyzer error: {}\nPG columns: {:?}",
                            our_result.as_ref().unwrap_err(),
                            stmt.columns()
                                .iter()
                                .map(|c| (c.name().to_string(), c.type_().oid()))
                                .collect::<Vec<_>>(),
                        ),
                    }),
                    Err(exec_err) => {
                        let our_err = our_result.as_ref().unwrap_err();
                        let our_msg = format!("{our_err}");
                        check_error_prefix(
                            &our_msg,
                            our_err.sqlstate(),
                            &exec_err,
                            analysis_sql,
                            "analyze (execute fallback)",
                        )
                    }
                }
            }
        }
    }

    /// The privileges of every role the migrations created must agree with
    /// PostgreSQL's: for each role, every user relation (and column),
    /// sequence, function, schema, type and language — plain and `WITH GRANT
    /// OPTION` — plus a small sample of built-ins whose seeded ACL isn't the
    /// default. Also compares the roles' `pg_auth_members` rows and the
    /// owner of every object the migrations created. Panics on the first
    /// disagreement.
    pub(crate) fn assert_privileges_match(&mut self, db: &crate::pg_catalog::PgCatalog) {
        let created = db.created_roles();
        let session_user: String = self
            .client
            .query_one("SELECT session_user::text", &[])
            .unwrap_or_else(|e| panic!("pg_sanity: session_user query failed: {e}"))
            .get(0);
        if let Some(message) = self.compare_owners(db, &session_user) {
            panic!("{message}");
        }
        if let Some(message) = self.compare_role_attributes(db, &created) {
            panic!("{message}");
        }
        if let Some(message) = self.compare_default_acls(db, &created, &session_user) {
            panic!("{message}");
        }
        if !created.is_empty()
            && let Some(message) = self.compare_memberships(db, &session_user, &created)
        {
            panic!("{message}");
        }
        // One transaction gives the matrix's reads one snapshot.
        self.client
            .batch_execute("BEGIN")
            .unwrap_or_else(|e| panic!("pg_sanity: privilege comparison BEGIN failed: {e}"));
        let mut divergence = None;
        // Every created role, plus the predefined roles that carry implicit
        // privileges: they exercise the model's special cases.
        let mut subjects = created;
        for role in [
            "pg_read_all_data",
            "pg_write_all_data",
            "pg_maintain",
            "pg_database_owner",
        ] {
            if !subjects.iter().any(|r| r == role) {
                subjects.push(role.to_owned());
            }
        }
        for role in subjects {
            if let Some(message) = self.compare_privileges(db, &role) {
                divergence = Some(message);
                break;
            }
        }
        let _ = self.client.batch_execute("COMMIT");
        if let Some(message) = divergence {
            panic!("{message}");
        }
    }

    /// Remember the roles the catalog has seen created. Called after every
    /// mirrored migration (even a failed one, whose committed roles stay), so
    /// `Drop` can clean the shared cluster up.
    pub(crate) fn remember_roles(&mut self, roles: &[String]) {
        for role in roles {
            if !self.created_roles.contains(role) {
                self.created_roles.push(role.clone());
            }
        }
    }

    /// Compare one role's privilege matrix with the server's,
    /// `has_*_privilege` by `has_*_privilege` — plain and `WITH GRANT
    /// OPTION`, and `has_any_column_privilege` for relations. Objects are
    /// matched by name (the analyzer's OIDs differ from PG's). Two-way: a
    /// user-schema object PG has and the model doesn't is a divergence too.
    fn compare_privileges(
        &mut self,
        db: &crate::pg_catalog::PgCatalog,
        role: &str,
    ) -> Option<String> {
        use crate::pg_catalog::RelKind;
        let param: &(dyn ToSql + Sync) = &role;

        const RELATION_PRIVS: [&str; 8] = [
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
            "MAINTAIN",
        ];
        const COLUMN_PRIVS: [&str; 4] = ["SELECT", "INSERT", "UPDATE", "REFERENCES"];
        const SEQUENCE_PRIVS: [&str; 3] = ["USAGE", "SELECT", "UPDATE"];
        let plain_and_option = |function: &str, argument: &str, privilege: &str| {
            format!(
                "{function}($1::name, {argument}, '{privilege}'), \
                 {function}($1::name, {argument}, '{privilege} WITH GRANT OPTION')"
            )
        };

        // Relations: the table privileges and the any-column ones, plain and
        // with grant option.
        let relation_columns: Vec<String> = RELATION_PRIVS
            .iter()
            .map(|p| plain_and_option("has_table_privilege", "c.oid", p))
            .chain(
                COLUMN_PRIVS
                    .iter()
                    .map(|p| plain_and_option("has_any_column_privilege", "c.oid", p)),
            )
            .collect();
        let sql = format!(
            "SELECT n.nspname, c.relname, {} \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'v', 'm', 'f', 'p') \
               AND n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
            relation_columns.join(", ")
        );
        let pg_rows = self.query(&sql, param);
        let mut pg_relations: HashMap<(String, String), Vec<bool>> = HashMap::new();
        for row in &pg_rows {
            let key = (row.get::<_, String>(0), row.get::<_, String>(1));
            let privs = (0..relation_columns.len() * 2)
                .map(|i| row.get::<_, bool>(i + 2))
                .collect();
            pg_relations.insert(key, privs);
        }
        // Two-way: every user relation PG has must be in the model.
        let mut model_relations: HashMap<(String, String), crate::oid::PgClassOid> = HashMap::new();
        for class in db.pg_class.values() {
            let Some(schema) = db.namespace_name(class.relnamespace) else {
                continue;
            };
            if !is_user_schema(schema)
                || !matches!(
                    class.relkind,
                    RelKind::Table
                        | RelKind::View
                        | RelKind::MaterializedView
                        | RelKind::ForeignTable
                        | RelKind::Partitioned
                )
            {
                continue;
            }
            model_relations.insert((schema.to_owned(), class.relname.clone()), class.oid);
        }
        for key in pg_relations.keys() {
            if !model_relations.contains_key(key) {
                return Some(format!(
                    "pg_sanity: relation {} exists on PG but not in the model while comparing \
                     role \"{role}\"",
                    crate::QualifiedName::new(key.0.clone(), key.1.clone())
                ));
            }
        }
        for ((schema, relname), &relid) in &model_relations {
            let Some(pg_privs) = pg_relations.get(&(schema.clone(), relname.clone())) else {
                return Some(format!(
                    "pg_sanity: relation {} does not exist on PG while comparing role \"{role}\"",
                    crate::QualifiedName::new(schema.clone(), relname.clone())
                ));
            };
            let at = |i: usize| i * 2;
            for (i, privilege) in [
                crate::Privilege::Select,
                crate::Privilege::Insert,
                crate::Privilege::Update,
                crate::Privilege::Delete,
                crate::Privilege::Truncate,
                crate::Privilege::References,
                crate::Privilege::Trigger,
                crate::Privilege::Maintain,
            ]
            .into_iter()
            .enumerate()
            {
                let ours = db.has_table_privilege(role, relid, privilege);
                let theirs = pg_privs[at(i)];
                if ours != theirs {
                    return Some(privilege_mismatch(
                        role,
                        &crate::QualifiedName::new(schema.clone(), relname.clone()).to_string(),
                        RELATION_PRIVS[i],
                        ours,
                        theirs,
                    ));
                }
                let ours = db.has_table_privilege_with_grant_option(role, relid, privilege);
                let theirs = pg_privs[at(i) + 1];
                if ours != theirs {
                    return Some(privilege_mismatch(
                        role,
                        &crate::QualifiedName::new(schema.clone(), relname.clone()).to_string(),
                        &format!("{} WITH GRANT OPTION", RELATION_PRIVS[i]),
                        ours,
                        theirs,
                    ));
                }
            }
            for (i, privilege) in [
                crate::Privilege::Select,
                crate::Privilege::Insert,
                crate::Privilege::Update,
                crate::Privilege::References,
            ]
            .into_iter()
            .enumerate()
            {
                let ours = db.has_any_column_privilege(role, relid, privilege);
                let theirs = pg_privs[at(RELATION_PRIVS.len() + i)];
                if ours != theirs {
                    return Some(privilege_mismatch(
                        role,
                        &crate::QualifiedName::new(schema.clone(), relname.clone()).to_string(),
                        &format!("any column {}", COLUMN_PRIVS[i]),
                        ours,
                        theirs,
                    ));
                }
                let ours = db.has_any_column_privilege_with_grant_option(role, relid, privilege);
                let theirs = pg_privs[at(RELATION_PRIVS.len() + i) + 1];
                if ours != theirs {
                    return Some(privilege_mismatch(
                        role,
                        &crate::QualifiedName::new(schema.clone(), relname.clone()).to_string(),
                        &format!("any column {} WITH GRANT OPTION", COLUMN_PRIVS[i]),
                        ours,
                        theirs,
                    ));
                }
            }
        }

        // Columns: SELECT, INSERT, UPDATE, REFERENCES, plain and with grant
        // option.
        let column_calls: Vec<String> = COLUMN_PRIVS
            .iter()
            .map(|p| plain_and_option("has_column_privilege", "c.oid, a.attnum", p))
            .collect();
        let pg_rows = self.query(
            &format!(
                "SELECT n.nspname, c.relname, a.attname, {} \
                 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE a.attnum > 0 AND NOT a.attisdropped \
                   AND c.relkind IN ('r', 'v', 'm', 'f', 'p') \
                   AND n.nspname <> 'information_schema' \
                   AND n.nspname NOT LIKE 'pg\\_%'",
                column_calls.join(", ")
            ),
            param,
        );
        let mut pg_columns: HashMap<(String, String, String), Vec<bool>> = HashMap::new();
        for row in &pg_rows {
            let key = (
                row.get::<_, String>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2),
            );
            let privs = (0..column_calls.len() * 2)
                .map(|i| row.get::<_, bool>(i + 3))
                .collect();
            pg_columns.insert(key, privs);
        }
        for class in db.pg_class.values() {
            let Some(schema) = db.namespace_name(class.relnamespace) else {
                continue;
            };
            if !is_user_schema(schema)
                || !matches!(
                    class.relkind,
                    RelKind::Table
                        | RelKind::View
                        | RelKind::MaterializedView
                        | RelKind::ForeignTable
                        | RelKind::Partitioned
                )
            {
                continue;
            }
            for attr in db.attributes_of(class.oid) {
                if attr.attnum <= 0 {
                    continue;
                }
                let Some(pg_privs) = pg_columns.get(&(
                    schema.to_owned(),
                    class.relname.clone(),
                    attr.attname.clone(),
                )) else {
                    return Some(format!(
                        "pg_sanity: column {}.{} does not exist on PG while comparing role \
                         \"{role}\"",
                        crate::QualifiedName::new(schema, class.relname.clone()),
                        attr.attname
                    ));
                };
                for (i, privilege) in [
                    crate::Privilege::Select,
                    crate::Privilege::Insert,
                    crate::Privilege::Update,
                    crate::Privilege::References,
                ]
                .into_iter()
                .enumerate()
                {
                    let ours = db.has_column_privilege(role, class.oid, attr.attnum, privilege);
                    let theirs = pg_privs[i * 2];
                    if ours != theirs {
                        return Some(privilege_mismatch(
                            role,
                            &format!(
                                "{}.{}",
                                crate::QualifiedName::new(schema, class.relname.clone()),
                                attr.attname
                            ),
                            COLUMN_PRIVS[i],
                            ours,
                            theirs,
                        ));
                    }
                    let ours = db.has_column_privilege_with_grant_option(
                        role,
                        class.oid,
                        attr.attnum,
                        privilege,
                    );
                    let theirs = pg_privs[i * 2 + 1];
                    if ours != theirs {
                        return Some(privilege_mismatch(
                            role,
                            &format!(
                                "{}.{}",
                                crate::QualifiedName::new(schema, class.relname.clone()),
                                attr.attname
                            ),
                            &format!("{} WITH GRANT OPTION", COLUMN_PRIVS[i]),
                            ours,
                            theirs,
                        ));
                    }
                }
            }
        }

        // Sequences: USAGE, SELECT, UPDATE, plain and with grant option.
        let sequence_calls: Vec<String> = SEQUENCE_PRIVS
            .iter()
            .map(|p| plain_and_option("has_sequence_privilege", "c.oid", p))
            .collect();
        let pg_rows = self.query(
            &format!(
                "SELECT n.nspname, c.relname, {} \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind = 'S' \
                   AND n.nspname <> 'information_schema' \
                   AND n.nspname NOT LIKE 'pg\\_%'",
                sequence_calls.join(", ")
            ),
            param,
        );
        let mut pg_sequences: HashMap<(String, String), Vec<bool>> = HashMap::new();
        for row in &pg_rows {
            let key = (row.get::<_, String>(0), row.get::<_, String>(1));
            let privs = (0..sequence_calls.len() * 2)
                .map(|i| row.get::<_, bool>(i + 2))
                .collect();
            pg_sequences.insert(key, privs);
        }
        let mut model_sequences: HashMap<(String, String), crate::oid::PgClassOid> = HashMap::new();
        for class in db.pg_class.values() {
            if class.relkind != RelKind::Sequence {
                continue;
            }
            let Some(schema) = db.namespace_name(class.relnamespace) else {
                continue;
            };
            if !is_user_schema(schema) {
                continue;
            }
            model_sequences.insert((schema.to_owned(), class.relname.clone()), class.oid);
        }
        for key in pg_sequences.keys() {
            if !model_sequences.contains_key(key) {
                return Some(format!(
                    "pg_sanity: sequence {} exists on PG but not in the model while comparing \
                     role \"{role}\"",
                    crate::QualifiedName::new(key.0.clone(), key.1.clone())
                ));
            }
        }
        for ((schema, relname), &relid) in &model_sequences {
            let Some(pg_privs) = pg_sequences.get(&(schema.clone(), relname.clone())) else {
                return Some(format!(
                    "pg_sanity: sequence {} does not exist on PG while comparing role \"{role}\"",
                    crate::QualifiedName::new(schema.clone(), relname.clone())
                ));
            };
            for (i, privilege) in [
                crate::Privilege::Usage,
                crate::Privilege::Select,
                crate::Privilege::Update,
            ]
            .into_iter()
            .enumerate()
            {
                let ours = db.has_sequence_privilege(role, relid, privilege);
                let theirs = pg_privs[i * 2];
                if ours != theirs {
                    return Some(privilege_mismatch(
                        role,
                        &crate::QualifiedName::new(schema.clone(), relname.clone()).to_string(),
                        SEQUENCE_PRIVS[i],
                        ours,
                        theirs,
                    ));
                }
                let ours = db.has_sequence_privilege_with_grant_option(role, relid, privilege);
                let theirs = pg_privs[i * 2 + 1];
                if ours != theirs {
                    return Some(privilege_mismatch(
                        role,
                        &crate::QualifiedName::new(schema.clone(), relname.clone()).to_string(),
                        &format!("{} WITH GRANT OPTION", SEQUENCE_PRIVS[i]),
                        ours,
                        theirs,
                    ));
                }
            }
        }

        // Functions, procedures and aggregates: EXECUTE, plain and with grant
        // option.
        let pg_rows = self.query(
            "SELECT n.nspname, p.proname, \
                    pg_catalog.array_to_string(ARRAY( \
                        SELECT pg_catalog.format_type(a.t, NULL) \
                        FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY a(t, i) \
                        ORDER BY a.i), ', '), \
                    has_function_privilege($1::name, p.oid, 'EXECUTE'), \
                    has_function_privilege($1::name, p.oid, 'EXECUTE WITH GRANT OPTION') \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
            param,
        );
        let mut pg_functions: HashMap<(String, String, String), (bool, bool)> = HashMap::new();
        for row in &pg_rows {
            pg_functions.insert(
                (
                    row.get::<_, String>(0),
                    row.get::<_, String>(1),
                    row.get::<_, String>(2),
                ),
                (row.get::<_, bool>(3), row.get::<_, bool>(4)),
            );
        }
        let mut model_functions: HashMap<(String, String, String), crate::oid::PgProcOid> =
            HashMap::new();
        for proc in db.pg_proc.values() {
            let Some(schema) = db.namespace_name(proc.pronamespace) else {
                continue;
            };
            if !is_user_schema(schema) {
                continue;
            }
            let signature = proc
                .proargtypes
                .iter()
                .map(|&t| crate::ddl::util::format_type_for_message(db, t))
                .collect::<Vec<_>>()
                .join(", ");
            model_functions.insert(
                (schema.to_owned(), proc.proname.clone(), signature),
                proc.oid,
            );
        }
        for key in pg_functions.keys() {
            if !model_functions.contains_key(key) {
                return Some(format!(
                    "pg_sanity: function {}({}) exists on PG but not in the model while \
                     comparing role \"{role}\"",
                    crate::QualifiedName::new(key.0.clone(), key.1.clone()),
                    key.2
                ));
            }
        }
        for ((schema, proname, signature), &proc_oid) in &model_functions {
            let Some(&(pg_priv, pg_option)) =
                pg_functions.get(&(schema.clone(), proname.clone(), signature.clone()))
            else {
                return Some(format!(
                    "pg_sanity: function {}({signature}) does not exist on PG while comparing \
                     role \"{role}\"",
                    crate::QualifiedName::new(schema.clone(), proname.clone())
                ));
            };
            let object = format!(
                "{}({signature})",
                crate::QualifiedName::new(schema.clone(), proname.clone())
            );
            let ours = db.has_function_privilege(role, proc_oid);
            if ours != pg_priv {
                return Some(privilege_mismatch(role, &object, "EXECUTE", ours, pg_priv));
            }
            let ours = db.has_function_privilege_with_grant_option(role, proc_oid);
            if ours != pg_option {
                return Some(privilege_mismatch(
                    role,
                    &object,
                    "EXECUTE WITH GRANT OPTION",
                    ours,
                    pg_option,
                ));
            }
        }

        // Schemas: USAGE, CREATE, plain and with grant option.
        let pg_rows = self.query(
            "SELECT n.nspname, \
                    has_schema_privilege($1::name, n.oid, 'USAGE'), \
                    has_schema_privilege($1::name, n.oid, 'USAGE WITH GRANT OPTION'), \
                    has_schema_privilege($1::name, n.oid, 'CREATE'), \
                    has_schema_privilege($1::name, n.oid, 'CREATE WITH GRANT OPTION') \
             FROM pg_catalog.pg_namespace n \
             WHERE n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
            param,
        );
        let mut pg_schemas: HashMap<String, Vec<bool>> = HashMap::new();
        for row in &pg_rows {
            pg_schemas.insert(
                row.get::<_, String>(0),
                (0..4).map(|i| row.get::<_, bool>(i + 1)).collect(),
            );
        }
        let mut model_schemas: HashMap<String, crate::oid::PgNamespaceOid> = HashMap::new();
        for namespace in db.pg_namespace.values() {
            if !is_user_schema(&namespace.nspname) {
                continue;
            }
            model_schemas.insert(namespace.nspname.clone(), namespace.oid);
        }
        for name in pg_schemas.keys() {
            if !model_schemas.contains_key(name) {
                return Some(format!(
                    "pg_sanity: schema {} exists on PG but not in the model while comparing role \
                     \"{role}\"",
                    typedpg_core::quote_identifier(name)
                ));
            }
        }
        for (name, &ns_oid) in &model_schemas {
            let Some(pg_privs) = pg_schemas.get(name) else {
                return Some(format!(
                    "pg_sanity: schema {} does not exist on PG while comparing role \"{role}\"",
                    typedpg_core::quote_identifier(name)
                ));
            };
            for (i, (privilege, label)) in [
                (crate::Privilege::Usage, "USAGE"),
                (crate::Privilege::Create, "CREATE"),
            ]
            .into_iter()
            .enumerate()
            {
                let ours = db.has_schema_privilege(role, ns_oid, privilege);
                if ours != pg_privs[i * 2] {
                    return Some(privilege_mismatch(
                        role,
                        &typedpg_core::quote_identifier(name),
                        label,
                        ours,
                        pg_privs[i * 2],
                    ));
                }
                let ours = db.has_schema_privilege_with_grant_option(role, ns_oid, privilege);
                if ours != pg_privs[i * 2 + 1] {
                    return Some(privilege_mismatch(
                        role,
                        &typedpg_core::quote_identifier(name),
                        &format!("{label} WITH GRANT OPTION"),
                        ours,
                        pg_privs[i * 2 + 1],
                    ));
                }
            }
        }

        // Types: USAGE, plain and with grant option.
        let pg_rows = self.query(
            "SELECT n.nspname, t.typname, \
                    has_type_privilege($1::name, t.oid, 'USAGE'), \
                    has_type_privilege($1::name, t.oid, 'USAGE WITH GRANT OPTION') \
             FROM pg_catalog.pg_type t \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
            param,
        );
        let mut pg_types: HashMap<(String, String), (bool, bool)> = HashMap::new();
        for row in &pg_rows {
            pg_types.insert(
                (row.get::<_, String>(0), row.get::<_, String>(1)),
                (row.get::<_, bool>(2), row.get::<_, bool>(3)),
            );
        }
        let mut model_types: HashMap<(String, String), crate::oid::PgTypeOid> = HashMap::new();
        for typ in db.pg_type.values() {
            let Some(schema) = db.namespace_name(typ.typnamespace) else {
                continue;
            };
            if !is_user_schema(schema) {
                continue;
            }
            model_types.insert((schema.to_owned(), typ.typname.clone()), typ.oid);
        }
        for key in pg_types.keys() {
            if !model_types.contains_key(key) {
                return Some(format!(
                    "pg_sanity: type {} exists on PG but not in the model while comparing role \
                     \"{role}\"",
                    crate::QualifiedName::new(key.0.clone(), key.1.clone())
                ));
            }
        }
        for ((schema, typname), &typ_oid) in &model_types {
            let Some(&(pg_priv, pg_option)) = pg_types.get(&(schema.clone(), typname.clone()))
            else {
                return Some(format!(
                    "pg_sanity: type {} does not exist on PG while comparing role \"{role}\"",
                    crate::QualifiedName::new(schema.clone(), typname.clone())
                ));
            };
            let object = crate::QualifiedName::new(schema.clone(), typname.clone()).to_string();
            let ours = db.has_type_privilege(role, typ_oid);
            if ours != pg_priv {
                return Some(privilege_mismatch(role, &object, "USAGE", ours, pg_priv));
            }
            let ours = db.has_type_privilege_with_grant_option(role, typ_oid);
            if ours != pg_option {
                return Some(privilege_mismatch(
                    role,
                    &object,
                    "USAGE WITH GRANT OPTION",
                    ours,
                    pg_option,
                ));
            }
        }

        // Languages: USAGE, plain and with grant option.
        let pg_rows = self.query(
            "SELECT l.lanname, \
                    has_language_privilege($1::name, l.oid, 'USAGE'), \
                    has_language_privilege($1::name, l.oid, 'USAGE WITH GRANT OPTION') \
             FROM pg_catalog.pg_language l",
            param,
        );
        let pg_languages: HashMap<String, (bool, bool)> = pg_rows
            .iter()
            .map(|row| {
                (
                    row.get::<_, String>(0),
                    (row.get::<_, bool>(1), row.get::<_, bool>(2)),
                )
            })
            .collect();
        for language in db.pg_language.values() {
            let Some(&(pg_priv, pg_option)) = pg_languages.get(&language.lanname) else {
                return Some(format!(
                    "pg_sanity: language {} does not exist on PG while comparing role \"{role}\"",
                    language.lanname
                ));
            };
            let ours = db.has_language_privilege(role, language.oid);
            if ours != pg_priv {
                return Some(privilege_mismatch(
                    role,
                    &format!("language {}", language.lanname),
                    "USAGE",
                    ours,
                    pg_priv,
                ));
            }
            let ours = db.has_language_privilege_with_grant_option(role, language.oid);
            if ours != pg_option {
                return Some(privilege_mismatch(
                    role,
                    &format!("language {}", language.lanname),
                    "USAGE WITH GRANT OPTION",
                    ours,
                    pg_option,
                ));
            }
        }
        for name in pg_languages.keys() {
            if !db.pg_language.values().any(|l| &l.lanname == name) {
                return Some(format!(
                    "pg_sanity: language {name} exists on PG but not in the model while comparing \
                     role \"{role}\""
                ));
            }
        }

        // A fixed sample of built-ins whose seeded ACL isn't the default.
        for (name, sel) in [
            ("pg_authid", "SELECT"),
            ("pg_statistic", "SELECT"),
            ("pg_subscription", "SELECT"),
            ("pg_shadow", "SELECT"),
        ] {
            let (ours, pg) = (
                db.resolve_table(Some("pg_catalog"), name)
                    .map(|c| db.has_table_privilege(role, c.oid, crate::Privilege::Select)),
                self.query_one_bool(
                    "SELECT has_table_privilege($2::name, $1::text::regclass, 'SELECT')",
                    &format!("pg_catalog.{name}"),
                    role,
                ),
            );
            if let Some(ours) = ours
                && ours != pg
            {
                return Some(privilege_mismatch(
                    role,
                    &format!("pg_catalog.{name}"),
                    sel,
                    ours,
                    pg,
                ));
            }
        }
        for (name, args, sql_name) in [
            ("pg_read_file", 1, "pg_catalog.pg_read_file(text)"),
            ("pg_ls_dir", 1, "pg_catalog.pg_ls_dir(text)"),
            ("lo_import", 1, "pg_catalog.lo_import(text)"),
            ("now", 0, "pg_catalog.now()"),
        ] {
            let ours = db
                .find_functions(Some("pg_catalog"), name)
                .into_iter()
                .find(|p| p.proargtypes.len() == args)
                .map(|p| db.has_function_privilege(role, p.oid));
            let pg = self.query_one_bool(
                "SELECT has_function_privilege($2::name, $1::text::regprocedure, 'EXECUTE')",
                sql_name,
                role,
            );
            if let Some(ours) = ours
                && ours != pg
            {
                return Some(privilege_mismatch(role, sql_name, "EXECUTE", ours, pg));
            }
        }

        None
    }

    /// Compare `pg_auth_members` with the model's memberships, for every
    /// membership a created role is the granted role, the member or the
    /// grantor of.
    fn compare_memberships(
        &mut self,
        db: &crate::pg_catalog::PgCatalog,
        session_user: &str,
        created: &[String],
    ) -> Option<String> {
        let rows = self
            .client
            .query(
                "SELECT r.rolname::text, m.rolname::text, g.rolname::text, \
                        am.admin_option, am.inherit_option, am.set_option \
                 FROM pg_catalog.pg_auth_members am \
                 JOIN pg_catalog.pg_roles r ON r.oid = am.roleid \
                 JOIN pg_catalog.pg_roles m ON m.oid = am.member \
                 JOIN pg_catalog.pg_roles g ON g.oid = am.grantor",
                &[],
            )
            .unwrap_or_else(|e| panic!("pg_sanity: pg_auth_members query failed: {e}"));
        let interesting = |role: &str, member: &str, grantor: &str| {
            created
                .iter()
                .any(|c| c == role || c == member || c == grantor)
        };
        let mut pg_rows: Vec<(String, String, String, bool, bool, bool)> = rows
            .iter()
            .map(|row| {
                (
                    row.get::<_, String>(0),
                    row.get::<_, String>(1),
                    row.get::<_, String>(2),
                    row.get::<_, bool>(3),
                    row.get::<_, bool>(4),
                    row.get::<_, bool>(5),
                )
            })
            .filter(|(role, member, grantor, ..)| interesting(role, member, grantor))
            .collect();
        let mut model_rows: Vec<(String, String, String, bool, bool, bool)> = db
            .roles
            .memberships
            .iter()
            .map(|m| {
                (
                    db.roles.name(m.role).to_owned(),
                    db.roles.name(m.member).to_owned(),
                    pg_role_name(db, m.grantor, session_user),
                    m.admin,
                    m.inherit,
                    m.set,
                )
            })
            .filter(|(role, member, grantor, ..)| interesting(role, member, grantor))
            .collect();
        pg_rows.sort();
        model_rows.sort();
        if pg_rows != model_rows {
            return Some(format!(
                "pg_sanity: pg_auth_members mismatch (role, member, grantor, admin, inherit, set)\n\
                 PG:    {pg_rows:?}\nmodel: {model_rows:?}"
            ));
        }
        None
    }

    /// Compare the `pg_authid` attributes the privilege model reads for every
    /// role the migrations created (`rolsuper`, `rolinherit`,
    /// `rolcreaterole`, `rolcreatedb`, `rolcanlogin`, `rolbypassrls`).
    fn compare_role_attributes(
        &mut self,
        db: &crate::pg_catalog::PgCatalog,
        created: &[String],
    ) -> Option<String> {
        if created.is_empty() {
            return None;
        }
        let mut pg_rows: Vec<(String, bool, bool, bool, bool, bool, bool)> = self
            .client
            .query(
                "SELECT rolname::text, rolsuper, rolinherit, rolcreaterole, rolcreatedb, \
                        rolcanlogin, rolbypassrls \
                 FROM pg_catalog.pg_roles WHERE rolname = ANY($1)",
                &[&created.to_vec()],
            )
            .unwrap_or_else(|e| panic!("pg_sanity: pg_roles query failed: {e}"))
            .iter()
            .map(|row| {
                (
                    row.get::<_, String>(0),
                    row.get::<_, bool>(1),
                    row.get::<_, bool>(2),
                    row.get::<_, bool>(3),
                    row.get::<_, bool>(4),
                    row.get::<_, bool>(5),
                    row.get::<_, bool>(6),
                )
            })
            .collect();
        pg_rows.sort_by(|a, b| a.0.cmp(&b.0));
        let mut model_rows: Vec<(String, bool, bool, bool, bool, bool, bool)> = Vec::new();
        for name in created {
            let Some(id) = db.roles.id(name) else {
                return Some(format!(
                    "pg_sanity: role \"{name}\" was created but is not in the model"
                ));
            };
            let Some(role) = db.roles.get(id) else {
                return Some(format!(
                    "pg_sanity: role \"{name}\" was created but is not in the model"
                ));
            };
            model_rows.push((
                name.clone(),
                role.superuser,
                role.inherit,
                role.createrole,
                role.createdb,
                role.login,
                role.bypassrls,
            ));
        }
        model_rows.sort_by(|a, b| a.0.cmp(&b.0));
        (pg_rows != model_rows).then(|| {
            format!(
                "pg_sanity: role attribute mismatch (rolname, rolsuper, rolinherit, \
                 rolcreaterole, rolcreatedb, rolcanlogin, rolbypassrls)\nPG:    {pg_rows:?}\nmodel: \
                 {model_rows:?}"
            )
        })
    }

    /// Compare `pg_default_acl` for the roles the migrations created and for
    /// the migrations' user (the bootstrap superuser): every
    /// `(defaclrole, defaclnamespace, defaclobjtype)` entry and each of its
    /// `defaclacl` items, as `aclexplode` reads them.
    fn compare_default_acls(
        &mut self,
        db: &crate::pg_catalog::PgCatalog,
        created: &[String],
        session_user: &str,
    ) -> Option<String> {
        let mut names = created.to_vec();
        if !names.iter().any(|name| name == session_user) {
            names.push(session_user.to_owned());
        }
        let pg_rows: Vec<(String, String, String, String, String, String, bool)> = self
            .client
            .query(
                "SELECT r.rolname::text, COALESCE(n.nspname::text, ''), \
                        d.defaclobjtype::text, \
                        CASE WHEN x.grantee = 0 THEN 'PUBLIC' \
                             ELSE pg_catalog.pg_get_userbyid(x.grantee) END, \
                        pg_catalog.pg_get_userbyid(x.grantor), x.privilege_type, x.is_grantable \
                 FROM pg_catalog.pg_default_acl d \
                 JOIN pg_catalog.pg_roles r ON r.oid = d.defaclrole \
                 LEFT JOIN pg_catalog.pg_namespace n ON n.oid = d.defaclnamespace \
                 CROSS JOIN LATERAL pg_catalog.aclexplode(d.defaclacl) x \
                 WHERE r.rolname = ANY($1)",
                &[&names],
            )
            .unwrap_or_else(|e| panic!("pg_sanity: pg_default_acl query failed: {e}"))
            .iter()
            .map(|row| {
                (
                    row.get::<_, String>(0),
                    row.get::<_, String>(1),
                    row.get::<_, String>(2),
                    row.get::<_, String>(3),
                    row.get::<_, String>(4),
                    row.get::<_, String>(5),
                    row.get::<_, bool>(6),
                )
            })
            .collect();
        let interesting = |role: crate::ddl::roles::RoleId| {
            let name = pg_role_name(db, role, session_user);
            names.contains(&name)
        };
        let mut model_rows: Vec<(String, String, String, String, String, String, bool)> =
            Vec::new();
        for (owner, namespace, class, acl) in db.acls.default_entries() {
            if !interesting(owner) {
                continue;
            }
            let owner_name = pg_role_name(db, owner, session_user);
            let namespace_name = namespace
                .and_then(|ns| db.namespace_name(ns))
                .unwrap_or("")
                .to_owned();
            for item in &acl {
                let grantee = match item.grantee {
                    crate::ddl::acl::Grantee::Public => "PUBLIC".to_owned(),
                    crate::ddl::acl::Grantee::Role(role) => pg_role_name(db, role, session_user),
                };
                let grantor = pg_role_name(db, item.grantor, session_user);
                for (privilege, grantable) in item.exploded() {
                    model_rows.push((
                        owner_name.clone(),
                        namespace_name.clone(),
                        class.objtype().to_string(),
                        grantee.clone(),
                        grantor.clone(),
                        privilege.to_owned(),
                        grantable,
                    ));
                }
            }
        }
        let mut pg_rows = pg_rows;
        pg_rows.sort();
        model_rows.sort();
        (pg_rows != model_rows).then(|| {
            format!(
                "pg_sanity: pg_default_acl mismatch (role, namespace, objtype, grantee, grantor, \
                 privilege, grantable)\nPG:    {pg_rows:?}\nmodel: {model_rows:?}"
            )
        })
    }

    /// Compare the owner (`relowner` / `proowner` / `nspowner` / `typowner` /
    /// `lanowner`) of every object the migrations created with PG's, and —
    /// both ways — the set of objects each side has. Seeded objects are
    /// included: their owner may have changed (`ALTER SCHEMA public OWNER
    /// TO`, `ALTER LANGUAGE plpgsql OWNER TO`, ...).
    fn compare_owners(
        &mut self,
        db: &crate::pg_catalog::PgCatalog,
        session_user: &str,
    ) -> Option<String> {
        use crate::ddl::acl::AclObject;
        use crate::pg_catalog::RelKind;

        let mut owner_query = |sql: &str| -> HashMap<(String, String), String> {
            self.client
                .query(sql, &[])
                .unwrap_or_else(|e| panic!("pg_sanity: owner query failed: {e}\nSQL:\n{sql}"))
                .iter()
                .map(|row| {
                    (
                        (row.get::<_, String>(0), row.get::<_, String>(1)),
                        row.get::<_, String>(2),
                    )
                })
                .collect()
        };
        let compare_kind = |what: &str,
                            ours: Vec<((String, String), crate::ddl::roles::RoleId)>,
                            pg: &HashMap<(String, String), String>|
         -> Option<String> {
            let mut seen: Vec<(String, String)> = Vec::new();
            for (key, owner) in ours {
                let Some(pg_owner) = pg.get(&key) else {
                    return Some(format!(
                        "pg_sanity: {what} {} exists in the model but not on PG",
                        crate::QualifiedName::new(key.0.clone(), key.1.clone())
                    ));
                };
                let ours = pg_role_name(db, owner, session_user);
                if ours != *pg_owner {
                    return Some(format!(
                        "pg_sanity: owner mismatch of {what} {}: model says \"{ours}\", PG says \
                         \"{pg_owner}\"",
                        crate::QualifiedName::new(key.0.clone(), key.1.clone())
                    ));
                }
                seen.push(key);
            }
            for key in pg.keys() {
                if !seen.contains(key) {
                    return Some(format!(
                        "pg_sanity: {what} {} exists on PG but not in the model",
                        crate::QualifiedName::new(key.0.clone(), key.1.clone())
                    ));
                }
            }
            None
        };

        let relations = owner_query(
            "SELECT n.nspname, c.relname, pg_catalog.pg_get_userbyid(c.relowner) \
             FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'v', 'm', 'f', 'p', 'S') \
               AND n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
        );
        let mut model_relations = Vec::new();
        for class in db.pg_class.values() {
            let Some(schema) = db.namespace_name(class.relnamespace) else {
                continue;
            };
            if !is_user_schema(schema) {
                continue;
            }
            if !matches!(
                class.relkind,
                RelKind::Table
                    | RelKind::View
                    | RelKind::MaterializedView
                    | RelKind::ForeignTable
                    | RelKind::Partitioned
                    | RelKind::Sequence
            ) {
                continue;
            }
            model_relations.push((
                (schema.to_owned(), class.relname.clone()),
                db.owner_of(AclObject::Relation(class.oid)),
            ));
        }
        if let Some(message) = compare_kind("relation", model_relations, &relations) {
            return Some(message);
        }

        let functions = owner_query(
            "SELECT n.nspname, p.proname || '(' || \
                    pg_catalog.array_to_string(ARRAY( \
                        SELECT pg_catalog.format_type(a.t, NULL) \
                        FROM pg_catalog.unnest(p.proargtypes) WITH ORDINALITY a(t, i) \
                        ORDER BY a.i), ', ') || ')', \
                    pg_catalog.pg_get_userbyid(p.proowner) \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
        );
        let mut model_functions = Vec::new();
        for proc in db.pg_proc.values() {
            let Some(schema) = db.namespace_name(proc.pronamespace) else {
                continue;
            };
            if !is_user_schema(schema) {
                continue;
            }
            let signature = proc
                .proargtypes
                .iter()
                .map(|&t| crate::ddl::util::format_type_for_message(db, t))
                .collect::<Vec<_>>()
                .join(", ");
            model_functions.push((
                (schema.to_owned(), format!("{}({signature})", proc.proname)),
                db.owner_of(AclObject::Function(proc.oid)),
            ));
        }
        if let Some(message) = compare_kind("function", model_functions, &functions) {
            return Some(message);
        }

        let schemas = owner_query(
            "SELECT n.nspname, '', pg_catalog.pg_get_userbyid(n.nspowner) \
             FROM pg_catalog.pg_namespace n \
             WHERE n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
        );
        let mut model_schemas = Vec::new();
        for namespace in db.pg_namespace.values() {
            if !is_user_schema(&namespace.nspname) {
                continue;
            }
            model_schemas.push((
                (namespace.nspname.clone(), String::new()),
                db.owner_of(AclObject::Schema(namespace.oid)),
            ));
        }
        if let Some(message) = compare_kind("schema", model_schemas, &schemas) {
            return Some(message);
        }

        let types = owner_query(
            "SELECT n.nspname, t.typname, pg_catalog.pg_get_userbyid(t.typowner) \
             FROM pg_catalog.pg_type t \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname <> 'information_schema' \
               AND n.nspname NOT LIKE 'pg\\_%'",
        );
        let mut model_types = Vec::new();
        for typ in db.pg_type.values() {
            let Some(schema) = db.namespace_name(typ.typnamespace) else {
                continue;
            };
            if !is_user_schema(schema) {
                continue;
            }
            model_types.push((
                (schema.to_owned(), typ.typname.clone()),
                db.owner_of(AclObject::Type(typ.oid)),
            ));
        }
        if let Some(message) = compare_kind("type", model_types, &types) {
            return Some(message);
        }

        let languages = owner_query(
            "SELECT l.lanname, '', pg_catalog.pg_get_userbyid(l.lanowner) \
             FROM pg_catalog.pg_language l",
        );
        let mut model_languages = Vec::new();
        for language in db.pg_language.values() {
            model_languages.push((
                (language.lanname.clone(), String::new()),
                db.owner_of(AclObject::Language(language.oid)),
            ));
        }
        if let Some(message) = compare_kind("language", model_languages, &languages) {
            return Some(message);
        }

        None
    }

    fn query(&mut self, sql: &str, param: &(dyn ToSql + Sync)) -> Vec<postgres::Row> {
        self.client
            .query(sql, &[param])
            .unwrap_or_else(|e| panic!("pg_sanity: privilege query failed: {e}\nSQL:\n{sql}"))
    }

    fn query_one_bool(&mut self, sql: &str, object: &str, role: &str) -> bool {
        self.client
            .query_one(sql, &[&object, &role])
            .unwrap_or_else(|e| panic!("pg_sanity: privilege query failed: {e}\nSQL:\n{sql}"))
            .get(0)
    }

    /// Resolve a PG type OID to its qualified name (`schema.type_name`) by
    /// querying `pg_catalog.pg_type`. Array types are flipped from their PG
    /// `_int4` underscore form into the bracketed `pg_catalog.int4[]` form
    /// so they line up with [`Type::Array`]. Inlines the OID as a literal
    /// (non-injectable: it's a u32 from PG itself) to dodge `to_sql` for
    /// the `oid` type.
    fn qualified_type_name(&mut self, oid: u32) -> Result<String, postgres::Error> {
        if let Some(name) = self.type_name_cache.get(&oid) {
            return Ok(name.clone());
        }
        let row = self.query_client.query_one(
            &format!(
                "SELECT n.nspname, t.typname, \
                 t.typsubscript = 'pg_catalog.array_subscript_handler'::pg_catalog.regproc, \
                 t.typelem::int8 \
                 FROM pg_catalog.pg_type t \
                 JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
                 WHERE t.oid = {oid}"
            ),
            &[],
        )?;
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let true_array: bool = row.get(2);
        let typelem: u32 = row.get::<_, i64>(3) as u32;

        // PG's IsTrueArrayType: `_record` (a pseudo-type) included.
        let qualified = if true_array && typelem != 0 {
            // Array — recurse to get the element's qname, then bracket.
            let elem_qname = self.qualified_type_name(typelem)?;
            format!("{elem_qname}[]")
        } else {
            QualifiedName::new(schema, name).to_string()
        };
        self.type_name_cache.insert(oid, qualified.clone());
        Ok(qualified)
    }
}

/// What PG makes of a statement run by a role: planning it
/// (`EXPLAIN (GENERIC_PLAN)`: parse analysis and the executor's start —
/// the schema, range-table and EXECUTE checks) and, when that succeeds,
/// executing it with NULL parameters (the checks made as it runs: a
/// sequence function, a column default's `nextval`) and again over rows
/// seeded into every table (the checks only rows reach: a row trigger, a
/// CHECK constraint, a generated expression).
struct RoleOutcome {
    explain: Result<(), postgres::Error>,
    executions: Vec<Result<(), postgres::Error>>,
    /// The prepared statement's Describe, made as the role under its login
    /// search path (when `explain` succeeded): the oracle compares the
    /// analysis's columns and parameters with it.
    described: Option<postgres::Statement>,
}

impl RoleOutcome {
    /// The first `insufficient_privilege` (42501) error PG raised.
    fn denied(&self) -> Option<&postgres::Error> {
        let denied = |r: &Result<(), postgres::Error>| matches!(r, Err(e) if e.code() == Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE));
        if denied(&self.explain) {
            return self.explain.as_ref().err();
        }
        self.executions.iter().find(|r| denied(r))?.as_ref().err()
    }
}

/// Whether `sql` is a statement `EXPLAIN (GENERIC_PLAN)` takes.
fn is_query_for_explain(sql: &str) -> bool {
    use typedpg_pg_query::ast::Node as N;
    typedpg_pg_query::parse(sql).ok().is_some_and(|tree| {
        matches!(
            tree.tree.stmts.first().and_then(|s| s.stmt.as_ref()),
            Some(
                N::SelectStmt(_)
                    | N::InsertStmt(_)
                    | N::UpdateStmt(_)
                    | N::DeleteStmt(_)
                    | N::MergeStmt(_)
            )
        )
    })
}

/// The inner statement of an `EXPLAIN [ANALYZE] <statement>`, and whether
/// it runs (`ANALYZE`).
fn explain_target(sql: &str) -> Option<(String, bool)> {
    use typedpg_pg_query::ast::Node;
    let parsed = typedpg_pg_query::parse(sql).ok()?;
    let statement = parsed.tree.stmts.first()?.stmt.as_ref()?;
    let Node::ExplainStmt(stmt) = statement else {
        return None;
    };
    let analyze = stmt.options.iter().any(|option| {
        let Some(Node::DefElem(option)) = option.as_ref() else {
            return false;
        };
        if option.defname != "analyze" {
            return false;
        }
        match option.arg.as_ref() {
            None => true,
            Some(Node::Boolean(b)) => b.boolval,
            Some(Node::Integer(i)) => i.ival != 0,
            Some(Node::String(s)) => {
                matches!(
                    s.sval.to_ascii_lowercase().as_str(),
                    "on" | "true" | "1" | "t"
                )
            }
            _ => true,
        }
    });
    stmt.query.as_ref()?;
    // The inner statement's own text: what follows `EXPLAIN` and its options
    // (a parenthesized list, or the legacy `ANALYZE` / `VERBOSE` keywords).
    use typedpg_pg_query::ast::Token;
    let scanned = typedpg_pg_query::scan(sql).ok()?;
    let tokens: Vec<_> = scanned
        .tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::SqlComment | Token::CComment))
        .collect();
    let word = |i: usize| {
        tokens
            .get(i)
            .and_then(|t| sql.get(t.start as usize..t.end as usize))
            .map(str::to_ascii_lowercase)
    };
    if word(0)? != "explain" {
        return None;
    }
    let mut i = 1;
    if word(i)? == "(" {
        let mut depth = 0usize;
        loop {
            match word(i)?.as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    } else {
        while matches!(word(i)?.as_str(), "analyze" | "analyse" | "verbose") {
            i += 1;
        }
    }
    let start = tokens.get(i)?.start as usize;
    let inner = sql
        .get(start..)?
        .trim_end()
        .trim_end_matches(';')
        .trim_end();
    Some((inner.to_owned(), analyze))
}

impl PgSanityServer {
    /// Mirror [`crate::PgCatalog::analyze_as`]: run `sql` as `role` on the
    /// query session and compare. The analyzer accepting means PG raises no
    /// `insufficient_privilege`; the analyzer denying means PG does, with a
    /// message the analyzer's starts with. Any other error of the analyzer
    /// must be PG's when it plans the statement as the role (a name the
    /// role's search path resolves differently). Errors PG raises only
    /// executing with NULL parameters (a NOT NULL column) say nothing of
    /// privileges and are ignored.
    pub(crate) fn assert_role_matches(
        &mut self,
        sql: &str,
        role: &str,
        ours: &Result<crate::resolve::AnalyzedQuery, crate::AnalyzeError>,
    ) {
        if let Some(div) = self.compare_role_matches(sql, role, ours) {
            panic!("{}", div.message);
        }
    }

    pub(crate) fn compare_role_matches(
        &mut self,
        sql: &str,
        role: &str,
        ours: &Result<crate::resolve::AnalyzedQuery, crate::AnalyzeError>,
    ) -> Option<Divergence> {
        // A role the migrations didn't create has no server-side twin.
        if matches!(ours, Err(crate::AnalyzeError::UndefinedObject(m)) if m.starts_with("role \""))
        {
            return None;
        }
        let pg = self.run_as_role(sql, role);
        let header = format!("SQL:\n---\n{sql}\n---\nrole: {role}");
        match ours {
            Ok(_) => {
                if let Some(e) = pg.denied() {
                    return Some(Divergence {
                        kind: DivergenceKind::AnalyzePgRejected,
                        message: format!(
                            "pg_sanity: the analyzer lets the role run the statement, PG denies it.\n\
                             {header}\nPG: {}",
                            render_pg_error(e)
                        ),
                    });
                }
                if let Err(e) = &pg.explain {
                    return Some(Divergence {
                        kind: DivergenceKind::AnalyzePgRejected,
                        message: format!(
                            "pg_sanity: the analyzer accepts the statement as the role, PG \
                             rejects it.\n{header}\nPG: {}",
                            render_pg_error(e)
                        ),
                    });
                }
                if let Ok(ours) = ours
                    && let Some(stmt) = &pg.described
                    && let Some(divergence) = self.compare_role_describe(&header, ours, stmt)
                {
                    return Some(divergence);
                }
                None
            }
            Err(e @ crate::AnalyzeError::InsufficientPrivilege(msg)) => match pg.denied() {
                Some(pg_err) => check_error_prefix(msg, e.sqlstate(), pg_err, sql, "privilege"),
                // A run that fails on something else — the NOT NULL column
                // NULL parameters hit, a CHECK the seeded row violates — is
                // not evidence of the denial either: if PG never raises the
                // privilege error, the denial is wrong.
                None => Some(Divergence {
                    kind: DivergenceKind::AnalyzeAcceptedExecuted,
                    message: format!(
                        "pg_sanity: the analyzer denies the role the statement, PG runs it.\n\
                         {header}\nanalyzer: {msg}\nPG: {}",
                        match &pg.explain {
                            Err(e) => render_pg_error(e),
                            Ok(()) => match pg.executions.iter().find_map(|r| r.as_ref().err()) {
                                Some(e) => format!(
                                    "planned, executed (another error, not a denial): {}",
                                    render_pg_error(e)
                                ),
                                None => "planned and executed".to_owned(),
                            },
                        }
                    ),
                }),
            },
            Err(e) => match &pg.explain {
                Err(pg_err) => {
                    check_error_prefix(&e.to_string(), e.sqlstate(), pg_err, sql, "role")
                }
                // An error only a role may hit as the statement runs (the
                // missing handler ANALYZE reaches for a role that may
                // analyze the table): PG's execution raises it.
                Ok(())
                    if pg.executions.iter().any(|r| {
                        r.as_ref().err().is_some_and(|pg_err| {
                            check_error_prefix(&e.to_string(), e.sqlstate(), pg_err, sql, "role")
                                .is_none()
                        })
                    }) =>
                {
                    None
                }
                Ok(()) => Some(Divergence {
                    kind: DivergenceKind::AnalyzeAcceptedExecuted,
                    message: format!(
                        "pg_sanity: the analyzer rejects the statement as the role, PG plans \
                         it.\n{header}\nanalyzer: {e}"
                    ),
                }),
            },
        }
    }

    /// Plan, then execute, `sql` as `role` in a transaction rolled back
    /// afterwards. `SET ROLE` doesn't apply the role's own settings
    /// (`ALTER ROLE ... SET`), which a session it logs in with would have:
    /// its `search_path` and `row_security` are set by hand.
    fn run_as_role(&mut self, sql: &str, role: &str) -> RoleOutcome {
        // `EXPLAIN [ANALYZE] <statement>`: what the inner statement does.
        // Planning it as the role (the `explain` step) mirrors EXPLAIN
        // without ANALYZE — the executor starts, nothing runs; executing it
        // mirrors EXPLAIN ANALYZE.
        let explain_target = explain_target(sql);
        let explainable = explain_target
            .as_ref()
            .map(|(inner, _)| is_query_for_explain(inner))
            .unwrap_or_else(|| is_query_for_explain(sql));
        // The seeding helpers live in the query session's pg_temp.
        let executable = explain_target.as_ref().map_or_else(
            || soundness::is_executable_by_role(sql),
            |(inner, analyze)| *analyze && soundness::is_executable_by_role(inner),
        );
        if executable {
            self.soundness.install(&mut self.query_client);
        }
        let mut tx = self
            .query_client
            .transaction()
            .unwrap_or_else(|e| panic!("pg_sanity: BEGIN failed: {e}"));
        let mut settings = |name: &'static str| -> Option<String> {
            tx.query_opt(
                "SELECT o.option_value \
                 FROM pg_catalog.pg_db_role_setting s, \
                      pg_catalog.pg_options_to_table(s.setconfig) o \
                 WHERE o.option_name = $2 \
                   AND s.setrole IN ((SELECT oid FROM pg_catalog.pg_roles WHERE rolname = $1), 0) \
                   AND s.setdatabase IN ((SELECT oid FROM pg_catalog.pg_database \
                                          WHERE datname = pg_catalog.current_database()), 0) \
                 ORDER BY s.setrole <> 0 DESC, s.setdatabase <> 0 DESC \
                 LIMIT 1",
                &[&role, &name],
            )
            .unwrap_or_else(|e| panic!("pg_sanity: reading the role's settings failed: {e}"))
            .map(|row| row.get(0))
        };
        let search_path = settings("search_path");
        let row_security = settings("row_security");
        let mut setup = format!("SET LOCAL ROLE {}", typedpg_core::quote_identifier(role));
        if let Some(path) = search_path {
            setup.push_str(&format!("; SET LOCAL search_path TO {path}"));
        }
        if let Some(value) = row_security {
            setup.push_str(&format!("; SET LOCAL row_security TO {value}"));
        }
        tx.batch_execute(&setup)
            .unwrap_or_else(|e| panic!("pg_sanity: {setup} failed: {}", render_pg_error(&e)));
        let explained = explain_target
            .as_ref()
            .map(|(inner, _)| inner.as_str())
            .unwrap_or(sql);
        let explain = if explainable {
            let mut sp = tx
                .transaction()
                .unwrap_or_else(|e| panic!("pg_sanity: SAVEPOINT failed: {e}"));
            let r = sp.batch_execute(&format!("EXPLAIN (GENERIC_PLAN) {explained}"));
            let _ = sp.rollback();
            r
        } else {
            Ok(())
        };
        // Describe as the role: the columns and parameter types PG infers
        // under the role's login search_path. An EXPLAIN's own columns are
        // its plan's, not the inner statement's.
        let described = if explain.is_ok() && explain_target.is_none() {
            tx.prepare(sql).ok()
        } else {
            None
        };
        // Executing runs the checks made as the statement's rows are
        // processed. Over the tables the migrations left (usually empty)
        // that reaches none of them; then one row per table is seeded as
        // the superuser and the statement runs again, so a row trigger, a
        // CHECK or a generated expression the analyzer denies shows.
        let run = explain_target
            .as_ref()
            .map(|(inner, _)| inner.as_str())
            .unwrap_or(sql);
        let mut executions = Vec::new();
        if explain.is_ok() && executable {
            executions.push(execute_as_role(&mut tx, run));
            tx.batch_execute("RESET ROLE")
                .unwrap_or_else(|e| panic!("pg_sanity: RESET ROLE failed: {e}"));
            if self.soundness.seed_for_role(&mut tx) {
                tx.batch_execute(&setup).unwrap_or_else(|e| {
                    panic!("pg_sanity: {setup} failed: {}", render_pg_error(&e))
                });
                executions.push(execute_as_role(&mut tx, run));
            }
        }
        let _ = tx.rollback();
        RoleOutcome {
            explain,
            executions,
            described,
        }
    }

    /// Compare the analysis the analyzer produced as `role` with PG's
    /// Describe of the same statement prepared as that role: the column
    /// names and types, and the parameter types, must agree.
    fn compare_role_describe(
        &mut self,
        header: &str,
        ours: &crate::resolve::AnalyzedQuery,
        stmt: &postgres::Statement,
    ) -> Option<Divergence> {
        let pg_columns: Vec<(String, u32)> = stmt
            .columns()
            .iter()
            .map(|c| (c.name().to_string(), c.type_().oid()))
            .collect();
        if ours.columns.len() != pg_columns.len() {
            return Some(Divergence {
                kind: DivergenceKind::ColumnCount,
                message: format!(
                    "pg_sanity: the role's analysis has another column count.\n{header}\n\
                     analyzer: {} columns ({:?})\nPG:       {} columns ({:?})",
                    ours.columns.len(),
                    ours.columns.iter().map(|c| &c.name).collect::<Vec<_>>(),
                    pg_columns.len(),
                    pg_columns.iter().map(|c| &c.0).collect::<Vec<_>>(),
                ),
            });
        }
        for (i, (our_col, (pg_name, pg_oid))) in
            ours.columns.iter().zip(pg_columns.iter()).enumerate()
        {
            let pg_name_stripped = if pg_name.as_str() == "?column?" {
                pg_name.as_str()
            } else {
                pg_name
                    .strip_suffix('!')
                    .or_else(|| pg_name.strip_suffix('?'))
                    .unwrap_or(pg_name)
            };
            if our_col.name != pg_name_stripped {
                return Some(Divergence {
                    kind: DivergenceKind::ColumnName,
                    message: format!(
                        "pg_sanity: the role's analysis has another column {i} name.\n\
                         {header}\nanalyzer: {:?}\nPG:       {:?}",
                        our_col.name, pg_name,
                    ),
                });
            }
            let pg_qname = self.qualified_type_name(*pg_oid).unwrap_or_else(|e| {
                panic!(
                    "pg_sanity: failed to resolve type oid {pg_oid} for the role's column \
                     '{}': {e}",
                    our_col.name
                )
            });
            let our_qname = qualified_type_name_for_compare(&our_col.pg_type);
            if our_qname != pg_qname {
                return Some(Divergence {
                    kind: DivergenceKind::ColumnType,
                    message: format!(
                        "pg_sanity: the role's analysis has another type for column '{}'.\n\
                         {header}\nanalyzer: {our_qname} (Type::{:?})\nPG:       {pg_qname} \
                         (oid {pg_oid})",
                        our_col.name, our_col.pg_type,
                    ),
                });
            }
        }
        let our_params: Vec<&Type> = ours
            .params
            .iter()
            .map(|p| &p.pg_type)
            .chain(
                ours.spreads
                    .iter()
                    .flat_map(|s| s.fields.iter().map(|f| &f.pg_type)),
            )
            .collect();
        let pg_params = stmt.params();
        if our_params.len() != pg_params.len() {
            return Some(Divergence {
                kind: DivergenceKind::ParamCount,
                message: format!(
                    "pg_sanity: the role's analysis has another parameter count.\n{header}\n\
                     analyzer: {} params\nPG:       {} params",
                    our_params.len(),
                    pg_params.len(),
                ),
            });
        }
        for (i, (our_ty, pg_ty)) in our_params.iter().zip(pg_params.iter()).enumerate() {
            let pg_oid = pg_ty.oid();
            if pg_oid == 0 {
                continue;
            }
            let pg_qname = self.qualified_type_name(pg_oid).unwrap_or_else(|e| {
                panic!(
                    "pg_sanity: failed to resolve type oid {pg_oid} for the role's \
                     parameter ${}: {e}",
                    i + 1,
                )
            });
            let our_qname = qualified_param_type_name_for_compare(our_ty);
            if our_qname != pg_qname {
                return Some(Divergence {
                    kind: DivergenceKind::ParamType,
                    message: format!(
                        "pg_sanity: the role's analysis has another type for parameter ${}.\n\
                         {header}\nanalyzer: {our_qname} (Type::{our_ty:?})\nPG:       {pg_qname} \
                         (oid {pg_oid})",
                        i + 1,
                    ),
                });
            }
        }
        None
    }
}

/// Execute `sql` as the role set on `tx`, with NULL parameters, in a
/// savepoint rolled back afterwards.
fn execute_as_role(tx: &mut postgres::Transaction<'_>, sql: &str) -> Result<(), postgres::Error> {
    let mut sp = tx
        .transaction()
        .unwrap_or_else(|e| panic!("pg_sanity: SAVEPOINT failed: {e}"));
    let r = sp.prepare(sql).and_then(|stmt| {
        let nulls: Vec<NullParam> = stmt.params().iter().map(|_| NullParam).collect();
        let params: Vec<&(dyn ToSql + Sync)> =
            nulls.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        sp.query(&stmt, &params).map(|_| ())
    });
    let _ = sp.rollback();
    r
}

/// Whether a schema holds user objects: not `pg_catalog`, a TOAST schema,
/// the temporary schema or `information_schema`.
fn is_user_schema(name: &str) -> bool {
    name != "information_schema" && !name.starts_with("pg_")
}

/// The name PG gives a model role: the migrations' user stands for the
/// session user (its real name is unknowable), every other role for its own
/// name.
fn pg_role_name(
    db: &crate::pg_catalog::PgCatalog,
    role: crate::ddl::roles::RoleId,
    session_user: &str,
) -> String {
    if role == db.roles.migrations_user {
        session_user.to_owned()
    } else {
        db.roles.name(role).to_owned()
    }
}

/// The panic message of a privilege matrix disagreement.
fn privilege_mismatch(role: &str, object: &str, privilege: &str, ours: bool, pg: bool) -> String {
    format!(
        "pg_sanity: privilege mismatch while comparing role \"{role}\":\n\
         object:    {object}\n\
         privilege: {privilege}\n\
         analyzer:  {ours}\n\
         PG:        {pg}"
    )
}

impl Drop for PgSanityServer {
    fn drop(&mut self) {
        self.soundness.write_stats();
        // Tear down the scratch database. We can't run DROP DATABASE while
        // any session is connected to it, so first drop our scratch client
        // (by replacing it via `mem::replace` … actually just close
        // implicitly when we open the admin connection — `Client` has no
        // explicit close; dropping it on the next admin connection is fine
        // because PG terminates the old session when its TCP socket closes,
        // which happens when the original client drops below).
        //
        // The order: open a fresh admin client *before* the scratch client
        // is dropped would race; instead, take ownership of the scratch
        // client out of `self` and drop it explicitly first.
        let scratch = std::mem::replace(
            &mut self.client,
            // Placeholder client we never use; created against the same
            // admin URL so the connection succeeds quickly.
            Client::connect(&self.admin_conn_str, NoTls)
                .expect("pg_sanity: drop: admin reconnect failed"),
        );
        drop(scratch);
        let query = std::mem::replace(
            &mut self.query_client,
            Client::connect(&self.admin_conn_str, NoTls)
                .expect("pg_sanity: drop: admin reconnect failed"),
        );
        drop(query);

        // The placeholder client we just created is connected to the admin
        // DB — perfect for issuing the DROP. Errors during drop are
        // intentionally swallowed (a panic in Drop would mask the original
        // test failure); the script-level cleanup tears the whole cluster
        // down anyway.
        let _ = self
            .client
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{}\"", self.db_name));

        // Roles are cluster-wide: drop the ones the migrations created so
        // concurrent test processes don't collide. Errors are swallowed (a
        // role may already be gone, or still owned by another database).
        // `pg_authid` is a shared catalog: under the lock, as the
        // migrations writing it run.
        if !self.created_roles.is_empty() {
            let lock = SharedCatalogLock::acquire(&self.admin_conn_str);
            for role in &self.created_roles {
                let quoted = typedpg_core::quote_identifier(role);
                // Its privileges on shared objects (a parameter, a
                // tablespace) would keep DROP ROLE from dropping it; DROP
                // OWNED revokes them (and owns nothing in this database).
                // Its own statement: it fails for a role already gone.
                let _ = self
                    .client
                    .batch_execute(&format!("DROP OWNED BY {quoted}"));
                let _ = self
                    .client
                    .batch_execute(&format!("DROP ROLE IF EXISTS {quoted}"));
            }
            drop(lock);
        }
    }
}

/// Build a unique-ish scratch database name: `pg_sanity_<pid>_<counter>_<nanos>`.
/// Underscores-only so it doesn't need quoting; lowercase prefix dodges any
/// PG identifier folding subtleties.
fn scratch_db_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("pg_sanity_{pid}_{n}_{nanos}")
}

/// Take a libpq-style connection string and replace its `dbname` with the
/// given name. Parse via `postgres::config::Config` to validate, then
/// rebuild the keyword=value form manually — `Config` has accessors but
/// no public `Display` for keyword form.
fn with_dbname(conn_str: &str, dbname: &str) -> Result<String, String> {
    let config: Config = conn_str
        .parse()
        .map_err(|e| format!("pg_sanity: invalid POSTGRES_URL ({conn_str:?}): {e}"))?;
    let mut parts: Vec<String> = Vec::new();
    if let Some(host) = config.get_hosts().first() {
        let host_str = match host {
            postgres::config::Host::Tcp(s) => s.clone(),
            #[cfg(unix)]
            postgres::config::Host::Unix(p) => p.to_string_lossy().into_owned(),
        };
        parts.push(format!("host={host_str}"));
    }
    if let Some(port) = config.get_ports().first() {
        parts.push(format!("port={port}"));
    }
    if let Some(user) = config.get_user() {
        parts.push(format!("user={user}"));
    }
    if let Some(pw) = config.get_password() {
        parts.push(format!("password={}", String::from_utf8_lossy(pw)));
    }
    parts.push(format!("dbname={dbname}"));
    Ok(parts.join(" "))
}

/// Compare the analyzer's outcome for a migration with PG's.
fn compare_apply_outcome<E: std::fmt::Display>(
    sql: &str,
    our_result: &Result<(), E>,
    pg_result: &Result<(), postgres::Error>,
) -> Option<Divergence> {
    match (our_result, pg_result) {
        (Ok(()), Ok(())) => None,
        (Err(_), Err(_)) => {
            let our_msg = format!("{}", our_result.as_ref().unwrap_err());
            // DDL errors are `DdlError`, outside `expected_sqlstate`'s
            // domain — wording-only comparison here.
            check_error_prefix(
                &our_msg,
                None,
                pg_result.as_ref().unwrap_err(),
                sql,
                "apply_sql",
            )
        }
        (Ok(()), Err(e)) => {
            // Treat protocol-level errors (no DbError → the postgres-rs
            // client received a wire frame it couldn't parse) as "PG
            // couldn't decide" and skip. Real rejections always carry
            // a SQLSTATE.
            e.as_db_error()?;
            Some(Divergence {
                kind: DivergenceKind::ApplyPgRejected,
                message: format!(
                    "pg_sanity: analyzer accepted DDL but PG rejected it.\n\
                     SQL:\n---\n{sql}\n---\nPG error: {}",
                    render_pg_error(e),
                ),
            })
        }
        (Err(e), Ok(())) => Some(Divergence {
            kind: DivergenceKind::ApplyAnalyzerRejected,
            message: format!(
                "pg_sanity: analyzer rejected DDL but PG accepted it.\n\
                 SQL:\n---\n{sql}\n---\nanalyzer error: {e}"
            ),
        }),
    }
}

/// Render a server-side message with the SQLSTATE appended — used for
/// human-readable diagnostics in panic messages. `postgres::Error::Display`
/// collapses to a useless "db error", so we always dig into `as_db_error()`
/// when present.
fn render_pg_error(e: &postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        format!("{} (SQLSTATE {})", db.message(), db.code().code())
    } else {
        e.to_string()
    }
}

/// Enforce the contract: the analyzer's error message must begin with the
/// PG server-side message verbatim. Trailing detail (column names, hints)
/// is allowed. When the wording prefix holds *and* the analyzer states an
/// expected SQLSTATE (`our_code`, from `pgmsg::expected_sqlstate`), the
/// code must match PG's too; `our_code = None` means the analyzer makes no
/// claim and only the wording is compared.
///
/// Returns a [`Divergence`] on either failure, or `None` when the contract
/// holds. If PG didn't send a structured `DbError` (a protocol-level issue,
/// not a real rejection) we can't compare meaningfully and return `None` —
/// the outer success/failure invariant has already been checked by the
/// caller.
fn check_error_prefix(
    our_msg: &str,
    our_code: Option<&'static str>,
    pg_err: &postgres::Error,
    sql: &str,
    kind: &str,
) -> Option<Divergence> {
    let db = pg_err.as_db_error()?;
    let pg_msg = db.message();
    if !our_msg.starts_with(pg_msg) {
        return Some(Divergence {
            kind: DivergenceKind::ErrorPrefix,
            message: format!(
                "pg_sanity: {kind} error must start with PG's message.\n\
                 SQL:\n---\n{sql}\n---\n\
                 PG (expected prefix): {pg_msg}\n\
                 analyzer:             {our_msg}\n\
                 PG (with SQLSTATE):   {}",
                render_pg_error(pg_err),
            ),
        });
    }
    if let Some(code) = our_code {
        let pg_code = db.code().code();
        if code != pg_code {
            return Some(Divergence {
                kind: DivergenceKind::SqlState,
                message: format!(
                    "pg_sanity: {kind} SQLSTATE mismatch (same message prefix).\n\
                     SQL:\n---\n{sql}\n---\n\
                     message:              {pg_msg}\n\
                     analyzer (expected):  SQLSTATE {code}\n\
                     PG:                   SQLSTATE {pg_code}",
                ),
            });
        }
    }
    // `DROP ROLE`'s DETAIL names every object that depends on the role; the
    // analyzer carries it inline after the message. Compare it whole,
    // including PG's order (by dependent object OID).
    if kind == "apply_sql"
        && pg_msg.starts_with("role \"")
        && pg_msg.ends_with("cannot be dropped because some objects depend on it")
        && let Some(detail) = db.detail()
    {
        let expected = format!("({})", detail.replace('\n', "; "));
        if !our_msg.ends_with(&expected) {
            return Some(Divergence {
                kind: DivergenceKind::ErrorPrefix,
                message: format!(
                    "pg_sanity: {kind} error DETAIL must match PG's.\n\
                     SQL:\n---\n{sql}\n---\n\
                     PG (expected): {expected}\n\
                     analyzer:      {our_msg}"
                ),
            });
        }
    }
    None
}

/// Render an analyzer [`Type`] into the same `schema.name[]?` shape that
/// [`PgSanityServer::qualified_type_name`] produces, so the two can be
/// compared as plain strings.
///
/// Domains are unwrapped to their base type for comparison purposes — PG's
/// wire-level Describe collapses domain columns to their base OID
/// (`getBaseTypeAndTypmod` in printtup's SendRowDescriptionCols). The
/// analyzer's `Type::Domain` shape is asserted in dedicated tests; this
/// comparison only validates the underlying physical type. Only the column
/// type itself is collapsed: an array *of* a domain is not a domain, so PG
/// reports it as the domain's array type (`nn[]`).
fn qualified_type_name_for_compare(ty: &Type) -> String {
    match ty {
        Type::Domain { base, .. } => qualified_type_name_for_compare(base),
        Type::Array { element, .. } => {
            format!("{}[]", qualified_param_type_name_for_compare(element))
        }
        other => qualified_param_type_name_for_compare(other),
    }
}

/// Like [`qualified_type_name_for_compare`] but for *input parameters*.
///
/// The one difference is domains: PG's wire-level Describe collapses a domain
/// *output column* to its base OID, but reports an *input parameter*'s type as
/// the domain itself. So here we render a [`Type::Domain`] by its own
/// qualified name rather than unwrapping to the base type.
fn qualified_param_type_name_for_compare(ty: &Type) -> String {
    match ty {
        Type::Basic { schema, name, .. }
        | Type::Enum { schema, name, .. }
        | Type::Range { schema, name, .. }
        | Type::Composite { schema, name, .. }
        | Type::Domain { schema, name, .. } => QualifiedName::new(schema, name).to_string(),
        Type::Array { element, .. } => {
            format!("{}[]", qualified_param_type_name_for_compare(element))
        }
        Type::AnonymousRecord { .. } => "pg_catalog.record".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::writes_shared_catalog;

    #[test]
    fn migrations_writing_shared_catalogs_are_serialized() {
        for sql in [
            "CREATE ROLE a",
            "CREATE TABLE t (a int); CREATE USER b",
            "ALTER ROLE a SET work_mem = '1MB'",
            "DROP ROLE a",
            "GRANT a TO b",
            "GRANT SET ON PARAMETER log_min_duration_statement TO a",
            "REVOKE ALL ON PARAMETER work_mem FROM a",
            "GRANT CREATE ON TABLESPACE pg_default TO a",
            "GRANT CONNECT ON DATABASE postgres TO a",
            "ALTER DATABASE postgres SET timezone TO 'UTC'",
            "ALTER SYSTEM SET work_mem = '1MB'",
            "COMMENT ON ROLE a IS 'x'",
            "REASSIGN OWNED BY a TO postgres",
            "DROP OWNED BY a",
            "DO $$ BEGIN CREATE ROLE a; END $$",
            // Unparsable: taken as one.
            "CREATE ROLE",
        ] {
            assert!(writes_shared_catalog(sql), "{sql}");
        }
        for sql in [
            "CREATE TABLE t (a int)",
            "GRANT SELECT ON t TO a",
            "COMMENT ON TABLE t IS 'x'",
            "DO $$ BEGIN PERFORM 1; END $$",
        ] {
            assert!(!writes_shared_catalog(sql), "{sql}");
        }
    }
}
