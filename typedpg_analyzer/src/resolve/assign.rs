//! Assignment targets shared by UPDATE SET, ON CONFLICT DO UPDATE SET,
//! MERGE ... UPDATE SET and INSERT column lists: multi-column assignments
//! (`SET (a, b) = …`), subscripted / field assignments (`SET arr[1] = …`,
//! `SET p.x = …`) and PG's duplicate-target rules.

use super::*;

/// What an assignment target (`col`, `col[i]`, `col.f[i]`, …) expects.
pub(crate) struct AssignTarget {
    /// The type the assigned value is coerced to.
    pub type_oid: PgTypeOid,
    /// The target carries indirection: the value lands inside the column,
    /// so the column-level NOT NULL / typmod checks don't apply.
    pub indirected: bool,
    /// PG's wording on a coercion failure (`transformAssignmentIndirection`):
    /// `subscripted assignment to "x"` after a subscript step, `subfield
    /// "x"` after a field step.
    last_step: Option<(bool, String)>,
}

/// Walk `indirection` from column `tc` like PG's
/// `transformAssignmentIndirection` (parse_target.c): a run of subscripts
/// steps into the container's element (a slice keeps the array type; jsonb
/// subscripts yield jsonb), a field name steps into a composite's attribute.
/// Subscript expressions are inferred (array subscripts are integers, jsonb
/// ones text or integer) so their parameters are typed.
pub(crate) fn assignment_target(
    tc: &crate::pg_catalog::PgAttribute,
    indirection: &[protobuf::Node],
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<AssignTarget, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let mut ty = tc.atttypid;
    let mut target_name = tc.attname.clone();
    let mut last_step = None;
    let mut i = 0;
    while i < indirection.len() {
        match indirection[i].node.as_ref() {
            Some(node::Node::AIndices(_)) => {
                let mut run: Vec<&protobuf::AIndices> = Vec::new();
                while let Some(node::Node::AIndices(ai)) =
                    indirection.get(i).and_then(|n| n.node.as_ref())
                {
                    run.push(ai);
                    i += 1;
                }
                let base = snapshot.unwrap_domain(ty);
                let te = snapshot.get_type(base);
                let (elem, index_goal) = if let Some(e) = te
                    .filter(|t| t.typcategory == TypCategory::Array)
                    .and_then(|t| t.typelem)
                {
                    let slice = run.iter().any(|a| a.is_slice);
                    ((if slice { base } else { e }), Some(oid::INT4))
                } else if te.is_some_and(|t| t.typname == "jsonb") {
                    // jsonb's subscript handler yields jsonb.
                    (base, None)
                } else {
                    let t = crate::ddl::util::format_type_for_message(snapshot, ty);
                    return Err(crate::error::RawError::new(
                        AnalyzeError::DatatypeMismatch(format!(
                            "cannot subscript type {t} because it does not support subscripting"
                        )),
                        None,
                        None,
                    )
                    .finalize_implicit());
                };
                for ai in run {
                    for idx in [ai.lidx.as_deref(), ai.uidx.as_deref()]
                        .into_iter()
                        .flatten()
                    {
                        let goal = match index_goal {
                            Some(t) => TypeGoal::implicit(t),
                            None => TypeGoal::NONE,
                        };
                        let e = expr::infer_expr(idx, ctx, params, goal)?;
                        // jsonb's subscript handler reads an untyped
                        // subscript as text (`jsonb_subscript_transform`).
                        if index_goal.is_none()
                            && let Some(node::Node::ParamRef(p)) = idx.node.as_ref()
                            && e.type_oid == oid::UNKNOWN
                            && params.get(p.number) == oid::UNKNOWN
                        {
                            params.record(p.number, oid::TEXT);
                        }
                    }
                }
                ty = elem;
                last_step = Some((true, target_name.clone()));
            }
            Some(node::Node::String(s)) => {
                let field = &s.sval;
                let base = snapshot.unwrap_domain(ty);
                let relid = snapshot
                    .get_type(base)
                    .filter(|t| t.typtype == TypType::Composite)
                    .and_then(|t| t.typrelid);
                let Some(relid) = relid else {
                    let t = crate::ddl::util::format_type_for_message(snapshot, ty);
                    return Err(crate::error::RawError::new(
                        AnalyzeError::DatatypeMismatch(format!(
                            "cannot assign to field \"{field}\" of column \"{target_name}\" \
                             because its type {t} is not a composite type"
                        )),
                        None,
                        None,
                    )
                    .finalize_implicit());
                };
                let Some(attr) = snapshot
                    .attributes_of(relid)
                    .iter()
                    .find(|a| &a.attname == field)
                else {
                    let t = crate::ddl::util::format_type_for_message(snapshot, ty);
                    return Err(crate::error::RawError::new(
                        AnalyzeError::UndefinedColumn(format!(
                            "cannot assign to field \"{field}\" of column \"{target_name}\" \
                             because there is no such column in data type {t}"
                        )),
                        None,
                        None,
                    )
                    .finalize_implicit());
                };
                ty = attr.atttypid;
                target_name = field.clone();
                last_step = Some((false, target_name.clone()));
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(AssignTarget {
        type_oid: ty,
        indirected: !indirection.is_empty(),
        last_step,
    })
}

impl AssignTarget {
    /// Infer `val` against this target. A plain column uses the regular
    /// assignment goal (`column "x" is of type …`); an indirected target
    /// reports PG's subscript / subfield wording on a coercion failure.
    pub(crate) fn infer_value(
        &self,
        val: &protobuf::Node,
        goal: TypeGoal,
        ctx: Ctx<'_>,
        params: &mut ParamCollector,
    ) -> Result<expr::ExprType, AnalyzeError> {
        let Some((subscript, name)) = &self.last_step else {
            return expr::infer_expr(val, ctx, params, goal);
        };
        match expr::infer_expr(val, ctx, params, TypeGoal::assignment(self.type_oid)) {
            Err(AnalyzeError::TypeMismatch { .. }) => {
                let mut scratch = params.clone();
                let actual = expr::infer_expr(val, ctx, &mut scratch, TypeGoal::NONE)
                    .map(|e| e.type_oid)
                    .unwrap_or(oid::UNKNOWN);
                Err(self.mismatch(*subscript, name, actual, ctx.snapshot))
            }
            other => other,
        }
    }

    /// The coercion-failure error for a value of type `actual`.
    pub(crate) fn mismatch_error(
        &self,
        column: &str,
        actual: PgTypeOid,
        snapshot: &PgCatalog,
    ) -> AnalyzeError {
        match &self.last_step {
            Some((subscript, name)) => self.mismatch(*subscript, name, actual, snapshot),
            None => {
                let expected = crate::ddl::util::format_type_for_message(snapshot, self.type_oid);
                let actual = crate::ddl::util::format_type_for_message(snapshot, actual);
                crate::error::RawError::new(
                    AnalyzeError::DatatypeMismatch(format!(
                        "column \"{column}\" is of type {expected} but expression is of type {actual}"
                    )),
                    None,
                    Some("You will need to rewrite or cast the expression.".into()),
                )
                .finalize_implicit()
            }
        }
    }

    fn mismatch(
        &self,
        subscript: bool,
        name: &str,
        actual: PgTypeOid,
        snapshot: &PgCatalog,
    ) -> AnalyzeError {
        let expected = crate::ddl::util::format_type_for_message(snapshot, self.type_oid);
        let actual = crate::ddl::util::format_type_for_message(snapshot, actual);
        let msg = if subscript {
            format!(
                "subscripted assignment to \"{name}\" requires type {expected} but expression is of type {actual}"
            )
        } else {
            format!("subfield \"{name}\" is of type {expected} but expression is of type {actual}")
        };
        crate::error::RawError::new(
            AnalyzeError::DatatypeMismatch(msg),
            None,
            Some("You will need to rewrite or cast the expression.".into()),
        )
        .finalize_implicit()
    }
}

/// The value source of one SET item after multi-assignment expansion.
enum SetValue<'a> {
    /// `col = expr`, or one element of `(a, b) = ROW(x, y)`.
    Expr(&'a protobuf::Node),
    /// Column `n` of `(a, b) = (SELECT …)`.
    SubqueryColumn(RawColumn),
}

/// Expand `SET (a, b) = (…)` like PG's `transformMultiAssignRef`
/// (parse_expr.c): a `ROW(…)` source contributes its elements, a sub-SELECT
/// its output columns (analyzed once, as a correlated subquery); anything
/// else is 0A000. The counts must agree (42601).
fn expand_set_items<'a>(
    target_list: &'a [protobuf::Node],
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Vec<(&'a protobuf::ResTarget, SetValue<'a>)>, AnalyzeError> {
    let mut out = Vec::with_capacity(target_list.len());
    let mut subquery_cols: Vec<RawColumn> = Vec::new();
    for item in target_list {
        let Some(node::Node::ResTarget(rt)) = item.node.as_ref() else {
            continue;
        };
        let Some(val) = rt.val.as_deref() else {
            continue;
        };
        let Some(node::Node::MultiAssignRef(mar)) = val.node.as_ref() else {
            out.push((rt.as_ref(), SetValue::Expr(val)));
            continue;
        };
        let source = mar.source.as_deref();
        let ncolumns = usize::try_from(mar.ncolumns).unwrap_or(0);
        let colno = usize::try_from(mar.colno).unwrap_or(1).max(1);
        let count_mismatch = || {
            crate::error::RawError::new(
                AnalyzeError::SyntaxError(
                    "number of columns does not match number of values".into(),
                ),
                None,
                None,
            )
            .finalize_implicit()
        };
        match source.and_then(|s| s.node.as_ref()) {
            Some(node::Node::RowExpr(row)) => {
                if row.args.len() != ncolumns {
                    return Err(count_mismatch());
                }
                out.push((rt.as_ref(), SetValue::Expr(&row.args[colno - 1])));
            }
            Some(node::Node::SubLink(sl))
                if matches!(
                    sl.subselect.as_deref().and_then(|n| n.node.as_ref()),
                    Some(node::Node::SelectStmt(_))
                ) =>
            {
                if colno == 1 {
                    let Some(node::Node::SelectStmt(sel)) =
                        sl.subselect.as_deref().and_then(|n| n.node.as_ref())
                    else {
                        unreachable!("guarded above");
                    };
                    let (cols, _) =
                        analyze_correlated_select(sel, ctx.snapshot, params, ctx.scope)?;
                    if cols.len() != ncolumns {
                        return Err(count_mismatch());
                    }
                    subquery_cols = cols;
                }
                let mut col = subquery_cols
                    .get(colno - 1)
                    .cloned()
                    .ok_or_else(count_mismatch)?;
                // A sub-SELECT's unknown-typed outputs resolve to text.
                if col.type_oid == oid::UNKNOWN {
                    col.type_oid = oid::TEXT;
                }
                // A scalar sub-SELECT yields NULL when it returns no row.
                col.nullable = true;
                out.push((rt.as_ref(), SetValue::SubqueryColumn(col)));
            }
            _ => {
                return Err(crate::error::RawError::new(
                    AnalyzeError::FeatureNotSupported(
                        "source for a multiple-column UPDATE item must be a sub-SELECT or ROW() \
                         expression"
                            .into(),
                    ),
                    None,
                    None,
                )
                .finalize_implicit());
            }
        }
    }
    Ok(out)
}

/// Analyze an UPDATE-style SET list against `table_attrs` — the shared core
/// of UPDATE, ON CONFLICT DO UPDATE and MERGE ... UPDATE (PG's
/// `transformUpdateTargetList`). `null_check` enables the static rejection
/// of `SET not_null_col = NULL`.
pub(crate) fn analyze_set_clause(
    target_list: &[protobuf::Node],
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    null_check: bool,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    let items = expand_set_items(target_list, ctx, params)?;
    // `(column, indirected)` of every assignment, for the rewriter's
    // `multiple assignments to same column` rule.
    let mut assigned: Vec<(&str, bool)> = Vec::with_capacity(items.len());
    for (rt, value) in items {
        // Same reasoning as analyze_insert: reject unknown columns up front
        // instead of letting the parameter fall back to text via the
        // UNKNOWN path.
        let tc = table_attrs
            .iter()
            .find(|c| c.attname == rt.name)
            .ok_or_else(|| {
                crate::scope::undefined_dml_column_error(
                    &rt.name,
                    table_relname,
                    table_attrs,
                    crate::error::SourceSpan::from_node_qname(rt.location),
                )
            })?;
        let target = assignment_target(tc, &rt.indirection, ctx, params)?;
        assigned.push((tc.attname.as_str(), target.indirected));
        let is_default = matches!(&value, SetValue::Expr(v) if is_set_to_default(v));
        match value {
            SetValue::Expr(val) => {
                // Catch `UPDATE … SET not_null_col = NULL` statically — PG
                // raises a runtime `null value in column … violates
                // not-null constraint` error. A NULL stored *inside* the
                // column (`arr[1] = NULL`) is fine.
                if null_check
                    && !target.indirected
                    && is_sql_null_literal(val)
                    && let Some(err) = null_assignment_error(tc, snapshot, table_relname, "assign")
                {
                    return Err(err);
                }
                if !target.indirected
                    && let Some(err) = crate::typmod::check_literal_assignment(
                        snapshot,
                        tc.atttypid,
                        snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                        val,
                    )
                {
                    return Err(err);
                }
                check_update_generated(tc, is_default, table_relname)?;
                let goal = TypeGoal::assignment(target.type_oid).with_source_column(&tc.attname);
                // Attach the target column's reference span so a
                // TypeMismatch surfaces a secondary label at the `col =`
                // site.
                let goal = match crate::error::SourceSpan::from_node_qname(rt.location) {
                    Some(s) => goal.with_source(s),
                    None => goal,
                };
                target.infer_value(val, goal, ctx, params)?;
                if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                    && (!tc.attnotnull || target.indirected)
                {
                    params.infer_nullable(p.number, true);
                }
            }
            SetValue::SubqueryColumn(col) => {
                check_update_generated(tc, false, table_relname)?;
                if col.type_oid != target.type_oid
                    && !crate::coerce::can_coerce(
                        col.type_oid,
                        target.type_oid,
                        crate::coerce::CoercionContext::Assignment,
                        snapshot,
                    )
                {
                    return Err(target.mismatch_error(&tc.attname, col.type_oid, snapshot));
                }
            }
        }
    }
    // PG's rewriter (`process_matched_tle`) merges several indirected
    // assignments to one column but rejects any repeat involving a plain
    // assignment.
    for (i, (name, indirected)) in assigned.iter().enumerate() {
        if let Some((_, prev_indirected)) = assigned[..i].iter().find(|(n, _)| n == name)
            && !(*indirected && *prev_indirected)
        {
            return Err(crate::error::RawError::new(
                AnalyzeError::SyntaxError(format!(
                    "multiple assignments to same column \"{name}\""
                )),
                None,
                None,
            )
            .finalize_implicit());
        }
    }
    Ok(())
}

/// Generated and `GENERATED ALWAYS` identity columns can only be updated to
/// `DEFAULT`.
fn check_update_generated(
    tc: &crate::pg_catalog::PgAttribute,
    is_default: bool,
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    if tc.attgenerated.is_some() && !is_default {
        return Err(AnalyzeError::Invalid(format!(
            "column \"{}\" can only be updated to DEFAULT \
             (generated column on `{}`)",
            tc.attname, table_relname,
        )));
    }
    if tc.attidentity == Some(AttIdentity::Always) && !is_default {
        return Err(AnalyzeError::Invalid(format!(
            "column \"{}\" can only be updated to DEFAULT \
             (identity column on `{}` defined as GENERATED ALWAYS)",
            tc.attname, table_relname,
        )));
    }
    Ok(())
}

/// PG's `checkInsertTargets`: a column may be named more than once only
/// when every occurrence assigns *into* it (`arr[1], arr[2]`) — 42701
/// `column "x" specified more than once` otherwise.
pub(crate) fn check_insert_target_duplicates(cols: &[protobuf::Node]) -> Result<(), AnalyzeError> {
    let mut whole: Vec<&str> = Vec::new();
    let mut partial: Vec<&str> = Vec::new();
    for n in cols {
        let Some(node::Node::ResTarget(rt)) = n.node.as_ref() else {
            continue;
        };
        let name = rt.name.as_str();
        let dup = if rt.indirection.is_empty() {
            let d = whole.contains(&name) || partial.contains(&name);
            whole.push(name);
            d
        } else {
            let d = whole.contains(&name);
            partial.push(name);
            d
        };
        if dup {
            return Err(crate::error::RawError::new(
                AnalyzeError::DuplicateColumn(format!(
                    "column \"{name}\" specified more than once"
                )),
                crate::error::SourceSpan::from_node_qname(rt.location),
                None,
            )
            .finalize_implicit());
        }
    }
    Ok(())
}
