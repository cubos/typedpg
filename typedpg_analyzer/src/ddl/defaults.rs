//! Type-checking of column / domain `DEFAULT` expressions.
//!
//! Mirrors `cookDefault` (`catalog/heap.c`): the expression is transformed
//! in `EXPR_KIND_COLUMN_DEFAULT`, which forbids column references,
//! subqueries, aggregates and window functions, and must then be coercible
//! to the column's type under assignment rules.

use pg_query::protobuf::{self, node};

use super::DdlError;
use crate::coerce::{CoercionContext, can_coerce};
use crate::expr::{TypeGoal, infer_expr};
use crate::nullability::NullabilityContext;
use crate::oid::PgTypeOid;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, ProKind, oid};
use crate::scope::Scope;

/// Check `expr` as the DEFAULT of column (or domain) `name` of type
/// `type_oid`.
pub(crate) fn check_default(
    interp: &PgCatalog,
    expr: &protobuf::Node,
    name: &str,
    type_oid: PgTypeOid,
) -> Result<(), DdlError> {
    check_default_kind(interp, expr)?;

    let scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let ctx = || crate::expr::Ctx::new(&scope, &null_ctx, interp);
    let wrap = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(format!("{e}"));

    let result = infer_expr(expr, ctx(), &mut params, TypeGoal::NONE).map_err(wrap)?;
    if result.type_oid == oid::UNKNOWN {
        // An untyped literal is coerced to the column type, which runs the
        // type's input function (`invalid input syntax for type ...`).
        let mut goal = TypeGoal::assignment(type_oid);
        goal.source_col_name = Some(name.to_owned());
        infer_expr(expr, ctx(), &mut params, goal).map_err(wrap)?;
        return Ok(());
    }
    if !can_coerce(
        result.type_oid,
        type_oid,
        CoercionContext::Assignment,
        interp,
    ) {
        return Err(DdlError::UnsupportedDdl(format!(
            "column \"{name}\" is of type {} but default expression is of type {}",
            super::util::format_type_for_message(interp, type_oid),
            super::util::format_type_for_message(interp, result.type_oid),
        )));
    }
    Ok(())
}

/// The `EXPR_KIND_COLUMN_DEFAULT` restrictions (`transformColumnRef`,
/// `transformSubLink`, `check_agglevels_and_constraints`,
/// `transformWindowFuncCall`).
fn check_default_kind(interp: &PgCatalog, expr: &protobuf::Node) -> Result<(), DdlError> {
    let Some(inner) = expr.node.as_ref() else {
        return Ok(());
    };
    match inner {
        node::Node::ColumnRef(_) => {
            return Err(DdlError::UnsupportedDdl(
                "cannot use column reference in DEFAULT expression".into(),
            ));
        }
        node::Node::SubLink(_) => {
            return Err(DdlError::UnsupportedDdl(
                "cannot use subquery in DEFAULT expression".into(),
            ));
        }
        node::Node::FuncCall(fc) => {
            if fc.over.is_some() {
                return Err(DdlError::UnsupportedDdl(
                    "window functions are not allowed in DEFAULT expressions".into(),
                ));
            }
            let parts: Vec<&str> = fc
                .funcname
                .iter()
                .filter_map(super::util::node_string)
                .collect();
            let (schema, name) = match parts.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => (None, ""),
            };
            let candidates = interp.find_functions(schema, name);
            let aggregate = fc.agg_star
                || (!candidates.is_empty()
                    && candidates
                        .iter()
                        .all(|p| matches!(p.prokind, ProKind::Aggregate)));
            if aggregate {
                return Err(DdlError::UnsupportedDdl(
                    "aggregate functions are not allowed in DEFAULT expressions".into(),
                ));
            }
            for arg in &fc.args {
                check_default_kind(interp, arg)?;
            }
        }
        node::Node::AExpr(e) => {
            for side in [e.lexpr.as_deref(), e.rexpr.as_deref()]
                .into_iter()
                .flatten()
            {
                check_default_kind(interp, side)?;
            }
        }
        node::Node::BoolExpr(b) => {
            for arg in &b.args {
                check_default_kind(interp, arg)?;
            }
        }
        node::Node::TypeCast(tc) => {
            if let Some(arg) = tc.arg.as_deref() {
                check_default_kind(interp, arg)?;
            }
        }
        node::Node::CoalesceExpr(c) => {
            for arg in &c.args {
                check_default_kind(interp, arg)?;
            }
        }
        node::Node::CaseExpr(c) => {
            for part in c.arg.iter().chain(c.defresult.iter()) {
                check_default_kind(interp, part)?;
            }
            for when in &c.args {
                if let Some(node::Node::CaseWhen(w)) = when.node.as_ref() {
                    for part in w.expr.iter().chain(w.result.iter()) {
                        check_default_kind(interp, part)?;
                    }
                }
            }
        }
        node::Node::ArrayExpr(a) => {
            for elem in &a.elements {
                check_default_kind(interp, elem)?;
            }
        }
        node::Node::RowExpr(r) => {
            for arg in &r.args {
                check_default_kind(interp, arg)?;
            }
        }
        _ => {}
    }
    Ok(())
}
