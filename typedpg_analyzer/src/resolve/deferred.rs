//! The errors the planner raises planning a subquery it substitutes into
//! the query reading it — a FROM subquery or VALUES list it pulls up, a
//! CTE it inlines or materializes.
//!
//! What fails when the planner folds a constant fails every execution
//! (see [`crate::fold`]), but only where the planner plans it. A pulled-up
//! subquery's WHERE joins the outer query's quals, planned whatever the
//! outer query reads; its outputs replace the outer query's references to
//! them, so one is planned where, and only where, the outer query reads it
//! (`SELECT 1 FROM (SELECT 1 / 0 AS q) s` succeeds, `SELECT q FROM …`
//! fails). A VALUES list of several rows in FROM is planned whole. A
//! subquery the planner doesn't pull up is planned on its own, its unread
//! outputs NULLed first (`remove_unused_subquery_outputs`), or not at all
//! when the outer query proves it empty: nothing in it is certain.

use typedpg_pg_query::protobuf::{self, SetOperation, node};

use crate::error::AnalyzeError;
use crate::expr::PlanTimeError;
use crate::expr::plan_fold::NextLevel;
use crate::pg_catalog::PgCatalog;

/// How the planner plans a FROM subquery (or a CTE's query) the query
/// reading it plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Maybe not at all, or only in part: nothing is certain.
    Uncertain,
    /// Whole, whatever the outer query reads (a VALUES list of several
    /// rows).
    Whole,
    /// Pulled up: its outputs where the outer query reads them, the rest
    /// with the outer query.
    PulledUp,
    /// Certainly not pulled up (it groups, sorts, limits, ... or is a
    /// `security_barrier` view): planned on its own once the outer query
    /// plans the relation it is, not when that relation is proven empty
    /// or removed first — its outputs where the outer query reads them
    /// (`remove_unused_subquery_outputs` drops the others), the rest whole.
    Separate,
}

/// How the planner plans `sel`, a FROM subquery or a CTE's query.
pub(crate) fn plan_of(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> Plan {
    if sel.op != SetOperation::SetopNone as i32 {
        return Plan::Uncertain;
    }
    if !sel.values_lists.is_empty() {
        // `VALUES … ORDER BY / LIMIT` is a subquery over the VALUES list.
        let plain = sel.sort_clause.is_empty()
            && sel.limit_count.is_none()
            && sel.limit_offset.is_none()
            && sel.locking_clause.is_empty()
            && sel.with_clause.is_none();
        return match (plain, sel.values_lists.len()) {
            (false, _) => Plan::Uncertain,
            // A single row is a subquery's target list, pulled up.
            (true, 1) => Plan::PulledUp,
            (true, _) => Plan::Whole,
        };
    }
    if pulled_up_in(sel, snapshot) {
        Plan::PulledUp
    } else if !sel.group_clause.is_empty()
        || sel.having_clause.is_some()
        || !sel.distinct_clause.is_empty()
        || !sel.sort_clause.is_empty()
        || sel.limit_count.is_some()
        || sel.limit_offset.is_some()
        || !sel.locking_clause.is_empty()
        || !sel.window_clause.is_empty()
        || sel.with_clause.is_some()
        || never_simple_target_list(sel, snapshot)
    {
        Plan::Separate
    } else {
        Plan::Uncertain
    }
}

/// Whether `sel`'s select list certainly keeps the planner from pulling
/// it up: it calls an aggregate (`count(*)`, or a name only aggregates
/// have), a window function, or a name only set-returning or volatile
/// functions have.
fn never_simple_target_list(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> bool {
    use crate::pg_catalog::{ProKind, ProVolatile};
    sel.target_list.iter().any(|t| {
        t.node.as_ref().is_some_and(|inner| {
            // Not below a sublink, whose calls are its own.
            let mut todo = vec![inner.to_ref()];
            while let Some(n) = todo.pop() {
                match n {
                    typedpg_pg_query::NodeRef::SubLink(_) => continue,
                    typedpg_pg_query::NodeRef::FuncCall(fc) => {
                        if fc.over.is_some() || fc.agg_star {
                            return true;
                        }
                        let names = crate::expr::extract_string_fields(&fc.funcname);
                        let (schema, name) = match names.as_slice() {
                            [n] => (None, n.as_str()),
                            [s, n] => (Some(s.as_str()), n.as_str()),
                            _ => continue,
                        };
                        let candidates = snapshot.find_functions(schema, name);
                        if !candidates.is_empty()
                            && candidates.iter().all(|p| {
                                matches!(p.prokind, ProKind::Aggregate | ProKind::Window)
                                    || p.proretset
                                    || p.provolatile == ProVolatile::Volatile
                            })
                        {
                            return true;
                        }
                    }
                    _ => {}
                }
                todo.extend(n.children());
            }
            false
        })
    })
}

/// How the level reading a subquery planned as `plan` (never
/// null-extended there when `kept`) analyzes it: `None` when nothing in
/// it is certain to be planned.
fn next_level(plan: Plan, sel: &protobuf::SelectStmt, kept: bool) -> Option<NextLevel> {
    if !crate::expr::plan_time_checks() {
        return None;
    }
    match plan {
        Plan::PulledUp if kept => Some(NextLevel::PulledUp),
        Plan::PulledUp => Some(NextLevel::PulledUpNullable),
        // Planned unless its relation is proven empty, or removed (the
        // nullable side of a join).
        Plan::Separate if kept && !crate::expr::plan_fold::level_may_be_empty() => {
            Some(NextLevel::Separate {
                // subquery_is_pushdown_safe: never into one with a LIMIT.
                pushed_down: sel.limit_count.is_none() && sel.limit_offset.is_none(),
            })
        }
        _ => None,
    }
}

/// Whether the planner certainly pulls `sel` up into the query reading it
/// (`is_simple_subquery`): a plain SELECT with no grouping, DISTINCT,
/// ORDER BY, LIMIT, locking or WITH, whose select list calls no
/// aggregate, window, set-returning or volatile function (and holds no
/// sublink, to be sure).
pub(crate) fn pulled_up_in(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> bool {
    simple_shape(sel) && simple_target_list(sel, snapshot)
}

/// [`pulled_up_in`] for `sel`, a SELECT already analyzed (whose select
/// list's calls the analysis recorded, see [`record_target_list`]).
pub(crate) fn pulled_up(sel: &protobuf::SelectStmt) -> bool {
    simple_shape(sel)
        && (!calls_in_target_list(sel)
            || SIMPLE_TARGETS.with(|m| m.borrow().get(&target_list_key(sel)).copied())
                == Some(true))
}

thread_local! {
    /// Per analyzed SELECT (see [`target_list_key`]): whether its select
    /// list is one [`pulled_up_in`] accepts.
    static SIMPLE_TARGETS: std::cell::RefCell<std::collections::HashMap<(usize, usize, i32), bool>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A key telling an analyzed SELECT apart: its address, and its select
/// list's length and first location (so a SELECT later allocated where a
/// dropped one was isn't taken for it).
fn target_list_key(sel: &protobuf::SelectStmt) -> (usize, usize, i32) {
    let first = sel
        .target_list
        .first()
        .and_then(|t| match t.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => Some(rt.location),
            _ => None,
        })
        .unwrap_or(-1);
    (
        std::ptr::from_ref(sel) as usize,
        sel.target_list.len(),
        first,
    )
}

/// Record, analyzing `sel`, whether its select list is one the planner
/// pulls up (see [`pulled_up`]).
pub(crate) fn record_target_list(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) {
    if calls_in_target_list(sel) {
        let simple = simple_target_list(sel, snapshot);
        SIMPLE_TARGETS.with(|m| m.borrow_mut().insert(target_list_key(sel), simple));
    }
}

/// The shape `is_simple_subquery` asks of a subquery, beside its select
/// list's calls.
fn simple_shape(sel: &protobuf::SelectStmt) -> bool {
    sel.op == SetOperation::SetopNone as i32
        && sel.values_lists.is_empty()
        && sel.with_clause.is_none()
        && sel.group_clause.is_empty()
        && sel.having_clause.is_none()
        && sel.distinct_clause.is_empty()
        && sel.sort_clause.is_empty()
        && sel.limit_count.is_none()
        && sel.limit_offset.is_none()
        && sel.locking_clause.is_empty()
        && sel.window_clause.is_empty()
        && sel.into_clause.is_none()
}

/// Whether `sel`'s select list calls any function or operator, or holds a
/// sublink or GROUPING.
fn calls_in_target_list(sel: &protobuf::SelectStmt) -> bool {
    sel.target_list.iter().any(|t| {
        t.node.as_ref().is_some_and(|n| {
            n.nodes().into_iter().any(|(n, _)| {
                matches!(
                    n,
                    typedpg_pg_query::NodeRef::FuncCall(_)
                        | typedpg_pg_query::NodeRef::SubLink(_)
                        | typedpg_pg_query::NodeRef::GroupingFunc(_)
                )
            })
        })
    })
}

/// Whether `sel`'s select list has nothing that keeps the planner from
/// pulling it up: no aggregate, window or set-returning call
/// (`hasAggs`, `hasWindowFuncs`, `hasTargetSRFs`), nothing volatile
/// (`contain_volatile_functions`), and — to be sure — no sublink nor
/// GROUPING. A function is known by its name: every function of that
/// name must be a plain, non-volatile, non-set-returning one.
fn simple_target_list(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> bool {
    use crate::pg_catalog::{ProKind, ProVolatile};
    let plain = |p: &crate::pg_catalog::PgProc| {
        p.prokind == ProKind::Function && !p.proretset && p.provolatile != ProVolatile::Volatile
    };
    sel.target_list.iter().all(|t| {
        let Some(inner) = t.node.as_ref() else {
            return true;
        };
        inner.nodes().into_iter().all(|(n, _)| match n {
            typedpg_pg_query::NodeRef::SubLink(_) | typedpg_pg_query::NodeRef::GroupingFunc(_) => {
                false
            }
            typedpg_pg_query::NodeRef::FuncCall(fc) => {
                if fc.over.is_some()
                    || fc.agg_star
                    || fc.agg_distinct
                    || fc.agg_within_group
                    || fc.agg_filter.is_some()
                    || !fc.agg_order.is_empty()
                {
                    return false;
                }
                let names = crate::expr::extract_string_fields(&fc.funcname);
                let (schema, name) = match names.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => return false,
                };
                let candidates = snapshot.find_functions(schema, name);
                !candidates.is_empty() && candidates.iter().all(|p| plain(p))
            }
            typedpg_pg_query::NodeRef::AExpr(e) => {
                let names = crate::expr::extract_string_fields(&e.name);
                names.last().is_none_or(|name| {
                    snapshot
                        .pg_operator
                        .values()
                        .filter(|o| &o.oprname == name)
                        .all(|o| {
                            o.oprcode
                                .and_then(|c| snapshot.pg_proc.get(&c))
                                .is_some_and(plain)
                        })
                })
            }
            typedpg_pg_query::NodeRef::TypeCast(_) => !snapshot.pg_cast.values().any(|c| {
                c.castfunc
                    .and_then(|f| snapshot.pg_proc.get(&f))
                    .is_some_and(|p| p.provolatile == ProVolatile::Volatile)
            }),
            _ => true,
        })
    })
}

// ─── Pulled-up outputs ──────────────────────────────────────────────────────

thread_local! {
    /// The expression the planner substitutes for a select-list entry
    /// that reads a pulled-up subquery's column (keyed by the entry's
    /// address, with the column reference itself to tell it apart).
    static SUBSTITUTED: std::cell::RefCell<
        std::collections::HashMap<usize, (protobuf::ColumnRef, std::sync::Arc<protobuf::Node>)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Forget what earlier statements recorded.
pub(crate) fn clear_recorded() {
    SUBSTITUTED.with(|m| m.borrow_mut().clear());
    SIMPLE_TARGETS.with(|m| m.borrow_mut().clear());
}

/// Record that the planner substitutes `expr` for `entry`, a column
/// reference of a select list reading a pulled-up subquery's column.
pub(crate) fn record_substitution(entry: &protobuf::Node, expr: std::sync::Arc<protobuf::Node>) {
    if let Some(node::Node::ColumnRef(cr)) = entry.node.as_ref() {
        SUBSTITUTED.with(|m| {
            m.borrow_mut()
                .insert(std::ptr::from_ref(entry) as usize, (cr.clone(), expr));
        });
    }
}

/// What the planner substitutes for `entry` (see [`record_substitution`]).
pub(crate) fn substituted(entry: &protobuf::Node) -> Option<std::sync::Arc<protobuf::Node>> {
    let Some(node::Node::ColumnRef(cr)) = entry.node.as_ref() else {
        return None;
    };
    SUBSTITUTED.with(|m| {
        m.borrow()
            .get(&(std::ptr::from_ref(entry) as usize))
            .filter(|(r, _)| cr == r)
            .map(|(_, e)| e.clone())
    })
}

/// The expressions of `sel`'s outputs, as the planner substitutes them
/// for references once it pulls `sel` up: each select-list entry (or the
/// single VALUES row's item) — itself what it substitutes, when it reads
/// a column pulled up from below. `None` past a `*`.
pub(crate) fn output_exprs(
    sel: &protobuf::SelectStmt,
) -> Vec<Option<std::sync::Arc<protobuf::Node>>> {
    let entry =
        |n: &protobuf::Node| Some(substituted(n).unwrap_or_else(|| std::sync::Arc::new(n.clone())));
    match sel.values_lists.as_slice() {
        [row] => match row.node.as_ref() {
            Some(node::Node::List(l)) => l.items.iter().map(entry).collect(),
            _ => Vec::new(),
        },
        [] => sel
            .target_list
            .iter()
            .map_while(|t| match t.node.as_ref() {
                Some(node::Node::ResTarget(rt)) if !is_star(t) => {
                    Some(rt.val.as_deref().and_then(entry))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The errors captured analyzing `sel`, by where they are planned.
#[derive(Default)]
pub(crate) struct Split {
    /// Each output column's: planned where the column is read.
    pub outputs: Vec<Option<AnalyzeError>>,
    /// The first of the others (its node's location with it): planned
    /// with the query.
    pub rest: Option<PlanTimeError>,
}

/// Split `errors`, captured analyzing `sel` (see
/// [`crate::expr::capture_plan_time_errors`]), between its output columns
/// and the rest, by the location of the node each is about. One in an
/// output whose column isn't known (after a `*`), or without a location,
/// is dropped: it may never be planned.
pub(crate) fn split(sel: &protobuf::SelectStmt, errors: Vec<PlanTimeError>) -> Split {
    // Each output expression's span, and the column it is (`None` past a
    // `*`, which expands to columns of its own).
    let entries: Vec<(Option<Span>, Option<usize>)> = match sel.values_lists.as_slice() {
        [row] => match row.node.as_ref() {
            Some(node::Node::List(l)) => l
                .items
                .iter()
                .enumerate()
                .map(|(i, n)| (span(n), Some(i)))
                .collect(),
            _ => Vec::new(),
        },
        [] => {
            let mut known = true;
            sel.target_list
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    known &= !is_star(t);
                    (span(t), known.then_some(i))
                })
                .collect()
        }
        _ => Vec::new(),
    };
    let mut out = Split {
        outputs: vec![None; entries.iter().filter(|(_, c)| c.is_some()).count()],
        rest: None,
    };
    for (location, e) in errors {
        let Some(at) = location else {
            continue;
        };
        match entries
            .iter()
            .find(|(s, _)| s.is_some_and(|(lo, hi)| (lo..=hi).contains(&at)))
        {
            Some((_, Some(column))) => {
                if let Some(slot) = out.outputs.get_mut(*column)
                    && slot.is_none()
                {
                    *slot = Some(e);
                }
            }
            Some((_, None)) => {}
            None => {
                if out.rest.is_none() {
                    out.rest = Some((location, e));
                }
            }
        }
    }
    out
}

/// The first and last location of a tree's nodes.
type Span = (i32, i32);

/// The first and last location of the nodes in `n`.
fn span(n: &protobuf::Node) -> Option<Span> {
    let inner = n.node.as_ref()?;
    let mut range: Option<(i32, i32)> = None;
    for (n, _) in inner.nodes() {
        if let Some(l) = n.location().filter(|l| *l >= 0) {
            range = Some(match range {
                Some((lo, hi)) => (lo.min(l), hi.max(l)),
                None => (l, l),
            });
        }
    }
    range
}

/// A `*` or `t.*` target entry.
fn is_star(t: &protobuf::Node) -> bool {
    matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
        if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
            Some(node::Node::ColumnRef(cr)) if cr.fields.iter().any(|f|
                matches!(f.node.as_ref(), Some(node::Node::AStar(_))))))
}

/// Analyze `sel` (with `run`) as a subquery the query reading it plans as
/// `plan` (never null-extended there when `kept`). Under the plan-time
/// checks, the errors of one certainly planned are captured and split; the
/// others are analyzed without the checks.
pub(crate) fn analyze<T>(
    plan: Plan,
    sel: &protobuf::SelectStmt,
    kept: bool,
    run: impl FnOnce() -> T,
) -> (T, Split) {
    if plan == Plan::Whole {
        return (run(), Split::default());
    }
    match next_level(plan, sel, kept) {
        Some(next) => {
            let (out, errors) = crate::expr::plan_fold::with_next_level(next, || {
                crate::expr::capture_plan_time_errors(run)
            });
            (out, split(sel, errors))
        }
        None if plan == Plan::PulledUp => (run(), Split::default()),
        None => (crate::expr::may_skip(run), Split::default()),
    }
}

/// Analyze the query of CTE `cte` (with `run`), and what its references
/// raise when planned: a CTE the planner may inline is planned like a
/// FROM subquery at each reference (`plan_of`); a MATERIALIZED one whole,
/// once, wherever it is referenced at all (an unreferenced one never is).
/// The errors go to `deferred` by output column (see
/// [`crate::scope::Deferred`]).
pub(crate) fn analyze_cte<T>(
    cte: &protobuf::CommonTableExpr,
    recursive: bool,
    snapshot: &PgCatalog,
    run: impl FnOnce() -> T,
) -> (T, Vec<Option<AnalyzeError>>, Option<AnalyzeError>) {
    let Some(node::Node::SelectStmt(sel)) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref())
    else {
        // A data-modifying CTE always runs.
        return (run(), Vec::new(), None);
    };
    let materialized = cte.ctematerialized == protobuf::CteMaterialize::Always as i32;
    let plan = match plan_of(sel, snapshot) {
        _ if recursive => Plan::Uncertain,
        Plan::Uncertain | Plan::Separate if materialized => Plan::Whole,
        // Inlined, it is planned with the relation it is.
        Plan::Separate => Plan::Uncertain,
        plan => plan,
    };
    if plan == Plan::Uncertain || !crate::expr::plan_time_checks() {
        return (crate::expr::may_skip(run), Vec::new(), None);
    }
    let (out, errors) = crate::expr::capture_plan_time_errors(run);
    if plan == Plan::Whole || materialized {
        let first = errors.into_iter().next().map(|(_, e)| e);
        return (out, Vec::new(), first);
    }
    let split = split(sel, errors);
    (out, split.outputs, split.rest.map(|(_, e)| e))
}

/// Analyze `sel` (with `run`) as a planned query the rewriter puts in
/// the one reading it (a view's, never null-extended there when `kept`),
/// and split what planning it raises (see [`split`]); `None` when nothing
/// of it is certain to be planned. A `security_barrier` view's
/// (`barrier`) is never pulled up.
pub(crate) fn planned_errors<T>(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    barrier: bool,
    kept: bool,
    run: impl FnOnce() -> T,
) -> Option<(T, Split)> {
    let plan = match plan_of(sel, snapshot) {
        _ if barrier && sel.op == SetOperation::SetopNone as i32 && sel.values_lists.is_empty() => {
            Plan::Separate
        }
        _ if barrier => Plan::Uncertain,
        plan => plan,
    };
    if plan == Plan::Whole {
        let (out, errors) =
            crate::expr::with_plan_time_checks(|| crate::expr::capture_plan_time_errors(run));
        let split = Split {
            outputs: Vec::new(),
            rest: errors.into_iter().next(),
        };
        return Some((out, split));
    }
    let next = next_level(plan, sel, kept)?;
    let (out, errors) = crate::expr::with_plan_time_checks(|| {
        crate::expr::plan_fold::with_next_level(next, || crate::expr::capture_plan_time_errors(run))
    });
    Some((out, split(sel, errors)))
}

/// Attach the errors [`analyze_cte`] found to the CTE's `columns`.
pub(crate) fn attach_to_cte(
    columns: &mut [crate::scope::ScopeColumn],
    outputs: Vec<Option<AnalyzeError>>,
    entry: Option<AnalyzeError>,
) {
    for (c, e) in columns.iter_mut().zip(outputs) {
        c.deferred.value = e.map(Box::new);
    }
    if let Some(e) = entry {
        let e = Box::new(e);
        for c in columns.iter_mut() {
            c.deferred.entry = Some(e.clone());
        }
    }
}

/// Raise, for a reference to a CTE at `location` the query plans, what
/// planning the CTE raises (see [`analyze_cte`]).
pub(crate) fn reference_cte(
    columns: &[crate::scope::ScopeColumn],
    location: i32,
) -> Result<(), AnalyzeError> {
    if !crate::expr::plan_time_checks() {
        return Ok(());
    }
    match columns.iter().find_map(|c| c.deferred.entry.as_deref()) {
        Some(e) => match crate::expr::plan_time_error(Some(location), e.clone()) {
            Some(e) => Err(e),
            None => Ok(()),
        },
        None => Ok(()),
    }
}

/// Analyze `sel`, an EXISTS subquery (with `run`): `simplify_EXISTS_query`
/// throws its target list, GROUP BY, DISTINCT, ORDER BY and WINDOW away
/// (or keeps them all, which is only more): what planning its FROM and
/// WHERE raises is raised, once the subquery is (see
/// [`crate::expr::sublink_planned`]).
pub(crate) fn analyze_exists<T>(
    sel: &protobuf::SelectStmt,
    run: impl FnOnce() -> Result<T, AnalyzeError>,
) -> Result<T, AnalyzeError> {
    if !crate::expr::plan_time_checks() {
        return run();
    }
    let (out, errors) = crate::expr::capture_plan_time_errors(run);
    let out = out?;
    let planned: Vec<Span> = sel
        .from_clause
        .iter()
        .chain(sel.where_clause.as_deref())
        .filter_map(span)
        .collect();
    for (location, e) in errors {
        if location.is_some_and(|at| planned.iter().any(|(lo, hi)| (*lo..=*hi).contains(&at)))
            && let Some(e) = crate::expr::plan_time_error(location, e)
        {
            return Err(e);
        }
    }
    Ok(out)
}
