//! Expand `GROUP BY` clauses with `GROUPING SETS`/`ROLLUP`/`CUBE` into the
//! flat list of grouping sets that PG would generate, and derive which
//! columns are *omitted* by at least one set.
//!
//! The result drives the nullability promotion in
//! [`crate::nullability::NullabilityContext::grouping_omitted`]: a column
//! that appears in every set keeps its base nullability, while one that is
//! present in some sets but absent from others must be reported as nullable
//! in the projection (PG fills those rows with NULL).
//!
//! For ordinary `GROUP BY a, b` (no `GroupingSet` nodes), the function
//! returns an empty set — every column is in the single grouping set, so no
//! promotion is needed.

use std::collections::HashSet;

use typedpg_pg_query::protobuf::{self, GroupingSetKind, node};

use crate::error::AnalyzeError;
use crate::expr;
use crate::pg_catalog::{ConType, PgCatalog};
use crate::resolve::ExpandedTarget;
use crate::scope::Scope;

/// Result of expanding a `GROUP BY` clause that contains
/// `GROUPING SETS`/`ROLLUP`/`CUBE`.
#[derive(Debug, Default)]
pub(crate) struct GroupingExpansion {
    /// Columns present in some grouping sets but absent from others — must
    /// be reported as nullable in the projection.
    pub omitted: HashSet<(String, String)>,
    /// Whether any grouping set is empty (i.e. aggregates the whole input).
    /// Non-COUNT aggregates can return NULL for that row when the input is
    /// empty, so they must be promoted to nullable.
    pub has_empty_set: bool,
}

/// Resolve `group_clause` into the columns that some grouping set omits and
/// whether any of those sets is empty.
///
/// Only plain `ColumnRef`s are tracked; non-column expressions in the
/// `GROUP BY` (e.g. `date_trunc('day', ts)`) contribute nothing — the
/// projection references them by the inner column anyway, and that column
/// might or might not be in scope.
pub(crate) fn expand_grouping_sets(
    group_clause: &[protobuf::Node],
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
) -> GroupingExpansion {
    if group_clause.is_empty() {
        return GroupingExpansion::default();
    }

    // Per top-level entry, the alternatives it contributes (each alternative
    // is one grouping set fragment).
    let mut per_entry: Vec<Vec<HashSet<(String, String)>>> = Vec::new();
    let mut saw_grouping_set = false;
    for node in group_clause {
        let alts = alternatives_for(node, scope, targets, &mut saw_grouping_set);
        per_entry.push(alts);
    }

    if !saw_grouping_set {
        // Plain `GROUP BY a, b, …` — single grouping set, no omissions, and
        // (assuming `group_clause` is non-empty) not the empty grouping set.
        return GroupingExpansion::default();
    }

    // Cartesian product of alternatives — each combination yields one final
    // grouping set (the union of one alternative from each entry).
    let mut sets: Vec<HashSet<(String, String)>> = vec![HashSet::new()];
    for alts in &per_entry {
        if alts.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(sets.len() * alts.len());
        for prefix in &sets {
            for alt in alts {
                let mut combined = prefix.clone();
                combined.extend(alt.iter().cloned());
                next.push(combined);
            }
        }
        sets = next;
    }

    if sets.is_empty() {
        return GroupingExpansion::default();
    }

    let union: HashSet<(String, String)> = sets.iter().flatten().cloned().collect();
    let mut intersection = sets[0].clone();
    for s in &sets[1..] {
        intersection.retain(|x| s.contains(x));
    }
    let omitted = union.difference(&intersection).cloned().collect();
    let has_empty_set = sets.iter().any(|s| s.is_empty());
    GroupingExpansion {
        omitted,
        has_empty_set,
    }
}

/// Alternatives contributed by one node in the top-level `group_clause`.
fn alternatives_for(
    node: &protobuf::Node,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<(String, String)>> {
    match node.node.as_ref() {
        Some(node::Node::GroupingSet(gs)) => {
            *saw_grouping_set = true;
            alternatives_for_grouping_set(gs, scope, targets, saw_grouping_set)
        }
        _ => vec![singleton_set(node, scope, targets)],
    }
}

fn alternatives_for_grouping_set(
    gs: &protobuf::GroupingSet,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<(String, String)>> {
    let kind = GroupingSetKind::try_from(gs.kind).unwrap_or(GroupingSetKind::Undefined);
    match kind {
        GroupingSetKind::GroupingSetEmpty => vec![HashSet::new()],
        GroupingSetKind::GroupingSetSimple => {
            // `(a, b)` — a single set with the union of its members.
            let mut set = HashSet::new();
            for item in &gs.content {
                set.extend(singleton_set(item, scope, targets));
            }
            vec![set]
        }
        GroupingSetKind::GroupingSetRollup => {
            // ROLLUP(a, b, c) → [{a,b,c}, {a,b}, {a}, {}]
            let items: Vec<HashSet<(String, String)>> = gs
                .content
                .iter()
                .map(|n| singleton_set(n, scope, targets))
                .collect();
            let mut alts = Vec::with_capacity(items.len() + 1);
            for cut in (0..=items.len()).rev() {
                let mut s = HashSet::new();
                for item in &items[..cut] {
                    s.extend(item.iter().cloned());
                }
                alts.push(s);
            }
            alts
        }
        GroupingSetKind::GroupingSetCube => {
            // CUBE(a, b) → powerset — 2^n sets.
            let items: Vec<HashSet<(String, String)>> = gs
                .content
                .iter()
                .map(|n| singleton_set(n, scope, targets))
                .collect();
            let n = items.len();
            let mut alts = Vec::with_capacity(1usize << n.min(16));
            for mask in 0..(1u32 << n) {
                let mut s = HashSet::new();
                for (i, item) in items.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        s.extend(item.iter().cloned());
                    }
                }
                alts.push(s);
            }
            alts
        }
        GroupingSetKind::GroupingSetSets => {
            // GROUPING SETS (s1, s2, …) — concatenate the alternatives of
            // each child. Each child is itself either an expression (one
            // alt = singleton) or a nested `GroupingSet`.
            let mut alts = Vec::new();
            for child in &gs.content {
                alts.extend(alternatives_for(child, scope, targets, saw_grouping_set));
            }
            if alts.is_empty() {
                vec![HashSet::new()]
            } else {
                alts
            }
        }
        GroupingSetKind::Undefined => vec![HashSet::new()],
    }
}

/// Resolve a GROUP BY leaf as a single column. Returns a singleton set
/// `{(table_alias, column_name)}` when the leaf denotes a column — directly,
/// or through a projection ordinal / output alias ([`group_leaf`]) — or an
/// empty set otherwise (opaque expression — does not drive nullability
/// promotion).
fn singleton_set(
    node: &protobuf::Node,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
) -> HashSet<(String, String)> {
    match group_leaf(node, scope, targets) {
        GroupLeaf::Column(key) => HashSet::from([key]),
        GroupLeaf::Other | GroupLeaf::Unresolved => HashSet::new(),
    }
}

/// What a GROUP BY leaf denotes.
enum GroupLeaf {
    /// A plain column `(table_alias, column_name)`.
    Column((String, String)),
    /// Some other expression (or an out-of-range ordinal, reported elsewhere).
    Other,
    /// A column reference that neither resolves nor names an output column.
    Unresolved,
}

/// PG's `findTargetlistEntrySQL92` for a GROUP BY leaf: an integer constant
/// is a position in the (star-expanded) target list, a bare name is an
/// input column first and an output-column alias otherwise, anything else is
/// an expression of its own.
fn group_leaf(node: &protobuf::Node, scope: &Scope, targets: &[ExpandedTarget<'_>]) -> GroupLeaf {
    let target_column = |rt: &protobuf::ResTarget| {
        rt.val
            .as_deref()
            .and_then(|v| resolve_group_column(v, scope))
            .map_or(GroupLeaf::Other, GroupLeaf::Column)
    };
    match node.node.as_ref() {
        Some(node::Node::AConst(ac)) => match &ac.val {
            Some(protobuf::a_const::Val::Ival(i)) => {
                match targets.get((i.ival as usize).wrapping_sub(1)) {
                    Some(ExpandedTarget::Expr(rt)) => target_column(rt),
                    Some(ExpandedTarget::StarColumn {
                        column: Some((alias, col)),
                        ..
                    }) => GroupLeaf::Column((alias.to_string(), col.to_string())),
                    _ => GroupLeaf::Other,
                }
            }
            _ => GroupLeaf::Other,
        },
        Some(node::Node::ColumnRef(cr)) => {
            if let Some(key) = resolve_group_column(node, scope) {
                return GroupLeaf::Column(key);
            }
            let alias = match cr.fields.as_slice() {
                [f] => match f.node.as_ref() {
                    Some(node::Node::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                },
                _ => None,
            };
            let target = alias.and_then(|a| {
                targets.iter().find_map(|t| match t {
                    ExpandedTarget::Expr(rt) if rt.name == a => Some(*rt),
                    _ => None,
                })
            });
            target.map_or(GroupLeaf::Unresolved, target_column)
        }
        _ => GroupLeaf::Other,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// GROUP BY validation (PG's parseCheckAggregates)
// ──────────────────────────────────────────────────────────────────────────────

/// PG's `parseCheckAggregates` (SQLSTATE 42803): in a *grouped* query — one
/// with `GROUP BY`, `HAVING`, or a plain (non-windowed) aggregate in the
/// projection / `HAVING` / `ORDER BY` — every column referenced outside an
/// aggregate must appear in the `GROUP BY`, or be functionally determined by
/// it via a fully-grouped primary key. Otherwise PG rejects with
/// `column "rel.col" must appear in the GROUP BY clause or be used in an
/// aggregate function`.
///
/// Deliberately conservative — over-rejecting valid SQL is the worst outcome,
/// so the check runs only when every `GROUP BY` entry is a plain, resolvable
/// column (expression grouping like `GROUP BY a+b` is skipped: matching a
/// target sub-expression against it needs semantic equality we don't model),
/// it honours the primary-key functional dependency, and it never descends
/// into subqueries or aggregate/window arguments. Columns resolving to an
/// outer query level are left to that query's own grouping.
pub(crate) fn check_grouping(
    sel: &protobuf::SelectStmt,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    use std::collections::HashSet;

    check_grouping_func_args(sel, scope, targets)?;

    // Grouped query? (A GROUPING(…) call makes it one, like an aggregate.)
    let mut grouped = !sel.group_clause.is_empty() || sel.having_clause.is_some();
    if !grouped {
        grouped = sel.target_list.iter().any(|t| match t.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt.val.as_deref().is_some_and(|v| {
                let kinds = expr::detect_func_kinds(v, snapshot);
                kinds.has_aggregate || kinds.has_grouping
            }),
            _ => false,
        });
    }
    if !grouped {
        return Ok(());
    }

    // Grouped columns — recursing into GROUPING SETS / ROLLUP / CUBE: a
    // column appearing in *any* grouping set satisfies the 42803 rule (rows
    // from sets that omit it just carry NULL there). Bail out (lenient) on
    // any non-plain-column leaf.
    let mut grouped_cols: HashSet<(String, String)> = HashSet::new();
    for g in &sel.group_clause {
        if !collect_grouped_columns(g, scope, targets, &mut grouped_cols) {
            return Ok(());
        }
    }

    // Columns of local sources — only these are subject to the check.
    let mut local_cols: HashSet<(String, String)> = HashSet::new();
    for src in &scope.sources {
        for c in &src.columns {
            local_cols.insert((c.table_alias.clone(), c.name.clone()));
        }
    }

    // Primary-key functional dependency: when a table's entire PK is grouped,
    // all of its columns are functionally determined and need not be grouped.
    let mut fully_grouped: HashSet<String> = HashSet::new();
    for src in &scope.sources {
        let Some(qn) = &src.source_qn else {
            continue;
        };
        let Some(class) = snapshot.resolve_table(Some(&qn.schema), &qn.name) else {
            continue;
        };
        let attrs = snapshot.attributes_of(class.oid);
        if let Some(pk) = snapshot
            .pg_constraint_values()
            .find(|c| c.conrelid == class.oid && matches!(c.contype, ConType::PrimaryKey))
        {
            let all_grouped = !pk.conkey.is_empty()
                && pk.conkey.iter().all(|&attnum| {
                    attrs.iter().find(|a| a.attnum == attnum).is_some_and(|a| {
                        grouped_cols.contains(&(src.alias.clone(), a.attname.clone()))
                    })
                });
            if all_grouped {
                fully_grouped.insert(src.alias.clone());
            }
        }
    }

    // The first ungrouped column in the projection / HAVING / ORDER BY is the
    // error PG reports. The projection is the star-expanded one: each column
    // a `*` contributes is a plain column reference of its own.
    let mut star_ungrouped = None;
    let mut nodes: Vec<&protobuf::Node> = Vec::new();
    for t in targets {
        match *t {
            ExpandedTarget::Expr(rt) => nodes.extend(rt.val.as_deref()),
            ExpandedTarget::StarColumn {
                column: Some((alias, col)),
                location,
            } => {
                let key = (alias.to_string(), col.to_string());
                if local_cols.contains(&key)
                    && !grouped_cols.contains(&key)
                    && !fully_grouped.contains(alias)
                {
                    star_ungrouped = Some((key.0, key.1, location));
                    break;
                }
            }
            ExpandedTarget::StarColumn { column: None, .. } => {}
        }
    }
    // Only the target-list expressions before an offending star column can
    // precede it.
    let n_target_nodes = nodes.len();
    if let Some(having) = sel.having_clause.as_deref() {
        nodes.push(having);
    }
    for s in &sel.sort_clause {
        if let Some(node::Node::SortBy(sb)) = s.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
        {
            nodes.push(inner);
        }
    }
    // Named windows: PG adds their PARTITION BY / ORDER BY expressions to
    // the target list as resjunk entries, so they are checked too.
    for w in &sel.window_clause {
        if let Some(node::Node::WindowDef(wd)) = w.node.as_ref() {
            nodes.extend(window_def_exprs(wd));
        }
    }
    let searched = if star_ungrouped.is_some() {
        &nodes[..n_target_nodes]
    } else {
        &nodes[..]
    };
    let ungrouped = searched
        .iter()
        .find_map(|node| {
            find_ungrouped(
                node,
                scope,
                snapshot,
                &grouped_cols,
                &local_cols,
                &fully_grouped,
            )
        })
        .or(star_ungrouped);
    if let Some((alias, col, location)) = ungrouped {
        // Point the caret at the offending column reference and hint at the
        // fix — PG reports the same message but with only a cursor position.
        let span = crate::error::SourceSpan::from_node_qname(location);
        return Err(crate::error::RawError::new(
            AnalyzeError::GroupingError(format!(
                "column \"{alias}.{col}\" must appear in the GROUP BY clause \
                 or be used in an aggregate function"
            )),
            span,
            Some(format!(
                "add `{alias}.{col}` to the GROUP BY clause, or wrap it in an aggregate like max({col})"
            )),
        )
        .with_primary_label("not in GROUP BY")
        .finalize_implicit());
    }
    Ok(())
}

/// Resolve a `GROUP BY` entry as a single column against `scope`. Returns the
/// `(table_alias, column_name)` for a plain `ColumnRef`, or `None` for any
/// other shape (expression, grouping set, select-list alias, …).
/// Collect every plain column mentioned by a GROUP BY entry into `out`,
/// descending through `GroupingSet` nodes (ROLLUP/CUBE/GROUPING SETS nest
/// them; the empty set has no content and is trivially fine). Returns
/// `false` when a leaf isn't a resolvable plain column — the caller then
/// skips the whole check rather than risk a false 42803.
/// PG's `finalize_grouping_exprs` (parse_agg.c): every argument of a
/// `GROUPING(…)` call of this query level (projection, HAVING, ORDER BY)
/// must be one of the GROUP BY expressions — `arguments to GROUPING must be
/// grouping expressions of the associated query level` (42803) otherwise.
///
/// PG compares transformed expressions with `equal()`. Modeled exactly for
/// a column argument: it can only equal a GROUP BY entry that is the same
/// column (directly, or via a projection ordinal / alias whose expression is
/// that column). Other arguments are only rejected when there is no GROUP BY
/// at all, and nothing is rejected when a GROUP BY entry can't be resolved.
fn check_grouping_func_args(
    sel: &protobuf::SelectStmt,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
) -> Result<(), AnalyzeError> {
    let mut calls: Vec<&protobuf::GroupingFunc> = Vec::new();
    for t in &sel.target_list {
        if let Some(node::Node::ResTarget(rt)) = t.node.as_ref()
            && let Some(v) = &rt.val
        {
            collect_grouping_funcs(v, &mut calls);
        }
    }
    if let Some(h) = &sel.having_clause {
        collect_grouping_funcs(h, &mut calls);
    }
    for sb in &sel.sort_clause {
        collect_grouping_funcs(sb, &mut calls);
    }
    if calls.is_empty() {
        return Ok(());
    }

    // The GROUP BY entries that are plain columns; `unresolved` when some
    // entry that could be a column couldn't be resolved.
    let mut columns: HashSet<(String, String)> = HashSet::new();
    let mut unresolved = false;
    let mut stack: Vec<&protobuf::Node> = sel.group_clause.iter().collect();
    while let Some(g) = stack.pop() {
        match g.node.as_ref() {
            Some(node::Node::GroupingSet(gs)) => stack.extend(gs.content.iter()),
            _ => match group_leaf(g, scope, targets) {
                GroupLeaf::Column(key) => {
                    columns.insert(key);
                }
                GroupLeaf::Unresolved => unresolved = true,
                GroupLeaf::Other => {}
            },
        }
    }

    let error = || {
        AnalyzeError::GroupingError(
            "arguments to GROUPING must be grouping expressions of the associated query level"
                .into(),
        )
    };
    for call in calls {
        for arg in &call.args {
            if sel.group_clause.is_empty() {
                return Err(error());
            }
            if matches!(arg.node.as_ref(), Some(node::Node::ColumnRef(_)))
                && !unresolved
                && let Some(key) = resolve_group_column(arg, scope)
                && !columns.contains(&key)
            {
                return Err(error());
            }
        }
    }
    Ok(())
}

/// `GROUPING(…)` calls of this query level inside `n` (not descending into
/// sub-selects, which are their own level).
fn collect_grouping_funcs<'a>(n: &'a protobuf::Node, out: &mut Vec<&'a protobuf::GroupingFunc>) {
    let each = |ns: &'a [protobuf::Node], out: &mut Vec<&'a protobuf::GroupingFunc>| {
        for n in ns {
            collect_grouping_funcs(n, out);
        }
    };
    match n.node.as_ref() {
        Some(node::Node::GroupingFunc(g)) => out.push(g),
        Some(node::Node::AExpr(e)) => {
            for x in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                collect_grouping_funcs(x, out);
            }
        }
        Some(node::Node::BoolExpr(b)) => each(&b.args, out),
        Some(node::Node::FuncCall(f)) => each(&f.args, out),
        Some(node::Node::TypeCast(c)) => {
            if let Some(a) = &c.arg {
                collect_grouping_funcs(a, out);
            }
        }
        Some(node::Node::CoalesceExpr(c)) => each(&c.args, out),
        Some(node::Node::MinMaxExpr(m)) => each(&m.args, out),
        Some(node::Node::RowExpr(r)) => each(&r.args, out),
        Some(node::Node::List(l)) => each(&l.items, out),
        Some(node::Node::NullTest(t)) => {
            if let Some(a) = &t.arg {
                collect_grouping_funcs(a, out);
            }
        }
        Some(node::Node::CaseExpr(c)) => {
            for x in [&c.arg, &c.defresult].into_iter().flatten() {
                collect_grouping_funcs(x, out);
            }
            each(&c.args, out);
        }
        Some(node::Node::CaseWhen(w)) => {
            for x in [&w.expr, &w.result].into_iter().flatten() {
                collect_grouping_funcs(x, out);
            }
        }
        Some(node::Node::SortBy(sb)) => {
            if let Some(x) = &sb.node {
                collect_grouping_funcs(x, out);
            }
        }
        _ => {}
    }
}

fn collect_grouped_columns(
    node: &protobuf::Node,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    out: &mut std::collections::HashSet<(String, String)>,
) -> bool {
    if let Some(node::Node::GroupingSet(gs)) = node.node.as_ref() {
        return gs
            .content
            .iter()
            .all(|inner| collect_grouped_columns(inner, scope, targets, out));
    }
    match group_leaf(node, scope, targets) {
        GroupLeaf::Column(key) => {
            out.insert(key);
            true
        }
        GroupLeaf::Other | GroupLeaf::Unresolved => false,
    }
}

fn resolve_group_column(node: &protobuf::Node, scope: &Scope) -> Option<(String, String)> {
    let node::Node::ColumnRef(cr) = node.node.as_ref()? else {
        return None;
    };
    let (table, column) = column_ref_parts(cr)?;
    scope
        .resolve_column(table, column, None)
        .ok()
        .map(|c| (c.table_alias.clone(), c.name.clone()))
}

/// Split a `ColumnRef`'s string fields into an optional table qualifier and
/// the column name. Returns `None` for a bare `*` / qualified `t.*` (no string
/// column name) or an unexpected shape.
fn column_ref_parts(cr: &protobuf::ColumnRef) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = cr
        .fields
        .iter()
        .filter_map(|f| match f.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [col] => Some((None, *col)),
        [tbl, col] => Some((Some(*tbl), *col)),
        [_schema, tbl, col] => Some((Some(*tbl), *col)),
        _ => None,
    }
}

/// The PARTITION BY and ORDER BY expressions of a window definition.
fn window_def_exprs(wd: &protobuf::WindowDef) -> impl Iterator<Item = &protobuf::Node> {
    wd.partition_clause.iter().chain(
        wd.order_clause
            .iter()
            .filter_map(|o| match o.node.as_ref() {
                Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                _ => None,
            }),
    )
}

/// Whether a `FuncCall` without `OVER` is an aggregate — its argument
/// columns don't need to be grouped, so the grouping walk skips it entirely.
/// A window call is *not* skipped: it runs over the grouped rows, so its
/// arguments, FILTER and window clauses are checked like any expression
/// (PG's `check_ungrouped_columns_walker` only stops at an `Aggref`).
fn is_plain_aggregate(fc: &protobuf::FuncCall, snapshot: &PgCatalog) -> bool {
    if fc.over.is_some() {
        return false;
    }
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return false,
    };
    snapshot
        .find_functions(schema, name)
        .iter()
        .any(|f| matches!(f.prokind, crate::pg_catalog::ProKind::Aggregate))
}

/// Find the first column reference in `node` that is local, not grouped, not
/// functionally determined by a grouped PK, and not inside an aggregate /
/// window call. Mirrors PG's `check_ungrouped_columns` walk; subqueries are
/// not descended into (a `SubLink` is its own scope).
fn find_ungrouped(
    node: &protobuf::Node,
    scope: &Scope,
    snapshot: &PgCatalog,
    grouped: &std::collections::HashSet<(String, String)>,
    local: &std::collections::HashSet<(String, String)>,
    fully_grouped: &std::collections::HashSet<String>,
) -> Option<(String, String, i32)> {
    let inner = node.node.as_ref()?;
    match inner {
        node::Node::ColumnRef(cr) => {
            let (table, column) = column_ref_parts(cr)?;
            let sc = scope.resolve_column(table, column, None).ok()?;
            let key = (sc.table_alias.clone(), sc.name.clone());
            if local.contains(&key) && !grouped.contains(&key) && !fully_grouped.contains(&key.0) {
                Some((key.0, key.1, cr.location))
            } else {
                None
            }
        }
        node::Node::FuncCall(fc) => {
            if is_plain_aggregate(fc, snapshot) {
                return None;
            }
            let window = fc.over.as_deref().into_iter().flat_map(window_def_exprs);
            fc.args
                .iter()
                .chain(fc.agg_filter.as_deref())
                .chain(window)
                .find_map(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped))
        }
        node::Node::AExpr(e) => e
            .lexpr
            .as_deref()
            .and_then(|l| find_ungrouped(l, scope, snapshot, grouped, local, fully_grouped))
            .or_else(|| {
                e.rexpr
                    .as_deref()
                    .and_then(|r| find_ungrouped(r, scope, snapshot, grouped, local, fully_grouped))
            }),
        node::Node::BoolExpr(b) => b
            .args
            .iter()
            .find_map(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::CoalesceExpr(c) => c
            .args
            .iter()
            .find_map(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::CaseExpr(c) => c
            .args
            .iter()
            .find_map(|w| find_ungrouped(w, scope, snapshot, grouped, local, fully_grouped))
            .or_else(|| {
                c.defresult
                    .as_deref()
                    .and_then(|d| find_ungrouped(d, scope, snapshot, grouped, local, fully_grouped))
            }),
        node::Node::CaseWhen(w) => w
            .expr
            .as_deref()
            .and_then(|e| find_ungrouped(e, scope, snapshot, grouped, local, fully_grouped))
            .or_else(|| {
                w.result
                    .as_deref()
                    .and_then(|r| find_ungrouped(r, scope, snapshot, grouped, local, fully_grouped))
            }),
        node::Node::TypeCast(c) => c
            .arg
            .as_deref()
            .and_then(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::NamedArgExpr(na) => na
            .arg
            .as_deref()
            .and_then(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::NullTest(t) => t
            .arg
            .as_deref()
            .and_then(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::BooleanTest(t) => t
            .arg
            .as_deref()
            .and_then(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::AArrayExpr(a) => a
            .elements
            .iter()
            .find_map(|e| find_ungrouped(e, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::AIndirection(ind) => ind
            .arg
            .as_deref()
            .and_then(|a| find_ungrouped(a, scope, snapshot, grouped, local, fully_grouped)),
        node::Node::List(l) => l
            .items
            .iter()
            .find_map(|i| find_ungrouped(i, scope, snapshot, grouped, local, fully_grouped)),
        // A SubLink is its own scope; columns inside it are governed there.
        node::Node::SubLink(_) => None,
        _ => None,
    }
}
