use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Subqueries (SubLink)
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_sublink(
    sub: &protobuf::SubLink,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        scope, snapshot, ..
    } = ctx;
    let sub_type = protobuf::SubLinkType::try_from(sub.sub_link_type)
        .unwrap_or(protobuf::SubLinkType::ExprSublink);

    match sub_type {
        protobuf::SubLinkType::ExistsSublink => {
            // Walk the subselect to collect any params referenced inside —
            // without this, `EXISTS(SELECT 1 FROM t WHERE x = $p1)` would
            // drop `$p1` from the param list entirely. Outer scope is
            // seeded so correlated refs (`outer.col`) resolve correctly
            // and feed types into the param resolver.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let _ = crate::resolve::analyze_correlated_select(
                    sel,
                    snapshot,
                    params,
                    scope,
                    ctx.null_ctx,
                )?;
            }
            Ok(ExprType::scalar(oid::BOOL, false))
        }
        protobuf::SubLinkType::ExprSublink => {
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let first = single_sublink_column(sub, sel, ctx, params)?;
                let guaranteed_one_row = yields_exactly_one_row(sel, snapshot)
                    || reads_one_row_cte(sel, &scope.ctes, snapshot);
                let nullable = if guaranteed_one_row {
                    first.nullable
                } else {
                    true
                };
                // The sublink carries its column's collation, implicitly,
                // and its value — an array's elements included (no row is
                // NULL, not an array with other elements).
                return Ok(
                    ExprType::scalar_with_typmod(first.type_oid, nullable, first.typmod)
                        .with_collation((first.collation, false))
                        .with_elem_nullable(first.elem_nullable),
                );
            }
            Ok(ExprType::scalar(oid::UNKNOWN, true))
        }
        protobuf::SubLinkType::AnySublink | protobuf::SubLinkType::AllSublink => {
            // Walk the subselect so params inside `col = ANY(SELECT …)` /
            // `col = ALL(SELECT …)` are collected with the right types.
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let (cols, _) = crate::resolve::analyze_correlated_select(
                    sel,
                    snapshot,
                    params,
                    scope,
                    ctx.null_ctx,
                )?;

                // Arity check: `lhs IN (SELECT …)` / `lhs = ANY(SELECT …)`
                // requires the LHS and the subquery to match column counts.
                // PG rejects mismatches with `subquery has too many columns`
                // or `subquery has too few columns`.
                // A ROW's arguments go through transformExpressionList,
                // which expands `t.*` / `(expr).*` into their columns.
                let lhs_row: Option<std::borrow::Cow<'_, [protobuf::Node]>> =
                    match sub.testexpr.as_deref().and_then(|n| n.node.as_ref()) {
                        Some(node::Node::RowExpr(r)) => Some(expand_row_args(&r.args, ctx, params)),
                        _ => None,
                    };
                let lhs_arity = lhs_row.as_ref().map_or(1, |r| r.len());
                if lhs_arity != cols.len() {
                    let pg_msg = if cols.len() < lhs_arity {
                        "subquery has too few columns"
                    } else {
                        "subquery has too many columns"
                    };
                    return Err(AnalyzeError::Invalid(format!(
                        "{pg_msg} (subquery has {}, lhs has {lhs_arity})",
                        cols.len(),
                    )));
                }

                // Resolve the comparison operator between each LHS expression
                // and the matching subquery column. PG applies the same
                // operator-resolution rules here as for a plain `a OP b`, so
                // `int_col IN (SELECT text_col …)` is rejected with
                // `operator does not exist: integer = text`. The LHS lives in
                // `testexpr` and is *only* reachable through this SubLink, so we
                // must walk it here — that also pins LHS params/columns.
                let lhs_nodes: Vec<&protobuf::Node> = match &lhs_row {
                    Some(r) => r.iter().collect(),
                    None => sub.testexpr.as_deref().into_iter().collect(),
                };
                // `oper_name` is `=` for `IN`, or the written operator for
                // `<op> ANY/ALL`. Default to `=` if the parser left it empty.
                let op_name = {
                    let joined = extract_string_fields(&sub.oper_name).join(".");
                    if joined.is_empty() {
                        "=".to_string()
                    } else {
                        joined
                    }
                };
                // `x op ANY (SELECT …)` is NULL or FALSE for a NULL `x` when
                // every comparison is strict (a row's `=` is an AND of its
                // fields' comparisons, never TRUE with a NULL field).
                let mut strict = sub_type == protobuf::SubLinkType::AnySublink
                    && (lhs_row.is_none() || op_name == "=");
                for (lhs_node, col) in lhs_nodes.iter().zip(cols.iter()) {
                    let lhs = infer_expr(lhs_node, ctx, params, TypeGoal::NONE)?;
                    let l_oid = lhs.type_oid;
                    let r_oid = col.type_oid;
                    match snapshot.find_operator(&op_name, Some(l_oid), r_oid) {
                        Some(op) if l_oid != oid::UNKNOWN && r_oid != oid::UNKNOWN => {
                            strict &= ctx.proc_is_strict(op.code);
                        }
                        _ => strict = false,
                    }
                    // An UNKNOWN side (bare literal / unpinned param) is coerced
                    // by PG to its peer — pin params and skip the rejection.
                    if l_oid == oid::UNKNOWN {
                        if r_oid != oid::UNKNOWN {
                            coerce_unknown_to(
                                lhs_node,
                                ctx,
                                params,
                                snapshot.unwrap_domain(r_oid),
                            )?;
                        }
                        continue;
                    }
                    if r_oid == oid::UNKNOWN {
                        continue;
                    }
                    if snapshot
                        .find_operator(&op_name, Some(l_oid), r_oid)
                        .is_none()
                    {
                        let left_pg = crate::ddl::util::format_type_for_message(snapshot, l_oid);
                        let right_pg = crate::ddl::util::format_type_for_message(snapshot, r_oid);
                        // PG positions it at the sublink's operator (`IN`,
                        // `= ANY`, the row comparison's `=`).
                        let span = crate::error::SourceSpan::from_node_token(sub.location)
                            .or_else(|| crate::error::SourceSpan::from_location(sub.location));
                        let err = crate::pgmsg::operator_does_not_exist(
                            &left_pg, &op_name, &right_pg, span,
                        );
                        return Err(crate::expr::operators::with_cast_note(
                            err, snapshot, &op_name, l_oid, r_oid,
                        )
                        .finalize_implicit());
                    }
                }
                ctx.note_strict(sub.location, crate::nonnull::StrictNode::Sublink, strict);
            }
            Ok(ExprType::scalar(oid::BOOL, true))
        }
        protobuf::SubLinkType::ArraySublink => {
            // `ARRAY(SELECT expr FROM …)` — returns an array of the subquery's
            // single output column. The array itself is always NOT NULL (an
            // empty result produces `{}`, not NULL), even though individual
            // elements may be nullable. PG (transformSubLink): when the
            // element is itself an array the result is that same array type
            // (a multi-dimensional array), otherwise its array type.
            let mut array_oid = oid::UNKNOWN;
            let mut typmod = None;
            let mut collation = (None, false);
            let mut elem_nullable = None;
            if let Some(subselect) = &sub.subselect
                && let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref()
            {
                let first = single_sublink_column(sub, sel, ctx, params)?;
                // The array carries the column's typmod (exprTypmod of an
                // ARRAY sublink is its subquery column's) and, implicitly,
                // its collation.
                typmod = first.typmod;
                collation = (first.collation, false);
                let elem = first.type_oid;
                let elem_is_array = snapshot
                    .get_type(elem)
                    .is_some_and(|t| t.typcategory == TypCategory::Array && t.typelem.is_some());
                // The elements are the column's values (or, for an array
                // column, the multidimensional result's sub-arrays').
                elem_nullable = (!elem_is_array).then_some(first.nullable);
                array_oid = if elem_is_array {
                    elem
                } else {
                    snapshot.array_type_of(elem).ok_or_else(|| {
                        crate::pgmsg::no_array_type_for(&crate::ddl::util::format_type_for_message(
                            snapshot, elem,
                        ))
                    })?
                };
            }
            Ok(ExprType::scalar_with_typmod(array_oid, false, typmod)
                .with_collation(collation)
                .with_elem_nullable(elem_nullable))
        }
        _ => Err(crate::error::RawError::unsupported(
            format!(
                "typedpg does not support {} subqueries yet",
                sub_type.as_str_name()
            ),
            crate::error::SourceSpan::from_location(sub.location),
            None,
        )
        .finalize_implicit()),
    }
}

/// Whether the (already analyzed) scalar subquery `sel` always yields
/// exactly one row, so that its value is NULL only when its column is — an
/// empty result makes the sublink NULL. That holds for an aggregate query
/// of its own level without GROUP BY / HAVING (`count(t.id)` over an outer
/// `t` is the outer query's aggregate, not one of the subquery's), and for
/// a query without FROM or WHERE; in both only when neither LIMIT / OFFSET
/// nor a set-returning select list can drop the row.
fn yields_exactly_one_row(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> bool {
    if sel.op != protobuf::SetOperation::SetopNone as i32 {
        return false;
    }
    let const_of = |n: &Option<Box<protobuf::Node>>| match n.as_deref().map(|n| n.node.as_ref()) {
        None => Some(None),
        Some(Some(node::Node::AConst(ac))) if ac.isnull => Some(None),
        Some(Some(node::Node::AConst(protobuf::AConst {
            val: Some(a_const::Val::Ival(i)),
            ..
        }))) => Some(Some(i.ival)),
        _ => None,
    };
    // LIMIT ALL / NULL / n ≥ 1, and OFFSET NULL / 0.
    let limit_keeps = matches!(const_of(&sel.limit_count), Some(None) | Some(Some(1..)));
    let offset_keeps = matches!(const_of(&sel.limit_offset), Some(None) | Some(Some(0)));
    if !limit_keeps
        || !offset_keeps
        || !sel.group_clause.is_empty()
        || sel.having_clause.is_some()
        || crate::resolve::count_srf_calls(&sel.target_list, snapshot) > 0
    {
        return false;
    }
    if !sel.values_lists.is_empty() {
        // A VALUES list of one row.
        return sel.values_lists.len() == 1;
    }
    let has_aggs = crate::grouping::level_info(sel).is_some_and(|l| l.has_aggs);
    has_aggs || (sel.from_clause.is_empty() && sel.where_clause.is_none())
}

/// Whether scalar subquery `sel` reads every row of a data-modifying CTE
/// that returns exactly one row (`SELECT id FROM ins`), and only them.
fn reads_one_row_cte(
    sel: &protobuf::SelectStmt,
    ctes: &std::collections::HashMap<String, Vec<crate::scope::ScopeColumn>>,
    snapshot: &PgCatalog,
) -> bool {
    let [item] = sel.from_clause.as_slice() else {
        return false;
    };
    let Some(node::Node::RangeVar(rv)) = item.node.as_ref() else {
        return false;
    };
    sel.op == protobuf::SetOperation::SetopNone as i32
        && sel.with_clause.is_none()
        && sel.where_clause.is_none()
        && sel.group_clause.is_empty()
        && sel.having_clause.is_none()
        && sel.limit_count.is_none()
        && sel.limit_offset.is_none()
        && sel.distinct_clause.is_empty()
        && rv.schemaname.is_empty()
        && crate::resolve::cte_returns_one_row(ctes, &rv.relname)
        && crate::resolve::count_srf_calls(&sel.target_list, snapshot) == 0
}

/// The single output column of an EXPR / ARRAY sublink's subquery, as PG's
/// `transformSubLink` requires: no column is `subquery must return a column`,
/// more than one is `subquery must return only one column` (both 42601).
/// An unknown-typed target (`SELECT NULL`, `SELECT 'x'`, `SELECT $1`) has
/// already been resolved to text by the subquery's own
/// `resolveTargetListUnknowns`, so a bare untyped param is pinned to text.
fn single_sublink_column(
    sub: &protobuf::SubLink,
    sel: &protobuf::SelectStmt,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<crate::resolve::RawColumn, AnalyzeError> {
    let (cols, _) = crate::resolve::analyze_correlated_select(
        sel,
        ctx.snapshot,
        params,
        ctx.scope,
        ctx.null_ctx,
    )?;
    let span = crate::error::SourceSpan::from_location(sub.location);
    let mut cols = cols.into_iter();
    let Some(mut first) = cols.next() else {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError("subquery must return a column".to_string()),
            span,
            None,
        )
        .finalize_implicit());
    };
    if cols.next().is_some() {
        return Err(crate::pgmsg::subquery_must_return_one_column(span).finalize_implicit());
    }
    if first.type_oid == oid::UNKNOWN {
        if let [target] = sel.target_list.as_slice()
            && let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
            && let Some(node::Node::ParamRef(p)) = rt.val.as_deref().and_then(|v| v.node.as_ref())
            && params.get(p.number) == oid::UNKNOWN
        {
            params.record(p.number, oid::TEXT);
        }
        first.type_oid = oid::TEXT;
    }
    Ok(first)
}
