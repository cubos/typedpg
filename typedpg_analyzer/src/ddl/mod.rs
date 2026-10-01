//! DDL interpreter: applies DDL statements to a [`PgCatalog`](crate::PgCatalog)
//! in memory.
//!
//! This module parses SQL migration files using `typedpg_pg_query` and mutates the
//! snapshot as if the DDL had been executed against a real PostgreSQL instance.

mod acl;
pub mod aggregates;
pub mod alter;
pub(crate) mod cluster;
pub(crate) mod cmdtag;
pub(crate) mod coldeps;
pub mod collations;
mod comment;
mod conversion_procs;
pub(crate) mod conversions;
mod defaults;
pub(crate) mod depend;
mod dml;
mod do_block;
pub mod drop;
pub(crate) mod event_triggers;
mod expr_kind;
pub mod extensions;
pub(crate) mod fdw;
pub(crate) mod function_body;
pub mod functions;
mod guc;
pub mod indexes;
pub(crate) mod languages;
pub(crate) mod maintenance;
pub(crate) mod opclass;
pub mod operators;
pub(crate) mod policies;
pub(crate) mod prepared;
pub(crate) mod publications;
pub(crate) mod reloptions;
pub(crate) mod rules;
pub mod schema_stmt;
pub(crate) mod seqparams;
pub mod sequences;
pub(crate) mod session;
pub(crate) mod statistics;
pub mod tables;
pub(crate) mod text_search;
pub(crate) mod triggers;
mod txblock;
pub mod types;
pub mod util;
pub mod views;
pub(crate) mod volatile;

#[cfg(any(test, feature = "internal"))]
pub(crate) use views::serialize_subnode;

use typedpg_pg_query::protobuf::node;

use crate::pg_catalog::PgCatalog;

// ─── Error type ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DdlError {
    Parse(String),
    /// An error in a migration file, located: what
    /// [`PgCatalog::apply_migration`](crate::PgCatalog::apply_migration)
    /// returns. `source` is the statement's error; `diagnostic` the
    /// rendered location in `filename` — `-->` line, snippet with a caret,
    /// help — that follows its message.
    Migration {
        filename: String,
        source: Box<DdlError>,
        diagnostic: String,
    },
    UnsupportedDdl(String),
    TypeNotFound(String),
    TableNotFound(String),
    DuplicateObject(String),
    ExtensionError(String),
    DependencyError(String),
    ViewAnalysis {
        view: String,
        source: Box<crate::error::AnalyzeError>,
    },
    /// An invariant the DDL interpreter relies on was violated — typically a
    /// catalog row that should have been inserted moments before turned out
    /// to be missing, or the OID counter overflowed `u32`. Surfaced as an
    /// error so callers can report the offending DDL without crashing the
    /// macro host process.
    Internal(String),
}

impl std::fmt::Display for DdlError {
    /// Display emits the variant's stored message verbatim — variants are for
    /// pattern matching on the kind of failure, not for adding a prefix to
    /// the message. This keeps wording aligned with PG, where the
    /// server-side message contains the full diagnostic; the
    /// `pglite_sanity` cross-check requires our messages to *start with*
    /// PG's message verbatim.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DdlError::Parse(msg)
            | DdlError::UnsupportedDdl(msg)
            | DdlError::TypeNotFound(msg)
            | DdlError::TableNotFound(msg)
            | DdlError::DuplicateObject(msg)
            | DdlError::ExtensionError(msg)
            | DdlError::DependencyError(msg) => write!(f, "{msg}"),
            DdlError::Internal(msg) => write!(f, "internal DDL interpreter error: {msg}"),
            // The statement's message comes first, verbatim; the
            // diagnostic carries the file name.
            DdlError::Migration { diagnostic, .. } => write!(f, "{}", diagnostic.trim_end()),
            DdlError::ViewAnalysis { view, source } => {
                // Lead with the inner analyzer message so it stays
                // verbatim-aligned with PG's wording (the `pglite_sanity`
                // mirror checks `starts_with` against PG); append the view
                // identifier as supplementary context.
                write!(f, "{source} (while analyzing view '{view}')")
            }
        }
    }
}

impl std::error::Error for DdlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DdlError::Migration { source, .. } => Some(source.as_ref()),
            DdlError::ViewAnalysis { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

// Extension membership is tracked via `pg_depend` rows with deptype=Extension
// (refclassid=PG_EXTENSION_RELID); see `ddl/extensions.rs`.

// ─── Dispatcher ─────────────────────────────────────────────────────────────

/// Apply one migration: its statements, in the transactions the migration
/// runner gives them ([`txblock::Transaction`]).
///
/// On failure, also returns where in `sql` the error is (see
/// [`MigrationFailure`]).
pub(crate) fn apply_migration(db: &mut PgCatalog, sql: &str) -> Result<(), MigrationFailure> {
    // Errors the analyzer raises while the statements are applied keep their
    // plain messages (that is what `DdlError` reports), but their spans —
    // AST locations, offsets into `sql` — and hints are captured to locate
    // the failure in the file.
    let lex = crate::param::LexOutput::identity(sql);
    let _capture = crate::error::DiagContextGuard::capture(sql, &lex);
    let mut located = None;
    let result = in_migration(db, |db| {
        let parsed = typedpg_pg_query::parse(sql).map_err(|e| {
            let (message, position) = match e {
                typedpg_pg_query::Error::Parse { message, position } => (message, position),
                other => (other.to_string(), None),
            };
            located = position.map(|p| crate::error::CapturedDiagnostic {
                pg_message: message.clone(),
                primary: Some(crate::error::DiagnosticLabel::new(
                    crate::error::SourceSpan::syntax_error_at(sql, p, &message),
                    "",
                )),
                secondaries: Vec::new(),
                hint: None,
                notes: Vec::new(),
            });
            // A grammar error carries PG's message verbatim.
            DdlError::Parse(message)
        })?;
        let mut tx = txblock::Transaction::start(db, sql, &parsed.protobuf.stmts);
        for raw_stmt in &parsed.protobuf.stmts {
            let Some(stmt) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
                continue;
            };
            crate::error::take_captured();
            let result = tx
                .check(db, stmt)
                .and_then(|()| tx.check_setting(stmt))
                .and_then(|()| {
                    with_statement_sql(db, sql, raw_stmt, |db| match stmt {
                        node::Node::TransactionStmt(t) => tx.control(db, t),
                        _ => apply_statement(db, stmt),
                    })
                });
            if let Err(e) = result {
                located = Some(locate_in_statement(sql, raw_stmt, &e));
                tx.abort(db);
                return Err(e);
            }
            tx.after(stmt);
        }
        tx.finish(db);
        Ok(())
    });
    result.map_err(|error| MigrationFailure {
        error,
        location: located.map(Box::new),
    })
}

/// A migration that failed: the error, and — when known — where in the
/// migration's SQL it is, as a diagnostic whose spans are byte offsets
/// into it.
pub(crate) struct MigrationFailure {
    pub error: DdlError,
    pub location: Option<Box<crate::error::CapturedDiagnostic>>,
}

/// Where `error`, raised by the statement `raw_stmt` of `sql`, is: the
/// diagnostic the analyzer captured for it when that is this error's and
/// points into the statement — the offending token, with its label and
/// hint — else the statement's first token.
fn locate_in_statement(
    sql: &str,
    raw_stmt: &typedpg_pg_query::protobuf::RawStmt,
    error: &DdlError,
) -> crate::error::CapturedDiagnostic {
    let (start, end) = statement_range(sql, raw_stmt);
    let message = error.to_string();
    let captured = crate::error::take_captured().filter(|c| message.starts_with(&c.pg_message));
    let in_statement = |label: &crate::error::DiagnosticLabel| {
        label.span.start >= start && label.span.start < end.max(start + 1)
    };
    match captured {
        Some(mut c) if c.primary.as_ref().is_some_and(in_statement) => {
            c.secondaries.retain(in_statement);
            c
        }
        captured => {
            // The error has no usable position: point at the name it is
            // about when the statement spells it once, else at the
            // statement.
            let (span, label) = match named_token(sql, start, end, &message) {
                Some(span) => (span, ""),
                None => {
                    let first = first_token_offset(sql, start);
                    let span = crate::error::SourceSpan::at_token(sql, first)
                        .unwrap_or_else(|| crate::error::SourceSpan::one_char_at(first));
                    (span, "in this statement")
                }
            };
            let (hint, notes) = captured.map(|c| (c.hint, c.notes)).unwrap_or_default();
            crate::error::CapturedDiagnostic {
                pg_message: message,
                primary: Some(crate::error::DiagnosticLabel::new(span, label)),
                secondaries: Vec::new(),
                hint,
                notes,
            }
        }
    }
}

/// The one token of `sql[start..end]` naming what `message` is about: the
/// name PG quotes in a `… "name" does not exist` / `… already exists` /
/// `… is not …` message (its last dotted part), when exactly one
/// identifier of the statement spells it — case-folded unless quoted, as
/// PG reads identifiers. `None` when the statement has none or several.
fn named_token(
    sql: &str,
    start: usize,
    end: usize,
    message: &str,
) -> Option<crate::error::SourceSpan> {
    let first_line = message.lines().next()?;
    let (_, rest) = first_line.split_once('"')?;
    let (quoted, after) = rest.split_once('"')?;
    if !(after.starts_with(" does not exist")
        || after.starts_with(" already exists")
        || after.starts_with(" is not "))
    {
        return None;
    }
    let name = quoted.rsplit('.').next()?;
    if name.is_empty() {
        return None;
    }
    let text = sql.get(start..end)?;
    let scanned = typedpg_pg_query::scan(text).ok()?;
    let mut found = scanned.tokens.iter().filter_map(|t| {
        let (s, e) = (usize::try_from(t.start).ok()?, usize::try_from(t.end).ok()?);
        let token = text.get(s..e)?;
        let spelled = match token.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            Some(inner) => inner.replace("\"\"", "\""),
            None => token.to_lowercase(),
        };
        (spelled == name).then(|| crate::error::SourceSpan::new(start + s, start + e))
    });
    let span = found.next()?;
    found.next().is_none().then_some(span)
}

/// The byte range of `raw_stmt` in `sql` (`stmt_len` 0 means "to the end").
fn statement_range(sql: &str, raw_stmt: &typedpg_pg_query::protobuf::RawStmt) -> (usize, usize) {
    let start = usize::try_from(raw_stmt.stmt_location)
        .unwrap_or(0)
        .min(sql.len());
    let end = match usize::try_from(raw_stmt.stmt_len) {
        Ok(0) | Err(_) => sql.len(),
        Ok(len) => (start + len).min(sql.len()),
    };
    (start, end)
}

/// The offset of the first token at or after `at`: a statement's location
/// is where the previous one ended, before the whitespace and comments
/// that lead into it.
fn first_token_offset(sql: &str, mut at: usize) -> usize {
    let bytes = sql.as_bytes();
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes[at..].starts_with(b"--") {
            at = sql[at..].find('\n').map_or(sql.len(), |n| at + n + 1);
        } else if bytes[at..].starts_with(b"/*") {
            // Block comments nest.
            let mut depth = 0usize;
            while at < bytes.len() {
                if bytes[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    at += 1;
                }
            }
        } else {
            return at;
        }
    }
}

/// Parse and apply all DDL statements in a SQL string that runs inside
/// another statement (an extension's scripts).
pub(crate) fn apply_sql_to(db: &mut PgCatalog, sql: &str) -> Result<(), DdlError> {
    // The statements' locations are offsets into `sql`, not the migration.
    let _barrier = crate::error::DiagContextGuard::barrier();
    in_migration(db, |db| {
        let parsed = parse(sql)?;
        for raw_stmt in &parsed.protobuf.stmts {
            let Some(stmt) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
                continue;
            };
            with_statement_sql(db, sql, raw_stmt, |db| apply_statement(db, stmt))?;
        }
        Ok(())
    })
}

/// Run `f` in the migrations' session.
fn in_migration(
    db: &mut PgCatalog,
    f: impl FnOnce(&mut PgCatalog) -> Result<(), DdlError>,
) -> Result<(), DdlError> {
    let was_in_migration = std::mem::replace(&mut db.in_migration, true);
    // `$user` resolves only in the migrations' session.
    db.refresh_search_path();
    let result = f(db);
    db.in_migration = was_in_migration;
    db.refresh_search_path();
    result
}

fn parse(sql: &str) -> Result<typedpg_pg_query::ParseResult, DdlError> {
    // A grammar error carries PG's message verbatim.
    typedpg_pg_query::parse(sql).map_err(|e| {
        DdlError::Parse(match e {
            typedpg_pg_query::Error::Parse { message, .. } => message,
            other => other.to_string(),
        })
    })
}

/// Run `f` with [`PgCatalog::statement_sql`] set to the text of `raw_stmt`.
fn with_statement_sql(
    db: &mut PgCatalog,
    sql: &str,
    raw_stmt: &typedpg_pg_query::protobuf::RawStmt,
    f: impl FnOnce(&mut PgCatalog) -> Result<(), DdlError>,
) -> Result<(), DdlError> {
    let (start, end) = statement_range(sql, raw_stmt);
    let text = sql.get(start..end).map(str::to_owned);
    // Statements may apply nested SQL (extension scripts): restore the
    // outer statement's text afterwards.
    let outer = std::mem::replace(&mut db.statement_sql, text);
    let result = f(db);
    db.statement_sql = outer;
    result
}

/// Dispatch a single parsed statement.
fn apply_statement(db: &mut PgCatalog, stmt: &node::Node) -> Result<(), DdlError> {
    match stmt {
        // ── Tables ──────────────────────────────────────────────────
        node::Node::CreateStmt(s) => tables::create_table(db, s),
        node::Node::AlterTableStmt(s) => tables::alter_table(db, s),
        node::Node::CreateForeignTableStmt(s) => tables::create_foreign_table(db, s),
        node::Node::RefreshMatViewStmt(s) => views::refresh_materialized_view(db, s),

        // ── Types ───────────────────────────────────────────────────
        node::Node::CreateDomainStmt(s) => types::create_domain(db, s),
        node::Node::CreateEnumStmt(s) => types::create_enum(db, s),
        node::Node::CompositeTypeStmt(s) => types::create_composite(db, s),
        node::Node::CreateRangeStmt(s) => types::create_range(db, s),
        node::Node::AlterEnumStmt(s) => types::alter_enum(db, s),
        node::Node::AlterDomainStmt(s) => types::alter_domain(db, s),

        // ── Drop ────────────────────────────────────────────────────
        node::Node::DropStmt(s) => drop::drop_objects(db, s),

        // ── Schema ──────────────────────────────────────────────────
        node::Node::CreateSchemaStmt(s) => schema_stmt::create_schema(db, s),

        // ── Sequences ───────────────────────────────────────────────
        node::Node::CreateSeqStmt(s) => sequences::create_sequence(db, s),
        node::Node::AlterSeqStmt(s) => sequences::alter_sequence(db, s),

        // ── Functions ───────────────────────────────────────────────
        node::Node::CreateFunctionStmt(s) => functions::create_function(db, s),
        node::Node::AlterFunctionStmt(s) => functions::alter_function(db, s),

        // ── Views ───────────────────────────────────────────────────
        node::Node::ViewStmt(s) => views::create_view(db, s),
        node::Node::CreateTableAsStmt(s) => views::create_table_as(db, s),

        // ── Extensions ──────────────────────────────────────────────
        node::Node::CreateExtensionStmt(s) => extensions::create_extension(db, s),
        node::Node::AlterExtensionStmt(s) => extensions::alter_extension(db, s),

        // ── Type definitions (CREATE TYPE name (...)) and casts ─────
        node::Node::DefineStmt(s) => {
            use typedpg_pg_query::protobuf::ObjectType;
            match ObjectType::try_from(s.kind).unwrap_or(ObjectType::Undefined) {
                ObjectType::ObjectType => types::define_type(db, s),
                ObjectType::ObjectOperator => operators::define_operator(db, s),
                ObjectType::ObjectAggregate => aggregates::define_aggregate(db, s),
                ObjectType::ObjectCollation => collations::define_collation(db, s),
                ObjectType::ObjectTsconfiguration
                | ObjectType::ObjectTsdictionary
                | ObjectType::ObjectTsparser
                | ObjectType::ObjectTstemplate => text_search::define(db, s),
                // Other DefineStmt kinds (text search, etc.) are irrelevant
                // for static type analysis.
                _ => Ok(()),
            }
        }
        node::Node::CreateCastStmt(s) => types::create_cast(db, s),

        // ── ALTER ... RENAME / SET SCHEMA ───────────────────────────
        node::Node::RenameStmt(s) => alter::rename(db, s),
        node::Node::AlterObjectSchemaStmt(s) => alter::set_schema(db, s),

        // ── Session state (search_path) ─────────────────────────────
        node::Node::VariableSetStmt(s) => session::variable_set(db, s),
        node::Node::AlterDatabaseSetStmt(s) => session::alter_database_set(db, s),
        // Top-level transaction control goes through the migration's
        // `txblock::Transaction`; this is what one nested in another
        // statement's SQL does.
        node::Node::TransactionStmt(s) => session::transaction(db, s),
        node::Node::SelectStmt(s) if s.into_clause.is_some() => views::select_into(db, s),
        node::Node::SelectStmt(s) => {
            dml::check_statement(db, stmt)?;
            session::select_side_effects(db, s)
        }
        node::Node::InsertStmt(_)
        | node::Node::UpdateStmt(_)
        | node::Node::DeleteStmt(_)
        | node::Node::MergeStmt(_)
        | node::Node::CallStmt(_) => dml::check_statement(db, stmt),

        // ── Indexes ─────────────────────────────────────────────────
        // Indexes don't change query result types, but expression indexes
        // forbid VOLATILE functions (CREATE INDEX walks the expression
        // tree to detect them).
        node::Node::IndexStmt(s) => indexes::create_index(db, s),

        // ── No-ops (irrelevant for type analysis) ───────────────────
        node::Node::CommentStmt(s) => comment::comment_on(db, s),
        node::Node::AlterOwnerStmt(s) => comment::alter_owner(db, s),
        node::Node::CreateTrigStmt(s) => {
            triggers::create_trigger(db, s).and_then(|()| coldeps::record_trigger(db, s))
        }
        node::Node::GrantStmt(s) => acl::grant(db, s),
        node::Node::CreatePolicyStmt(s) => {
            policies::create_policy(db, s).and_then(|()| coldeps::record_policy(db, s))
        }
        node::Node::AlterPolicyStmt(s) => {
            policies::alter_policy(db, s).and_then(|()| coldeps::record_policy_alter(db, s))
        }
        node::Node::RuleStmt(s) => rules::create_rule(db, s).and_then(|()| coldeps::record_rule(db, s)),
        node::Node::CreateAmStmt(s) => opclass::create_am(db, s),
        node::Node::CreateConversionStmt(s) => conversions::create_conversion(db, s),
        node::Node::CreatePlangStmt(s) => languages::create_language(db, s),
        node::Node::CreateTransformStmt(s) => languages::create_transform(db, s),
        node::Node::AlterTsconfigurationStmt(s) => text_search::alter_configuration(db, s),
        node::Node::AlterTsdictionaryStmt(s) => text_search::alter_dictionary(db, s),
        node::Node::CreateEventTrigStmt(s) => event_triggers::create_event_trigger(db, s),
        node::Node::AlterEventTrigStmt(s) => event_triggers::alter_event_trigger(db, s),
        node::Node::CreatePublicationStmt(s) => publications::create_publication(db, s),
        node::Node::AlterPublicationStmt(s) => publications::alter_publication(db, s),
        // Compiled first (plpgsql_compile_inline), then run.
        node::Node::DoStmt(s) => {
            function_body::do_block(db, s)?;
            do_block::execute(db, s)
        }
        node::Node::AlterExtensionContentsStmt(s) => extensions::alter_extension_contents(db, s),
        node::Node::CreateFdwStmt(s) => fdw::create_fdw(db, s),
        node::Node::AlterFdwStmt(s) => fdw::alter_fdw(db, s),
        node::Node::CreateForeignServerStmt(s) => fdw::create_server(db, s),
        node::Node::AlterForeignServerStmt(s) => fdw::alter_server(db, s),
        node::Node::CreateUserMappingStmt(s) => fdw::create_user_mapping(db, s),
        node::Node::AlterUserMappingStmt(s) => fdw::alter_user_mapping(db, s),
        node::Node::DropUserMappingStmt(s) => fdw::drop_user_mapping(db, s),
        node::Node::ImportForeignSchemaStmt(s) => fdw::import_foreign_schema(db, s),
        node::Node::CreateStatsStmt(s) => statistics::create_statistics(db, s),
        node::Node::AlterStatsStmt(s) => statistics::alter_statistics(db, s),
        node::Node::CreateOpClassStmt(s) => opclass::create_opclass(db, s),
        node::Node::CreateOpFamilyStmt(s) => opclass::create_opfamily(db, s),
        node::Node::AlterOpFamilyStmt(s) => opclass::alter_opfamily(db, s),
        node::Node::TruncateStmt(s) => maintenance::truncate(db, s),
        node::Node::CopyStmt(s) => maintenance::copy(db, s),
        node::Node::ClusterStmt(s) => maintenance::cluster(db, s),
        node::Node::ReindexStmt(s) => maintenance::reindex(db, s),
        node::Node::VacuumStmt(s) => maintenance::vacuum(db, s),
        node::Node::LockStmt(s) => maintenance::lock(db, s),
        node::Node::SecLabelStmt(s) => maintenance::security_label(s),
        node::Node::AlterDefaultPrivilegesStmt(s) => acl::alter_default_privileges(db, s),
        node::Node::ConstraintsSetStmt(s) => session::set_constraints(db, s),
        node::Node::ExplainStmt(s) => session::explain(db, s),
        node::Node::CreateTableSpaceStmt(s) => cluster::create_tablespace(db, s),
        node::Node::DropTableSpaceStmt(s) => cluster::drop_tablespace(db, s),
        node::Node::CreateSubscriptionStmt(s) => cluster::create_subscription(db, s),
        node::Node::AlterSubscriptionStmt(s) => cluster::alter_subscription(db, &s.subname),
        node::Node::DropSubscriptionStmt(s) => cluster::drop_subscription(db, s),
        node::Node::AlterOperatorStmt(s) => operators::alter_operator(db, s),
        node::Node::GrantRoleStmt(_)
        | node::Node::CreateRoleStmt(_)
        | node::Node::AlterRoleStmt(_)
        | node::Node::VariableShowStmt(_)
        | node::Node::NotifyStmt(_)
        | node::Node::ListenStmt(_)
        | node::Node::UnlistenStmt(_)
        // Statements PG accepts in a migration that don't change anything the
        // static analysis reads: DML and procedure calls (like the SELECT /
        // INSERT above), prepared statements and cursors, statistics,
        // database / role / system settings, ownership, publications and
        // subscriptions, event triggers, operator families, text search
        // configuration, foreign-data wrappers / servers / user mappings,
        // tablespaces, conversions, languages, transforms, security labels,
        // and type property changes (`ALTER TYPE t SET (...)`).
        | node::Node::DeclareCursorStmt(_)
        | node::Node::FetchStmt(_)
        | node::Node::ClosePortalStmt(_)
        | node::Node::CheckPointStmt(_)
        | node::Node::LoadStmt(_)
        | node::Node::AlterDatabaseStmt(_)
        | node::Node::AlterDatabaseRefreshCollStmt(_)
        | node::Node::AlterRoleSetStmt(_)
        | node::Node::AlterSystemStmt(_)
        | node::Node::DropRoleStmt(_)
        | node::Node::ReassignOwnedStmt(_)
        | node::Node::DropOwnedStmt(_)
        | node::Node::AlterTableSpaceOptionsStmt(_)
        | node::Node::AlterCollationStmt(_)
        | node::Node::AlterObjectDependsStmt(_) => Ok(()),
        node::Node::AlterTypeStmt(s) => types::alter_type(db, s),
        node::Node::AlterTableMoveAllStmt(s) => tables::alter_table_move_all(db, s),
        node::Node::PrepareStmt(s) => prepared::prepare(db, s),
        node::Node::ExecuteStmt(s) => prepared::execute(db, s),
        node::Node::DeallocateStmt(s) => prepared::deallocate(db, s),
        node::Node::DiscardStmt(s) => prepared::discard(db, s),

        // ── Unknown DDL — surface as an error ───────────────────────
        other => Err(DdlError::UnsupportedDdl(format!("{other:?}"))),
    }
}
