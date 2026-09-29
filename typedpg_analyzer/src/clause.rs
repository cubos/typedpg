//! Clause-context tracking — the analyzer's analogue of PostgreSQL's
//! `ParseExprKind`.
//!
//! Every SQL position that coerces an expression to a specific type carries
//! a [`ClauseKind`]: it owns the PG-verbatim wording (`argument of WHERE
//! must be type boolean, not type X`), the coercion target, and whether
//! aggregate/window calls are forbidden there. [`coerce_clause_expr`] is the
//! one walker implementing PG's error ordering for all of them:
//!
//! 1. bottom-up resolution failures win (`function … does not exist` from
//!    inside the expression beats any clause-level complaint);
//! 2. the aggregate/window placement rule fires next (`aggregate functions
//!    are not allowed in WHERE` outranks the boolean complaint for
//!    `WHERE min(id)`);
//! 3. the clause's own coercion wording comes last.
//!
//! Before this module the rewrite existed in six hand-rolled copies (WHERE/
//! HAVING/JOIN ON, LIMIT/OFFSET, FILTER, CASE/WHEN, NOT/AND/OR, IS TRUE…) —
//! each a chance to diverge on ordering or wording.

use typedpg_pg_query::protobuf;

use crate::error::AnalyzeError;
use crate::expr::{self, Ctx, TypeGoal};
use crate::oid::PgTypeOid;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, oid};

/// The clause (or clause-like construct) coercing an expression. Mirrors the
/// distinctions PG's `ParseExprKind` draws for error wording and placement
/// rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClauseKind {
    Where,
    Having,
    JoinOn,
    Filter,
    Limit,
    Offset,
    /// A searched CASE's WHEN condition.
    CaseWhen,
    /// One operand of NOT / AND / OR (PG names the operator in the message).
    Not,
    And,
    Or,
    /// `IS [NOT] TRUE/FALSE/UNKNOWN` — the payload is PG's spelling of the
    /// test (`"IS TRUE"`, …).
    BoolTest(&'static str),
}

impl ClauseKind {
    /// PG's name for the construct, as it appears in `argument of {label}
    /// must be type …`.
    fn label(self) -> &'static str {
        match self {
            ClauseKind::Where => "WHERE",
            ClauseKind::Having => "HAVING",
            ClauseKind::JoinOn => "JOIN/ON",
            ClauseKind::Filter => "FILTER",
            ClauseKind::Limit => "LIMIT",
            ClauseKind::Offset => "OFFSET",
            ClauseKind::CaseWhen => "CASE/WHEN",
            ClauseKind::Not => "NOT",
            ClauseKind::And => "AND",
            ClauseKind::Or => "OR",
            ClauseKind::BoolTest(label) => label,
        }
    }

    /// The type the clause coerces its expression to, with PG's name for it
    /// in the message.
    fn expected(self) -> (PgTypeOid, &'static str) {
        match self {
            ClauseKind::Limit | ClauseKind::Offset => (oid::INT8, "bigint"),
            _ => (oid::BOOL, "boolean"),
        }
    }

    /// `Some(context)` when PG forbids aggregate/window calls in this
    /// position (`aggregate functions are not allowed in {context}`). The
    /// expression-level kinds (CASE/WHEN, NOT, …) return `None`: the
    /// enclosing clause owns that rule.
    fn aggregate_context(self) -> Option<&'static str> {
        match self {
            ClauseKind::Where => Some("WHERE"),
            // PG's ParseExprKindName for EXPR_KIND_JOIN_ON.
            ClauseKind::JoinOn => Some("JOIN conditions"),
            ClauseKind::Limit => Some("LIMIT"),
            ClauseKind::Offset => Some("OFFSET"),
            _ => None,
        }
    }

    /// Whether the diagnostic carries a caret label under the offending
    /// expression. (LIMIT/OFFSET historically render bare; the boolean
    /// clauses annotate.)
    fn caret_label(self) -> bool {
        !matches!(self, ClauseKind::Limit | ClauseKind::Offset)
    }
}

/// Coerce a clause expression to the kind's expected type with PG's error
/// ordering and wording. See the module docs for the three-step order.
pub(crate) fn coerce_clause_expr(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    kind: ClauseKind,
) -> Result<expr::ExprType, AnalyzeError> {
    let (goal_oid, goal_name) = kind.expected();
    let inferred = expr::infer_expr(node, ctx, params, TypeGoal::assignment(goal_oid));
    let e = match inferred {
        Ok(t) => {
            if let Some(c) = kind.aggregate_context() {
                check_no_aggregates_or_windows(node, ctx.snapshot, c)?;
            }
            return Ok(t);
        }
        Err(e) => e,
    };
    if !matches!(e, AnalyzeError::TypeMismatch { .. }) {
        return Err(e);
    }
    // The expression resolved but isn't the expected type — placement still
    // outranks the coercion complaint (`WHERE min(id)` is "aggregate
    // functions are not allowed in WHERE", not "argument of WHERE…").
    if let Some(c) = kind.aggregate_context() {
        check_no_aggregates_or_windows(node, ctx.snapshot, c)?;
    }
    // Re-infer with no goal (on a scratch collector) to learn the actual
    // type for the message.
    let mut params2 = params.clone();
    let actual_oid = expr::infer_expr(node, ctx, &mut params2, TypeGoal::NONE)
        .map(|t| t.type_oid)
        .unwrap_or(oid::UNKNOWN);
    let actual_pg = crate::ddl::util::format_type_for_message(ctx.snapshot, actual_oid);
    let span =
        crate::error::node_location(node).and_then(crate::error::SourceSpan::from_node_qname);
    let raw = crate::error::RawError::new(
        AnalyzeError::DatatypeMismatch(format!(
            "argument of {} must be type {goal_name}, not type {actual_pg}",
            kind.label()
        )),
        span,
        None,
    );
    let raw = if kind.caret_label() {
        raw.with_primary_label(format!("this is {actual_pg}, expected {goal_name}"))
    } else {
        raw
    };
    Err(raw.finalize_implicit())
}

/// Reject aggregate / window function calls in a context where PG forbids
/// them. Matches PG's `aggregate functions are not allowed in WHERE` /
/// `window functions are not allowed in WHERE` errors. `context` goes into
/// the error message (e.g. `"WHERE"`, `"GROUP BY"`, `"JOIN/ON"`).
pub(crate) fn check_no_aggregates_or_windows(
    node: &protobuf::Node,
    snapshot: &PgCatalog,
    context: &str,
) -> Result<(), AnalyzeError> {
    let kinds = expr::detect_func_kinds(node, snapshot);
    if kinds.has_aggregate {
        let span = kinds
            .agg_location
            .and_then(crate::error::SourceSpan::from_node_qname);
        // The classic fix for an aggregate in WHERE is a HAVING clause; for the
        // other clauses there's no single rewrite, so just point at the call.
        let hint = (context == "WHERE")
            .then(|| "to filter on an aggregate, use a HAVING clause instead of WHERE".to_string());
        return Err(crate::error::RawError::new(
            AnalyzeError::GroupingError(format!(
                "aggregate functions are not allowed in {context}"
            )),
            span,
            hint,
        )
        .with_primary_label("aggregate not allowed here")
        .finalize_implicit());
    }
    if kinds.has_grouping {
        let span = kinds
            .grouping_location
            .and_then(crate::error::SourceSpan::from_node_qname);
        return Err(crate::error::RawError::new(
            AnalyzeError::GroupingError(format!(
                "grouping operations are not allowed in {context}"
            )),
            span,
            None,
        )
        .with_primary_label("GROUPING not allowed here")
        .finalize_implicit());
    }
    if kinds.has_window {
        let span = kinds
            .window_location
            .and_then(crate::error::SourceSpan::from_node_qname);
        return Err(crate::error::RawError::new(
            AnalyzeError::WindowingError(format!("window functions are not allowed in {context}")),
            span,
            None,
        )
        .with_primary_label("window function not allowed here")
        .finalize_implicit());
    }
    Ok(())
}
