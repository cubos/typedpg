//! Syntactic walks over raw (pre-analysis) expression trees.
//!
//! PG runs several placement checks over the *transformed* tree
//! (`contain_vars_of_level`, `expression_returns_set`, the SRF / aggregate
//! placement rules in `parse_func.c` / `parse_agg.c`). The analyzer only has
//! the raw parse tree, so these helpers enumerate an expression's direct
//! sub-expressions *at the same query level* — a `SubLink`'s subquery is a
//! separate level and is never entered (only its `testexpr` is).

use super::*;
use typedpg_pg_query::{NodeBox, ast};

/// The direct sub-expressions of `node` that belong to the same query level.
pub(crate) fn expr_children(node: &Option<ast::Node>) -> Vec<&Option<ast::Node>> {
    let mut out: Vec<&Option<ast::Node>> = Vec::new();
    let Some(inner) = node.as_ref() else {
        return out;
    };
    fn push<'a>(out: &mut Vec<&'a Option<ast::Node>>, n: &'a Option<ast::Node>) {
        if n.is_some() {
            out.push(n);
        }
    }
    match inner {
        ast::Node::FuncCall(fc) => {
            out.extend(fc.args.iter());
            out.extend(fc.agg_order.iter());
            push(&mut out, &fc.agg_filter);
        }
        ast::Node::NamedArgExpr(na) => push(&mut out, &na.arg),
        ast::Node::AExpr(e) => {
            push(&mut out, &e.lexpr);
            push(&mut out, &e.rexpr);
        }
        ast::Node::BoolExpr(b) => out.extend(b.args.iter()),
        ast::Node::NullTest(t) => push(&mut out, &t.arg),
        ast::Node::BooleanTest(t) => push(&mut out, &t.arg),
        ast::Node::CoalesceExpr(c) => out.extend(c.args.iter()),
        ast::Node::MinMaxExpr(m) => out.extend(m.args.iter()),
        ast::Node::CaseExpr(c) => {
            push(&mut out, &c.arg);
            out.extend(c.args.iter());
            push(&mut out, &c.defresult);
        }
        ast::Node::CaseWhen(w) => {
            push(&mut out, &w.expr);
            push(&mut out, &w.result);
        }
        ast::Node::TypeCast(c) => push(&mut out, &c.arg),
        ast::Node::CollateClause(c) => push(&mut out, &c.arg),
        ast::Node::AIndirection(i) => {
            push(&mut out, &i.arg);
            out.extend(i.indirection.iter());
        }
        ast::Node::AIndices(i) => {
            push(&mut out, &i.lidx);
            push(&mut out, &i.uidx);
        }
        ast::Node::AArrayExpr(a) => out.extend(a.elements.iter()),
        ast::Node::RowExpr(r) => out.extend(r.args.iter()),
        ast::Node::List(l) => out.extend(l.items.iter()),
        ast::Node::SortBy(s) => push(&mut out, &s.node),
        ast::Node::ResTarget(rt) => push(&mut out, &rt.val),
        ast::Node::GroupingFunc(g) => out.extend(g.args.iter()),
        ast::Node::GroupingSet(g) => out.extend(g.content.iter()),
        ast::Node::SubLink(sl) => push(&mut out, &sl.testexpr),
        ast::Node::XmlExpr(x) => {
            out.extend(x.named_args.iter());
            out.extend(x.args.iter());
        }
        ast::Node::MultiAssignRef(m) => push(&mut out, &m.source),
        ast::Node::XmlSerialize(x) => push(&mut out, &x.expr),
        // SQL/JSON constructors: a `JsonValueExpr` wraps its raw expression.
        ast::Node::JsonValueExpr(v) => push(&mut out, &v.raw_expr),
        ast::Node::JsonKeyValue(kv) => {
            push(&mut out, &kv.key);
            push_json_value(&mut out, &kv.value);
        }
        ast::Node::JsonObjectConstructor(c) => out.extend(c.exprs.iter()),
        ast::Node::JsonArrayConstructor(c) => out.extend(c.exprs.iter()),
        ast::Node::JsonScalarExpr(s) => push(&mut out, &s.expr),
        ast::Node::JsonSerializeExpr(s) => push_json_value(&mut out, &s.expr),
        ast::Node::JsonParseExpr(p) => push_json_value(&mut out, &p.expr),
        ast::Node::JsonIsPredicate(p) => push(&mut out, &p.expr),
        ast::Node::JsonFuncExpr(f) => {
            push_json_value(&mut out, &f.context_item);
            push(&mut out, &f.pathspec);
            for p in &f.passing {
                if let Some(ast::Node::JsonArgument(a)) = p.as_ref() {
                    push_json_value(&mut out, &a.val);
                }
            }
            for b in [&f.on_empty, &f.on_error].into_iter().flatten() {
                push(&mut out, &b.expr);
            }
        }
        ast::Node::JsonObjectAgg(a) => {
            if let Some(kv) = a.arg.as_deref() {
                push(&mut out, &kv.key);
                push_json_value(&mut out, &kv.value);
            }
            push_json_agg(&mut out, &a.constructor);
        }
        ast::Node::JsonArrayAgg(a) => {
            push_json_value(&mut out, &a.arg);
            push_json_agg(&mut out, &a.constructor);
        }
        _ => {}
    }
    out
}

/// The raw expression of a `JsonValueExpr` field.
fn push_json_value<'a>(
    out: &mut Vec<&'a Option<ast::Node>>,
    v: &'a Option<NodeBox<ast::JsonValueExpr>>,
) {
    if let Some(v) = v.as_deref()
        && v.raw_expr.is_some()
    {
        out.push(&v.raw_expr);
    }
}

/// A JSON aggregate's ORDER BY and FILTER (its window, like a `FuncCall`'s
/// `OVER`, is not an argument).
fn push_json_agg<'a>(
    out: &mut Vec<&'a Option<ast::Node>>,
    c: &'a Option<NodeBox<ast::JsonAggConstructor>>,
) {
    if let Some(c) = c.as_deref() {
        out.extend(c.agg_order.iter());
        if c.agg_filter.is_some() {
            out.push(&c.agg_filter);
        }
    }
}

/// Pre-order visit of `node` and every same-level sub-expression.
pub(crate) fn visit_same_level<'a>(
    node: &'a Option<ast::Node>,
    f: &mut dyn FnMut(&'a Option<ast::Node>),
) {
    f(node);
    for child in expr_children(node) {
        visit_same_level(child, f);
    }
}

/// True when `fc` names a set-returning function. Decided by name like the
/// aggregate probe in `expr::detect_func_kinds`: any visible overload with
/// `proretset` counts, since the raw tree carries no resolved signature.
pub(crate) fn is_srf_call(fc: &ast::FuncCall, snapshot: &PgCatalog) -> bool {
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
pub(crate) fn count_srf_calls(nodes: &[Option<ast::Node>], snapshot: &PgCatalog) -> usize {
    let mut n = 0;
    for node in nodes {
        visit_same_level(node, &mut |e| {
            if let Some(ast::Node::FuncCall(fc)) = e.as_ref()
                && is_srf_call(fc, snapshot)
            {
                n += 1;
            }
        });
    }
    n
}

/// The set-returning calls a SELECT level evaluates in its projection: the
/// select list's, and those of the ORDER BY, GROUP BY and DISTINCT ON
/// items that aren't select-list expressions. PG adds such an item as a
/// resjunk target entry (`findTargetlistEntrySQL99`), so its calls run in
/// the same ProjectSet as the select list's: in lockstep with them (the
/// shorter ones padded with NULL), and leaving no row when they all yield
/// none — `SELECT 1 ORDER BY generate_series(1, 0)` is empty.
pub(crate) fn level_srf_calls<'a>(
    sel: &'a ast::SelectStmt,
    snapshot: &PgCatalog,
) -> Vec<&'a ast::FuncCall> {
    let targets: Vec<String> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.as_ref()? {
            ast::Node::ResTarget(rt) => rt.val.is_some().then(|| node_fingerprint(&rt.val)),
            _ => None,
        })
        .collect();
    let extra = sel
        .sort_clause
        .iter()
        .filter_map(|n| match n.as_ref()? {
            ast::Node::SortBy(sb) => sb.node.is_some().then_some(&sb.node),
            _ => Some(n),
        })
        .chain(&sel.group_clause)
        .chain(&sel.distinct_clause)
        .filter(|n| !targets.contains(&node_fingerprint(n)));
    let mut out = Vec::new();
    for n in sel.target_list.iter().chain(extra) {
        visit_same_level(n, &mut |e| {
            if let Some(ast::Node::FuncCall(fc)) = e.as_ref()
                && is_srf_call(fc, snapshot)
            {
                out.push(&**fc);
            }
        });
    }
    out
}

/// True when `node` contains a set-returning function call at this query
/// level (itself included).
fn contains_srf(node: &Option<ast::Node>, snapshot: &PgCatalog) -> bool {
    let mut found = false;
    visit_same_level(node, &mut |e| {
        if let Some(ast::Node::FuncCall(fc)) = e.as_ref()
            && is_srf_call(fc, snapshot)
        {
            found = true;
        }
    });
    found
}

fn srf_error(msg: String, hint: bool) -> AnalyzeError {
    crate::error::RawError::new(
        AnalyzeError::FeatureNotSupported(msg),
        None,
        hint.then(|| {
            "You might be able to move the set-returning function into a LATERAL FROM item."
                .to_string()
        }),
    )
    .finalize_implicit()
}

/// PG's `check_srf_call_placement` for a clause that forbids set-returning
/// functions (0A000 `set-returning functions are not allowed in WHERE`, …).
pub(crate) fn check_no_srf_in_clause(
    node: &Option<ast::Node>,
    snapshot: &PgCatalog,
    clause: &str,
) -> Result<(), AnalyzeError> {
    if contains_srf(node, snapshot) {
        return Err(srf_error(
            format!("set-returning functions are not allowed in {clause}"),
            false,
        ));
    }
    Ok(())
}

/// The constructs that may not *contain* a set-returning function even
/// where SRFs are otherwise allowed (0A000): CASE and COALESCE
/// (`transformCaseExpr` / `transformCoalesceExpr`), aggregate and window
/// function arguments (`check_agg_arguments_walker` / `ParseFuncOrColumn`)
/// and an aggregate's FILTER.
pub(crate) fn check_srf_nesting(
    node: &Option<ast::Node>,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let mut err: Option<AnalyzeError> = None;
    visit_same_level(node, &mut |e| {
        if err.is_some() {
            return;
        }
        let inner_has_srf =
            |nodes: Vec<&Option<ast::Node>>| nodes.into_iter().any(|n| contains_srf(n, snapshot));
        match e.as_ref() {
            Some(ast::Node::CaseExpr(_)) => {
                if inner_has_srf(expr_children(e)) {
                    err = Some(srf_error(
                        "set-returning functions are not allowed in CASE".into(),
                        true,
                    ));
                }
            }
            Some(ast::Node::CoalesceExpr(_)) => {
                if inner_has_srf(expr_children(e)) {
                    err = Some(srf_error(
                        "set-returning functions are not allowed in COALESCE".into(),
                        true,
                    ));
                }
            }
            Some(ast::Node::FuncCall(fc)) => {
                if fc.agg_filter.is_some() && contains_srf(&fc.agg_filter, snapshot) {
                    err = Some(srf_error(
                        "set-returning functions are not allowed in FILTER".into(),
                        false,
                    ));
                    return;
                }
                let args: Vec<&Option<ast::Node>> =
                    fc.args.iter().chain(fc.agg_order.iter()).collect();
                if fc.over.is_some() {
                    if inner_has_srf(args) {
                        err = Some(srf_error(
                            "window function calls cannot contain set-returning function calls"
                                .into(),
                            true,
                        ));
                    }
                } else if is_aggregate_call(fc, snapshot) && inner_has_srf(args) {
                    err = Some(srf_error(
                        "aggregate function calls cannot contain set-returning function calls"
                            .into(),
                        true,
                    ));
                }
            }
            _ => {}
        }
    });
    err.map_or(Ok(()), Err)
}

/// True when `fc` names an aggregate (by name, like `detect_func_kinds`).
pub(crate) fn is_aggregate_call(fc: &ast::FuncCall, snapshot: &PgCatalog) -> bool {
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return false,
    };
    snapshot
        .find_functions(schema, name)
        .iter()
        .any(|p| matches!(p.prokind, crate::pg_catalog::ProKind::Aggregate))
}
