//! A NULL cast to a domain that rejects it (`SELECT NULL::d` of a NOT NULL
//! domain, `CAST(NULL AS d)` of one with `CHECK (VALUE IS NOT NULL)`) that
//! every execution of a SELECT evaluates: the coercion fails when evaluated
//! (`ExecEvalConstraintNotNull` / `ExecEvalConstraintCheck`), naming the
//! domain cast to, so the statement is rejected.
//!
//! A query is followed only where it surely yields a row: a select list
//! with no WHERE, grouping or LIMIT over no FROM item (one row) or over a
//! single view, subquery or CTE that surely yields one; a VALUES list. The
//! planner drops a subquery's (or a view's) output columns the query above
//! doesn't use, so a failing column of one fails only where that query
//! reads it as a whole output column (`x`, `s.x`, `*`, `s.*`, `s`).
//!
//! A view's query was analyzed without execution when the view was
//! created (DefineView), so the expressions of its rows that every
//! evaluation fails (sub-arrays that can't agree) are found here too.

use super::*;

/// Fail when every execution of `sel` evaluates a NULL its domain rejects:
/// in a column of its own rows, or of an arm of its set operation.
pub(super) fn check_null_domain_casts(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    if !executes() || sel.limit_count.is_some() || sel.limit_offset.is_some() {
        return Ok(());
    }
    if sel.op != protobuf::SetOperation::SetopNone as i32 {
        for arm in [&sel.larg, &sel.rarg].into_iter().flatten() {
            check_null_domain_casts(arm, snapshot)?;
        }
        return Ok(());
    }
    // What the walk looks up is the statement's business only: its
    // dependencies come from its analysis.
    let (failing, _) =
        crate::ddl::depend::collect(|| failing_columns(sel, snapshot, &[], 0, false));
    match failing.into_iter().flatten().flatten().next() {
        Some(msg) => Err(AnalyzeError::Invalid(msg)),
        None => Ok(()),
    }
}

/// The views, subqueries and CTEs followed at most (a view can't read
/// itself, but a CTE list could be followed around in circles otherwise).
const MAX_DEPTH: usize = 32;

/// `Some(columns)` when every execution of `sel` yields a row; each column
/// then carries the error evaluating it raises, if it surely does.
/// `ctes` are the CTEs visible to `sel`'s FROM items, innermost first;
/// `stored` is set inside a view's query.
fn failing_columns(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
) -> Option<Vec<Option<String>>> {
    if depth > MAX_DEPTH || sel.limit_count.is_some() || sel.limit_offset.is_some() {
        return None;
    }
    if sel.op != protobuf::SetOperation::SetopNone as i32 {
        return union_failing_columns(sel, snapshot, ctes, depth, stored);
    }
    let mut visible: Vec<&protobuf::CommonTableExpr> = Vec::new();
    if let Some(with) = sel.with_clause.as_ref() {
        if with.recursive {
            return None;
        }
        visible.extend(
            with.ctes
                .iter()
                .rev()
                .filter_map(|c| match c.node.as_ref() {
                    Some(node::Node::CommonTableExpr(cte)) => Some(cte.as_ref()),
                    _ => None,
                }),
        );
    }
    visible.extend(ctes.iter().copied());
    // A stored expression is analyzed again, with and without execution,
    // where it reads nothing but itself: no FROM item, no CTE.
    let standalone = stored && sel.from_clause.is_empty() && visible.is_empty();
    let null_cast = |n: &protobuf::Node| -> Option<String> {
        let Some(node::Node::TypeCast(tc)) = n.node.as_ref() else {
            return None;
        };
        if !matches!(
            tc.arg.as_deref().and_then(|a| a.node.as_ref()),
            Some(node::Node::AConst(c)) if c.isnull
        ) {
            return None;
        }
        let target = crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, snapshot)?;
        snapshot.domain_null_violation_at_execution(target)
    };
    let eval_error = |n: &protobuf::Node| -> Option<String> {
        null_cast(n).or_else(|| {
            if !standalone {
                return None;
            }
            let scope = Scope::default();
            let null_ctx = NullabilityContext::default();
            let infer = || {
                let ctx = Ctx::new(&scope, &null_ctx, snapshot);
                expr::infer_expr(n, ctx, &mut ParamCollector::default(), TypeGoal::NONE)
            };
            // Only what execution adds: out of its query, the expression
            // could fail for other reasons.
            without_execution(infer).ok()?;
            infer().err().map(|e| e.to_string())
        })
    };
    if !sel.values_lists.is_empty() {
        let rows: Vec<&[protobuf::Node]> = sel
            .values_lists
            .iter()
            .filter_map(|row| match row.node.as_ref() {
                Some(node::Node::List(l)) => Some(l.items.as_slice()),
                _ => None,
            })
            .collect();
        let width = rows.first().map_or(0, |r| r.len());
        // One row is a plain projection, each cell evaluated where read; a
        // VALUES scan evaluates every cell of the rows it yields.
        if let [row] = rows.as_slice() {
            return Some(row.iter().map(eval_error).collect());
        }
        let first = rows.iter().flat_map(|r| r.iter()).find_map(eval_error);
        return Some(vec![first; width]);
    }
    if sel.where_clause.is_some() || !sel.group_clause.is_empty() || sel.having_clause.is_some() {
        return None;
    }
    let source = match sel.from_clause.as_slice() {
        [] => None,
        [item] => Some(from_item_rows(item, snapshot, &visible, depth, stored)?),
        _ => return None,
    };
    let mut out = Vec::new();
    for target in &sel.target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            return None;
        };
        let val = rt.val.as_deref()?;
        if let (Some(source), Some(node::Node::ColumnRef(cr))) = (&source, val.node.as_ref()) {
            match source.read(cr) {
                Read::Columns(cols) => {
                    out.extend(cols);
                    continue;
                }
                Read::Column(col) => {
                    out.push(col);
                    continue;
                }
                Read::Other => {}
            }
        }
        out.push(eval_error(val));
    }
    Some(out)
}

/// [`failing_columns`] of a UNION [ALL]: it yields the rows of both arms,
/// and an arm is evaluated whole — a column failing in one fails reading
/// any column. (INTERSECT and EXCEPT may skip their right arm.)
fn union_failing_columns(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
) -> Option<Vec<Option<String>>> {
    if sel.op != protobuf::SetOperation::SetopUnion as i32 || sel.with_clause.is_some() {
        return None;
    }
    let arms: Vec<Vec<Option<String>>> = [&sel.larg, &sel.rarg]
        .into_iter()
        .flatten()
        .filter_map(|arm| failing_columns(arm, snapshot, ctes, depth + 1, stored))
        .collect();
    let width = leftmost(sel).target_list.len().max(arms.first()?.len());
    let first = arms.iter().flatten().flatten().next().cloned();
    Some(vec![first; width])
}

/// The leftmost arm of a set operation, which names its columns.
fn leftmost(sel: &protobuf::SelectStmt) -> &protobuf::SelectStmt {
    match sel.larg.as_deref() {
        Some(l) if sel.op != protobuf::SetOperation::SetopNone as i32 => leftmost(l),
        _ => sel,
    }
}

/// The rows of the single FROM item a select list reads, when it surely
/// yields one.
struct SourceRows {
    /// The name the item is referred to by (its alias, else its name).
    name: String,
    /// Its output columns' names.
    names: Vec<String>,
    columns: Vec<Option<String>>,
}

/// What a select-list column reference reads of a [`SourceRows`].
enum Read {
    /// `*` / `s.*`: every column, each its own output column.
    Columns(Vec<Option<String>>),
    /// One output column: `x`, `s.x`, or the whole row `s` (whose
    /// construction evaluates every column).
    Column(Option<String>),
    Other,
}

impl SourceRows {
    fn read(&self, cr: &protobuf::ColumnRef) -> Read {
        let star = |n: &protobuf::Node| matches!(n.node, Some(node::Node::AStar(_)));
        fn name(n: &protobuf::Node) -> Option<&str> {
            match n.node.as_ref() {
                Some(node::Node::String(s)) => Some(s.sval.as_str()),
                _ => None,
            }
        }
        let column = |col: &str| {
            self.names
                .iter()
                .position(|n| n == col)
                .map(|i| self.columns.get(i).cloned().flatten())
        };
        match cr.fields.as_slice() {
            [f] if star(f) => Read::Columns(self.columns.clone()),
            [rel, f] if star(f) && name(rel) == Some(self.name.as_str()) => {
                Read::Columns(self.columns.clone())
            }
            [col] => match name(col).and_then(column) {
                Some(c) => Read::Column(c),
                // A whole-row reference by the item's name.
                None if name(col) == Some(self.name.as_str()) => {
                    Read::Column(self.columns.iter().flatten().next().cloned())
                }
                None => Read::Other,
            },
            [rel, col] if name(rel) == Some(self.name.as_str()) => {
                name(col).and_then(column).map_or(Read::Other, Read::Column)
            }
            _ => Read::Other,
        }
    }
}

/// The FROM item `item` as [`SourceRows`]: a view, a CTE or a subquery
/// whose query surely yields a row.
fn from_item_rows(
    item: &protobuf::Node,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
) -> Option<SourceRows> {
    let select = |n: &protobuf::Node| -> Option<protobuf::SelectStmt> {
        match n.node.as_ref() {
            Some(node::Node::SelectStmt(s)) => Some((**s).clone()),
            _ => None,
        }
    };
    // `view`: the query is a view's, stored without execution.
    let (name, alias, query, inner_ctes, mut names, view): (
        _,
        _,
        _,
        &[&protobuf::CommonTableExpr],
        _,
        _,
    ) = match item.node.as_ref()? {
        node::Node::RangeVar(rv) => {
            let cte = rv
                .schemaname
                .is_empty()
                .then(|| ctes.iter().position(|c| c.ctename == rv.relname))
                .flatten();
            if let Some(i) = cte {
                let cte = ctes[i];
                let query = select(cte.ctequery.as_deref()?)?;
                let names = string_names(&cte.aliascolnames);
                // A CTE sees the ones listed before it.
                (
                    rv.relname.clone(),
                    rv.alias.as_ref(),
                    query,
                    &ctes[i + 1..],
                    names,
                    false,
                )
            } else {
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                let class = snapshot.resolve_table(schema, &rv.relname)?;
                if class.relkind != crate::pg_catalog::RelKind::View {
                    return None;
                }
                // The view's query as it was bound (its names may have
                // come to mean other objects since).
                crate::ddl::views::reanalyze_view(snapshot, class.oid)?;
                let query = select(&crate::ddl::views::view_query(snapshot, class.oid)?)?;
                let names = snapshot
                    .attributes_of(class.oid)
                    .iter()
                    .filter(|a| a.attnum > 0)
                    .map(|a| a.attname.clone())
                    .collect();
                (
                    rv.relname.clone(),
                    rv.alias.as_ref(),
                    query,
                    &[][..],
                    names,
                    true,
                )
            }
        }
        node::Node::RangeSubselect(rs) => {
            let query = select(rs.subquery.as_deref()?)?;
            // Without an alias, it can't be referred to by name.
            (
                String::new(),
                rs.alias.as_ref(),
                query,
                &[][..],
                Vec::new(),
                false,
            )
        }
        _ => return None,
    };
    // A subquery doesn't see the CTEs of the query it is in only when it
    // is a view's; the others do.
    let inner_ctes: Vec<&protobuf::CommonTableExpr> =
        if matches!(item.node, Some(node::Node::RangeSubselect(_))) {
            ctes.to_vec()
        } else {
            inner_ctes.to_vec()
        };
    let columns = failing_columns(&query, snapshot, &inner_ctes, depth + 1, stored || view)?;
    if names.is_empty() {
        let named = leftmost(&query);
        names = named
            .target_list
            .iter()
            .map(|t| match t.node.as_ref() {
                Some(node::Node::ResTarget(rt)) if !rt.name.is_empty() => rt.name.clone(),
                Some(node::Node::ResTarget(rt)) => rt
                    .val
                    .as_deref()
                    .and_then(super::target_list::infer_column_name)
                    .unwrap_or_else(|| "?column?".to_owned()),
                _ => String::new(),
            })
            .collect();
        if !named.values_lists.is_empty() {
            names = (1..=columns.len()).map(|i| format!("column{i}")).collect();
        }
    }
    let mut name = name;
    if let Some(alias) = alias {
        name = alias.aliasname.clone();
        for (slot, n) in names.iter_mut().zip(string_names(&alias.colnames)) {
            *slot = n;
        }
    }
    // A target list naming the same column as another, or `*`, would
    // need its expansion: stay out.
    if names.len() != columns.len() {
        return None;
    }
    Some(SourceRows {
        name,
        names,
        columns,
    })
}

fn string_names(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}
