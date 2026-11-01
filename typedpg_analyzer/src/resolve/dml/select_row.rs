//! The row an `INSERT ... SELECT` surely stores.
//!
//! Some SELECTs yield a row on every execution, whatever the tables hold:
//! one without FROM, one computing aggregates without GROUP BY (or with
//! `GROUP BY ()`) nor HAVING (a row even over no input), one reading only
//! such a subquery or CTE — when no set-returning function, LIMIT 0 or
//! OFFSET drops it, and its WHERE holds over the row's constants.
//! An INSERT storing that row fails every execution the row fails: a NULL
//! into a NOT NULL column (or one left to a NULL default), a constant out
//! of its column's range. Unlike a VALUES row, or a SELECT the planner
//! pulls up and folds, a subquery that isn't pulled up (`count(*)`,
//! `OFFSET 0`, a volatile output) is evaluated, and its outputs converted
//! to the columns' types, by the executor: PREPARE succeeds, every EXECUTE
//! fails.

use super::*;

/// The SELECT levels followed at most.
const MAX_DEPTH: usize = 16;

/// What each output column of the row a SELECT surely yields holds: the
/// constant it is, when known (positions past the end: unknown).
type Outputs<'a> = Vec<Option<&'a protobuf::Node>>;

/// A FROM subquery's row, its columns' names and its alias.
type Subquery<'a> = (Outputs<'a>, Vec<Option<&'a str>>, Option<&'a str>);

/// The error every execution of INSERT ... SELECT `sel` (its output
/// columns typed `column_types`; `with`, the INSERT's WITH) raises
/// storing the row `sel` surely yields, as [`row_violation`] orders them — a conversion of a constant
/// to its column failing first, as the row is built. None when the
/// statement isn't run, or a DO INSTEAD rule replaces the INSERT.
pub(super) fn sure_row_violation(
    sel: &protobuf::SelectStmt,
    with: Option<&protobuf::WithClause>,
    tgt: &InsertTarget,
    column_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<AnalyzeError> {
    if !crate::resolve::executes() || returning_rewritten(snapshot, tgt.oid, &[DmlEvent::Insert]) {
        return None;
    }
    let ctes: Vec<&protobuf::CommonTableExpr> = match with {
        Some(w) if !w.recursive => w
            .ctes
            .iter()
            .filter_map(|c| match c.node.as_ref() {
                Some(node::Node::CommonTableExpr(c)) => Some(&**c),
                _ => None,
            })
            .collect(),
        Some(_) => return None,
        None => Vec::new(),
    };
    let outputs = sure_row(sel, snapshot, &ctes, 0)?;
    let items: Vec<protobuf::Node> = (0..column_types.len())
        .map(|i| {
            outputs
                .get(i)
                .copied()
                .flatten()
                .cloned()
                .unwrap_or(protobuf::Node { node: None })
        })
        .collect();
    let known_at = |i: usize| outputs.get(i).copied().flatten().is_some();
    let stored = |i: usize| {
        let tc = target_col_at(tgt, i)?;
        (tgt.col_indirection.get(i).is_none_or(Vec::is_empty) && !tgt.discards(tc)).then_some(tc)
    };
    // The conversions to the columns' types, as the row is built.
    let mut violations: Vec<RowViolation> = items
        .iter()
        .enumerate()
        .filter(|&(i, _)| known_at(i))
        .filter_map(|(i, v)| {
            let tc = stored(i)?;
            let error = crate::typmod::evaluated_assignment_error(
                snapshot,
                tc.atttypid,
                snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                v,
            )?;
            Some(RowViolation {
                order: (false, tc.attnum),
                error,
            })
        })
        .collect();
    // A NULL, read through the SELECT's output type; a value, as the
    // column reads it only when that is the column's type.
    let known = |i: usize| match (known_at(i), target_col_at(tgt, i), column_types.get(i)) {
        (true, Some(tc), Some(&t)) => {
            let source = (t != oid::UNKNOWN).then_some(t);
            if is_null_value(&items[i], tc, source, snapshot) {
                Known::Null
            } else if t == snapshot.unwrap_domain(tc.atttypid) {
                Known::Value(&items[i])
            } else {
                Known::Unknown
            }
        }
        _ => Known::Unknown,
    };
    violations.extend(row_violation(tgt, &items, &known, snapshot));
    violations
        .into_iter()
        .min_by_key(|v| v.order)
        .map(|v| v.error)
}

/// The row `sel` yields on every execution (see the module docs), as
/// [`Outputs`]; `None` when it may yield none. `ctes` are the CTEs its
/// table names may mean.
fn sure_row<'a>(
    sel: &'a protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&'a protobuf::CommonTableExpr],
    depth: usize,
) -> Option<Outputs<'a>> {
    let inner = sure_level(sel, snapshot, ctes, depth)?;
    let mut out: Outputs<'a> = Vec::with_capacity(sel.target_list.len());
    for t in &sel.target_list {
        let Some(node::Node::ResTarget(rt)) = t.node.as_ref() else {
            return Some(Vec::new());
        };
        let Some(val) = rt.val.as_deref() else {
            return Some(Vec::new());
        };
        if let Some(node::Node::ColumnRef(cr)) = val.node.as_ref() {
            if is_star(cr) {
                // `*` or `s.*` alone reads the subquery's columns as they
                // are; beside other items, positions are not followed.
                return Some(match &inner {
                    Some((outputs, _, _)) if sel.target_list.len() == 1 => outputs.clone(),
                    _ => Vec::new(),
                });
            }
            out.push(column_value(inner.as_ref(), cr));
            continue;
        }
        out.push(is_constant(val).then_some(val));
    }
    Some(out)
}

/// Whether `sel` yields a row on every execution, its select list
/// evaluated over it (see the module docs): `Some` of the subquery it
/// reads alone, if any. A WHERE must hold over that row's constants —
/// whether the planner pushes it down into the subquery and folds it, or
/// evaluates it over the row.
fn sure_level<'a>(
    sel: &'a protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&'a protobuf::CommonTableExpr],
    depth: usize,
) -> Option<Option<Subquery<'a>>> {
    if depth > MAX_DEPTH
        || sel.op != SetOperation::SetopNone as i32
        || !sel.values_lists.is_empty()
        || sel.with_clause.is_some()
        || sel.into_clause.is_some()
        || !sel.locking_clause.is_empty()
        || !sel.group_clause.iter().all(empty_grouping_set)
        || sel.having_clause.is_some()
        || !keeps_first_row(sel)
        || !crate::resolve::walk::level_srf_calls(sel, snapshot).is_empty()
    {
        return None;
    }
    // A subquery read alone.
    let mut inner: Option<Subquery<'a>> = None;
    if sel.group_clause.is_empty() && !aggregates(sel, snapshot) {
        match sel.from_clause.as_slice() {
            [] => {}
            [from] => inner = Some(from_subquery(from, snapshot, ctes, depth)?),
            _ => return None,
        }
        if let Some(qual) = sel.where_clause.as_deref() {
            let column = |cr: &protobuf::ColumnRef| column_value(inner.as_ref(), cr);
            if crate::resolve::failing_rows::qual_truth(qual, &column) != Some(true) {
                return None;
            }
        }
    }
    Some(inner)
}

/// A `*` or `s.*` column reference.
fn is_star(cr: &protobuf::ColumnRef) -> bool {
    cr.fields
        .last()
        .is_some_and(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))))
}

/// The constant the column `cr` reads of the subquery `inner` holds: one
/// of its outputs, named once.
fn column_value<'a>(
    inner: Option<&Subquery<'a>>,
    cr: &protobuf::ColumnRef,
) -> Option<&'a protobuf::Node> {
    let (outputs, names, alias) = inner?;
    let fields: Vec<Option<&str>> = cr
        .fields
        .iter()
        .map(|f| match f.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let c = match (fields.as_slice(), alias) {
        ([Some(c)], _) => *c,
        ([Some(a), Some(c)], Some(alias)) if a == alias => *c,
        _ => return None,
    };
    let mut at = names
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == Some(c))
        .map(|(i, _)| i);
    let i = at.next().filter(|_| at.next().is_none())?;
    // An untyped literal is resolved to text as the subquery's output, no
    // longer read as the column's type.
    outputs
        .get(i)
        .copied()
        .flatten()
        .filter(|v| !is_untyped_string(v))
}

/// The error every execution of the SELECT `sel` raises evaluating its
/// select list over the row it surely reads of a subquery the planner
/// doesn't pull up (`x::int2` over `(SELECT 40000 AS x LIMIT 1) s`): the
/// subquery's output is a constant the executor computes, and converts or
/// operates on as the select list says — out of range, a division by
/// zero, on every execution. (Pulled up, the planner folds it, which is
/// the query's analysis' to report.) The leftmost failing output, as
/// ExecProject evaluates them in order.
pub(in crate::resolve) fn sure_select_error(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
) -> Option<AnalyzeError> {
    if !crate::resolve::executes() {
        return None;
    }
    let inner = sure_level(sel, snapshot, &[], 0)??;
    let columns = |cr: &protobuf::ColumnRef| {
        column_value(Some(&inner), cr).map(|n| std::sync::Arc::new(n.clone()))
    };
    sel.target_list.iter().find_map(|t| {
        let Some(node::Node::ResTarget(rt)) = t.node.as_ref() else {
            return None;
        };
        let val = rt.val.as_deref()?;
        crate::fold::error_in(val, snapshot, &columns).map(|e| e.at(val))
    })
}

/// FROM item `from` read alone, when it surely yields a row: a subquery,
/// or a reference to one of `ctes` (inlined or materialized, a CTE yields
/// its query's rows).
fn from_subquery<'a>(
    from: &'a protobuf::Node,
    snapshot: &PgCatalog,
    ctes: &[&'a protobuf::CommonTableExpr],
    depth: usize,
) -> Option<Subquery<'a>> {
    let (sub, renames, alias, visible) = match from.node.as_ref()? {
        node::Node::RangeSubselect(rs) => {
            (rs.subquery.as_deref()?, Vec::new(), rs.alias.as_ref(), ctes)
        }
        // A schema-less name is the nearest CTE so named (a CTE's query
        // sees the CTEs before it).
        node::Node::RangeVar(rv) if rv.schemaname.is_empty() && rv.inh => {
            let k = ctes.iter().rposition(|c| c.ctename == rv.relname)?;
            (
                ctes[k].ctequery.as_deref()?,
                ctes[k].aliascolnames.iter().collect(),
                rv.alias.as_ref(),
                &ctes[..k],
            )
        }
        _ => return None,
    };
    let Some(node::Node::SelectStmt(sub)) = sub.node.as_ref() else {
        return None;
    };
    let outputs = sure_row(sub, snapshot, visible, depth + 1)?;
    let mut names = output_names(sub);
    let alias_names = alias.map(|a| a.colnames.iter().collect::<Vec<_>>());
    for given in [Some(renames), alias_names].into_iter().flatten() {
        for (name, given) in names.iter_mut().zip(given) {
            *name = match given.node.as_ref() {
                Some(node::Node::String(s)) => Some(s.sval.as_str()),
                _ => None,
            };
        }
    }
    let alias = match (alias, from.node.as_ref()) {
        (Some(a), _) => Some(a.aliasname.as_str()),
        (None, Some(node::Node::RangeVar(rv))) => Some(rv.relname.as_str()),
        (None, _) => None,
    };
    Some((outputs, names, alias))
}

/// Whether `sel` computes aggregates (without GROUP BY): one row,
/// whatever its FROM and WHERE leave. Only a call whose every visible overload is
/// an aggregate counts (`is_aggregate_call` takes any).
fn aggregates(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> bool {
    let surely_aggregate = |fc: &protobuf::FuncCall| {
        let parts = expr::extract_string_fields(&fc.funcname);
        let (schema, name) = match parts.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] => (Some(s.as_str()), n.as_str()),
            _ => return false,
        };
        let procs = snapshot.find_functions(schema, name);
        fc.over.is_none()
            && !procs.is_empty()
            && procs
                .iter()
                .all(|p| matches!(p.prokind, crate::pg_catalog::ProKind::Aggregate))
    };
    let mut found = false;
    for t in &sel.target_list {
        crate::resolve::walk::visit_same_level(t, &mut |e| {
            if let Some(node::Node::FuncCall(fc)) = e.node.as_ref()
                && surely_aggregate(fc)
            {
                found = true;
            }
        });
    }
    found
}

/// `GROUP BY ()`: one group, even over no input.
fn empty_grouping_set(n: &protobuf::Node) -> bool {
    matches!(n.node.as_ref(), Some(node::Node::GroupingSet(gs))
        if gs.kind == protobuf::GroupingSetKind::GroupingSetEmpty as i32)
}

/// Whether `sel`'s LIMIT / OFFSET keep its first row: no OFFSET (or
/// `OFFSET 0`), no LIMIT (or one of a positive constant, or ALL / NULL).
fn keeps_first_row(sel: &protobuf::SelectStmt) -> bool {
    use typedpg_pg_query::protobuf::a_const::Val;
    let count = |n: Option<&protobuf::Node>| -> Option<Option<i32>> {
        match n.and_then(|n| n.node.as_ref()) {
            None => Some(None),
            Some(node::Node::AConst(c)) if c.isnull => Some(None),
            Some(node::Node::AConst(c)) => match &c.val {
                Some(Val::Ival(i)) => Some(Some(i.ival)),
                _ => None,
            },
            _ => None,
        }
    };
    matches!(count(sel.limit_offset.as_deref()), Some(None | Some(0)))
        && matches!(
            count(sel.limit_count.as_deref()),
            Some(None) | Some(Some(1..))
        )
}

/// The names `sel`'s output columns are read by, when plain: an `AS`
/// name, or a column reference's (FigureColname's other names aside).
fn output_names(sel: &protobuf::SelectStmt) -> Vec<Option<&str>> {
    sel.target_list
        .iter()
        .map(|t| {
            let Some(node::Node::ResTarget(rt)) = t.node.as_ref() else {
                return None;
            };
            if !rt.name.is_empty() {
                return Some(rt.name.as_str());
            }
            match rt.val.as_deref().and_then(|v| v.node.as_ref()) {
                Some(node::Node::ColumnRef(cr)) => match cr.fields.last()?.node.as_ref() {
                    Some(node::Node::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect()
}

/// A constant: a literal, maybe cast.
fn is_constant(e: &protobuf::Node) -> bool {
    match e.node.as_ref() {
        Some(node::Node::AConst(_)) => true,
        Some(node::Node::TypeCast(tc)) => tc.arg.as_deref().is_some_and(is_constant),
        _ => false,
    }
}

/// An untyped string literal (`'abc'`, not `'abc'::text`).
fn is_untyped_string(e: &protobuf::Node) -> bool {
    matches!(e.node.as_ref(), Some(node::Node::AConst(c))
        if matches!(c.val, Some(typedpg_pg_query::protobuf::a_const::Val::Sval(_))))
}
