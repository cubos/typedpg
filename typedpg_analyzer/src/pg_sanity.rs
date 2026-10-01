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
        let pg_result = self.client.batch_execute(sql);
        self.soundness.schema_changed();
        match (our_result, &pg_result) {
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

    /// Compare `analysis_sql` (the rewritten SQL with `$N` placeholders that
    /// the analyzer fed to its static pass) against PG's Parse+Describe.
    /// Panics if outcomes diverge or any column's name or qualified type
    /// name disagrees.
    pub(crate) fn assert_analyze_matches(
        &mut self,
        analysis_sql: &str,
        our_result: &Result<AnalyzedQuery, AnalyzeError>,
    ) {
        if let Some(div) = self.compare_analyze_matches(analysis_sql, our_result) {
            panic!("{}", div.message);
        }
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
                self.soundness
                    .check(&mut self.query_client, analysis_sql, ours, stmt)
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
                "SELECT n.nspname, t.typname, t.typcategory::text, t.typelem::int8 \
                 FROM pg_catalog.pg_type t \
                 JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
                 WHERE t.oid = {oid}"
            ),
            &[],
        )?;
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let category: String = row.get(2);
        let typelem: u32 = row.get::<_, i64>(3) as u32;

        let qualified = if category == "A" && typelem != 0 {
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
