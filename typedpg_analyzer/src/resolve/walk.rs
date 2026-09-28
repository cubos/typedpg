//! Syntactic walks over raw (pre-analysis) expression trees.
//!
//! PG runs several placement checks over the *transformed* tree
//! (`contain_vars_of_level`, `expression_returns_set`, the SRF / aggregate
//! placement rules in `parse_func.c` / `parse_agg.c`). The analyzer only has
//! the raw parse tree, so these helpers enumerate an expression's direct
//! sub-expressions *at the same query level* — a `SubLink`'s subquery is a
//! separate level and is never entered (only its `testexpr` is).

use super::*;

/// The direct sub-expressions of `node` that belong to the same query level.
pub(crate) fn expr_children(node: &protobuf::Node) -> Vec<&protobuf::Node> {
    let mut out: Vec<&protobuf::Node> = Vec::new();
    let Some(inner) = node.node.as_ref() else {
        return out;
    };
    fn push<'a>(out: &mut Vec<&'a protobuf::Node>, n: &'a Option<Box<protobuf::Node>>) {
        if let Some(n) = n.as_deref() {
            out.push(n);
        }
    }
    match inner {
        node::Node::FuncCall(fc) => {
            out.extend(fc.args.iter());
            out.extend(fc.agg_order.iter());
            if let Some(f) = fc.agg_filter.as_deref() {
                out.push(f);
            }
        }
        node::Node::NamedArgExpr(na) => push(&mut out, &na.arg),
        node::Node::AExpr(e) => {
            push(&mut out, &e.lexpr);
            push(&mut out, &e.rexpr);
        }
        node::Node::BoolExpr(b) => out.extend(b.args.iter()),
        node::Node::NullTest(t) => push(&mut out, &t.arg),
        node::Node::BooleanTest(t) => push(&mut out, &t.arg),
        node::Node::CoalesceExpr(c) => out.extend(c.args.iter()),
        node::Node::MinMaxExpr(m) => out.extend(m.args.iter()),
        node::Node::CaseExpr(c) => {
            push(&mut out, &c.arg);
            out.extend(c.args.iter());
            push(&mut out, &c.defresult);
        }
        node::Node::CaseWhen(w) => {
            push(&mut out, &w.expr);
            push(&mut out, &w.result);
        }
        node::Node::TypeCast(c) => push(&mut out, &c.arg),
        node::Node::CollateClause(c) => push(&mut out, &c.arg),
        node::Node::AIndirection(i) => {
            push(&mut out, &i.arg);
            out.extend(i.indirection.iter());
        }
        node::Node::AIndices(i) => {
            push(&mut out, &i.lidx);
            push(&mut out, &i.uidx);
        }
        node::Node::AArrayExpr(a) => out.extend(a.elements.iter()),
        node::Node::RowExpr(r) => out.extend(r.args.iter()),
        node::Node::List(l) => out.extend(l.items.iter()),
        node::Node::SortBy(s) => push(&mut out, &s.node),
        node::Node::ResTarget(rt) => push(&mut out, &rt.val),
        node::Node::GroupingFunc(g) => out.extend(g.args.iter()),
        node::Node::GroupingSet(g) => out.extend(g.content.iter()),
        node::Node::SubLink(sl) => push(&mut out, &sl.testexpr),
        node::Node::XmlExpr(x) => {
            out.extend(x.named_args.iter());
            out.extend(x.args.iter());
        }
        node::Node::MultiAssignRef(m) => push(&mut out, &m.source),
        _ => {}
    }
    out
}

/// Pre-order visit of `node` and every same-level sub-expression.
pub(crate) fn visit_same_level<'a>(
    node: &'a protobuf::Node,
    f: &mut dyn FnMut(&'a protobuf::Node),
) {
    f(node);
    for child in expr_children(node) {
        visit_same_level(child, f);
    }
}

/// True when `fc` names a set-returning function. Decided by name like the
/// aggregate probe in `expr::detect_func_kinds`: any visible overload with
/// `proretset` counts, since the raw tree carries no resolved signature.
pub(crate) fn is_srf_call(fc: &protobuf::FuncCall, snapshot: &PgCatalog) -> bool {
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return false,
    };
    fc.over.is_none()
        && snapshot
            .find_functions(schema, name)
            .iter()
            .any(|p| p.proretset)
}

/// Number of set-returning function calls in `nodes` at this query level.
pub(crate) fn count_srf_calls(nodes: &[protobuf::Node], snapshot: &PgCatalog) -> usize {
    let mut n = 0;
    for node in nodes {
        visit_same_level(node, &mut |e| {
            if let Some(node::Node::FuncCall(fc)) = e.node.as_ref()
                && is_srf_call(fc, snapshot)
            {
                n += 1;
            }
        });
    }
    n
}
