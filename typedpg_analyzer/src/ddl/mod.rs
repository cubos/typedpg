//! DDL interpreter: applies DDL statements to a [`PgCatalog`](crate::PgCatalog)
//! in memory.
//!
//! This module parses SQL migration files using `pg_query` and mutates the
//! snapshot as if the DDL had been executed against a real PostgreSQL instance.

mod acl;
pub mod aggregates;
pub mod alter;
pub mod collations;
mod comment;
mod defaults;
mod dml;
pub mod drop;
mod expr_kind;
pub mod extensions;
pub(crate) mod function_body;
pub mod functions;
pub mod indexes;
mod maintenance;
mod opclass;
pub mod operators;
mod policies;
mod rules;
pub mod schema_stmt;
pub mod sequences;
pub(crate) mod session;
pub(crate) mod statistics;
pub mod tables;
pub(crate) mod triggers;
mod txblock;
pub mod types;
pub mod util;
pub mod views;
pub(crate) mod volatile;

#[cfg(any(test, feature = "internal"))]
pub(crate) use views::serialize_subnode;

use pg_query::protobuf::node;

use crate::pg_catalog::PgCatalog;

// ─── Error type ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DdlError {
    Parse(String),
    Migration {
        filename: String,
        source: Box<DdlError>,
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
            DdlError::Migration { filename, source } => {
                write!(f, "in migration '{filename}': {source}")
            }
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

/// Parse and apply all DDL statements in a SQL string.
pub(crate) fn apply_sql_to(db: &mut PgCatalog, sql: &str) -> Result<(), DdlError> {
    let parsed = pg_query::parse(sql).map_err(|e| DdlError::Parse(e.to_string()))?;
    let tx = txblock::TxContext::of(sql, &parsed.protobuf.stmts);

    for raw_stmt in &parsed.protobuf.stmts {
        let Some(stmt) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        tx.check(stmt)?;
        apply_statement(db, stmt)?;
    }

    Ok(())
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
            use pg_query::protobuf::ObjectType;
            match ObjectType::try_from(s.kind).unwrap_or(ObjectType::Undefined) {
                ObjectType::ObjectType => types::define_type(db, s),
                ObjectType::ObjectOperator => operators::define_operator(db, s),
                ObjectType::ObjectAggregate => aggregates::define_aggregate(db, s),
                ObjectType::ObjectCollation => collations::define_collation(db, s),
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
        node::Node::CreateTrigStmt(s) => triggers::create_trigger(db, s),
        node::Node::GrantStmt(s) => acl::grant(db, s),
        node::Node::CreatePolicyStmt(s) => policies::create_policy(db, s),
        node::Node::AlterPolicyStmt(s) => policies::alter_policy(db, s),
        node::Node::RuleStmt(s) => rules::create_rule(db, s),
        node::Node::CreateAmStmt(s) => opclass::create_am(db, s),
        node::Node::CreateStatsStmt(s) => statistics::create_statistics(db, s),
        node::Node::AlterStatsStmt(s) => statistics::alter_statistics(db, s),
        node::Node::CreateOpClassStmt(s) => opclass::create_opclass(db, s),
        node::Node::CreateOpFamilyStmt(s) => opclass::create_opfamily(db, s),
        node::Node::TruncateStmt(s) => maintenance::truncate(db, s),
        node::Node::ClusterStmt(s) => maintenance::cluster(db, s),
        node::Node::ReindexStmt(s) => maintenance::reindex(db, s),
        node::Node::VacuumStmt(s) => maintenance::vacuum(db, s),
        node::Node::LockStmt(s) => maintenance::lock(db, s),
        node::Node::SecLabelStmt(s) => maintenance::security_label(s),
        node::Node::AlterDefaultPrivilegesStmt(s) => maintenance::alter_default_privileges(db, s),
        node::Node::GrantRoleStmt(_)
        | node::Node::ConstraintsSetStmt(_)
        | node::Node::CreateRoleStmt(_)
        | node::Node::AlterRoleStmt(_)
        | node::Node::AlterOpFamilyStmt(_)
        | node::Node::AlterOperatorStmt(_)
        | node::Node::DoStmt(_)
        | node::Node::CopyStmt(_)
        | node::Node::VariableShowStmt(_)
        | node::Node::DiscardStmt(_)
        | node::Node::ExplainStmt(_)
        | node::Node::NotifyStmt(_)
        | node::Node::ListenStmt(_)
        | node::Node::UnlistenStmt(_)
        | node::Node::AlterExtensionContentsStmt(_)
        // Statements PG accepts in a migration that don't change anything the
        // static analysis reads: DML and procedure calls (like the SELECT /
        // INSERT above), prepared statements and cursors, statistics,
        // database / role / system settings, ownership, publications and
        // subscriptions, event triggers, operator families, text search
        // configuration, foreign-data wrappers / servers / user mappings,
        // tablespaces, conversions, languages, transforms, security labels,
        // and type property changes (`ALTER TYPE t SET (...)`).
        | node::Node::PrepareStmt(_)
        | node::Node::ExecuteStmt(_)
        | node::Node::DeallocateStmt(_)
        | node::Node::DeclareCursorStmt(_)
        | node::Node::FetchStmt(_)
        | node::Node::ClosePortalStmt(_)
        | node::Node::CheckPointStmt(_)
        | node::Node::LoadStmt(_)
        | node::Node::AlterDatabaseStmt(_)
        | node::Node::AlterDatabaseSetStmt(_)
        | node::Node::AlterDatabaseRefreshCollStmt(_)
        | node::Node::AlterRoleSetStmt(_)
        | node::Node::AlterSystemStmt(_)
        | node::Node::DropRoleStmt(_)
        | node::Node::ReassignOwnedStmt(_)
        | node::Node::DropOwnedStmt(_)
        | node::Node::CreateEventTrigStmt(_)
        | node::Node::AlterEventTrigStmt(_)
        | node::Node::CreatePublicationStmt(_)
        | node::Node::AlterPublicationStmt(_)
        | node::Node::CreateSubscriptionStmt(_)
        | node::Node::AlterSubscriptionStmt(_)
        | node::Node::DropSubscriptionStmt(_)
        | node::Node::AlterTsconfigurationStmt(_)
        | node::Node::AlterTsdictionaryStmt(_)
        | node::Node::CreateFdwStmt(_)
        | node::Node::AlterFdwStmt(_)
        | node::Node::CreateForeignServerStmt(_)
        | node::Node::AlterForeignServerStmt(_)
        | node::Node::CreateUserMappingStmt(_)
        | node::Node::AlterUserMappingStmt(_)
        | node::Node::DropUserMappingStmt(_)
        | node::Node::ImportForeignSchemaStmt(_)
        | node::Node::CreateTableSpaceStmt(_)
        | node::Node::DropTableSpaceStmt(_)
        | node::Node::AlterTableSpaceOptionsStmt(_)
        | node::Node::CreateConversionStmt(_)
        | node::Node::CreatePlangStmt(_)
        | node::Node::CreateTransformStmt(_)
        | node::Node::AlterCollationStmt(_)
        | node::Node::AlterObjectDependsStmt(_) => Ok(()),
        node::Node::AlterTypeStmt(s) => types::alter_type(db, s),

        // ── Unknown DDL — surface as an error ───────────────────────
        other => Err(DdlError::UnsupportedDdl(format!("{other:?}"))),
    }
}
