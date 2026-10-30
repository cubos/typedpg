//! Queries in a migration (SELECT, INSERT, UPDATE, DELETE, MERGE, CALL).
//! They don't change the schema, but PG parses and analyzes them against
//! it (`parse_analyze`), so a data migration naming a missing table or
//! column, or mistyping a value, fails.

use typedpg_pg_query::protobuf::node;

use super::DdlError;
use crate::error::AnalyzeError;
use crate::pg_catalog::PgCatalog;

/// Analyze `stmt` like the `sql!` macro would, failing the migration on
/// what PG's parse analysis rejects. Analyzer limitations
/// (`Unsupported`, internal errors) leave the statement unchecked.
pub(crate) fn check_statement(interp: &PgCatalog, stmt: &node::Node) -> Result<(), DdlError> {
    analyze_statement(interp, stmt).map(drop)
}

/// [`check_statement`] for a statement the migration runs — planned and
/// executed, not only analyzed: what the planner and the executor's start
/// raise on every execution fails it too, as it fails an application
/// query ([`crate::resolve::analyze_executed`]).
pub(crate) fn check_executed(interp: &PgCatalog, stmt: &node::Node) -> Result<(), DdlError> {
    verdict(crate::resolve::analyze_executed(interp, stmt, || {
        crate::resolve::analyze_raw_node(interp, stmt, &[])
    }))
    .map(drop)
}

/// [`check_statement`], returning the statement's result columns when the
/// analysis could tell them. The statement runs: it is rewritten too.
pub(crate) fn analyze_statement(
    interp: &PgCatalog,
    stmt: &node::Node,
) -> Result<Option<Vec<crate::resolve::RawColumn>>, DdlError> {
    crate::expr::with_rewriting(true, || analyze_parsed(interp, stmt))
}

/// [`analyze_statement`] for a statement only stored (a rule's action):
/// parse analysis alone, the rewriter runs once something fires it.
pub(crate) fn analyze_stored_statement(
    interp: &PgCatalog,
    stmt: &node::Node,
) -> Result<Option<Vec<crate::resolve::RawColumn>>, DdlError> {
    crate::expr::with_rewriting(false, || analyze_parsed(interp, stmt))
}

fn analyze_parsed(
    interp: &PgCatalog,
    stmt: &node::Node,
) -> Result<Option<Vec<crate::resolve::RawColumn>>, DdlError> {
    verdict(crate::resolve::analyze_raw_node(interp, stmt, &[]))
}

/// The migration's verdict on an analysis: PG's errors fail it, the
/// analyzer's own gaps leave the statement unchecked (`Ok(None)`).
fn verdict(
    analyzed: Result<
        (
            Vec<crate::resolve::RawColumn>,
            Vec<crate::resolve::RawParam>,
        ),
        AnalyzeError,
    >,
) -> Result<Option<Vec<crate::resolve::RawColumn>>, DdlError> {
    match analyzed {
        Ok((columns, _)) => Ok(Some(columns)),
        Err(
            AnalyzeError::Unsupported(_)
            | AnalyzeError::UnsupportedJoinType(_)
            | AnalyzeError::Internal(_)
            | AnalyzeError::Lex(_)
            | AnalyzeError::Serde(_)
            | AnalyzeError::Io(_),
        ) => Ok(None),
        Err(e) => Err(DdlError::UnsupportedDdl(e.to_string())),
    }
}
