//! A NULL cast to a domain that rejects it (`SELECT NULL::d` of a NOT NULL
//! domain, `CAST(NULL AS d)` of one with `CHECK (VALUE IS NOT NULL)`) that
//! every execution of a statement evaluates: the coercion fails when
//! evaluated (`ExecEvalConstraintNotNull` / `ExecEvalConstraintCheck`),
//! naming the domain cast to, so the statement is rejected.
//!
//! A query is followed only where it surely evaluates a row: a select list
//! with no grouping over no FROM item (one row) or over a single view,
//! subquery or CTE that surely yields one; a VALUES list; a UNION [ALL] of
//! such, or an EXCEPT or INTERSECT that surely reads one. A WHERE must be
//! provably true over that row (`y = 1` of a view's `1 AS y`: the planner
//! folds it), or surely fail itself. A LIMIT other than 0 keeps the row,
//! and of several rows the first one, which every plan fetches; an OFFSET
//! skips it after its evaluation (unless an ORDER BY could postpone that
//! past the Limit). The planner drops a subquery's (or a view's) output
//! columns the query above doesn't use, so a failing column of one fails
//! only where that query reads it (`x`, `s.x`, `*`, `s.*`, `s`,
//! `x IS NULL`, `x = 1`); a scalar or ARRAY subquery reads its column.
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
    match rows.and_then(|r| r.all.any()) {
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
#[derive(Clone)]
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
    // are evaluated is unknown.
    let rows = Rows {
        sorted: rows.first.any(),
        ..rows
    };
    if count.is_some() && !rows.count.at_most_one {
        if offset != 0 {
            return None;
        }
        return Some(Rows {
            all: rows.first.clone(),
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
    if !sel.group_clause.is_empty() || sel.having_clause.is_some() {
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
    // A WHERE not surely true must surely fail: only a row reaching it
    // evaluates it.
    let where_fails = match sel.where_clause.as_deref() {
        Some(w) if level.truth(w) != Some(true) => {
            if !count.at_least_one {
                return None;
            }
            count.at_least_one = false;
            Some(w)
        }
        _ => None,
    };
    let (all, values) = level_errors(sel, level, where_fails)?;
    // Over the source's first row, a WHERE that may not fail may let it
    // through or not: what the first fetch raises is unknown.
    let first = level_errors(
        sel,
        Level {
            first: true,
            ..level
        },
        where_fails,
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
    Some(Rows {
        projected: source.as_ref().is_some_and(|s| s.rows.projected),
        types: level_types(sel, level),
        ..Rows::new(all, first, values, count)
    })
}

/// What evaluating `sel`'s select list over `level`'s source rows (all of
/// them, or the first) surely raises, and the constants it yields;
/// `where_fails` is a WHERE that must fail, which every row reaching the
/// select list would have passed.
fn level_errors(
    sel: &protobuf::SelectStmt,
    level: Level<'_>,
    where_fails: Option<&protobuf::Node>,
) -> Option<(Errors, Vec<Option<Value>>)> {
    let source = level.source;
    let mut always = source.and_then(|s| s.rows.errors(level.first).always.clone());
    if let Some(w) = where_fails {
        always = always.or(Some(level.surely_fails(w)?));
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
            let leaf = |s: &protobuf::SelectStmt| {
                s.op == Op::SetopNone as i32
                    && s.group_clause.is_empty()
                    && s.having_clause.is_none()
                    && s.distinct_clause.is_empty()
            };
            // The planner puts first the input it estimates fewer groups
            // of, the left one on a tie (generate_nonunion_paths): an
            // estimate is at least one group (estimate_num_groups), so a
            // leaf it estimates one row of — every one yielding at most one
            // row — is first against one estimated so too (the other's
            // rows estimate may be less for a set operation, or a grouped
            // leaf).
            let left_outer = left.as_ref().is_some_and(|l| l.count.at_most_one)
                && sel
                    .larg
                    .as_deref()
                    .is_some_and(|l| l.op == Op::SetopNone as i32)
                && sel.rarg.as_deref().is_some_and(leaf);
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
    Some(Rows {
        sorted,
        projected: true,
        ..Rows::unknown_values(
            Errors::uniform(all, width),
            Errors::uniform(first, width),
            count,
        )
    })
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
            }
            node::Node::ColumnRef(cr) => {
                let source = self.source?;
                if self.qual && source.rows.projected {
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
            node::Node::AExpr(e) if e.kind() == protobuf::AExprKind::AexprOp => {
                if self.evaluates_operands(e)
                    && let Some(f) = e.lexpr.iter().chain(&e.rexpr).find_map(|a| self.error(a))
                {
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

    /// The operator `e` runs a function that evaluates each of its
    /// operands: ExecInterpExpr evaluates a call's arguments before it,
    /// and the planner neither folds a domain's coercion
    /// (eval_const_expressions) nor drops an argument. It may where the
    /// function is a SQL one it inlines (whose body may ignore an
    /// argument), or one whose support function simplifies the call
    /// (`range_contains_elem` is false for an empty range): those, and the
    /// functions a user could give a support function, are left alone.
    ///
    /// The operator is the one its operands' types resolve to; where those
    /// aren't known, every operator of its name must qualify.
    fn evaluates_operands(&self, e: &protobuf::AExpr) -> bool {
        let snapshot = self.snapshot;
        let evaluates = |code: Option<crate::oid::PgProcOid>| {
            const SIMPLIFIED: [&str; 2] = ["range_contains_elem", "elem_contained_by_range"];
            code.and_then(|f| snapshot.pg_proc.get(&f))
                .is_some_and(|p| {
                    p.prolang != crate::pg_catalog::SQL_LANGUAGE
                        && snapshot.namespace_name(p.pronamespace) == Some("pg_catalog")
                        && !SIMPLIFIED.contains(&p.proname.as_str())
                })
        };
        if let Some(op) = self.operator(e) {
            return evaluates(op.code);
        }
        let Some(node::Node::String(name)) = e.name.last().and_then(|n| n.node.as_ref()) else {
            return false;
        };
        snapshot
            .pg_operator
            .values()
            .filter(|o| o.oprname == name.sval)
            .all(|o| evaluates(o.oprcode))
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
        rows.all.always.clone().or_else(|| rows.all.column(0))
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
