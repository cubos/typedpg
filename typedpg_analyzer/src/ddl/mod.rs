//! DDL interpreter: applies DDL statements to a [`PgCatalog`](crate::PgCatalog)
//! in memory.
//!
//! This module parses SQL migration files using `pg_query` and mutates the
//! snapshot as if the DDL had been executed against a real PostgreSQL instance.

pub mod aggregates;
pub mod alter;
pub mod collations;
pub mod drop;
pub mod extensions;
pub mod functions;
pub mod indexes;
pub mod operators;
pub mod schema_stmt;
pub mod sequences;
pub(crate) mod session;
pub mod tables;
pub mod types;
pub mod util;
pub mod views;
mod volatile;

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

    for raw_stmt in &parsed.protobuf.stmts {
        let Some(stmt) = raw_stmt.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
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

        // ── Types ───────────────────────────────────────────────────
        node::Node::CreateDomainStmt(s) => types::create_domain(db, s),
        node::Node::CreateEnumStmt(s) => types::create_enum(db, s),
        node::Node::CompositeTypeStmt(s) => types::create_composite(db, s),
        node::Node::CreateRangeStmt(s) => types::create_range(db, s),
        node::Node::AlterEnumStmt(s) => types::alter_enum(db, s),

        // ── Drop ────────────────────────────────────────────────────
        node::Node::DropStmt(s) => drop::drop_objects(db, s),

        // ── Schema ──────────────────────────────────────────────────
        node::Node::CreateSchemaStmt(s) => schema_stmt::create_schema(db, s),

        // ── Sequences ───────────────────────────────────────────────
        node::Node::CreateSeqStmt(s) => sequences::create_sequence(db, s),
        node::Node::AlterSeqStmt(s) => sequences::alter_sequence(db, s),

        // ── Functions ───────────────────────────────────────────────
        node::Node::CreateFunctionStmt(s) => functions::create_function(db, s),

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
        node::Node::SelectStmt(s) => session::select_side_effects(db, s),

        // ── Indexes ─────────────────────────────────────────────────
        // Indexes don't change query result types, but expression indexes
        // forbid VOLATILE functions (CREATE INDEX walks the expression
        // tree to detect them).
        node::Node::IndexStmt(s) => indexes::create_index(db, s),

        // ── No-ops (irrelevant for type analysis) ───────────────────
        node::Node::GrantStmt(_)
        | node::Node::CommentStmt(_)
        | node::Node::CreateTrigStmt(_)
        | node::Node::RuleStmt(_)
        | node::Node::ConstraintsSetStmt(_)
        | node::Node::CreatePolicyStmt(_)
        | node::Node::AlterPolicyStmt(_)
        | node::Node::AlterOwnerStmt(_)
        | node::Node::AlterDefaultPrivilegesStmt(_)
        | node::Node::CreateRoleStmt(_)
        | node::Node::AlterRoleStmt(_)
        | node::Node::GrantRoleStmt(_)
        | node::Node::CreateOpClassStmt(_)
        | node::Node::AlterOpFamilyStmt(_)
        | node::Node::AlterOperatorStmt(_)
        | node::Node::InsertStmt(_)
        | node::Node::UpdateStmt(_)
        | node::Node::DeleteStmt(_)
        | node::Node::DoStmt(_)
        | node::Node::TruncateStmt(_)
        | node::Node::CopyStmt(_)
        | node::Node::ClusterStmt(_)
        | node::Node::VacuumStmt(_)
        | node::Node::ReindexStmt(_)
        | node::Node::LockStmt(_)
        | node::Node::VariableShowStmt(_)
        | node::Node::DiscardStmt(_)
        | node::Node::ExplainStmt(_)
        | node::Node::AlterFunctionStmt(_)
        | node::Node::AlterDomainStmt(_)
        | node::Node::NotifyStmt(_)
        | node::Node::ListenStmt(_)
        | node::Node::UnlistenStmt(_)
        | node::Node::AlterExtensionContentsStmt(_)
        | node::Node::CreateAmStmt(_) => Ok(()),

        // ── Unknown DDL — surface as an error ───────────────────────
        other => Err(DdlError::UnsupportedDdl(format!("{other:?}"))),
    }
}
