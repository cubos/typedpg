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

/// [`check_statement`], returning the statement's result columns when the
/// analysis could tell them.
pub(crate) fn analyze_statement(
    interp: &PgCatalog,
    stmt: &node::Node,
) -> Result<Option<Vec<crate::resolve::RawColumn>>, DdlError> {
    match crate::resolve::analyze_raw_node(interp, stmt, &[]) {
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
