//! Placement rules of the DDL expression kinds. PG transforms CHECK,
//! index, generation and policy expressions under a `ParseExprKind` that
//! forbids some constructs: set-returning functions
//! (`check_srf_call_placement`), aggregates
//! (`check_agglevels_and_constraints`), window functions
//! (`transformWindowFuncCall`) and — except in policies — sub-selects
//! (`transformSubLink`).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf;

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
        }
    }
}

/// Reject the constructs `kind` forbids anywhere in `expr`.
pub(crate) fn check_expr_kind(
    interp: &PgCatalog,
    expr: &protobuf::Node,
    kind: ExprKind,
) -> Result<(), DdlError> {
    if let Some(context) = kind.subquery_context()
        && let Some(inner) = expr.node.as_ref()
        && (matches!(inner, protobuf::node::Node::SubLink(_))
            || inner
                .nodes()
                .into_iter()
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
        .map_err(|e| DdlError::UnsupportedDdl(e.to_string()))
}
