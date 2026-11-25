//! Placement rules of the DDL expression kinds. PG transforms CHECK,
//! index, generation and policy expressions under a `ParseExprKind` that
//! forbids some constructs: set-returning functions
//! (`check_srf_call_placement`), aggregates
//! (`check_agglevels_and_constraints`), window functions
//! (`transformWindowFuncCall`) and — except in policies — sub-selects
//! (`transformSubLink`).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast;

use super::DdlError;
use crate::pg_catalog::PgCatalog;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExprKind {
    /// EXPR_KIND_CHECK_CONSTRAINT / EXPR_KIND_DOMAIN_CHECK.
    CheckConstraint,
    /// EXPR_KIND_INDEX_EXPRESSION.
    IndexExpression,
    /// EXPR_KIND_INDEX_PREDICATE.
    IndexPredicate,
    /// EXPR_KIND_GENERATED_COLUMN.
    GeneratedColumn,
    /// EXPR_KIND_POLICY.
    Policy,
    /// EXPR_KIND_PARTITION_BOUND.
    PartitionBound,
    /// EXPR_KIND_PARTITION_EXPRESSION.
    PartitionExpression,
    /// EXPR_KIND_TRIGGER_WHEN.
    TriggerWhen,
    /// EXPR_KIND_ALTER_COL_TRANSFORM: ALTER COLUMN TYPE's USING.
    AlterColTransform,
    /// EXPR_KIND_COPY_WHERE: COPY FROM's WHERE.
    CopyWhere,
}

impl ExprKind {
    /// `ParseExprKindName`, as used in "aggregate functions are not
    /// allowed in {…}".
    fn name(self) -> &'static str {
        match self {
            ExprKind::CheckConstraint => "check constraints",
            ExprKind::IndexExpression => "index expressions",
            ExprKind::IndexPredicate => "index predicates",
            ExprKind::GeneratedColumn => "column generation expressions",
            ExprKind::Policy => "policy expressions",
            ExprKind::PartitionBound => "partition bound",
            ExprKind::PartitionExpression => "partition key expressions",
            ExprKind::TriggerWhen => "trigger WHEN conditions",
            ExprKind::AlterColTransform => "transform expressions",
            ExprKind::CopyWhere => "COPY FROM WHERE conditions",
        }
    }

    /// transformSubLink's "cannot use subquery in {…}", or `None` where
    /// sub-selects are allowed.
    fn subquery_context(self) -> Option<&'static str> {
        match self {
            ExprKind::CheckConstraint => Some("check constraint"),
            ExprKind::IndexExpression => Some("index expression"),
            ExprKind::IndexPredicate => Some("index predicate"),
            ExprKind::GeneratedColumn => Some("column generation expression"),
            ExprKind::Policy => None,
            ExprKind::PartitionBound => Some("partition bound"),
            ExprKind::PartitionExpression => Some("partition key expression"),
            ExprKind::TriggerWhen => Some("trigger WHEN condition"),
            ExprKind::AlterColTransform => Some("transform expression"),
            ExprKind::CopyWhere => Some("COPY FROM WHERE condition"),
        }
    }
}

/// Reject the constructs `kind` forbids anywhere in `expr`.
pub(crate) fn check_expr_kind(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    kind: ExprKind,
) -> Result<(), DdlError> {
    if let Some(context) = kind.subquery_context()
        && let Some(inner) = expr
        && (matches!(inner, ast::Node::SubLink(_))
            || inner
                .nodes()
                .any(|(n, ..)| matches!(n, NodeRef::SubLink(_))))
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot use subquery in {context}"
        )));
    }
    // check_srf_call_placement: none of these kinds admits a set-returning
    // function (`set-returning functions are not allowed in {…}`).
    crate::resolve::check_no_srf_in_clause(expr, interp, kind.name())
        .map_err(|e| DdlError::UnsupportedDdl(e.to_string()))?;
    crate::clause::check_no_aggregates_or_windows(expr, interp, kind.name())
        .map_err(|e| DdlError::UnsupportedDdl(e.to_string()))?;
    reject_parameters(expr)
}

/// transformParamRef with no parameter hook: an expression a DDL statement
/// stores (a default, a constraint, an index expression, ...) has no `$n`.
pub(crate) fn reject_parameters(expr: Option<&ast::Node>) -> Result<(), DdlError> {
    let Some(expr) = expr else {
        return Ok(());
    };
    let number = match expr {
        ast::Node::ParamRef(p) => Some(p.number),
        _ => expr.nodes().find_map(|(n, ..)| match n {
            NodeRef::ParamRef(p) => Some(p.number),
            _ => None,
        }),
    };
    match number {
        Some(n) => Err(DdlError::Parse(format!("there is no parameter ${n}"))),
        None => Ok(()),
    }
}
