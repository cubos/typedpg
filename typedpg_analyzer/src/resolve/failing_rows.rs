//! A NULL cast to a domain that rejects it (`SELECT NULL::d` of a NOT NULL
//! domain, `CAST(NULL AS d)` of one with `CHECK (VALUE IS NOT NULL)`) that
//! every execution of a statement evaluates: the coercion fails when
//! evaluated (`ExecEvalConstraintNotNull` / `ExecEvalConstraintCheck`),
//! naming the domain cast to, so the statement is rejected.
//!
//! A query is followed only where it surely evaluates a row: a select list
//! over no FROM item (one row, one group if grouped) or over a single
//! view, subquery or CTE that surely yields one; a VALUES list; a UNION
//! [ALL] of such, or an EXCEPT or INTERSECT that surely reads one. A WHERE
//! must be provably true over that row (`y = 1` of a view's `1 AS y`: the
//! planner folds it; over a set operation, a constant every arm gives the
//! column), or surely fail itself, or — over a set operation — be pushed
//! down into none of its arms, running over their projected rows, or read
//! a column every leaf of a flattened UNION ALL computes from no FROM
//! item, which it evaluates there — or be pushed down into leaves that are
//! all select lists over no FROM item, where it is folded over each one's
//! expressions: a leaf whose One-Time Filter surely fails, or that runs
//! and projects a failing column, fails. A LIMIT other than 0 keeps the row, and
//! of several rows the first one, which every plan fetches — and past the
//! rows of an unsorted UNION ALL's left arm, the right one's first — and
//! the ORDER BY keys of every row, which sorting evaluates whatever the
//! OFFSET; an OFFSET skips a row after its evaluation (unless an ORDER BY
//! could postpone that past the Limit). The planner drops a
//! subquery's (or a view's, or a flattened UNION ALL leaf's) output
//! columns the query above doesn't use, so a failing column of one fails
//! only where that query reads it (`x`, `s.x`, `*`, `s.*`, `s`,
//! `x IS NULL`, `x = 1`); a set operation it doesn't flatten keeps them
//! all. A scalar or ARRAY subquery reads its column; an operator its
//! operands, where its function surely evaluates them (an inlined SQL
//! body is simplified over the constant operands substituted for its
//! parameters: a CASE's decided WHENs, COALESCE's NULL arguments, an
//! AND's true ones); a BETWEEN its first comparison, or the second one
//! when a NULL bound folds the first one away.
//!
//! A view's query was analyzed without execution when the view was
//! created (DefineView), so the expressions of its rows that every
//! evaluation fails (sub-arrays that can't agree) are found here too.

use super::*;

/// Fail when every execution of `sel` evaluates a NULL its domain rejects.
pub(super) fn check_null_domain_casts(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    check_rows(sel, snapshot, &[])
}

/// Fail when every execution of `INSERT ... SELECT` (or VALUES) `sel`
/// evaluates a NULL its domain rejects: every column it yields is stored,
/// so every one is evaluated. Only into a table without rules (which could
/// drop the query, or the columns it reads — a view's rewrite could reach
/// some); `ctes` are the INSERT's WITH list.
pub(super) fn check_insert_select(
    sel: &protobuf::SelectStmt,
    ctes: &[protobuf::Node],
    target: crate::oid::PgClassOid,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let table = snapshot.pg_class.get(&target).is_some_and(|c| {
        matches!(
            c.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        )
    });
    if !table || snapshot.rules.get(&target).is_some_and(|r| !r.is_empty()) {
        return Ok(());
    }
    check_rows(sel, snapshot, &cte_list(ctes))
}

fn check_rows(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
) -> Result<(), AnalyzeError> {
    if !executes() {
        return Ok(());
    }
    // What the walk looks up is the statement's business only: its
    // dependencies come from its analysis.
    let (rows, _) = crate::ddl::depend::collect(|| query_rows(sel, snapshot, ctes, 0, false));
    // Nothing above the statement pushes a qualification down.
    match rows.and_then(|r| {
        r.all
            .any()
            .or(r.projection)
            .or(r.fail_after.map(|(_, f)| f))
    }) {
        Some(f) => Err(AnalyzeError::Invalid(f.message)),
        None => Ok(()),
    }
}

/// The views, subqueries and CTEs followed at most (a view can't read
/// itself, but a CTE list could be followed around in circles otherwise).
const MAX_DEPTH: usize = 32;

/// How many rows a query yields.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Count {
    /// Surely one or more (else maybe none).
    at_least_one: bool,
    /// At most one (else maybe more).
    at_most_one: bool,
}

const ONE: Count = Count {
    at_least_one: true,
    at_most_one: true,
};

/// A literal a WHERE can be decided over.
#[derive(Clone, PartialEq, Eq)]
enum Value {
    Int(i64),
    Str(String),
    Bool(bool),
}

/// An error evaluating a row surely raises.
#[derive(Clone, PartialEq)]
struct Failure {
    message: String,
    /// Raised by a domain's coercion. That is parallel restricted
    /// (max_parallel_hazard_walker), so a query evaluating one runs in no
    /// parallel worker: an Append over it is no Parallel Append, and runs
    /// its children in order.
    domain: bool,
}

/// The errors reading some of a query's rows surely raises.
#[derive(Clone, Default)]
struct Errors {
    /// Each output column's error, raised wherever the column is read.
    columns: Vec<Option<Failure>>,
    /// An error raised whatever is read (a WHERE that fails).
    always: Option<Failure>,
}

impl Errors {
    /// The same error for each of `width` columns.
    fn uniform(failure: Option<Failure>, width: usize) -> Errors {
        Errors {
            columns: vec![failure; width],
            always: None,
        }
    }

    /// The error reading the whole row raises.
    fn any(&self) -> Option<Failure> {
        self.always
            .clone()
            .or_else(|| self.columns.iter().flatten().next().cloned())
    }

    fn column(&self, i: usize) -> Option<Failure> {
        self.columns.get(i).cloned().flatten()
    }
}

/// What evaluating a query surely does.
struct Rows {
    /// What reading every row raises.
    all: Errors,
    /// What fetching its first row raises, whatever stops after it: every
    /// plan fetches that one (a Limit stops after the rows it returns).
    first: Errors,
    /// What fetching its first row raises where it is sorted or hashed (a
    /// set operation's arm, read through a HashAggregate, a Sort, a
    /// MergeAppend or a SetOp): an unsorted UNION ALL's first row there is
    /// that of each of its arms, not only of the left one.
    sorted: Option<Failure>,
    /// Its column errors are a set operation's arms', raised projecting
    /// the arm's row: a qualification above pushed down into the arms
    /// (subquery_push_qual) runs before that, and may skip it.
    projected: bool,
    /// Each output column a qualification above can't be pushed down into
    /// the arms by (`qual_is_pushdown_safe`): one whose type differs among
    /// the arms (`compare_tlist_datatypes`), or any of a set operation
    /// with an EXCEPT, a LIMIT or a component that is a set operation of
    /// its own. Such a qualification runs over the projected rows.
    unpushable: Vec<bool>,
    /// A qualification reading the whole row can't be pushed down: the
    /// set operation isn't a UNION ALL the planner flattens
    /// (`is_simple_union_all`), so the row is a Var of the subquery
    /// (`qual_is_pushdown_safe`'s point 4).
    whole_row_unpushable: bool,
    /// A set operation the planner doesn't flatten keeps every output
    /// column (remove_unused_subquery_outputs leaves a set operation
    /// alone): reading any of its rows projects its arms whole.
    keeps_columns: bool,
    /// What reading every row (the first one) of a query over such a set
    /// operation raises projecting its arms, unless a qualification above
    /// is pushed down into them and skips the row: what a query reading
    /// its rows with nothing above it surely raises.
    projection: Option<Failure>,
    first_projection: Option<Failure>,
    /// The column types of each leaf of a set operation (as its own
    /// output's, unknown where not known).
    leaf_types: Vec<Vec<Option<PgTypeOid>>>,
    /// What sorting the rows by the query's ORDER BY surely raises: its
    /// keys' evaluation over every row, which every plan for it does (a
    /// Sort reads every row, a MergeAppend primes each input).
    ordered: Option<Failure>,
    /// Each output column a qualification pushed down into the query
    /// (into every leaf of a flattened UNION ALL) evaluates, where it
    /// reads it: the query's leaves compute it from no FROM item, so the
    /// qualification is its expression, evaluated over each leaf's row,
    /// and raises its error.
    qual_reads: Vec<bool>,
    /// Its output columns' names, where known (`*` expanded).
    names: Option<Vec<String>>,
    /// Fetching more than this many rows surely raises this error: an
    /// unsorted UNION ALL's Append runs its arms in order, so past the
    /// rows of its left one it fetches its right one's first row.
    fail_after: Option<(i64, Failure)>,
    /// The leaves of a set operation a qualification above is pushed down
    /// into, each one a select list over no FROM item (see [`Leaf`]).
    leaves: Option<Leaves>,
    /// A select list over no FROM item the planner pulls up into the query
    /// reading it: a qualification there reads its expressions, no Var —
    /// each conjunct a gating qual, in no equivalence class.
    var_free: bool,
    /// The constant each output column surely holds.
    values: Vec<Option<Value>>,
    /// Each output column's type, where known (an operator over it is
    /// resolved by it).
    types: Vec<Option<PgTypeOid>>,
    count: Count,
}

impl Rows {
    fn new(all: Errors, first: Errors, values: Vec<Option<Value>>, count: Count) -> Rows {
        Rows {
            sorted: first.any(),
            projected: false,
            unpushable: Vec::new(),
            whole_row_unpushable: false,
            keeps_columns: false,
            projection: None,
            first_projection: None,
            leaf_types: Vec::new(),
            ordered: None,
            qual_reads: Vec::new(),
            names: None,
            fail_after: None,
            leaves: None,
            var_free: false,
            all,
            first,
            values,
            types: Vec::new(),
            count,
        }
    }

    fn unknown_values(all: Errors, first: Errors, count: Count) -> Rows {
        let values = vec![None; all.columns.len()];
        Rows::new(all, first, values, count)
    }

    fn errors(&self, first: bool) -> &Errors {
        if first { &self.first } else { &self.all }
    }

    /// A qualification reading `read` of these rows can't be pushed down
    /// into a set operation's arms: it reads an unpushable column, or the
    /// whole row (`qual_is_pushdown_safe`).
    fn unpushable(&self, read: &Read) -> bool {
        match read {
            Read::Column(i) => self.unpushable.get(*i).copied().unwrap_or(false),
            Read::WholeRow => self.whole_row_unpushable,
            Read::All | Read::Other => false,
        }
    }

    /// Column `i`'s type: an unknown-typed one (`'a'`, `NULL`) is text, as
    /// a query's output (resolveTargetListUnknowns).
    fn type_of(&self, i: usize) -> Option<PgTypeOid> {
        let t = self.types.get(i).copied().flatten()?;
        Some(if t == oid::UNKNOWN { oid::TEXT } else { t })
    }
}

/// A leaf of a set operation that is a select list over no FROM item: a
/// qualification pushed down into it has its columns replaced by the
/// leaf's expressions (a flattened UNION ALL's child gets it through
/// apply_child_basequals, another set operation's leaf through
/// subquery_push_qual), and is evaluated there as a gating qual (a
/// One-Time Filter) before the leaf's row is projected.
#[derive(Clone)]
struct Leaf {
    /// Its select list's expressions.
    exprs: Vec<protobuf::Node>,
    /// What evaluating each of them raises.
    errors: Vec<Option<Failure>>,
}

/// How the leaves of a set operation a qualification is pushed down into
/// are read.
#[derive(Clone)]
struct Leaves {
    leaves: Vec<Leaf>,
    /// Flattened into an append relation (is_simple_union_all): every
    /// qualification above goes down to each leaf, its volatile conjuncts
    /// in a One-Time Filter of their own below the others', and each leaf
    /// projects only the columns read above. Else only a qualification
    /// `qual_is_pushdown_safe` lets through is pushed down, as one
    /// One-Time Filter, and each leaf projects every column.
    flattened: bool,
    /// An unsorted UNION ALL: an Append runs the leaves in order.
    in_order: bool,
    /// A UNION (not ALL) above every leaf: the first fetch reads them
    /// all (a HashAggregate or a Sort).
    all_at_once: bool,
    /// The tree has an INTERSECT, which may read only one of its inputs.
    intersect: bool,
}

/// What a leaf does with a qualification pushed down into it.
enum LeafOutcome {
    /// A conjunct is folded to false: the leaf is left out (dummy).
    Dummy,
    /// Its One-Time Filter surely raises this.
    Fails(Failure),
    /// Every conjunct holds: its row is projected.
    Runs,
    Unknown,
}

fn cte_list(nodes: &[protobuf::Node]) -> Vec<&protobuf::CommonTableExpr> {
    nodes
        .iter()
        .rev()
        .filter_map(|c| match c.node.as_ref() {
            Some(node::Node::CommonTableExpr(cte)) => Some(cte.as_ref()),
            _ => None,
        })
        .collect()
}

/// `Some(rows)` when every execution of `sel` surely evaluates its rows.
/// `ctes` are the CTEs visible to `sel`'s FROM items, innermost first;
/// `stored` is set inside a view's query.
fn query_rows(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
) -> Option<Rows> {
    if depth > MAX_DEPTH {
        return None;
    }
    let rows = if sel.op != protobuf::SetOperation::SetopNone as i32 {
        set_op_rows(sel, snapshot, ctes, depth, stored)?
    } else {
        select_rows(sel, snapshot, ctes, depth, stored)?
    };
    limit_rows(sel, rows)
}

/// A LIMIT / OFFSET bound: `None` for an absent, NULL or ALL one, the
/// integer of a literal one; `Err` for anything else.
fn bound(n: Option<&protobuf::Node>) -> Result<Option<i64>, ()> {
    let Some(n) = n else { return Ok(None) };
    match n.node.as_ref() {
        Some(node::Node::AConst(c)) if c.isnull => Ok(None),
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Ival(i)),
            ..
        })) if i.ival >= 0 => Ok(Some(i64::from(i.ival))),
        _ => Err(()),
    }
}

/// `rows` through `sel`'s LIMIT and OFFSET (ExecLimit fetches — evaluates
/// — the rows it skips, and stops after the ones it returns).
fn limit_rows(sel: &protobuf::SelectStmt, rows: Rows) -> Option<Rows> {
    let count = bound(sel.limit_count.as_deref()).ok()?;
    let offset = bound(sel.limit_offset.as_deref()).ok()?.unwrap_or(0);
    if count == Some(0) {
        return None;
    }
    if count.is_none() && offset == 0 {
        return Some(rows);
    }
    // A Limit fetches its input's rows in order, the first one surely (a
    // Sort below it reads them all); which ones past it a LIMIT returns
    // are evaluated is unknown. A qualification above isn't pushed down
    // into a query with a LIMIT or OFFSET (subquery_is_pushdown_safe).
    // Nor is one pushed down through it, below: its projections are
    // its own rows'.
    let mut rows = rows;
    rows.leaves = None;
    rows.all.always = rows.all.always.take().or(rows.projection.take());
    rows.first.always = rows.first.always.take().or(rows.first_projection.take());
    // Fetching more rows than come before a failing one raises its error
    // (an OFFSET's skipped rows are fetched too).
    let past = rows.fail_after.take().and_then(|(n, f)| {
        let fetched = count.map(|c| c.saturating_add(offset));
        fetched.is_none_or(|k| k > n).then_some(f)
    });
    // Sorted, every row's ORDER BY keys are evaluated before the first
    // one is returned or skipped.
    let ordered = rows.ordered.clone().filter(|_| !sel.sort_clause.is_empty());
    let raised = ordered.or(past);
    let rows = Rows {
        sorted: rows.first.any(),
        unpushable: vec![true; rows.all.columns.len()],
        whole_row_unpushable: true,
        keeps_columns: rows.keeps_columns || sel.op != protobuf::SetOperation::SetopNone as i32,
        ..rows
    };
    // Sorting, the planner may postpone an expensive column past the
    // Limit (make_sort_input_target): a skipped row's, or one of a row
    // past the first, is not evaluated — only what the keys raise is
    // known then.
    let only_raised = |rows: Rows, f: Failure, count: Count| {
        let errors = Errors {
            columns: vec![None; rows.all.columns.len()],
            always: Some(f),
        };
        Rows {
            sorted: errors.any(),
            first: errors.clone(),
            all: errors,
            count,
            ..rows
        }
    };
    if count.is_some() && !rows.count.at_most_one {
        if offset != 0 {
            let count = Count {
                at_least_one: false,
                at_most_one: false,
            };
            return raised.map(|f| only_raised(rows, f, count));
        }
        let mut first = rows.first.clone();
        first.always = first.always.or(raised);
        return Some(Rows {
            sorted: first.any(),
            first: first.clone(),
            all: first,
            count: Count {
                at_least_one: rows.count.at_least_one,
                at_most_one: count == Some(1),
            },
            ..rows
        });
    }
    let mut rows = rows;
    if offset == 0 {
        rows.all.always = rows.all.always.take().or(raised.clone());
        rows.first.always = rows.first.always.take().or(raised);
        return Some(rows);
    }
    let count = Count {
        at_least_one: false,
        ..rows.count
    };
    if !sel.sort_clause.is_empty() {
        return raised.map(|f| only_raised(rows, f, count));
    }
    rows.all.always = rows.all.always.take().or(raised.clone());
    rows.first.always = rows.first.always.take().or(raised);
    Some(Rows { count, ..rows })
}

/// The rows of a SELECT that isn't a set operation, before its LIMIT.
fn select_rows(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
) -> Option<Rows> {
    let mut visible: Vec<&protobuf::CommonTableExpr> = Vec::new();
    if let Some(with) = sel.with_clause.as_ref() {
        if with.recursive {
            return None;
        }
        visible.extend(cte_list(&with.ctes));
    }
    visible.extend(ctes.iter().copied());
    let level = Level {
        snapshot,
        ctes: &visible,
        depth,
        stored,
        // A stored expression is analyzed again, with and without
        // execution, where it reads nothing but itself: no FROM item, no
        // CTE.
        standalone: stored && sel.from_clause.is_empty() && visible.is_empty(),
        source: None,
        first: false,
        qual: false,
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
        // VALUES scan evaluates every cell of the rows it yields, in order.
        if let [row] = rows.as_slice() {
            let errors = Errors {
                columns: row.iter().map(|c| level.error(c)).collect(),
                always: None,
            };
            let values = row.iter().map(|c| level.value(c)).collect();
            return Some(Rows {
                types: row.iter().map(|c| level.type_of(c)).collect(),
                qual_reads: vec![true; row.len()],
                names: Some((1..=row.len()).map(|i| format!("column{i}")).collect()),
                ..Rows::new(errors.clone(), errors, values, ONE)
            });
        }
        let all = rows
            .iter()
            .flat_map(|r| r.iter())
            .find_map(|c| level.error(c));
        let first = rows
            .first()
            .and_then(|r| r.iter().find_map(|c| level.error(c)));
        return Some(Rows {
            // Sorted or hashed, they are all read.
            sorted: all.clone(),
            ..Rows::unknown_values(
                Errors::uniform(all, width),
                Errors::uniform(first, width),
                Count {
                    at_least_one: true,
                    at_most_one: false,
                },
            )
        });
    }
    // Grouped, the one row of no FROM item is one group (a key naming an
    // output column, or its position, is no set-returning function the
    // select list doesn't show), once per grouping set: `()` is one, as is
    // `GROUPING SETS (())`; `GROUPING SETS ((), ())` two.
    let mut grouping_sets = 1usize;
    let grouped_row = sel.group_clause.is_empty()
        || sel.from_clause.is_empty()
            && sel.group_clause.iter().all(|g| match g.node.as_ref() {
                Some(node::Node::ColumnRef(_))
                | Some(node::Node::AConst(protobuf::AConst {
                    val: Some(protobuf::a_const::Val::Ival(_)),
                    ..
                })) => true,
                Some(node::Node::GroupingSet(gs)) => match gs.kind() {
                    protobuf::GroupingSetKind::GroupingSetEmpty => true,
                    protobuf::GroupingSetKind::GroupingSetSets
                        if gs.content.iter().all(|s| {
                            matches!(
                                s.node.as_ref(),
                                Some(node::Node::GroupingSet(e))
                                    if e.kind() == protobuf::GroupingSetKind::GroupingSetEmpty
                            )
                        }) && !gs.content.is_empty() =>
                    {
                        grouping_sets = grouping_sets.saturating_mul(gs.content.len());
                        true
                    }
                    _ => false,
                },
                _ => false,
            });
    if !grouped_row || sel.having_clause.is_some() {
        return None;
    }
    let source = match sel.from_clause.as_slice() {
        [] => None,
        [item] => Some(from_item_rows(item, snapshot, &visible, depth, stored)?),
        _ => return None,
    };
    let level = Level {
        source: source.as_ref(),
        ..level
    };
    let mut count = source.as_ref().map_or(ONE, |s| s.rows.count);
    if grouping_sets > 1 {
        count.at_most_one = false;
    }
    // What reading the source's rows raises projecting a set operation's
    // arms (see [`Rows::projection`]).
    let projection = match source.as_ref() {
        Some(s) if s.rows.keeps_columns => (s.rows.all.any(), s.rows.first.any()),
        Some(s) => (s.rows.projection.clone(), s.rows.first_projection.clone()),
        None => (None, None),
    };
    // A WHERE not surely true must surely fail: only a row reaching it
    // evaluates it. One that is pushed down into no arm runs over the
    // projected rows, all of them (the set operation runs whole below
    // it), whatever it lets through.
    let where_fails = sel
        .where_clause
        .as_deref()
        .filter(|w| level.truth(w) != Some(true));
    let unpushed = where_fails
        .filter(|w| level.unpushed(w))
        .map(|_| projection.clone());
    // Pushed down into a set operation's leaves, it is evaluated in each.
    let pushed = where_fails
        .and_then(|w| level.pushed_down(sel, w))
        .unwrap_or_default();
    if where_fails.is_some() {
        if !count.at_least_one
            && !unpushed.as_ref().is_some_and(|(all, _)| all.is_some())
            && pushed.all.is_none()
        {
            return None;
        }
        count.at_least_one = false;
    }
    let unpushed = unpushed.unwrap_or((None, None));
    let (all, values) = level_errors(sel, level, where_fails, unpushed.0, pushed.all.clone())?;
    // Over the source's first row, a WHERE that may not fail may let it
    // through or not: what the first fetch raises is unknown.
    let first = level_errors(
        sel,
        Level {
            first: true,
            ..level
        },
        where_fails,
        unpushed.1,
        pushed.first.clone(),
    )
    .map_or_else(Errors::default, |(e, _)| e);
    // A set-returning function in the select list may leave no row, or
    // several; the other columns are evaluated below it all the same.
    if !super::walk::level_srf_calls(sel, snapshot).is_empty() {
        count = Count {
            at_least_one: false,
            at_most_one: false,
        };
    }
    let projected = source.as_ref().is_some_and(|s| s.rows.projected);
    // Each row the source yields reaches the select list, or a WHERE
    // over its projected rows that may only make the query fetch more of
    // them; a set-returning function could yield more rows than it
    // reads.
    let fail_after = source
        .as_ref()
        .and_then(|s| s.rows.fail_after.clone())
        .filter(|_| where_fails.is_none_or(|w| level.unpushed(w)))
        .or(pushed.fail_after)
        .filter(|_| super::walk::level_srf_calls(sel, snapshot).is_empty());
    let equated = equated_exprs(sel.where_clause.as_deref(), snapshot);
    let mut sorted_by = false;
    let ordered = sel.sort_clause.iter().find_map(|k| match k.node.as_ref() {
        Some(node::Node::SortBy(sb)) => {
            let key = sb.node.as_deref()?;
            if order_key_redundant(sel, level, key, &equated) {
                return None;
            }
            sorted_by = true;
            order_key_error(sel, level, &all, key)
        }
        _ => None,
    });
    // A Sort reads every row the WHERE lets through: what filtering them
    // all raises too.
    let ordered = ordered.or_else(|| all.always.clone().filter(|_| sorted_by));
    // Without a WHERE, a qualification above may still be pushed down
    // through this query (pulled up) into the arms.
    let (projection, first_projection) = if where_fails.is_none() {
        projection
    } else {
        (None, None)
    };
    Some(Rows {
        projected,
        unpushable: if projected {
            level_unpushable(sel, level)
        } else {
            Vec::new()
        },
        projection,
        first_projection,
        ordered,
        types: level_types(sel, level),
        qual_reads: if source.is_none() {
            vec![true; values.len()]
        } else {
            Vec::new()
        },
        names: level_names(sel, level),
        fail_after,
        var_free: plain_leaf(sel, snapshot).is_some(),
        ..Rows::new(all, first, values, count)
    })
}

/// The names of the columns `sel`'s select list yields over `level`'s
/// source (`*` expanded), as FigureColname gives them.
fn level_names(sel: &protobuf::SelectStmt, level: Level<'_>) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for target in &sel.target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            return None;
        };
        let val = rt.val.as_deref()?;
        if let (Some(source), Some(node::Node::ColumnRef(cr))) = (level.source, val.node.as_ref())
            && let Read::All = source.read(cr)
        {
            names.extend(source.names.iter().cloned());
            continue;
        }
        names.push(if rt.name.is_empty() {
            super::target_list::infer_column_name(val).unwrap_or_else(|| "?column?".to_owned())
        } else {
            rt.name.clone()
        });
    }
    Some(names)
}

/// What evaluating ORDER BY key `key` of `sel` over every row of `level`'s
/// source surely raises: an output column's error (by position or by
/// alias), else the expression's.
fn order_key_error(
    sel: &protobuf::SelectStmt,
    level: Level<'_>,
    all: &Errors,
    key: &protobuf::Node,
) -> Option<Failure> {
    match key.node.as_ref()? {
        node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Ival(i)),
            ..
        }) => return all.column(usize::try_from(i.ival).ok()?.checked_sub(1)?),
        node::Node::ColumnRef(cr) => {
            if let [f] = cr.fields.as_slice()
                && let Some(node::Node::String(name)) = f.node.as_ref()
            {
                // An output column's alias wins over the FROM item's
                // columns (findTargetlistEntrySQL92).
                let mut column = 0;
                for target in &sel.target_list {
                    let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
                        return None;
                    };
                    if let (Some(source), Some(node::Node::ColumnRef(cr))) = (
                        level.source,
                        rt.val.as_deref().and_then(|v| v.node.as_ref()),
                    ) && let Read::All = source.read(cr)
                    {
                        column += source.names.len();
                        continue;
                    }
                    if rt.name == name.sval {
                        return all.column(column);
                    }
                    column += 1;
                }
            }
        }
        _ => {}
    }
    level.error(key)
}

/// The conjuncts of the qualification `n`: its ANDed terms, flattened.
fn conjuncts(n: &protobuf::Node) -> Vec<&protobuf::Node> {
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b)) if b.boolop == protobuf::BoolExprType::AndExpr as i32 => {
            b.args.iter().flat_map(conjuncts).collect()
        }
        _ => vec![n],
    }
}

/// The operator expression `e` may be an equality the planner puts in an
/// equivalence class: an operator not named as an ordering one is (a
/// user's could be a btree equality), and so is a single-item IN, which
/// the parser makes one `=` (a NOT of `<>` is negated into one, too).
fn may_equate(e: &protobuf::AExpr) -> bool {
    let ordering = matches!(
        expr::extract_string_fields(&e.name)
            .last()
            .map(String::as_str),
        Some("<" | ">" | "<=" | ">=")
    );
    matches!(
        e.kind(),
        protobuf::AExprKind::AexprOp | protobuf::AExprKind::AexprIn
    ) && !ordering
}

/// The column a column reference names (its last field).
fn column_name(cr: &protobuf::ColumnRef) -> Option<&str> {
    match cr.fields.last()?.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// The key two expressions an equivalence class could hold have alike:
/// their structure without locations, qualifiers or type-name spellings
/// (erring towards matching).
fn equivalence_key(n: &protobuf::Node, snapshot: &PgCatalog) -> String {
    super::node_fingerprint(&super::predtest_unqualify(
        &crate::grouping::resolve_type_names(n, snapshot),
    ))
}

/// The expressions a conjunct of `qual` that may be an equality (see
/// [`may_equate`]) compares, as [`equivalence_key`]s: an equivalence
/// class may hold them, with a constant, which leaves an ORDER BY key
/// equal to one redundant (`pathkey_is_redundant`): the Sort by it is
/// dropped, and with it the key's evaluation over every row. Only the
/// compared expressions are members (`a + 0 = 1` doesn't make `a` one).
fn equated_exprs(
    qual: Option<&protobuf::Node>,
    snapshot: &PgCatalog,
) -> std::collections::HashSet<String> {
    let mut equated = std::collections::HashSet::new();
    for c in qual.map(conjuncts).unwrap_or_default() {
        let mut c = c;
        // A NOT of `<>` is negated into `=`.
        while let Some(node::Node::BoolExpr(b)) = c.node.as_ref()
            && b.boolop == protobuf::BoolExprType::NotExpr as i32
            && let [arg] = b.args.as_slice()
        {
            c = arg;
        }
        // The expressions an equality compares.
        fn compared(e: &protobuf::AExpr) -> Vec<&protobuf::Node> {
            let mut members: Vec<&protobuf::Node> = e.lexpr.iter().map(|l| &**l).collect();
            match e.rexpr.as_deref().and_then(|r| r.node.as_ref()) {
                Some(node::Node::List(l)) => members.extend(l.items.iter()),
                Some(_) => members.extend(e.rexpr.as_deref()),
                None => {}
            }
            members
        }
        let mut members = match c.node.as_ref() {
            Some(node::Node::AExpr(e)) if may_equate(e) => compared(e),
            Some(node::Node::AExpr(_)) => continue,
            // Simplified, another conjunct may become one (`a = 1 OR a =
            // 1` loses its duplicate arm): any equality in it counts.
            _ => {
                let mut members = Vec::new();
                super::walk::visit_same_level(c, &mut |x| {
                    if let Some(node::Node::AExpr(e)) = x.node.as_ref()
                        && may_equate(e)
                    {
                        members.extend(compared(e));
                    }
                });
                members
            }
        };
        // A row comparison `(a, b) = (1, 2)` is one equality per column
        // (make_row_comparison_op).
        let fields: Vec<&protobuf::Node> = members
            .iter()
            .filter_map(|m| match m.node.as_ref() {
                Some(node::Node::RowExpr(r)) => Some(r.args.iter()),
                _ => None,
            })
            .flatten()
            .collect();
        members.extend(fields);
        equated.extend(members.into_iter().map(|m| equivalence_key(m, snapshot)));
    }
    equated
}

/// The equalities the conjunct `c` may leave as conjuncts of its own once
/// simplified, for the planner to put in equivalence classes: `c` itself,
/// an AND's, and those every arm of an OR has (find_duplicate_ors drops a
/// duplicate arm, and pulls out what all of them AND); under a NOT or a
/// cast, any (a NOT of `<>` is negated into `=`).
fn promoted_equalities<'a>(
    c: &'a protobuf::Node,
    snapshot: &PgCatalog,
) -> Vec<&'a protobuf::AExpr> {
    use protobuf::BoolExprType as B;
    match c.node.as_ref() {
        Some(node::Node::AExpr(e)) if may_equate(e) => vec![e],
        Some(node::Node::BoolExpr(b)) if b.boolop == B::AndExpr as i32 => b
            .args
            .iter()
            .flat_map(|a| promoted_equalities(a, snapshot))
            .collect(),
        Some(node::Node::BoolExpr(b)) if b.boolop == B::OrExpr as i32 => {
            let key = |e: &protobuf::AExpr| {
                equivalence_key(
                    &protobuf::Node {
                        node: Some(node::Node::AExpr(Box::new(e.clone()))),
                    },
                    snapshot,
                )
            };
            let arms: Vec<Vec<&protobuf::AExpr>> = b
                .args
                .iter()
                .map(|a| promoted_equalities(a, snapshot))
                .collect();
            let Some((first, rest)) = arms.split_first() else {
                return Vec::new();
            };
            let rest: Vec<std::collections::HashSet<String>> = rest
                .iter()
                .map(|arm| arm.iter().map(|e| key(e)).collect())
                .collect();
            first
                .iter()
                .copied()
                .filter(|e| {
                    let k = key(e);
                    rest.iter().all(|arm| arm.contains(&k))
                })
                .collect()
        }
        Some(node::Node::AExpr(_)) => Vec::new(),
        Some(_) => {
            let mut all = Vec::new();
            super::walk::visit_same_level(c, &mut |x| {
                if let Some(node::Node::AExpr(e)) = x.node.as_ref()
                    && may_equate(e)
                {
                    all.push(e.as_ref());
                }
            });
            all
        }
        None => Vec::new(),
    }
}

/// ORDER BY key `key` of `sel` may be one the planner drops as redundant:
/// a constant (one reading no column), or one (through the output column
/// the key names, by position or alias) in `equated` (see
/// [`equated_exprs`]).
fn order_key_redundant(
    sel: &protobuf::SelectStmt,
    level: Level<'_>,
    key: &protobuf::Node,
    equated: &std::collections::HashSet<String>,
) -> bool {
    let redundant = |n: &protobuf::Node| {
        let mut reads = false;
        super::walk::visit_same_level(n, &mut |x| {
            reads |= matches!(x.node, Some(node::Node::ColumnRef(_)));
        });
        !reads || equated.contains(&equivalence_key(n, level.snapshot))
    };
    let column = |name: &str| protobuf::Node {
        node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
            fields: vec![protobuf::Node {
                node: Some(node::Node::String(protobuf::String { sval: name.into() })),
            }],
            location: -1,
        })),
    };
    let position = match key.node.as_ref() {
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Ival(i)),
            ..
        })) => usize::try_from(i.ival).ok().and_then(|i| i.checked_sub(1)),
        _ => None,
    };
    let alias = match key.node.as_ref() {
        Some(node::Node::ColumnRef(cr)) if cr.fields.len() == 1 => column_name(cr),
        _ => None,
    };
    if position.is_none() && alias.is_none() {
        return redundant(key);
    }
    // As [`order_key_error`] resolves it.
    let mut at = 0;
    for target in &sel.target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            return true;
        };
        let val = rt.val.as_deref();
        if let (Some(source), Some(node::Node::ColumnRef(cr))) =
            (level.source, val.and_then(|v| v.node.as_ref()))
            && let Read::All = source.read(cr)
        {
            let width = source.names.len();
            if let Some(p) = position.filter(|p| (at..at + width).contains(p)) {
                return redundant(&column(&source.names[p - at]));
            }
            at += width;
            continue;
        }
        if position == Some(at) || alias.is_some_and(|a| a == rt.name) {
            return val.is_none_or(redundant);
        }
        at += 1;
    }
    position.is_some() || redundant(key)
}

/// Which columns `sel`'s select list yields over `level`'s projected
/// source a qualification above can't push down: those reading such a
/// column of it.
fn level_unpushable(sel: &protobuf::SelectStmt, level: Level<'_>) -> Vec<bool> {
    let mut unpushable = Vec::new();
    for target in &sel.target_list {
        let val = match target.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt.val.as_deref(),
            _ => None,
        };
        let (Some(source), Some(node::Node::ColumnRef(cr))) =
            (level.source, val.and_then(|v| v.node.as_ref()))
        else {
            unpushable.push(false);
            continue;
        };
        match source.read(cr) {
            Read::All => unpushable
                .extend((0..source.names.len()).map(|i| source.rows.unpushable(&Read::Column(i)))),
            read @ Read::Column(_) => unpushable.push(source.rows.unpushable(&read)),
            Read::WholeRow | Read::Other => unpushable.push(false),
        }
    }
    unpushable
}

/// What evaluating `sel`'s select list over `level`'s source rows (all of
/// them, or the first) surely raises, and the constants it yields;
/// `where_fails` is a WHERE that must fail, which every row reaching the
/// select list would have passed.
fn level_errors(
    sel: &protobuf::SelectStmt,
    level: Level<'_>,
    where_fails: Option<&protobuf::Node>,
    unpushed: Option<Failure>,
    pushed: Option<Failure>,
) -> Option<(Errors, Vec<Option<Value>>)> {
    let source = level.source;
    let mut always = source.and_then(|s| s.rows.errors(level.first).always.clone());
    if let Some(w) = where_fails {
        always = match unpushed {
            Some(p) => Some(p),
            None => always.or(Some(level.surely_fails(w).or(pushed)?)),
        };
    }
    // The select list's own expressions are evaluated for the rows that
    // reach it; the columns it reads of the FROM item are evaluated there.
    let evaluated = where_fails.is_none() && source.is_none_or(|s| s.rows.count.at_least_one);
    let mut columns = Vec::new();
    let mut values = Vec::new();
    for target in &sel.target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            return None;
        };
        let val = rt.val.as_deref()?;
        if let (Some(source), Some(node::Node::ColumnRef(cr))) = (source, val.node.as_ref())
            && let Read::All = source.read(cr)
        {
            let errors = source.rows.errors(level.first);
            columns.extend((0..source.names.len()).map(|i| errors.column(i)));
            values.extend(source.rows.values.iter().cloned());
            continue;
        }
        columns.push(if evaluated || is_column_ref(val) {
            level.error(val)
        } else {
            None
        });
        values.push(level.value(val));
    }
    Some((Errors { columns, always }, values))
}

/// The type of each column `sel`'s select list yields over `level`'s
/// source, where known.
fn level_types(sel: &protobuf::SelectStmt, level: Level<'_>) -> Vec<Option<PgTypeOid>> {
    let mut types = Vec::new();
    for target in &sel.target_list {
        let val = match target.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt.val.as_deref(),
            _ => None,
        };
        if let (Some(source), Some(node::Node::ColumnRef(cr))) =
            (level.source, val.and_then(|v| v.node.as_ref()))
            && let Read::All = source.read(cr)
        {
            types.extend((0..source.names.len()).map(|i| source.rows.type_of(i)));
            continue;
        }
        types.push(val.and_then(|v| level.type_of(v)));
    }
    types
}

fn is_column_ref(n: &protobuf::Node) -> bool {
    matches!(n.node, Some(node::Node::ColumnRef(_)))
}

/// The rows of a set operation, before its LIMIT. An arm is evaluated
/// whole — a column failing in one fails reading any column.
///
/// A UNION [ALL] yields the rows of both arms, and every plan for a UNION
/// fetches the first row of each (a HashAggregate or a Sort reads them all,
/// a MergeAppend primes each child), and so does a sorted UNION ALL; an
/// Append runs an unsorted one's in order, so only the left one's first
/// row is fetched first.
///
/// EXCEPT and INTERSECT read their outer input, then their inner one only
/// if the outer one has a row (setop_fill_hash_table,
/// setop_retrieve_sorted). The outer one of an EXCEPT is its left arm,
/// read whole; an INTERSECT's may be either.
fn set_op_rows(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    ctes: &[&protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
) -> Option<Rows> {
    use protobuf::SetOperation as Op;
    if sel.with_clause.is_some() {
        return None;
    }
    let arms = [&sel.larg, &sel.rarg]
        .map(|arm| query_rows(arm.as_deref()?, snapshot, ctes, depth + 1, stored));
    let width = leftmost(sel).target_list.len();
    let [left, right] = &arms;
    let (all, first, sorted, count) = match Op::try_from(sel.op).ok()? {
        Op::SetopUnion => {
            let arms: Vec<&Rows> = [left, right].into_iter().flatten().collect();
            let all = arms.iter().find_map(|a| a.all.any());
            let each = arms.iter().find_map(|a| a.sorted.clone());
            let first = if sel.all && sel.sort_clause.is_empty() {
                left.as_ref()
                    .and_then(|l| l.first.any())
                    .filter(|f| f.domain)
            } else {
                each.clone()
            };
            let count = Count {
                at_least_one: arms.iter().any(|a| a.count.at_least_one),
                at_most_one: false,
            };
            (all, first, each, count)
        }
        Op::SetopExcept => {
            let left = left.as_ref()?;
            let inner = right
                .as_ref()
                .filter(|_| left.count.at_least_one)
                .and_then(|r| r.sorted.clone());
            let all = left.all.any().or_else(|| inner.clone());
            let first = left.sorted.clone().or(inner);
            let count = Count {
                at_least_one: false,
                at_most_one: left.count.at_most_one,
            };
            (all, first.clone(), first, count)
        }
        Op::SetopIntersect => {
            // What reading `a` as the outer input surely raises.
            let outer = |a: &Rows, b: Option<&Rows>| {
                a.sorted.clone().or_else(|| {
                    b.filter(|_| a.count.at_least_one)
                        .and_then(|b| b.sorted.clone())
                })
            };
            // A grouped leaf is estimated its rows (build_setop_child_paths),
            // at least one but where proven empty (a dummy relation).
            let leaf = |s: &protobuf::SelectStmt, rows: Option<&Rows>| {
                s.op == Op::SetopNone as i32
                    && s.having_clause.is_none()
                    && (s.group_clause.is_empty() && s.distinct_clause.is_empty()
                        || rows.is_some_and(|r| r.count.at_least_one))
            };
            // The planner puts first the input it estimates fewer groups
            // of, the left one on a tie (generate_nonunion_paths): an
            // estimate is at least one group (estimate_num_groups), so a
            // leaf it estimates one row of — every one yielding at most one
            // row — is first against one estimated so too (the other's
            // rows estimate may be less for a set operation, or a grouped
            // leaf that may be empty).
            let left_outer = left.as_ref().is_some_and(|l| l.count.at_most_one)
                && sel
                    .larg
                    .as_deref()
                    .is_some_and(|l| l.op == Op::SetopNone as i32)
                && sel.rarg.as_deref().is_some_and(|r| leaf(r, right.as_ref()));
            let first = if left_outer {
                outer(left.as_ref()?, right.as_ref())
            } else {
                let (left, right) = (left.as_ref()?, right.as_ref()?);
                outer(left, Some(right)).filter(|_| outer(right, Some(left)).is_some())
            };
            let count = Count {
                at_least_one: false,
                at_most_one: [left, right]
                    .into_iter()
                    .flatten()
                    .any(|a| a.count.at_most_one),
            };
            (first.clone(), first.clone(), first, count)
        }
        Op::SetopNone | Op::Undefined => return None,
    };
    let known = arms.iter().flatten().next()?;
    let width = width.max(known.all.columns.len());
    // The leaves of the set operation tree (a parenthesized arm with an
    // ORDER BY, a LIMIT or a WITH is a leaf of its own:
    // transformSetOperationTree).
    let mut leaf_types = Vec::new();
    for (arm, rows) in [&sel.larg, &sel.rarg].into_iter().zip(&arms) {
        match (arm.as_deref(), rows) {
            (Some(a), Some(r)) if in_set_operation_tree(a) => {
                leaf_types.extend(r.leaf_types.iter().cloned())
            }
            (Some(a), Some(r)) if a.op == Op::SetopNone as i32 => leaf_types.push(r.types.clone()),
            _ => {}
        }
    }
    // A UNION ALL whose leaves all yield its column types is flattened
    // into an append relation (is_simple_union_all): a qualification
    // above goes to each leaf. Else the set operation is a subquery,
    // which `qual_is_pushdown_safe` pushes a qualification into only
    // where it reads no column of a type some leaf doesn't yield
    // (compare_tlist_datatypes), nor the whole row, and the tree has no
    // EXCEPT nor a component that is a set operation of its own.
    let mismatched: Vec<bool> = (0..width)
        .map(|i| types_differ(snapshot, &leaf_types, i))
        .collect();
    let flattened =
        union_all_tree(sel) && sel.sort_clause.is_empty() && !mismatched.contains(&true);
    let unsafe_tree = pushdown_unsafe_tree(sel);
    let unpushable: Vec<bool> = mismatched
        .iter()
        .map(|&m| !flattened && (unsafe_tree || m))
        .collect();
    // Flattened, each leaf is planned on its own, computing only the
    // columns read above (remove_unused_subquery_outputs): a column fails
    // where some arm's does. Else every arm is projected whole.
    let (all_errors, first_errors) = if flattened {
        let known: Vec<&Rows> = arms.iter().flatten().collect();
        let all_errors = Errors {
            columns: (0..width)
                .map(|i| known.iter().find_map(|a| a.all.column(i)))
                .collect(),
            always: known.iter().find_map(|a| a.all.always.clone()),
        };
        // Its Append runs the arms in order (see the UNION ALL above).
        let domain = |f: Option<Failure>| f.filter(|f| f.domain);
        let first_errors = match left {
            Some(l) => Errors {
                columns: (0..width).map(|i| domain(l.first.column(i))).collect(),
                always: domain(l.first.always.clone()),
            },
            None => Errors::uniform(None, width),
        };
        (all_errors, first_errors)
    } else {
        (
            Errors::uniform(all.clone(), width),
            Errors::uniform(first, width),
        )
    };
    // Flattened, a qualification pushed down into each leaf evaluates the
    // columns it reads of every one.
    let qual_reads = if flattened {
        (0..width)
            .map(|i| {
                arms.iter()
                    .flatten()
                    .all(|a| a.qual_reads.get(i) == Some(&true))
            })
            .collect()
    } else {
        Vec::new()
    };
    let names = left.as_ref().and_then(|l| l.names.clone());
    // A column every row holds the same constant of, of the same type in
    // each leaf: a UNION's rows are its arms', an EXCEPT's and an
    // INTERSECT's its left one's.
    let values: Vec<Option<Value>> = (0..width)
        .map(|i| {
            if types_differ(snapshot, &leaf_types, i) {
                return None;
            }
            let from: Vec<&Option<Rows>> = if sel.op == Op::SetopUnion as i32 {
                arms.iter().collect()
            } else {
                vec![left]
            };
            let mut values = from
                .into_iter()
                .map(|a| a.as_ref().and_then(|a| a.values.get(i).cloned().flatten()));
            let first = values.next().flatten()?;
            values.all(|v| v.as_ref() == Some(&first)).then_some(first)
        })
        .collect();
    // Unsorted and kept whole, a UNION ALL's Append runs its arms in
    // order: past the left one's rows (at most one), the right one's
    // first row is fetched (see the UNION above for why only a domain's
    // failure counts).
    let fail_after =
        if sel.op == Op::SetopUnion as i32 && sel.all && sel.sort_clause.is_empty() && !flattened {
            left.as_ref().and_then(|l| {
                l.fail_after.clone().or_else(|| {
                    let f = right.as_ref()?.first.any().filter(|f| f.domain)?;
                    l.count.at_most_one.then_some((1, f))
                })
            })
        } else {
            None
        };
    // The leaves a qualification above is pushed down into, where every
    // one is a select list over no FROM item (an EXCEPT takes none).
    let leaves = (!unsafe_tree)
        .then(|| {
            let mut leaves = Vec::new();
            for (arm, rows) in [&sel.larg, &sel.rarg].into_iter().zip(&arms) {
                let (arm, rows) = (arm.as_deref()?, rows.as_ref()?);
                if in_set_operation_tree(arm) {
                    leaves.extend(rows.leaves.as_ref()?.leaves.iter().cloned());
                } else {
                    leaves.push(Leaf {
                        exprs: plain_leaf(arm, snapshot)?,
                        errors: rows.all.columns.clone(),
                    });
                }
            }
            leaves
                .iter()
                .all(|l| l.exprs.len() == width && l.errors.len() == width)
                .then_some(leaves)
        })
        .flatten()
        .map(|leaves| {
            let intersect = intersect_tree(sel);
            Leaves {
                leaves,
                flattened,
                in_order: union_all_tree(sel) && sel.sort_clause.is_empty(),
                all_at_once: !intersect
                    && (sel.op == Op::SetopUnion as i32 && !sel.all || !sel.sort_clause.is_empty()),
                intersect,
            }
        });
    // A column's type is its leaves', where they agree.
    let types = (0..width)
        .map(|i| {
            if types_differ(snapshot, &leaf_types, i) {
                return None;
            }
            leaf_types.iter().find_map(|l| {
                l.get(i).copied().flatten().filter(|&t| {
                    snapshot
                        .pg_type
                        .get(&t)
                        .is_some_and(|ty| ty.typtype != crate::pg_catalog::TypType::Pseudo)
                })
            })
        })
        .collect();
    Some(Rows {
        types,
        leaves,
        fail_after,
        sorted,
        projected: true,
        qual_reads,
        names,
        whole_row_unpushable: !flattened,
        keeps_columns: !flattened,
        unpushable,
        leaf_types,
        // A Sort reads every row, of each arm a MergeAppend's input.
        ordered: (!sel.sort_clause.is_empty()).then_some(all).flatten(),
        values,
        ..Rows::unknown_values(all_errors, first_errors, count)
    })
}

/// A set operation arm that is part of its parent's tree, not a leaf
/// subquery of its own (transformSetOperationTree's `isLeaf`).
fn in_set_operation_tree(s: &protobuf::SelectStmt) -> bool {
    s.op != protobuf::SetOperation::SetopNone as i32
        && s.sort_clause.is_empty()
        && s.limit_count.is_none()
        && s.limit_offset.is_none()
        && s.locking_clause.is_empty()
        && s.with_clause.is_none()
}

/// No qualification is pushed down into the set operation `sel`
/// (recurse_pushdown_safe): its tree has an EXCEPT, or a component that is
/// a set operation of its own.
fn pushdown_unsafe_tree(sel: &protobuf::SelectStmt) -> bool {
    sel.op == protobuf::SetOperation::SetopExcept as i32
        || [&sel.larg, &sel.rarg].into_iter().flatten().any(|arm| {
            if in_set_operation_tree(arm) {
                pushdown_unsafe_tree(arm)
            } else {
                arm.op != protobuf::SetOperation::SetopNone as i32
            }
        })
}

/// `sel`'s tree has an INTERSECT.
fn intersect_tree(sel: &protobuf::SelectStmt) -> bool {
    sel.op == protobuf::SetOperation::SetopIntersect as i32
        || [&sel.larg, &sel.rarg]
            .into_iter()
            .flatten()
            .any(|arm| in_set_operation_tree(arm) && intersect_tree(arm))
}

/// The select list of `s`, a set operation's leaf that reads no FROM item
/// and does nothing but project its one row.
fn plain_leaf(s: &protobuf::SelectStmt, snapshot: &PgCatalog) -> Option<Vec<protobuf::Node>> {
    let plain = s.op == protobuf::SetOperation::SetopNone as i32
        && s.from_clause.is_empty()
        && s.where_clause.is_none()
        && s.group_clause.is_empty()
        && s.having_clause.is_none()
        && s.distinct_clause.is_empty()
        && s.window_clause.is_empty()
        && s.sort_clause.is_empty()
        && s.limit_count.is_none()
        && s.limit_offset.is_none()
        && s.locking_clause.is_empty()
        && s.with_clause.is_none()
        && s.values_lists.is_empty()
        && s.into_clause.is_none()
        && super::walk::level_srf_calls(s, snapshot).is_empty();
    if !plain {
        return None;
    }
    s.target_list
        .iter()
        .map(|t| match t.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => {
                rt.val.as_deref().filter(|v| !is_column_ref(v)).cloned()
            }
            _ => None,
        })
        .collect()
}

/// `sel`'s tree is all UNION ALL, whatever its leaves
/// (is_simple_union_all_recurse).
fn union_all_tree(sel: &protobuf::SelectStmt) -> bool {
    sel.op == protobuf::SetOperation::SetopUnion as i32
        && sel.all
        && [&sel.larg, &sel.rarg]
            .into_iter()
            .flatten()
            .all(|arm| !in_set_operation_tree(arm) || union_all_tree(arm))
}

/// Two leaves surely give column `i` different types: the set operation's
/// type then differs from one of theirs. Only concrete types count — an
/// unknown-typed literal is coerced in the leaf itself.
fn types_differ(snapshot: &PgCatalog, leaves: &[Vec<Option<PgTypeOid>>], i: usize) -> bool {
    let mut known = leaves
        .iter()
        .filter_map(|l| l.get(i).copied().flatten())
        .filter(|&t| {
            snapshot
                .pg_type
                .get(&t)
                .is_some_and(|ty| ty.typtype != crate::pg_catalog::TypType::Pseudo)
        });
    let Some(first) = known.next() else {
        return false;
    };
    known.any(|t| t != first)
}

/// The leftmost arm of a set operation, which names its columns.
fn leftmost(sel: &protobuf::SelectStmt) -> &protobuf::SelectStmt {
    match sel.larg.as_deref() {
        Some(l) if sel.op != protobuf::SetOperation::SetopNone as i32 => leftmost(l),
        _ => sel,
    }
}

/// A query level's expressions, evaluated over its single FROM item's row.
#[derive(Clone, Copy)]
struct Level<'a> {
    snapshot: &'a PgCatalog,
    ctes: &'a [&'a protobuf::CommonTableExpr],
    depth: usize,
    stored: bool,
    standalone: bool,
    source: Option<&'a SourceRows>,
    /// Over the source's first row only.
    first: bool,
    /// In a WHERE.
    qual: bool,
}

impl Level<'_> {
    /// The error evaluating `n` surely raises.
    fn error(&self, n: &protobuf::Node) -> Option<Failure> {
        match n.node.as_ref()? {
            node::Node::TypeCast(tc) => {
                if let Some(e) = self.null_cast(tc) {
                    return Some(e);
                }
                // A cast evaluates its argument where no cast into the
                // target could drop it (see [`casts_evaluate_argument`]).
                if let Some(e) = tc.arg.as_deref().and_then(|a| self.error(a))
                    && tc
                        .type_name
                        .as_ref()
                        .and_then(|t| crate::ddl::util::resolve_type_name(t, self.snapshot))
                        .is_some_and(|t| casts_evaluate_argument(self.snapshot, t))
                {
                    return Some(e);
                }
            }
            node::Node::ColumnRef(cr) => {
                let source = self.source?;
                if self.qual && source.rows.projected && !source.rows.unpushable(&source.read(cr)) {
                    // Pushed down into the arms, the qualification runs
                    // before their projection — evaluating the columns it
                    // reads of each leaf.
                    return match source.read(cr) {
                        Read::Column(i) if source.rows.qual_reads.get(i) == Some(&true) => {
                            source.rows.errors(self.first).column(i)
                        }
                        _ => None,
                    };
                }
                let errors = source.rows.errors(self.first);
                return match source.read(cr) {
                    Read::Column(i) => errors.column(i),
                    Read::WholeRow => errors.columns.iter().flatten().next().cloned(),
                    Read::All | Read::Other => None,
                };
            }
            // Its argument is evaluated (a domain's coercion is not
            // folded).
            node::Node::NullTest(nt) => return self.error(nt.arg.as_deref()?),
            node::Node::SubLink(sl) => return self.sublink_error(sl),
            // COALESCE drops its NULL constant arguments, and evaluates the
            // first one left first.
            node::Node::CoalesceExpr(c) => {
                if let Some(f) = c
                    .args
                    .iter()
                    .find(|a| !is_null_const(self.snapshot, a))
                    .and_then(|a| self.error(a))
                {
                    return Some(f);
                }
            }
            node::Node::AExpr(e) => {
                if let Some(f) = self.a_expr_error(e) {
                    return Some(f);
                }
            }
            _ => {}
        }
        if !self.standalone {
            return None;
        }
        let scope = Scope::default();
        let null_ctx = NullabilityContext::default();
        // (Without the plan-time checks: what planning raises is the
        // query's analysis' to report, where the planner plans it.)
        let infer = || {
            let ctx = Ctx::new(&scope, &null_ctx, self.snapshot);
            expr::may_skip(|| {
                expr::infer_expr(n, ctx, &mut ParamCollector::default(), TypeGoal::NONE)
            })
        };
        // Only what execution adds: out of its query, the expression could
        // fail for other reasons.
        without_execution(infer).ok()?;
        infer().err().map(|e| Failure {
            message: e.to_string(),
            domain: false,
        })
    }

    /// The error evaluating the operator expression `e` surely raises.
    fn a_expr_error(&self, e: &protobuf::AExpr) -> Option<Failure> {
        use protobuf::AExprKind as K;
        let operands = || e.lexpr.iter().chain(&e.rexpr).find_map(|a| self.error(a));
        match e.kind() {
            K::AexprOp => self.operator_error(e),
            // A DistinctExpr is folded only when every argument is a
            // constant, and never inlined (eval_const_expressions); a
            // NULL literal side makes it a NullTest of the other
            // (transformAExprDistinct). A ScalarArrayOpExpr is folded only
            // when both are constants, and evaluates both, the scalar
            // first, whatever the array (ExecEvalScalarArrayOp).
            K::AexprDistinct | K::AexprNotDistinct | K::AexprOpAny | K::AexprOpAll => operands(),
            // `a BETWEEN b AND c` is `a >= b AND a <= c`, `NOT BETWEEN`
            // `a < b OR a > c` (SYMMETRIC ORs / ANDs two of those, the
            // same comparison first: transformAExprBetween): the first
            // one is evaluated first.
            K::AexprBetween | K::AexprNotBetween | K::AexprBetweenSym | K::AexprNotBetweenSym => {
                let Some(node::Node::List(bounds)) = e.rexpr.as_deref()?.node.as_ref() else {
                    return None;
                };
                let (first, second) = match e.kind() {
                    K::AexprBetween | K::AexprBetweenSym => (">=", "<="),
                    _ => ("<", ">"),
                };
                let [low, high] = bounds.items.as_slice() else {
                    return None;
                };
                let lexpr = e.lexpr.as_deref()?;
                let low_cmp = comparison(first, lexpr, low);
                if let Some(f) = self.operator_error(&low_cmp) {
                    return Some(f);
                }
                // A NULL first bound folds a strict comparison to NULL
                // (evaluate_function), which the AND (OR) drops: the
                // second one, over the other bound, is evaluated first.
                let strict = self
                    .operator(&low_cmp)
                    .and_then(|op| op.code)
                    .and_then(|f| self.snapshot.pg_proc.get(&f))
                    .is_some_and(|p| p.proisstrict);
                if !(strict && is_null_const(self.snapshot, low)) {
                    return None;
                }
                self.operator_error(&comparison(second, lexpr, high))
            }
            // `a IN (b, ...)` of literals is a ScalarArrayOpExpr `a = ANY
            // (ARRAY[b, ...])`, or `a = b OR ...` where they have no
            // common type, or `a = b` for one (transformAExprIn): `a = b`
            // evaluating `a`, all of those do, first.
            K::AexprIn => {
                let Some(node::Node::List(list)) = e.rexpr.as_deref()?.node.as_ref() else {
                    return None;
                };
                let literals = list
                    .items
                    .iter()
                    .all(|i| matches!(i.node.as_ref(), Some(node::Node::AConst(c)) if !c.isnull));
                if !literals {
                    return None;
                }
                let name = match e.name.as_slice() {
                    [n] => match n.node.as_ref()? {
                        node::Node::String(s) => s.sval.as_str(),
                        _ => return None,
                    },
                    _ => return None,
                };
                self.operator_error(&comparison(name, e.lexpr.as_deref()?, list.items.first()?))
            }
            _ => None,
        }
    }

    /// The type `n` surely has, where it is simple enough to tell: a
    /// constant, a cast, a column of the source, an operator over such or
    /// a scalar subquery.
    fn type_of(&self, n: &protobuf::Node) -> Option<PgTypeOid> {
        match n.node.as_ref()? {
            node::Node::AConst(c) => Some(expr::literals::infer_a_const(c).ok()?.type_oid),
            node::Node::TypeCast(tc) => {
                crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, self.snapshot)
            }
            node::Node::ColumnRef(cr) => {
                let source = self.source?;
                match source.read(cr) {
                    Read::Column(i) => source.rows.type_of(i),
                    _ => None,
                }
            }
            node::Node::AExpr(e) if e.kind() == protobuf::AExprKind::AexprOp => {
                Some(self.operator(e)?.result_type_oid)
            }
            node::Node::SqlvalueFunction(_) => {
                let scope = Scope::default();
                let null_ctx = NullabilityContext::default();
                let ctx = Ctx::new(&scope, &null_ctx, self.snapshot);
                expr::infer_expr(n, ctx, &mut ParamCollector::default(), TypeGoal::NONE)
                    .ok()
                    .map(|t| t.type_oid)
            }
            node::Node::SubLink(sl)
                if sl.sub_link_type == protobuf::SubLinkType::ExprSublink as i32 =>
            {
                let Some(node::Node::SelectStmt(sub)) = sl.subselect.as_deref()?.node.as_ref()
                else {
                    return None;
                };
                query_rows(sub, self.snapshot, self.ctes, self.depth + 1, self.stored)?.type_of(0)
            }
            _ => None,
        }
    }

    /// The operator `e` resolves to, when its operands' types are known.
    fn operator(&self, e: &protobuf::AExpr) -> Option<crate::lookup::ResolvedOperator> {
        let name = expr::extract_string_fields(&e.name).join(".");
        let left = match e.lexpr.as_deref() {
            Some(l) => Some(self.type_of(l)?),
            None => None,
        };
        let right = self.type_of(e.rexpr.as_deref()?)?;
        match self.snapshot.find_operator_detailed(&name, left, right) {
            crate::lookup::OperatorMatch::Found(op) => Some(op),
            _ => None,
        }
    }

    /// The error an operand of the operator `e` surely raises, where the
    /// call evaluates it (see [`operator_evaluates`]): the leftmost one
    /// failing, as ExecInterpExpr evaluates a call's arguments in order —
    /// unless the function is a SQL one, whose inlined body may evaluate
    /// them in another order: then the failing ones must raise the same
    /// error.
    fn operator_error(&self, e: &protobuf::AExpr) -> Option<Failure> {
        let operands: Vec<&protobuf::Node> = e.lexpr.iter().chain(&e.rexpr).map(|a| &**a).collect();
        let errors: Vec<Option<Failure>> = operands.iter().map(|a| self.error(a)).collect();
        let leftmost = errors.iter().position(Option::is_some)?;
        // Failing operands all raising the same error, the order they are
        // evaluated in doesn't matter: any one an inlined body evaluates
        // raises it.
        let ordered = errors
            .iter()
            .flatten()
            .all(|f| Some(f) == errors[leftmost].as_ref());
        let args: Vec<Arg> = operands
            .iter()
            .map(|a| Arg {
                ty: self.type_of(a),
                null: is_null_const(self.snapshot, a),
                not_null: self.not_null_const(a),
                value: self.value(a),
            })
            .collect();
        let resolved = self.operator(e).map(|op| op.code);
        let candidates: Vec<usize> = if ordered {
            (0..operands.len())
                .filter(|&i| errors[i].is_some())
                .collect()
        } else {
            vec![leftmost]
        };
        let failing = candidates.into_iter().find(|&failing| {
            operator_evaluates(self.snapshot, e, resolved, failing, ordered, &args, 0)
        })?;
        errors[failing].clone()
    }

    /// `n` surely is no NULL constant once the planner simplified it: a
    /// non-NULL literal (a string one is read by the type's input
    /// function, which can't return NULL for one), a built-in cast of one,
    /// a stable SQL value function (`CURRENT_DATE`, not folded), a column
    /// holding a constant, or an expression whose evaluation surely fails.
    fn not_null_const(&self, n: &protobuf::Node) -> bool {
        match n.node.as_ref() {
            Some(node::Node::AConst(c)) => !c.isnull,
            Some(node::Node::TypeCast(tc))
                if tc.arg.as_deref().is_some_and(|a| self.not_null_const(a))
                    && tc
                        .type_name
                        .as_ref()
                        .and_then(|t| crate::ddl::util::resolve_type_name(t, self.snapshot))
                        .is_some_and(|t| builtin_casts_into(self.snapshot, t)) =>
            {
                true
            }
            Some(node::Node::SqlvalueFunction(_)) => true,
            Some(node::Node::ColumnRef(cr))
                if self.source.is_some_and(|s| match s.read(cr) {
                    Read::Column(i) => s.rows.values.get(i).is_some_and(Option::is_some),
                    _ => false,
                }) =>
            {
                true
            }
            _ => self.error(n).is_some(),
        }
    }

    /// `NULL::d` of a domain `d` rejecting it.
    fn null_cast(&self, tc: &protobuf::TypeCast) -> Option<Failure> {
        if !matches!(
            tc.arg.as_deref().and_then(|a| a.node.as_ref()),
            Some(node::Node::AConst(c)) if c.isnull
        ) {
            return None;
        }
        let target = crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, self.snapshot)?;
        let message = self.snapshot.domain_null_violation_at_execution(target)?;
        Some(Failure {
            message,
            domain: true,
        })
    }

    /// A scalar or ARRAY subquery evaluates its rows' one column (an
    /// uncorrelated one as an initplan, where its value is needed).
    fn sublink_error(&self, sl: &protobuf::SubLink) -> Option<Failure> {
        let scalar = sl.sub_link_type == protobuf::SubLinkType::ExprSublink as i32;
        if !scalar && sl.sub_link_type != protobuf::SubLinkType::ArraySublink as i32 {
            return None;
        }
        let Some(node::Node::SelectStmt(sub)) = sl.subselect.as_deref()?.node.as_ref() else {
            return None;
        };
        let rows = query_rows(sub, self.snapshot, self.ctes, self.depth + 1, self.stored)?;
        // A scalar subquery stops at its second row.
        if scalar && !rows.count.at_most_one {
            return None;
        }
        rows.all
            .always
            .clone()
            .or_else(|| rows.all.column(0))
            .or_else(|| rows.projection.clone())
    }

    /// The constant `n` surely is.
    fn value(&self, n: &protobuf::Node) -> Option<Value> {
        match n.node.as_ref()? {
            node::Node::ColumnRef(cr) => {
                let source = self.source?;
                match source.read(cr) {
                    Read::Column(i) => source.rows.values.get(i).cloned().flatten(),
                    _ => None,
                }
            }
            // An integer literal cast to an integer type it fits, or to a
            // domain over one without CHECK constraints (its NOT NULL
            // holds): the value is the literal's.
            node::Node::TypeCast(tc) => {
                let Some(Value::Int(i)) = literal_value(tc.arg.as_deref()?) else {
                    return None;
                };
                let type_name = tc.type_name.as_ref()?;
                if !type_name.typmods.is_empty() || !type_name.array_bounds.is_empty() {
                    return None;
                }
                let t = crate::ddl::util::resolve_type_name(type_name, self.snapshot)?;
                let fits = match self.snapshot.unwrap_domain(t) {
                    oid::INT2 => i16::try_from(i).is_ok(),
                    oid::INT4 => i32::try_from(i).is_ok(),
                    oid::INT8 => true,
                    _ => false,
                };
                let checked = self.snapshot.domain_chain(t).iter().any(|d| {
                    self.snapshot
                        .domain_constraints
                        .get(d)
                        .into_iter()
                        .flatten()
                        .any(|c| c.kind == crate::ddl::types::DomainConstraintKind::Check)
                });
                (fits && !checked).then_some(Value::Int(i))
            }
            _ => literal_value(n),
        }
    }

    /// What `n`, a qualification, surely evaluates to (see
    /// [`truth_with`]).
    fn truth(&self, n: &protobuf::Node) -> Option<bool> {
        truth_with(n, &|v| self.value(v))
    }

    /// The error a qualification surely raises: one testing a failing
    /// value, ANDed with ones that hold (the planner orders the others by
    /// cost, so one that may be false could run first and skip it).
    fn surely_fails(&self, n: &protobuf::Node) -> Option<Failure> {
        let level = Level {
            qual: true,
            ..*self
        };
        match n.node.as_ref()? {
            node::Node::BoolExpr(b) if b.boolop == protobuf::BoolExprType::AndExpr as i32 => {
                let mut failing = None;
                let mut left = Vec::new();
                for a in &b.args {
                    if self.truth(a) == Some(true) {
                        continue;
                    }
                    failing = Some(self.surely_fails(a)?);
                    left.push(a);
                }
                // Over a flattened UNION ALL, each leaf is an appendrel
                // member, whose restrictions contradicting each other
                // leave it out (relation_excluded_by_constraints).
                let member = self
                    .source
                    .is_some_and(|s| s.rows.projected && !s.rows.qual_reads.is_empty());
                let args: Vec<&protobuf::Node> = b.args.iter().collect();
                if !self.equalities_consistent(&args) || member && refutable(&left, self.snapshot) {
                    return None;
                }
                failing
            }
            // An OR whose arguments left (the ones not folded to false
            // nor NULL) all surely fail evaluates one of them first; one
            // folded to true folds it away. A NOT evaluates its argument.
            node::Node::BoolExpr(b) if b.boolop == protobuf::BoolExprType::OrExpr as i32 => {
                let mut failing = None;
                for a in &b.args {
                    match self.truth(a) {
                        Some(true) => return None,
                        Some(false) => continue,
                        None if is_null_const(self.snapshot, a) => continue,
                        None => {}
                    }
                    let f = self.surely_fails(a)?;
                    failing.get_or_insert(f);
                }
                failing
            }
            node::Node::BoolExpr(b) if b.boolop == protobuf::BoolExprType::NotExpr as i32 => {
                match b.args.as_slice() {
                    [a] => self.surely_fails(a),
                    _ => None,
                }
            }
            node::Node::BoolExpr(_) => None,
            _ => level.error(n),
        }
    }

    /// The equalities `conjuncts` may leave as conjuncts (see
    /// [`promoted_equalities`]) contradict nothing: two of them may
    /// contradict each other through their equivalence class (`b = 3 AND
    /// b = 4`, or `b = c AND b = 3 AND c = 4`), and the planner derive a
    /// constant false from it, leaving the scan dummy
    /// (generate_base_implied_equalities_const). Equalities each of a
    /// column of the source and a constant, the constants of a column all
    /// equal, make classes of a column and one constant (`b = 3 AND b =
    /// 3`: `3 = 3` is folded to true). Over no FROM item, a qualification
    /// reads no Var: each conjunct is a gating qual, in no class.
    fn equalities_consistent(&self, conjuncts: &[&protobuf::Node]) -> bool {
        let equalities: Vec<&protobuf::AExpr> = conjuncts
            .iter()
            .flat_map(|c| promoted_equalities(c, self.snapshot))
            .collect();
        if equalities.len() < 2 {
            return true;
        }
        let Some(source) = self.source.filter(|s| !s.rows.var_free) else {
            return true;
        };
        // Constants of one column are equal when they are the same
        // literal, or the same number (`3`, `3.0`, `3::int8`, and of a
        // column of an integer or numeric type `'3'`).
        let column_const = |e: &protobuf::AExpr| -> Option<(usize, EqConst)> {
            if expr::extract_string_fields(&e.name) != ["="] {
                return None;
            }
            // A single-item IN is one `=` (transformAExprIn).
            let r = match (e.kind(), e.rexpr.as_deref().and_then(|r| r.node.as_ref())) {
                (protobuf::AExprKind::AexprOp, _) => e.rexpr.as_deref()?,
                (protobuf::AExprKind::AexprIn, Some(node::Node::List(l))) => {
                    match l.items.as_slice() {
                        [item] => item,
                        _ => return None,
                    }
                }
                _ => return None,
            };
            let l = e.lexpr.as_deref()?;
            let (col, lit) = match (l.node.as_ref()?, r.node.as_ref()?) {
                (node::Node::ColumnRef(cr), _) => (cr, r),
                (_, node::Node::ColumnRef(cr)) => (cr, l),
                _ => return None,
            };
            let Read::Column(i) = source.read(col) else {
                return None;
            };
            let numeric = source
                .rows
                .type_of(i)
                .is_some_and(|t| [oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC].contains(&t));
            Some((i, eq_const(lit, numeric, self.snapshot)?))
        };
        let pairs: Option<Vec<(usize, EqConst)>> =
            equalities.iter().map(|e| column_const(e)).collect();
        pairs.is_some_and(|pairs| {
            pairs
                .iter()
                .all(|(c, v)| pairs.iter().filter(|(d, _)| d == c).all(|(_, w)| w == v))
        })
    }

    /// What the WHERE `w` of `sel` raises where it is pushed down into the
    /// leaves of the set operation `sel` reads (see [`Leaf`]): reading
    /// every row, fetching the first one, and past how many rows.
    fn pushed_down(&self, sel: &protobuf::SelectStmt, w: &protobuf::Node) -> Option<Pushed> {
        let source = self.source?;
        let leaves = source.rows.leaves.as_ref()?;
        // Not flattened, the qualification goes down only where
        // `qual_is_pushdown_safe` lets every conjunct through (no column
        // that can't be, no whole row, no subquery); a volatile function
        // could be left above, or run first in the leaf's filter.
        let mut safe = true;
        super::walk::visit_same_level(w, &mut |x| match x.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) => {
                let read = source.read(cr);
                safe &= matches!(read, Read::Column(_) | Read::WholeRow)
                    && (leaves.flattened
                        || matches!(read, Read::Column(_)) && !source.rows.unpushable(&read));
            }
            Some(node::Node::SubLink(_)) => safe = false,
            Some(node::Node::FuncCall(_)) => safe &= leaves.flattened,
            _ => {}
        });
        if !safe || !self.equalities_consistent(&conjuncts(w)) {
            return None;
        }
        // The columns the select list reads (projected by a flattened
        // UNION ALL's leaf, which computes no other).
        let mut read = vec![false; source.names.len()];
        for target in &sel.target_list {
            if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
                && let Some(node::Node::ColumnRef(cr)) =
                    rt.val.as_deref().and_then(|v| v.node.as_ref())
            {
                match source.read(cr) {
                    Read::Column(i) => read[i] = true,
                    Read::All | Read::WholeRow => read.iter_mut().for_each(|r| *r = true),
                    Read::Other => {}
                }
            }
        }
        let level = Level {
            source: None,
            first: false,
            qual: true,
            standalone: false,
            ..*self
        };
        // What each leaf does: `Ok(None)` yields a row, `Ok(Some(f))`
        // raises `f`; `Err(true)` is left out, `Err(false)` unknown.
        let results: Vec<Result<Option<Failure>, bool>> = leaves
            .leaves
            .iter()
            .map(|leaf| {
                let q = substitute(w, source, leaf)?;
                Some(match level.leaf_outcome(&q, leaves.flattened) {
                    LeafOutcome::Dummy => Err(true),
                    LeafOutcome::Unknown => Err(false),
                    LeafOutcome::Fails(f) => Ok(Some(f)),
                    LeafOutcome::Runs => Ok(leaf
                        .errors
                        .iter()
                        .enumerate()
                        .filter(|&(i, _)| !leaves.flattened || read[i])
                        .find_map(|(_, e)| e.clone())),
                })
            })
            .collect::<Option<_>>()?;
        let raised = |r: &Result<Option<Failure>, bool>| r.clone().ok().flatten();
        // Reading every row runs every leaf — of an INTERSECT, maybe only
        // one input.
        let all = if leaves.intersect {
            results
                .iter()
                .map(raised)
                .collect::<Option<Vec<_>>>()
                .and_then(|f| f.into_iter().next())
        } else {
            results.iter().find_map(raised)
        };
        // An Append runs its leaves in order: the ones left out first, the
        // rows of the ones that hold, up to one that raises (only a
        // domain's failure counts: see the UNION in [`set_op_rows`]).
        let mut fail_after = None;
        if leaves.in_order && !leaves.all_at_once && !leaves.intersect {
            let mut rows = 0i64;
            for r in &results {
                match r {
                    Err(true) => {}
                    Ok(None) => rows += 1,
                    Ok(Some(f)) if f.domain => {
                        fail_after = Some((rows, f.clone()));
                        break;
                    }
                    Ok(Some(_)) | Err(false) => break,
                }
            }
        }
        let first = if leaves.all_at_once || leaves.intersect {
            all.clone()
        } else {
            fail_after
                .as_ref()
                .filter(|(rows, _)| *rows == 0)
                .map(|(_, f)| f.clone())
        };
        Some(Pushed {
            all,
            first,
            fail_after,
        })
    }

    /// The conjuncts the qualification `q` is split into once folded over
    /// a leaf's constants: an AND's, and those of an OR left with one
    /// argument (the others folded to false or NULL).
    fn leaf_conjuncts<'n>(&self, q: &'n protobuf::Node, out: &mut Vec<&'n protobuf::Node>) {
        use protobuf::BoolExprType as B;
        match q.node.as_ref() {
            Some(node::Node::BoolExpr(b)) if b.boolop == B::AndExpr as i32 => {
                for a in &b.args {
                    self.leaf_conjuncts(a, out);
                }
            }
            Some(node::Node::BoolExpr(b)) if b.boolop == B::OrExpr as i32 => {
                let left: Vec<&protobuf::Node> = b
                    .args
                    .iter()
                    .filter(|a| self.truth(a) != Some(false) && !is_null_const(self.snapshot, a))
                    .collect();
                match left.as_slice() {
                    [only] if self.truth(only).is_none() => self.leaf_conjuncts(only, out),
                    _ => out.push(q),
                }
            }
            _ => out.push(q),
        }
    }

    /// What a leaf does with the qualification `q` pushed down into it (its
    /// columns replaced by the leaf's expressions): each conjunct folded
    /// to false or NULL leaves it out, to true is dropped; the others are
    /// its One-Time Filter, run in an order of their costs — every one
    /// must surely fail. Flattened, a volatile conjunct is in a filter of
    /// its own, run after.
    fn leaf_outcome(&self, q: &protobuf::Node, flattened: bool) -> LeafOutcome {
        let mut failure = None;
        let mut unknown = false;
        let mut volatile = false;
        let mut failing = Vec::new();
        let mut parts = Vec::new();
        self.leaf_conjuncts(q, &mut parts);
        for c in parts {
            match self.truth(c) {
                Some(true) => continue,
                Some(false) => return LeafOutcome::Dummy,
                None if is_null_const(self.snapshot, c) => return LeafOutcome::Dummy,
                None => {}
            }
            if flattened && surely_volatile(c, self.snapshot) {
                volatile = true;
                continue;
            }
            match self.surely_fails(c) {
                Some(f) => {
                    failure.get_or_insert(f);
                    failing.push(c);
                }
                None => unknown = true,
            }
        }
        // Flattened, the leaf is an appendrel member, left out where its
        // restrictions contradict each other (relation_excluded_by_constraints).
        if flattened && refutable(&failing, self.snapshot) {
            unknown = true;
        }
        match failure {
            _ if unknown => LeafOutcome::Unknown,
            Some(f) => LeafOutcome::Fails(f),
            None if volatile => LeafOutcome::Unknown,
            None => LeafOutcome::Runs,
        }
    }

    /// The qualification `n` over a set operation the planner keeps whole
    /// is pushed down into none of its arms, nor folded to a constant
    /// (which could leave the scan dummy): each of its conjuncts not
    /// surely true reads a column that can't be pushed down
    /// (`qual_is_pushdown_safe`), through nothing but operators, casts
    /// and NULL tests of columns and non-NULL literals.
    fn unpushed(&self, n: &protobuf::Node) -> bool {
        let Some(source) = self
            .source
            .filter(|s| s.rows.keeps_columns || s.rows.projection.is_some())
        else {
            return false;
        };
        let conjuncts: Vec<&protobuf::Node> = conjuncts(n)
            .into_iter()
            .filter(|c| self.truth(c) != Some(true))
            .collect();
        if !self.equalities_consistent(&conjuncts) {
            return false;
        }
        let reads_column = |n: &protobuf::Node| {
            let mut reads = false;
            super::walk::visit_same_level(n, &mut |x| {
                reads |= matches!(x.node, Some(node::Node::ColumnRef(_)));
            });
            reads
        };
        conjuncts.into_iter().all(|c| {
            let mut simple = true;
            let mut reads = false;
            super::walk::visit_same_level(c, &mut |x| match x.node.as_ref() {
                Some(node::Node::ColumnRef(cr)) => {
                    reads |= source.rows.unpushable(&source.read(cr));
                }
                Some(node::Node::AConst(a)) => simple &= !a.isnull,
                // An IN list of literals is one `=`, or `= ANY` of an
                // array constant (transformAExprIn).
                Some(node::Node::AExpr(e)) => {
                    simple &= matches!(
                        e.kind(),
                        protobuf::AExprKind::AexprOp | protobuf::AExprKind::AexprIn
                    );
                }
                Some(node::Node::List(l)) => {
                    simple &= l
                        .items
                        .iter()
                        .all(|i| matches!(i.node, Some(node::Node::AConst(_))));
                }
                // Each argument reading a column, none is folded to a
                // constant.
                Some(node::Node::BoolExpr(b)) => simple &= b.args.iter().all(reads_column),
                Some(node::Node::TypeCast(_) | node::Node::NullTest(_)) => {}
                Some(node::Node::TypeName(_) | node::Node::String(_)) => {}
                _ => simple = false,
            });
            simple && reads
        })
    }
}

/// Restrictions `failing` may contradict each other, the planner proving
/// it (predicate_refuted_by): two different ones, or one that is folded
/// into several (an AND inside, or a NOT of an AND or OR, which
/// eval_const_expressions negates into one). One alone, or repeated,
/// refutes nothing.
fn refutable(failing: &[&protobuf::Node], snapshot: &PgCatalog) -> bool {
    let distinct: std::collections::HashSet<String> = failing
        .iter()
        .map(|c| equivalence_key(c, snapshot))
        .collect();
    distinct.len() > 1
        || failing.iter().any(|c| {
            let mut compound = false;
            super::walk::visit_same_level(c, &mut |x| {
                if let Some(node::Node::BoolExpr(b)) = x.node.as_ref() {
                    compound |= b.boolop == protobuf::BoolExprType::AndExpr as i32
                        || b.boolop == protobuf::BoolExprType::NotExpr as i32
                            && matches!(
                                b.args.first().and_then(|a| a.node.as_ref()),
                                Some(node::Node::BoolExpr(_))
                            );
                }
            });
            compound
        })
}

/// What a WHERE pushed down into a set operation's leaves surely raises
/// (see [`Level::pushed_down`]).
#[derive(Default)]
struct Pushed {
    all: Option<Failure>,
    first: Option<Failure>,
    fail_after: Option<(i64, Failure)>,
}

/// `n` calls a function every overload of which is volatile.
fn surely_volatile(n: &protobuf::Node, snapshot: &PgCatalog) -> bool {
    let mut volatile = false;
    super::walk::visit_same_level(n, &mut |x| {
        if let Some(node::Node::FuncCall(fc)) = x.node.as_ref() {
            volatile |= crate::ddl::volatile::surely_volatile(fc, snapshot);
        }
    });
    volatile
}

/// The qualification `n` over `source` pushed down into `leaf`: each
/// column replaced by the leaf's expression, a whole-row NULL test by
/// one of each column (eval_const_expressions expands a ROW's); `None`
/// where it reads something else, or through an expression not followed.
fn substitute(n: &protobuf::Node, source: &SourceRows, leaf: &Leaf) -> Option<protobuf::Node> {
    let sub = |n: &protobuf::Node| substitute(n, source, leaf);
    let subs = |ns: &[protobuf::Node]| ns.iter().map(sub).collect::<Option<Vec<_>>>();
    let boxed = |n: &Option<Box<protobuf::Node>>| match n {
        Some(n) => sub(n).map(|n| Some(Box::new(n))),
        None => Some(None),
    };
    let wrap = |n: node::Node| protobuf::Node { node: Some(n) };
    let inner = n.node.as_ref()?;
    Some(match inner {
        node::Node::ColumnRef(cr) => match source.read(cr) {
            Read::Column(i) => leaf.exprs.get(i)?.clone(),
            _ => return None,
        },
        node::Node::NullTest(nt)
            if matches!(
                nt.arg.as_deref().and_then(|a| a.node.as_ref()),
                Some(node::Node::ColumnRef(cr)) if matches!(source.read(cr), Read::WholeRow)
            ) =>
        {
            let tests: Vec<protobuf::Node> = leaf
                .exprs
                .iter()
                .map(|e| {
                    wrap(node::Node::NullTest(Box::new(protobuf::NullTest {
                        arg: Some(Box::new(e.clone())),
                        argisrow: false,
                        ..(**nt).clone()
                    })))
                })
                .collect();
            match <[protobuf::Node; 1]>::try_from(tests) {
                Ok([t]) => t,
                Err(args) => wrap(node::Node::BoolExpr(Box::new(protobuf::BoolExpr {
                    boolop: protobuf::BoolExprType::AndExpr as i32,
                    args,
                    location: -1,
                    ..Default::default()
                }))),
            }
        }
        node::Node::NullTest(nt) => wrap(node::Node::NullTest(Box::new(protobuf::NullTest {
            arg: boxed(&nt.arg)?,
            ..(**nt).clone()
        }))),
        node::Node::AExpr(e) => wrap(node::Node::AExpr(Box::new(protobuf::AExpr {
            lexpr: boxed(&e.lexpr)?,
            rexpr: boxed(&e.rexpr)?,
            ..(**e).clone()
        }))),
        node::Node::List(l) => wrap(node::Node::List(protobuf::List {
            items: subs(&l.items)?,
        })),
        node::Node::BoolExpr(b) => wrap(node::Node::BoolExpr(Box::new(protobuf::BoolExpr {
            args: subs(&b.args)?,
            ..(**b).clone()
        }))),
        node::Node::TypeCast(tc) => wrap(node::Node::TypeCast(Box::new(protobuf::TypeCast {
            arg: boxed(&tc.arg)?,
            ..(**tc).clone()
        }))),
        node::Node::CoalesceExpr(c) => {
            wrap(node::Node::CoalesceExpr(Box::new(protobuf::CoalesceExpr {
                args: subs(&c.args)?,
                ..(**c).clone()
            })))
        }
        node::Node::FuncCall(fc)
            if fc.agg_order.is_empty() && fc.agg_filter.is_none() && fc.over.is_none() =>
        {
            wrap(node::Node::FuncCall(Box::new(protobuf::FuncCall {
                args: subs(&fc.args)?,
                ..(**fc).clone()
            })))
        }
        _ => {
            let mut reads = false;
            super::walk::visit_same_level(n, &mut |x| {
                reads |= matches!(x.node, Some(node::Node::ColumnRef(_)));
            });
            if reads {
                return None;
            }
            n.clone()
        }
    })
}

/// How deep [`call_evaluates`] reads SQL function bodies into the calls
/// they make.
const MAX_BODY_DEPTH: usize = 8;

/// The planner support functions that never simplify a call away: they
/// answer only index-condition and selectivity requests
/// (`like_regex_support`, `network_subset_support`), in-place
/// modification ones (`array_append_support`), or reduce a length
/// coercion to its first argument (`TemporalSimplify`, `numeric_support`,
/// `varchar_support`, `varbit_support`). Another one — say
/// `range_contains_elem_support`, which folds `elem <@ 'empty'` to false
/// — may drop an argument.
const ARGUMENT_KEEPING_SUPPORT: [&str; 14] = [
    "textlike_support",
    "texticlike_support",
    "textregexeq_support",
    "texticregexeq_support",
    "text_starts_with_support",
    "network_subset_support",
    "array_append_support",
    "array_prepend_support",
    "varchar_support",
    "numeric_support",
    "varbit_support",
    "timestamp_support",
    "interval_support",
    "time_support",
];

/// `p` has no support function that could simplify its calls away.
fn keeps_arguments(snapshot: &PgCatalog, p: &crate::pg_catalog::PgProc) -> bool {
    p.prosupport.is_none_or(|s| {
        snapshot.pg_proc.get(&s).is_some_and(|f| {
            snapshot.namespace_name(f.pronamespace) == Some("pg_catalog")
                && ARGUMENT_KEEPING_SUPPORT.contains(&f.proname.as_str())
        })
    })
}

/// FirstNormalObjectId (transam.h): below, what initdb made.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// Every cast into `target` (its base type, for a domain) is built in, or
/// needs no function: a cast of a non-NULL constant, folded by the
/// planner, is no NULL constant (a user's cast function could return one).
fn builtin_casts_into(snapshot: &PgCatalog, target: PgTypeOid) -> bool {
    let target = snapshot.unwrap_domain(target);
    snapshot
        .pg_cast
        .values()
        .filter(|c| c.casttarget == target)
        .all(|c| c.castfunc.is_none_or(|f| f.get() < FIRST_NORMAL_OBJECT_ID))
}

/// A cast into `target` evaluates its argument: none is done by a SQL
/// function (inlined, its body could ignore it) or one whose support
/// function could drop it.
fn casts_evaluate_argument(snapshot: &PgCatalog, target: PgTypeOid) -> bool {
    let target = snapshot.unwrap_domain(target);
    snapshot
        .pg_cast
        .values()
        .filter(|c| c.casttarget == target)
        .all(|c| {
            c.castfunc.is_none_or(|f| {
                snapshot.pg_proc.get(&f).is_some_and(|p| {
                    p.prolang != crate::pg_catalog::SQL_LANGUAGE && keeps_arguments(snapshot, p)
                })
            })
        })
}

/// What a call is known to get as one of its arguments.
#[derive(Clone)]
struct Arg {
    /// Its type, where known (a polymorphic parameter has it).
    ty: Option<PgTypeOid>,
    /// It surely is a NULL constant once simplified.
    null: bool,
    /// It surely is no NULL constant once simplified.
    not_null: bool,
    /// The constant it surely is.
    value: Option<Value>,
}

/// `n` surely is a NULL constant once simplified: a NULL literal, or one
/// cast to a type that is no domain (a domain's coercion is not folded).
fn is_null_const(snapshot: &PgCatalog, n: &protobuf::Node) -> bool {
    match n.node.as_ref() {
        Some(node::Node::AConst(c)) => c.isnull,
        Some(node::Node::TypeCast(tc)) => {
            tc.arg
                .as_deref()
                .is_some_and(|a| is_null_const(snapshot, a))
                && tc
                    .type_name
                    .as_ref()
                    .and_then(|t| crate::ddl::util::resolve_type_name(t, snapshot))
                    .is_some_and(|t| snapshot.unwrap_domain(t) == t)
        }
        _ => false,
    }
}

/// The operator `e` evaluates its operand `operand` (counted from its left
/// one): its function — `code`, where its operands' types resolved it;
/// else every operator of its name and arity must — evaluates that
/// argument (see [`call_evaluates`]). `args` tell what is known of each
/// operand: a strict function needs every other one to be no NULL
/// constant, which it is folded to NULL over (evaluate_function); an
/// inlined body needs each to be known a NULL constant or not, as it gets
/// it substituted. `ordered`: it doesn't matter in which order the
/// operands are evaluated (an inlined body may reorder them).
fn operator_evaluates(
    snapshot: &PgCatalog,
    e: &protobuf::AExpr,
    code: Option<Option<crate::oid::PgProcOid>>,
    operand: usize,
    ordered: bool,
    args: &[Arg],
    depth: usize,
) -> bool {
    let others = |known: &dyn Fn(&Arg) -> bool| {
        args.iter()
            .enumerate()
            .all(|(i, a)| i == operand || known(a))
    };
    let evaluates = |code: Option<crate::oid::PgProcOid>| {
        let Some(p) = code.and_then(|f| snapshot.pg_proc.get(&f)) else {
            return false;
        };
        let sql = p.prolang == crate::pg_catalog::SQL_LANGUAGE;
        if p.proisstrict && !others(&|a| a.not_null)
            || sql && (!ordered || !others(&|a| a.not_null || a.null))
        {
            return false;
        }
        call_evaluates(snapshot, p, operand, args, depth)
    };
    if let Some(code) = code {
        return evaluates(code);
    }
    let Some(node::Node::String(name)) = e.name.last().and_then(|n| n.node.as_ref()) else {
        return false;
    };
    let prefix = e.lexpr.is_none();
    snapshot
        .pg_operator
        .values()
        .filter(|o| o.oprname == name.sval && o.oprleft.is_none() == prefix)
        .all(|o| evaluates(o.oprcode))
}

/// A call of `p` evaluates its argument `arg`: ExecInterpExpr evaluates a
/// call's arguments before it, and the planner neither folds a domain's
/// coercion (eval_const_expressions) nor drops an argument — unless a
/// support function simplifies the call, or `p` is a SQL function it
/// inlines (`inline_function`), whose body is then evaluated instead,
/// with the arguments `args` substituted: that body must evaluate the
/// parameter.
fn call_evaluates(
    snapshot: &PgCatalog,
    p: &crate::pg_catalog::PgProc,
    arg: usize,
    args: &[Arg],
    depth: usize,
) -> bool {
    if !keeps_arguments(snapshot, p) {
        return false;
    }
    if p.prolang != crate::pg_catalog::SQL_LANGUAGE {
        return true;
    }
    // A body that isn't one expression is never inlined: the call
    // evaluates its arguments, as any other.
    let Some(body) = snapshot.inline_sql_bodies.get(&p.oid) else {
        return true;
    };
    depth < MAX_BODY_DEPTH
        && Body {
            snapshot,
            proc: p,
            args,
            depth,
        }
        .evaluates(body, arg)
}

/// The expression of an inlinable SQL function's body, its parameters
/// substituted by the call's arguments.
struct Body<'a> {
    snapshot: &'a PgCatalog,
    proc: &'a crate::pg_catalog::PgProc,
    args: &'a [Arg],
    depth: usize,
}

impl Body<'_> {
    /// The parameter `n` refers to: `$n`, or its name.
    fn param(&self, n: &protobuf::Node) -> Option<usize> {
        match n.node.as_ref()? {
            node::Node::ParamRef(p) => usize::try_from(p.number).ok()?.checked_sub(1),
            node::Node::ColumnRef(cr) => match cr.fields.as_slice() {
                [f] => match f.node.as_ref()? {
                    node::Node::String(s) => self
                        .proc
                        .proargnames
                        .iter()
                        .position(|a| !a.is_empty() && *a == s.sval),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
        .filter(|&i| i < self.proc.proargtypes.len() && i < self.args.len())
    }

    /// The type `n` surely has, where it is simple enough to tell: a
    /// parameter's (a polymorphic one's is its argument's), a literal's,
    /// a cast's, an operator's result.
    fn type_of(&self, n: &protobuf::Node) -> Option<PgTypeOid> {
        if let Some(i) = self.param(n) {
            let declared = self.proc.proargtypes[i];
            return if crate::polymorphic::is_polymorphic(declared) {
                self.args[i].ty
            } else {
                Some(declared)
            };
        }
        match n.node.as_ref()? {
            node::Node::AConst(c) => Some(expr::literals::infer_a_const(c).ok()?.type_oid),
            node::Node::TypeCast(tc) => {
                crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, self.snapshot)
            }
            node::Node::AExpr(e) if e.kind() == protobuf::AExprKind::AexprOp => {
                Some(self.operator(e)?.result_type_oid)
            }
            _ => None,
        }
    }

    fn operator(&self, e: &protobuf::AExpr) -> Option<crate::lookup::ResolvedOperator> {
        let name = expr::extract_string_fields(&e.name).join(".");
        let left = match e.lexpr.as_deref() {
            Some(l) => Some(self.type_of(l)?),
            None => None,
        };
        let right = self.type_of(e.rexpr.as_deref()?)?;
        match self.snapshot.find_operator_detailed(&name, left, right) {
            crate::lookup::OperatorMatch::Found(op) => Some(op),
            _ => None,
        }
    }

    /// The constant `n` surely is: a literal, or a parameter given one.
    fn value(&self, n: &protobuf::Node) -> Option<Value> {
        match self.param(n) {
            Some(i) => self.args[i].value.clone(),
            None => literal_value(n),
        }
    }

    /// What `n`, a condition, is folded to (see [`truth_with`]).
    fn truth(&self, n: &protobuf::Node) -> Option<bool> {
        truth_with(n, &|v| self.value(v))
    }

    /// `n` surely evaluates parameter `k` (the failing argument, no
    /// constant), and isn't folded away.
    fn evaluates(&self, n: &protobuf::Node, k: usize) -> bool {
        if self.param(n) == Some(k) {
            return true;
        }
        match n.node.as_ref() {
            Some(node::Node::TypeCast(tc)) => {
                tc.arg.as_deref().is_some_and(|a| self.evaluates(a, k))
                    && tc
                        .type_name
                        .as_ref()
                        .and_then(|t| crate::ddl::util::resolve_type_name(t, self.snapshot))
                        .is_some_and(|t| casts_evaluate_argument(self.snapshot, t))
            }
            Some(node::Node::AExpr(e)) if e.kind() == protobuf::AExprKind::AexprOp => {
                let operands: Vec<&protobuf::Node> =
                    e.lexpr.iter().chain(&e.rexpr).map(|a| &**a).collect();
                let reading: Vec<bool> = operands.iter().map(|a| self.evaluates(a, k)).collect();
                let Some(operand) = reading.iter().position(|&r| r) else {
                    return false;
                };
                let args: Vec<Arg> = operands
                    .iter()
                    .enumerate()
                    .map(|(i, a)| Arg {
                        ty: self.type_of(a),
                        // The parameter read is no constant.
                        null: i != operand && self.null_const(a),
                        not_null: i == operand || self.not_null_const(a),
                        value: self.value(a),
                    })
                    .collect();
                operator_evaluates(
                    self.snapshot,
                    e,
                    self.operator(e).map(|op| op.code),
                    operand,
                    true,
                    &args,
                    self.depth + 1,
                )
            }
            // A NullTest of no constant isn't folded; a DistinctExpr is
            // folded only when every argument is a constant (see
            // [`Level::a_expr_error`]).
            Some(node::Node::NullTest(nt)) => {
                nt.arg.as_deref().is_some_and(|a| self.evaluates(a, k))
            }
            Some(node::Node::AExpr(e))
                if matches!(
                    e.kind(),
                    protobuf::AExprKind::AexprDistinct | protobuf::AExprKind::AexprNotDistinct
                ) =>
            {
                e.lexpr.iter().chain(&e.rexpr).any(|a| self.evaluates(a, k))
            }
            // COALESCE drops its NULL constant arguments, and keeps the
            // first other one, no constant, first (a non-NULL constant
            // first is its value).
            Some(node::Node::CoalesceExpr(c)) => c
                .args
                .iter()
                .find(|a| !self.null_const(a))
                .is_some_and(|a| self.evaluates(a, k)),
            // An AND or OR drops its NULL constant arguments and the ones
            // folded to the value that doesn't decide it (`$2 > 0` over
            // `1`, in an AND), is folded to a constant when one is folded
            // to the deciding one, and stops at the first one deciding
            // it: each one left must evaluate the parameter (no constant
            // then). A NOT evaluates its argument.
            Some(node::Node::BoolExpr(b)) => {
                let deciding = match protobuf::BoolExprType::try_from(b.boolop) {
                    Ok(protobuf::BoolExprType::AndExpr) => false,
                    Ok(protobuf::BoolExprType::OrExpr) => true,
                    _ => return b.args.len() == 1 && self.evaluates(&b.args[0], k),
                };
                let mut left = Vec::new();
                for a in &b.args {
                    match self.truth(a) {
                        Some(t) if t == deciding => return false,
                        Some(_) => {}
                        None if self.null_const(a) => {}
                        None => left.push(a),
                    }
                }
                !left.is_empty() && left.iter().all(|a| self.evaluates(a, k))
            }
            // A searched CASE drops the WHENs folded to false (or NULL),
            // and stops at one folded to true, its result then the
            // CASE's; it evaluates the first WHEN left first
            // (eval_const_expressions' CaseExpr).
            Some(node::Node::CaseExpr(c)) if c.arg.is_none() => {
                for w in &c.args {
                    let Some(node::Node::CaseWhen(w)) = w.node.as_ref() else {
                        return false;
                    };
                    let Some(cond) = w.expr.as_deref() else {
                        return false;
                    };
                    match self.truth(cond) {
                        Some(true) => {
                            return w.result.as_deref().is_some_and(|r| self.evaluates(r, k));
                        }
                        Some(false) => continue,
                        None => return self.evaluates(cond, k),
                    }
                }
                c.defresult.as_deref().is_some_and(|d| self.evaluates(d, k))
            }
            _ => false,
        }
    }

    /// `n` surely is a NULL constant once simplified: a parameter given
    /// one, or a NULL literal (maybe cast).
    fn null_const(&self, n: &protobuf::Node) -> bool {
        match self.param(n) {
            Some(i) => self.args[i].null,
            None => is_null_const(self.snapshot, n),
        }
    }

    /// `n` surely is no NULL constant once simplified: a parameter given
    /// none, a non-NULL literal, a built-in cast of such.
    fn not_null_const(&self, n: &protobuf::Node) -> bool {
        if let Some(i) = self.param(n) {
            return self.args[i].not_null;
        }
        match n.node.as_ref() {
            Some(node::Node::AConst(c)) => !c.isnull,
            Some(node::Node::TypeCast(tc)) => {
                tc.arg.as_deref().is_some_and(|a| self.not_null_const(a))
                    && tc
                        .type_name
                        .as_ref()
                        .and_then(|t| crate::ddl::util::resolve_type_name(t, self.snapshot))
                        .is_some_and(|t| builtin_casts_into(self.snapshot, t))
            }
            _ => false,
        }
    }
}

/// What `qual` surely evaluates to over a row whose columns `column`
/// tells the constants of (a literal each, where known): comparisons of
/// constants (see [`truth_with`]).
pub(super) fn qual_truth<'a>(
    qual: &protobuf::Node,
    column: &dyn Fn(&protobuf::ColumnRef) -> Option<&'a protobuf::Node>,
) -> Option<bool> {
    truth_with(qual, &|n| match n.node.as_ref()? {
        node::Node::ColumnRef(cr) => literal_value(column(cr)?),
        _ => literal_value(n),
    })
}

/// A constant an equality compares a column with (see [`eq_const`]).
#[derive(PartialEq)]
enum EqConst {
    /// The canonical decimal form of a number.
    Number(String),
    Literal(Value),
}

/// The constant `n` is, compared with a column — of an integer or numeric
/// type where `numeric`: a literal, or a number (a literal one, or one
/// cast to such a type, maybe a string).
fn eq_const(n: &protobuf::Node, numeric: bool, snapshot: &PgCatalog) -> Option<EqConst> {
    // Equal numbers are one value of whatever type they are read as, a
    // string only of a numeric one.
    let string = matches!(
        n.node.as_ref(),
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Sval(_)),
            ..
        }))
    );
    if (numeric || !string)
        && let Some(number) = number_const(n, snapshot)
    {
        return Some(EqConst::Number(number));
    }
    literal_value(n).map(EqConst::Literal)
}

/// The canonical decimal form of the number `n` surely is.
fn number_const(n: &protobuf::Node, snapshot: &PgCatalog) -> Option<String> {
    match n.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            protobuf::a_const::Val::Ival(i) => Some(i.ival.to_string()),
            protobuf::a_const::Val::Fval(f) => canonical_decimal(&f.fval),
            protobuf::a_const::Val::Sval(s) => canonical_decimal(&s.sval),
            _ => None,
        },
        node::Node::TypeCast(tc) => {
            let t = crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, snapshot)?;
            if ![oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC].contains(&t)
                || tc.type_name.as_ref().is_some_and(|t| !t.typmods.is_empty())
            {
                return None;
            }
            number_const(tc.arg.as_deref()?, snapshot)
        }
        _ => None,
    }
}

/// `[+-]digits[.digits]` (surrounding spaces allowed) without its sign
/// when zero, leading zeros or trailing fractional ones.
fn canonical_decimal(s: &str) -> Option<String> {
    let s = s.trim();
    let (negative, s) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty()
        || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    let mut out = String::new();
    if negative && (int != "0" || !frac.is_empty()) {
        out.push('-');
    }
    out.push_str(int);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

/// The constant the literal `n` is.
fn literal_value(n: &protobuf::Node) -> Option<Value> {
    match n.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            protobuf::a_const::Val::Ival(i) => Some(Value::Int(i64::from(i.ival))),
            protobuf::a_const::Val::Sval(s) => Some(Value::Str(s.sval.clone())),
            protobuf::a_const::Val::Boolval(b) => Some(Value::Bool(b.boolval)),
            _ => None,
        },
        _ => None,
    }
}

/// What `n`, a condition, surely evaluates to, its operands' constants
/// told by `value`: comparisons of constants, which the planner folds
/// (and which evaluate so at run time).
fn truth_with(
    n: &protobuf::Node,
    value: &dyn Fn(&protobuf::Node) -> Option<Value>,
) -> Option<bool> {
    use protobuf::BoolExprType as B;
    match n.node.as_ref()? {
        node::Node::AConst(_) => match value(n)? {
            Value::Bool(b) => Some(b),
            _ => None,
        },
        node::Node::BoolExpr(b) => {
            let args: Vec<Option<bool>> = b.args.iter().map(|a| truth_with(a, value)).collect();
            match B::try_from(b.boolop).ok()? {
                B::AndExpr if args.contains(&Some(false)) => Some(false),
                B::AndExpr => args.iter().all(|a| *a == Some(true)).then_some(true),
                B::OrExpr if args.contains(&Some(true)) => Some(true),
                B::OrExpr => args.iter().all(|a| *a == Some(false)).then_some(false),
                B::NotExpr => match args.as_slice() {
                    [Some(a)] => Some(!a),
                    _ => None,
                },
                B::Undefined => None,
            }
        }
        node::Node::NullTest(nt) => {
            value(nt.arg.as_deref()?)?;
            Some(nt.nulltesttype == protobuf::NullTestType::IsNotNull as i32)
        }
        node::Node::AExpr(e) => {
            let name = match e.name.as_slice() {
                [n] => match n.node.as_ref()? {
                    node::Node::String(s) => s.sval.as_str(),
                    _ => return None,
                },
                _ => return None,
            };
            let l = value(e.lexpr.as_deref()?)?;
            match e.kind() {
                protobuf::AExprKind::AexprOp => compare(name, &l, &value(e.rexpr.as_deref()?)?),
                protobuf::AExprKind::AexprIn => {
                    let Some(node::Node::List(list)) = e.rexpr.as_deref()?.node.as_ref() else {
                        return None;
                    };
                    let eq: Vec<Option<bool>> = list
                        .items
                        .iter()
                        .map(|r| compare("=", &l, &value(r)?))
                        .collect();
                    let any = if eq.contains(&Some(true)) {
                        Some(true)
                    } else {
                        eq.iter().all(|e| *e == Some(false)).then_some(false)
                    };
                    // `<>` is NOT IN.
                    match name {
                        "=" => any,
                        "<>" => any.map(|a| !a),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The operator expression `l <op> r`, as the parser builds out of a
/// BETWEEN or an IN.
fn comparison(op: &str, l: &protobuf::Node, r: &protobuf::Node) -> protobuf::AExpr {
    protobuf::AExpr {
        kind: protobuf::AExprKind::AexprOp as i32,
        name: vec![protobuf::Node {
            node: Some(node::Node::String(protobuf::String { sval: op.into() })),
        }],
        lexpr: Some(Box::new(l.clone())),
        rexpr: Some(Box::new(r.clone())),
        ..Default::default()
    }
}

/// `l <op> r` of two constants, when decided: integers (a string literal
/// against one is read as one, as its unknown type is resolved), strings
/// and booleans by equality (the default collation is deterministic).
fn compare(op: &str, l: &Value, r: &Value) -> Option<bool> {
    let int = |v: &Value| match v {
        Value::Int(i) => Some(*i),
        Value::Str(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(_) => None,
    };
    let ord = match (l, r) {
        (Value::Int(_), _) | (_, Value::Int(_)) => int(l)?.cmp(&int(r)?),
        (Value::Str(a), Value::Str(b)) if matches!(op, "=" | "<>" | "!=") => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) if matches!(op, "=" | "<>" | "!=") => a.cmp(b),
        _ => return None,
    };
    use std::cmp::Ordering::*;
    Some(match op {
        "=" => ord == Equal,
        "<>" | "!=" => ord != Equal,
        "<" => ord == Less,
        "<=" => ord != Greater,
        ">" => ord == Greater,
        ">=" => ord != Less,
        _ => return None,
    })
}

/// The rows of the single FROM item a select list reads, when it surely
/// evaluates them.
struct SourceRows {
    /// The name the item is referred to by (its alias, else its name).
    name: String,
    /// Its output columns' names.
    names: Vec<String>,
    rows: Rows,
}

/// What a select-list column reference reads of a [`SourceRows`].
enum Read {
    /// `*` / `s.*`: every column, each its own output column.
    All,
    /// One column: `x`, `s.x`.
    Column(usize),
    /// The whole row `s`, whose construction evaluates every column.
    WholeRow,
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
        let column = |col: &str| self.names.iter().position(|n| n == col).map(Read::Column);
        match cr.fields.as_slice() {
            [f] if star(f) => Read::All,
            [rel, f] if star(f) && name(rel) == Some(self.name.as_str()) => Read::All,
            [col] => match name(col).and_then(column) {
                Some(c) => c,
                // A whole-row reference by the item's name.
                None if name(col) == Some(self.name.as_str()) => Read::WholeRow,
                None => Read::Other,
            },
            [rel, col] if name(rel) == Some(self.name.as_str()) => {
                name(col).and_then(column).unwrap_or(Read::Other)
            }
            _ => Read::Other,
        }
    }
}
/// The FROM item `item` as [`SourceRows`]: a view, a CTE or a subquery
/// whose query's rows are surely evaluated.
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
    // `view`: the query is this view's, stored without execution.
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
                    None,
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
                    Some(class.oid),
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
                None,
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
    let mut rows = query_rows(
        &query,
        snapshot,
        &inner_ctes,
        depth + 1,
        stored || view.is_some(),
    )?;
    // Only a subquery in FROM is surely planned as one a qualification is
    // pushed down into (a view may be a security barrier, a CTE
    // materialized).
    if !matches!(item.node, Some(node::Node::RangeSubselect(_))) {
        rows.leaves = None;
    }
    // A CTE read more than once is materialized: its columns are Vars.
    if !matches!(item.node, Some(node::Node::RangeSubselect(_))) && view.is_none() {
        rows.var_free = false;
    }
    // A view's columns have the types it was created with.
    if let Some(view) = view {
        rows.types = snapshot
            .attributes_of(view)
            .iter()
            .filter(|a| a.attnum > 0)
            .map(|a| Some(a.atttypid))
            .collect();
    }
    if names.is_empty()
        && let Some(n) = rows.names.as_ref()
    {
        names = n.clone();
    }
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
            names = (1..=rows.all.columns.len())
                .map(|i| format!("column{i}"))
                .collect();
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
    if names.len() != rows.all.columns.len() {
        return None;
    }
    Some(SourceRows { name, names, rows })
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
