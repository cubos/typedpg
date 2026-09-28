//! Volatility check for DDL expressions.
//!
//! PostgreSQL forbids `VOLATILE` (and in some contexts also `STABLE`)
//! functions inside `CHECK` constraints, `GENERATED ... STORED`
//! expressions, and index expressions — the constraint or generated
//! value would otherwise return different results between rows or
//! between scans, breaking the on-disk invariants those features rely
//! on.
//!
//! This module walks the expression AST, resolves each `FuncCall` to a
//! `pg_proc` row in the catalog, and rejects callees whose
//! `provolatile` is `Volatile`. STABLE callees are accepted today —
//! PG actually rejects them too in CHECK / GENERATED / index, but we
//! treat that gap as a separate todo.

use pg_query::protobuf::{self, node};

use super::DdlError;
use crate::pg_catalog::{PgCatalog, ProVolatile};

/// Returns the qualified name (`[schema, name]` or `[name]`) of a
/// `FuncCall.funcname`. Identifiers are positional `String` nodes.
fn funcname_parts(funcname: &[protobuf::Node]) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = funcname
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [name] => Some((None, *name)),
        [schema, name] => Some((Some(*schema), *name)),
        _ => None,
    }
}

/// Returns the volatility of a `FuncCall` by resolving it against
/// `pg_proc` (any overload — provolatile is per-name in our scope).
/// `None` if the call doesn't resolve to a known function.
fn funcall_volatility(fc: &protobuf::FuncCall, snapshot: &PgCatalog) -> Option<ProVolatile> {
    let (schema, name) = funcname_parts(&fc.funcname)?;
    snapshot
        .find_functions(schema, name)
        .into_iter()
        .next()
        .map(|p| p.provolatile)
}

/// Walk `node` and return `Err` if any `FuncCall` resolves to a function
/// marked `VOLATILE`. `location` selects PG's wording for that context.
pub(super) fn check_no_volatile(
    node: &protobuf::Node,
    location: ExprLocation,
    snapshot: &PgCatalog,
) -> Result<(), DdlError> {
    walk(node, location, snapshot)
}

#[derive(Clone, Copy)]
pub(super) enum ExprLocation {
    Generated,
    Index,
}

impl ExprLocation {
    fn error(self, fname: &str) -> DdlError {
        match self {
            ExprLocation::Generated => DdlError::UnsupportedDdl(format!(
                "generation expression is not immutable: \
                 function \"{fname}\" must be marked IMMUTABLE"
            )),
            ExprLocation::Index => DdlError::UnsupportedDdl(format!(
                "functions in index expression must be marked IMMUTABLE \
                 (function \"{fname}\" is not)"
            )),
        }
    }
}

fn walk(node: &protobuf::Node, loc: ExprLocation, snapshot: &PgCatalog) -> Result<(), DdlError> {
    let Some(inner) = node.node.as_ref() else {
        return Ok(());
    };
    match inner {
        node::Node::FuncCall(fc) => {
            if matches!(
                funcall_volatility(fc, snapshot),
                Some(ProVolatile::Volatile)
            ) {
                let name = funcname_parts(&fc.funcname)
                    .map(|(_, n)| n.to_owned())
                    .unwrap_or_else(|| "<unknown>".into());
                return Err(loc.error(&name));
            }
            for arg in &fc.args {
                walk(arg, loc, snapshot)?;
            }
            if let Some(filter) = fc.agg_filter.as_deref() {
                walk(filter, loc, snapshot)?;
            }
        }
        node::Node::AExpr(e) => {
            if let Some(l) = e.lexpr.as_deref() {
                walk(l, loc, snapshot)?;
            }
            if let Some(r) = e.rexpr.as_deref() {
                walk(r, loc, snapshot)?;
            }
        }
        node::Node::BoolExpr(b) => {
            for arg in &b.args {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::TypeCast(tc) => {
            if let Some(arg) = tc.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::NamedArgExpr(na) => {
            if let Some(arg) = na.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::CollateClause(cc) => {
            if let Some(arg) = cc.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::CoalesceExpr(c) => {
            for a in &c.args {
                walk(a, loc, snapshot)?;
            }
        }
        node::Node::MinMaxExpr(m) => {
            for a in &m.args {
                walk(a, loc, snapshot)?;
            }
        }
        node::Node::NullIfExpr(n) => {
            for a in &n.args {
                walk(a, loc, snapshot)?;
            }
        }
        node::Node::CaseExpr(c) => {
            if let Some(arg) = c.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
            for w in &c.args {
                walk(w, loc, snapshot)?;
            }
            if let Some(d) = c.defresult.as_deref() {
                walk(d, loc, snapshot)?;
            }
        }
        node::Node::CaseWhen(cw) => {
            if let Some(e) = cw.expr.as_deref() {
                walk(e, loc, snapshot)?;
            }
            if let Some(r) = cw.result.as_deref() {
                walk(r, loc, snapshot)?;
            }
        }
        node::Node::List(l) => {
            for item in &l.items {
                walk(item, loc, snapshot)?;
            }
        }
        node::Node::SubLink(sl) => {
            if let Some(testexpr) = sl.testexpr.as_deref() {
                walk(testexpr, loc, snapshot)?;
            }
            // We don't descend into subselects — PG's IMMUTABLE check
            // disallows them entirely in CHECK / GENERATED / index, but
            // we leave that as a separate gap.
        }
        node::Node::AArrayExpr(arr) => {
            for e in &arr.elements {
                walk(e, loc, snapshot)?;
            }
        }
        node::Node::RowExpr(r) => {
            for a in &r.args {
                walk(a, loc, snapshot)?;
            }
        }
        // Leaf-like nodes carry no sub-expression — nothing to walk.
        _ => {}
    }
    Ok(())
}
