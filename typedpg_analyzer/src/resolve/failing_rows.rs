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
//! planner folds it), or surely fail itself, or — over a set operation —
//! be pushed down into none of its arms, running over their projected
//! rows. A LIMIT other than 0 keeps the row, and of several rows the first
//! one, which every plan fetches, and the ORDER BY keys of every row, which
//! sorting evaluates; an OFFSET skips it after its evaluation (unless an
//! ORDER BY could postpone that past the Limit). The planner drops a
//! subquery's (or a view's, or a flattened UNION ALL leaf's) output
//! columns the query above doesn't use, so a failing column of one fails
//! only where that query reads it (`x`, `s.x`, `*`, `s.*`, `s`,
//! `x IS NULL`, `x = 1`); a set operation it doesn't flatten keeps them
//! all. A scalar or ARRAY subquery reads its column; an operator its
//! operands, where its function surely evaluates them.
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
    match rows.and_then(|r| r.all.any().or(r.projection)) {
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
    rows.all.always = rows.all.always.take().or(rows.projection.take());
    rows.first.always = rows.first.always.take().or(rows.first_projection.take());
    let rows = Rows {
        sorted: rows.first.any(),
        unpushable: vec![true; rows.all.columns.len()],
        whole_row_unpushable: true,
        keeps_columns: rows.keeps_columns || sel.op != protobuf::SetOperation::SetopNone as i32,
        ..rows
    };
    if count.is_some() && !rows.count.at_most_one {
        if offset != 0 {
            return None;
        }
        // Sorted, every row's ORDER BY keys are evaluated before the
        // first one is returned.
        let mut first = rows.first.clone();
        first.always = first.always.or_else(|| rows.ordered.clone());
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
    if offset == 0 {
        return Some(rows);
    }
    // Sorting, the planner may postpone an expensive column past the
    // Limit (make_sort_input_target): a skipped row's is not evaluated.
    if !sel.sort_clause.is_empty() {
        return None;
    }
    Some(Rows {
        count: Count {
            at_least_one: false,
            ..rows.count
        },
        ..rows
    })
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
    if where_fails.is_some() {
        if !count.at_least_one && !unpushed.as_ref().is_some_and(|(all, _)| all.is_some()) {
            return None;
        }
        count.at_least_one = false;
    }
    let unpushed = unpushed.unwrap_or((None, None));
    let (all, values) = level_errors(sel, level, where_fails, unpushed.0)?;
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
    let equated = equated_columns(sel.where_clause.as_deref());
    let ordered = sel.sort_clause.iter().find_map(|k| match k.node.as_ref() {
        Some(node::Node::SortBy(sb)) => {
            let key = sb.node.as_deref()?;
            if order_key_redundant(sel, level, key, &equated) {
                return None;
            }
            order_key_error(sel, level, &all, key)
        }
        _ => None,
    });
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
        ..Rows::new(all, first, values, count)
    })
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

/// The columns read by a conjunct of `qual` that may hold an equality
/// (see [`may_equate`]): an equivalence class may hold them, with a
/// constant, which leaves an ORDER BY key over them redundant
/// (`pathkey_is_redundant`): the Sort by it is dropped, and with it the
/// key's evaluation over every row.
fn equated_columns(qual: Option<&protobuf::Node>) -> std::collections::HashSet<&str> {
    let mut equated = std::collections::HashSet::new();
    for c in qual.map(conjuncts).unwrap_or_default() {
        let mut equality = false;
        let mut columns = Vec::new();
        super::walk::visit_same_level(c, &mut |x| match x.node.as_ref() {
            Some(node::Node::AExpr(e)) => equality |= may_equate(e),
            Some(node::Node::ColumnRef(cr)) => columns.extend(column_name(cr)),
            _ => {}
        });
        if equality {
            equated.extend(columns);
        }
    }
    equated
}

/// ORDER BY key `key` of `sel` may be one the planner drops as redundant:
/// a constant, or one whose every column (through the output column the
/// key names, by position or alias) is in `equated` (see
/// [`equated_columns`]).
fn order_key_redundant(
    sel: &protobuf::SelectStmt,
    level: Level<'_>,
    key: &protobuf::Node,
    equated: &std::collections::HashSet<&str>,
) -> bool {
    let reads_equated = |n: &protobuf::Node| {
        let mut all = true;
        super::walk::visit_same_level(n, &mut |x| {
            if let Some(node::Node::ColumnRef(cr)) = x.node.as_ref() {
                all &= column_name(cr).is_some_and(|c| equated.contains(c));
            }
        });
        all
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
        return reads_equated(key);
    }
    // As [`order_key_error`] resolves it.
    let mut column = 0;
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
            if let Some(p) = position.filter(|p| (column..column + width).contains(p)) {
                return equated.contains(source.names[p - column].as_str());
            }
            column += width;
            continue;
        }
        if position == Some(column) || alias.is_some_and(|a| a == rt.name) {
            return val.is_none_or(reads_equated);
        }
        column += 1;
    }
    position.is_some() || reads_equated(key)
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
) -> Option<(Errors, Vec<Option<Value>>)> {
    let source = level.source;
    let mut always = source.and_then(|s| s.rows.errors(level.first).always.clone());
    if let Some(w) = where_fails {
        always = match unpushed {
            Some(p) => Some(p),
            None => always.or(Some(level.surely_fails(w)?)),
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
    Some(Rows {
        sorted,
        projected: true,
        whole_row_unpushable: !flattened,
        keeps_columns: !flattened,
        unpushable,
        leaf_types,
        // A Sort reads every row, of each arm a MergeAppend's input.
        ordered: (!sel.sort_clause.is_empty()).then_some(all).flatten(),
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
                    return None;
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
                let op = match e.kind() {
                    K::AexprBetween | K::AexprBetweenSym => ">=",
                    _ => "<",
                };
                self.operator_error(&comparison(op, e.lexpr.as_deref()?, bounds.items.first()?))
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
        let types: Vec<Option<PgTypeOid>> = operands.iter().map(|a| self.type_of(a)).collect();
        let resolved = self.operator(e).map(|op| op.code);
        let candidates: Vec<usize> = if ordered {
            (0..operands.len())
                .filter(|&i| errors[i].is_some())
                .collect()
        } else {
            vec![leftmost]
        };
        let failing = candidates.into_iter().find(|&failing| {
            // Every other operand surely is no NULL constant: a strict
            // function over one is folded to NULL (evaluate_function), and
            // an inlined body gets it substituted.
            let others = operands
                .iter()
                .enumerate()
                .all(|(i, a)| i == failing || self.not_null_const(a));
            operator_evaluates(
                self.snapshot,
                e,
                resolved,
                failing,
                others,
                ordered,
                &types,
                0,
            )
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
            node::Node::AConst(c) => match c.val.as_ref()? {
                protobuf::a_const::Val::Ival(i) => Some(Value::Int(i64::from(i.ival))),
                protobuf::a_const::Val::Sval(s) => Some(Value::Str(s.sval.clone())),
                protobuf::a_const::Val::Boolval(b) => Some(Value::Bool(b.boolval)),
                _ => None,
            },
            node::Node::ColumnRef(cr) => {
                let source = self.source?;
                match source.read(cr) {
                    Read::Column(i) => source.rows.values.get(i).cloned().flatten(),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// What `n`, a qualification, surely evaluates to: comparisons of
    /// constants, which the planner folds.
    fn truth(&self, n: &protobuf::Node) -> Option<bool> {
        use protobuf::BoolExprType as B;
        match n.node.as_ref()? {
            node::Node::AConst(_) => match self.value(n)? {
                Value::Bool(b) => Some(b),
                _ => None,
            },
            node::Node::BoolExpr(b) => {
                let args: Vec<Option<bool>> = b.args.iter().map(|a| self.truth(a)).collect();
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
                self.value(nt.arg.as_deref()?)?;
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
                let l = self.value(e.lexpr.as_deref()?)?;
                match e.kind() {
                    protobuf::AExprKind::AexprOp => {
                        compare(name, &l, &self.value(e.rexpr.as_deref()?)?)
                    }
                    protobuf::AExprKind::AexprIn => {
                        let Some(node::Node::List(list)) = e.rexpr.as_deref()?.node.as_ref() else {
                            return None;
                        };
                        let eq: Vec<Option<bool>> = list
                            .items
                            .iter()
                            .map(|r| compare("=", &l, &self.value(r)?))
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
                for a in &b.args {
                    if self.truth(a) == Some(true) {
                        continue;
                    }
                    failing = Some(self.surely_fails(a)?);
                }
                failing
            }
            node::Node::BoolExpr(_) => None,
            _ => level.error(n),
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
        // Two equalities may contradict each other through their
        // equivalence class (`b = 3 AND b = 4`, or `b = c AND b = 3 AND c
        // = 4`): the planner derives a constant false from it, leaving the
        // scan dummy (generate_base_implied_equalities_const).
        let equalities = conjuncts
            .iter()
            .filter(|c| matches!(c.node.as_ref(), Some(node::Node::AExpr(e)) if may_equate(e)))
            .count();
        if equalities > 1 {
            return false;
        }
        conjuncts.into_iter().all(|c| {
            let mut simple = true;
            let mut reads = false;
            super::walk::visit_same_level(c, &mut |x| match x.node.as_ref() {
                Some(node::Node::ColumnRef(cr)) => {
                    reads |= source.rows.unpushable(&source.read(cr));
                }
                Some(node::Node::AConst(a)) => simple &= !a.isnull,
                Some(node::Node::AExpr(e)) => {
                    simple &= e.kind() == protobuf::AExprKind::AexprOp;
                }
                Some(node::Node::TypeCast(_) | node::Node::NullTest(_)) => {}
                Some(node::Node::TypeName(_) | node::Node::String(_)) => {}
                _ => simple = false,
            });
            simple && reads
        })
    }
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

/// The operator `e` evaluates its operand `operand` (counted from its left
/// one): its function — `code`, where its operands' types resolved it;
/// else every operator of its name and arity must — evaluates that
/// argument (see [`call_evaluates`]). `others` tells that every other
/// operand surely is no NULL constant, which a strict function (or an
/// inlined body) needs; `ordered`, that it doesn't matter in which order
/// the operands are evaluated (an inlined body may reorder them).
#[allow(clippy::too_many_arguments)]
fn operator_evaluates(
    snapshot: &PgCatalog,
    e: &protobuf::AExpr,
    code: Option<Option<crate::oid::PgProcOid>>,
    operand: usize,
    others: bool,
    ordered: bool,
    types: &[Option<PgTypeOid>],
    depth: usize,
) -> bool {
    let evaluates = |code: Option<crate::oid::PgProcOid>| {
        let Some(p) = code.and_then(|f| snapshot.pg_proc.get(&f)) else {
            return false;
        };
        let sql = p.prolang == crate::pg_catalog::SQL_LANGUAGE;
        if (p.proisstrict || sql) && !others || sql && !ordered {
            return false;
        }
        call_evaluates(snapshot, p, operand, types, depth)
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

/// A call of `p` evaluates its argument `arg`, none of the others being a
/// NULL constant: ExecInterpExpr evaluates a call's arguments before it,
/// and the planner neither folds a domain's coercion
/// (eval_const_expressions) nor drops an argument — unless a support
/// function simplifies the call, or `p` is a SQL function it inlines
/// (`inline_function`), whose body is then evaluated instead: that body
/// must evaluate the parameter. `types` are the arguments' types, where
/// known (the body's polymorphic parameters have them).
fn call_evaluates(
    snapshot: &PgCatalog,
    p: &crate::pg_catalog::PgProc,
    arg: usize,
    types: &[Option<PgTypeOid>],
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
            types,
            depth,
        }
        .evaluates(body, arg)
}

/// The expression of an inlinable SQL function's body, its parameters
/// substituted by the call's arguments.
struct Body<'a> {
    snapshot: &'a PgCatalog,
    proc: &'a crate::pg_catalog::PgProc,
    types: &'a [Option<PgTypeOid>],
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
        .filter(|&i| i < self.proc.proargtypes.len())
    }

    /// The type `n` surely has, where it is simple enough to tell: a
    /// parameter's (a polymorphic one's is its argument's), a literal's,
    /// a cast's, an operator's result.
    fn type_of(&self, n: &protobuf::Node) -> Option<PgTypeOid> {
        if let Some(i) = self.param(n) {
            let declared = self.proc.proargtypes[i];
            return if crate::polymorphic::is_polymorphic(declared) {
                self.types.get(i).copied().flatten()
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
                let others = operands
                    .iter()
                    .enumerate()
                    .all(|(i, a)| i == operand || self.not_null_const(a));
                let types: Vec<Option<PgTypeOid>> =
                    operands.iter().map(|a| self.type_of(a)).collect();
                operator_evaluates(
                    self.snapshot,
                    e,
                    self.operator(e).map(|op| op.code),
                    operand,
                    others,
                    true,
                    &types,
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
            // COALESCE keeps its first argument, no constant, first.
            Some(node::Node::CoalesceExpr(c)) => {
                c.args.first().is_some_and(|a| self.evaluates(a, k))
            }
            // An AND or OR is folded to a constant when one of its
            // arguments is the one deciding it (`$2 > 0` over a constant
            // `$2`), and stops at the first one deciding it: each must
            // evaluate the parameter (no constant then).
            Some(node::Node::BoolExpr(b)) => {
                !b.args.is_empty() && b.args.iter().all(|a| self.evaluates(a, k))
            }
            _ => false,
        }
    }

    /// `n` surely is no NULL constant once simplified: a parameter (the
    /// call's arguments are none), a non-NULL literal, a built-in cast of
    /// such.
    fn not_null_const(&self, n: &protobuf::Node) -> bool {
        if self.param(n).is_some() {
            return true;
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
    // A view's columns have the types it was created with.
    if let Some(view) = view {
        rows.types = snapshot
            .attributes_of(view)
            .iter()
            .filter(|a| a.attnum > 0)
            .map(|a| Some(a.atttypid))
            .collect();
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
