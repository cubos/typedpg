//! What the planner's constant folding (`eval_const_expressions`) and its
//! early exits make of a statement, as far as the plan-time checks
//! ([`super::with_plan_time_checks`]) need it.
//!
//! The planner compiles a constant regex pattern while folding a match of
//! two constants, and while estimating the selectivity of a restriction
//! `col ~ 'pattern'` — but only for what it reaches:
//!
//! - folding stops at an AND argument that folds to FALSE, an OR argument
//!   that folds to TRUE (`simplify_and_arguments` / `simplify_or_arguments`),
//!   skips a CASE branch whose WHEN folds to FALSE or NULL and everything
//!   after one that folds to TRUE, and the COALESCE arguments after a
//!   non-NULL constant: what follows is never folded, nor are its sublinks
//!   planned;
//! - a relation is estimated only when it isn't proven empty first
//!   (`relation_excluded_by_constraints`: a constant FALSE / NULL
//!   restriction, an equivalence class equating two different constants,
//!   `col IS NULL` over a NOT NULL column; partition pruning; an inheritance
//!   child's constraints), nor removed (`remove_useless_joins`).
//!
//! Each check here errs towards *not* raising: an expression whose value
//! the analyzer can't compute counts as one that may fold to a constant,
//! and a relation as one that may be proven empty. The errors left are the
//! ones every execution raises.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use typedpg_pg_query::protobuf::{self, a_const, node};

use crate::pg_catalog::{PgCatalog, ProVolatile, RelKind};

/// What an expression folds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Folded {
    /// A boolean constant.
    Bool(bool),
    /// The NULL constant.
    Null,
    /// Possibly a constant (of a value the analyzer doesn't compute).
    Const,
    /// Not a constant: it reads a column, a parameter, a sublink, or calls
    /// something the planner doesn't fold.
    Var,
}

impl Folded {
    /// Whether a planner may have replaced the expression with a constant.
    pub(crate) fn may_be_const(self) -> bool {
        self != Folded::Var
    }

    /// Whether the expression certainly folds to a known constant (a
    /// boolean or NULL), which decides where folding goes next.
    pub(crate) fn known(self) -> bool {
        matches!(self, Folded::Bool(_) | Folded::Null)
    }
}

/// `parse_bool_with_len`: the strings `boolin` accepts.
fn parse_bool(s: &str) -> Option<bool> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let prefix_of = |word: &str, min: usize| s.len() >= min && word.starts_with(s.as_str());
    if prefix_of("true", 1) || prefix_of("yes", 1) || s == "1" || s == "on" {
        Some(true)
    } else if prefix_of("false", 1) || prefix_of("no", 1) || s == "0" || prefix_of("off", 2) {
        Some(false)
    } else {
        None
    }
}

fn extract_names(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// Whether every function `fc` may resolve to is one the planner doesn't
/// fold (stable or volatile).
pub(crate) fn never_folded(fc: &protobuf::FuncCall, snapshot: &PgCatalog) -> bool {
    let names = extract_names(&fc.funcname);
    let (schema, name) = match names.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return false,
    };
    let candidates = snapshot.find_functions(schema, name);
    !candidates.is_empty()
        && candidates
            .iter()
            .all(|p| p.provolatile != ProVolatile::Immutable)
}

/// Whether `n` may call a volatile function (`contain_volatile_functions`):
/// a function or operator some candidate of which is volatile, or one the
/// analyzer can't find.
pub(crate) fn may_be_volatile(n: &protobuf::Node, snapshot: &PgCatalog) -> bool {
    let Some(inner) = n.node.as_ref() else {
        return false;
    };
    let volatile = |p: &crate::pg_catalog::PgProc| p.provolatile == ProVolatile::Volatile;
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        match n {
            typedpg_pg_query::NodeRef::FuncCall(fc) => {
                let names = extract_names(&fc.funcname);
                let (schema, name) = match names.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => return true,
                };
                let candidates = snapshot.find_functions(schema, name);
                if candidates.is_empty() || candidates.iter().any(|p| volatile(p)) {
                    return true;
                }
            }
            typedpg_pg_query::NodeRef::AExpr(e) => {
                let names = extract_names(&e.name);
                if let Some(name) = names.last()
                    && snapshot.pg_operator.values().any(|o| {
                        &o.oprname == name
                            && o.oprcode
                                .and_then(|c| snapshot.pg_proc.get(&c))
                                .is_none_or(volatile)
                    })
                {
                    return true;
                }
            }
            // A cast's function may be a volatile one.
            typedpg_pg_query::NodeRef::TypeCast(_)
            | typedpg_pg_query::NodeRef::NextValueExpr(_)
                if snapshot.pg_cast.values().any(|c| {
                    c.castfunc
                        .and_then(|f| snapshot.pg_proc.get(&f))
                        .is_some_and(volatile)
                }) =>
            {
                return true;
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    false
}

/// The value of a whole-number literal.
fn int_literal(n: &protobuf::Node) -> Option<i64> {
    match n.node.as_ref()? {
        node::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            a_const::Val::Ival(i) => Some(i64::from(i.ival)),
            _ => None,
        },
        _ => None,
    }
}

/// What `n` folds to.
pub(crate) fn fold(n: &protobuf::Node, snapshot: &PgCatalog) -> Folded {
    fold_with(n, snapshot, &|_| Folded::Var)
}

/// What `n` folds to, in the scope of `ctx`: a column the planner
/// replaces by a pulled-up subquery's output is what that folds to (see
/// [`crate::scope::Substitution`]).
pub(crate) fn fold_in(n: &protobuf::Node, ctx: super::Ctx<'_>) -> Folded {
    fold_with(n, ctx.snapshot, &|cr| {
        let parts = extract_names(&cr.fields);
        let (table, column) = match parts.as_slice() {
            [c] if parts.len() == cr.fields.len() => (None, c.as_str()),
            [t, c] if parts.len() == cr.fields.len() => (Some(t.as_str()), c.as_str()),
            _ => return Folded::Const,
        };
        let Ok(col) = ctx.scope.resolve_column(table, column, None) else {
            return Folded::Const;
        };
        match col.deferred.substitution {
            crate::scope::Substitution::Never if !col.planned_unknown => Folded::Var,
            crate::scope::Substitution::Certain => match col.deferred.expr.as_deref() {
                Some(e) => fold(e, ctx.snapshot),
                // A table's column passed through stays a Var (unless
                // virtual generated: its expression replaces it).
                None if col.origin.as_ref().is_some_and(|o| {
                    !ctx.snapshot.attributes_of(o.relid).iter().any(|a| {
                        a.attname == o.column
                            && a.attgenerated == Some(crate::pg_catalog::AttGenerated::Virtual)
                    })
                }) =>
                {
                    Folded::Var
                }
                None => Folded::Const,
            },
            _ => Folded::Const,
        }
    })
}

fn fold_with(
    n: &protobuf::Node,
    snapshot: &PgCatalog,
    col: &dyn Fn(&protobuf::ColumnRef) -> Folded,
) -> Folded {
    let Some(inner) = n.node.as_ref() else {
        return Folded::Const;
    };
    let all = |args: &[protobuf::Node]| -> Folded {
        if args
            .iter()
            .any(|a| fold_with(a, snapshot, col) == Folded::Var)
        {
            Folded::Var
        } else {
            Folded::Const
        }
    };
    match inner {
        node::Node::AConst(c) => {
            if c.isnull {
                return Folded::Null;
            }
            match c.val.as_ref() {
                Some(a_const::Val::Boolval(b)) => Folded::Bool(b.boolval),
                Some(a_const::Val::Sval(s)) => {
                    parse_bool(&s.sval).map_or(Folded::Const, Folded::Bool)
                }
                _ => Folded::Const,
            }
        }
        node::Node::TypeCast(c) => match c.arg.as_deref().map(|a| fold_with(a, snapshot, col)) {
            Some(Folded::Var) => Folded::Var,
            Some(Folded::Null) => Folded::Null,
            Some(Folded::Bool(b))
                if c.type_name.as_ref().is_some_and(|t| {
                    t.array_bounds.is_empty()
                        && matches!(
                            extract_names(&t.names).as_slice(),
                            [n] | [_, n] if n == "bool" || n == "boolean"
                        )
                }) =>
            {
                Folded::Bool(b)
            }
            _ => Folded::Const,
        },
        node::Node::CollateClause(c) => c
            .arg
            .as_deref()
            .map_or(Folded::Const, |a| fold_with(a, snapshot, col)),
        node::Node::ColumnRef(cr) => col(cr),
        // A custom plan of an EXECUTE has its arguments as constants.
        node::Node::ParamRef(_) => {
            if custom_plan() {
                Folded::Const
            } else {
                Folded::Var
            }
        }
        node::Node::SubLink(_)
        | node::Node::SqlvalueFunction(_)
        | node::Node::GroupingFunc(_)
        | node::Node::CurrentOfExpr(_)
        | node::Node::SetToDefault(_) => Folded::Var,
        node::Node::BoolExpr(b) => match protobuf::BoolExprType::try_from(b.boolop) {
            Ok(protobuf::BoolExprType::NotExpr) => {
                match b.args.first().map(|a| fold_with(a, snapshot, col)) {
                    Some(Folded::Bool(v)) => Folded::Bool(!v),
                    Some(other) => other,
                    None => Folded::Const,
                }
            }
            Ok(op) => {
                let or = op == protobuf::BoolExprType::OrExpr;
                let (mut var, mut unknown, mut null) = (false, false, false);
                for a in &b.args {
                    match fold_with(a, snapshot, col) {
                        Folded::Bool(v) if v == or => return Folded::Bool(or),
                        Folded::Bool(_) => {}
                        Folded::Null => null = true,
                        Folded::Const => unknown = true,
                        Folded::Var => var = true,
                    }
                }
                if unknown {
                    Folded::Const
                } else if var {
                    Folded::Var
                } else if null {
                    Folded::Null
                } else {
                    Folded::Bool(!or)
                }
            }
            Err(_) => Folded::Const,
        },
        node::Node::AExpr(e) => {
            let mut operands: Vec<&protobuf::Node> = Vec::new();
            for side in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                match side.node.as_ref() {
                    Some(node::Node::List(l)) => operands.extend(l.items.iter()),
                    _ => operands.push(side),
                }
            }
            let folded: Vec<Folded> = operands
                .iter()
                .map(|o| fold_with(o, snapshot, col))
                .collect();
            let kind = protobuf::AExprKind::try_from(e.kind);
            if matches!(
                kind,
                Ok(protobuf::AExprKind::AexprDistinct | protobuf::AExprKind::AexprNotDistinct)
            ) {
                return if folded.contains(&Folded::Var) {
                    Folded::Var
                } else {
                    Folded::Const
                };
            }
            // A (strict) operator over a NULL constant folds to NULL.
            if folded.contains(&Folded::Null) {
                return if kind == Ok(protobuf::AExprKind::AexprOp) {
                    Folded::Null
                } else {
                    Folded::Const
                };
            }
            if folded.contains(&Folded::Var) {
                return Folded::Var;
            }
            if kind == Ok(protobuf::AExprKind::AexprOp)
                && let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref())
                && let (Some(l), Some(r)) = (int_literal(l), int_literal(r))
            {
                let v = match extract_names(&e.name).as_slice() {
                    [o] if o == "=" => Some(l == r),
                    [o] if o == "<>" || o == "!=" => Some(l != r),
                    [o] if o == "<" => Some(l < r),
                    [o] if o == ">" => Some(l > r),
                    [o] if o == "<=" => Some(l <= r),
                    [o] if o == ">=" => Some(l >= r),
                    _ => None,
                };
                if let Some(v) = v {
                    return Folded::Bool(v);
                }
            }
            Folded::Const
        }
        node::Node::NullTest(t) => {
            let is_null = protobuf::NullTestType::try_from(t.nulltesttype)
                == Ok(protobuf::NullTestType::IsNull);
            match t.arg.as_deref().map(|a| fold_with(a, snapshot, col)) {
                Some(Folded::Var) => Folded::Var,
                Some(Folded::Null) => Folded::Bool(is_null),
                Some(Folded::Bool(_)) => Folded::Bool(!is_null),
                _ => Folded::Const,
            }
        }
        node::Node::BooleanTest(t) => match t.arg.as_deref().map(|a| fold_with(a, snapshot, col)) {
            Some(Folded::Var) => Folded::Var,
            _ => Folded::Const,
        },
        node::Node::CoalesceExpr(c) => {
            let mut var = false;
            for a in &c.args {
                match fold_with(a, snapshot, col) {
                    Folded::Null => {}
                    Folded::Var => var = true,
                    v => return if var { Folded::Var } else { v },
                }
            }
            if var { Folded::Var } else { Folded::Null }
        }
        node::Node::CaseExpr(c) => {
            let test = c.arg.as_deref().map(|a| fold_with(a, snapshot, col));
            let mut var = false;
            for w in &c.args {
                let Some(node::Node::CaseWhen(w)) = w.node.as_ref() else {
                    return Folded::Const;
                };
                match case_when_with(test, w.expr.as_deref(), snapshot, col) {
                    Folded::Bool(false) | Folded::Null => {}
                    Folded::Bool(true) => {
                        return if var {
                            Folded::Var
                        } else {
                            w.result
                                .as_deref()
                                .map_or(Folded::Const, |r| fold_with(r, snapshot, col))
                        };
                    }
                    Folded::Const => return Folded::Const,
                    Folded::Var => var = true,
                }
            }
            if var {
                Folded::Var
            } else {
                c.defresult
                    .as_deref()
                    .map_or(Folded::Null, |d| fold_with(d, snapshot, col))
            }
        }
        node::Node::FuncCall(fc) => {
            if fc.over.is_some()
                || fc.agg_filter.is_some()
                || fc.agg_star
                || !fc.agg_order.is_empty()
            {
                return Folded::Var;
            }
            if all(&fc.args) == Folded::Var || never_folded(fc, snapshot) {
                Folded::Var
            } else {
                Folded::Const
            }
        }
        node::Node::NamedArgExpr(a) => a
            .arg
            .as_deref()
            .map_or(Folded::Const, |a| fold_with(a, snapshot, col)),
        node::Node::MinMaxExpr(m) => all(&m.args),
        node::Node::RowExpr(r) => all(&r.args),
        node::Node::AArrayExpr(a) => all(&a.elements),
        node::Node::AIndirection(i) => i.arg.as_deref().map_or(Folded::Const, |a| {
            if fold_with(a, snapshot, col) == Folded::Var {
                Folded::Var
            } else {
                Folded::Const
            }
        }),
        _ => Folded::Const,
    }
}

/// What a CASE's WHEN folds to: its condition, or `test = value` for a
/// CASE with a test expression folding to `test`.
pub(crate) fn case_when(
    test: Option<Folded>,
    cond: Option<&protobuf::Node>,
    snapshot: &PgCatalog,
) -> Folded {
    case_when_with(test, cond, snapshot, &|_| Folded::Var)
}

fn case_when_with(
    test: Option<Folded>,
    cond: Option<&protobuf::Node>,
    snapshot: &PgCatalog,
    col: &dyn Fn(&protobuf::ColumnRef) -> Folded,
) -> Folded {
    let cond = cond.map_or(Folded::Const, |e| fold_with(e, snapshot, col));
    match test {
        Some(t) => {
            if t == Folded::Null || cond == Folded::Null {
                Folded::Null
            } else if t == Folded::Var || cond == Folded::Var {
                Folded::Var
            } else {
                Folded::Const
            }
        }
        None => cond,
    }
}

/// Whether the result of a CASE branch whose WHEN folds to `when` may be
/// folded: not when the WHEN is a FALSE or NULL constant (the branch is
/// dropped) — nor, conservatively, a constant of unknown value.
pub(crate) fn case_result_reached(when: Folded) -> bool {
    matches!(when, Folded::Bool(true) | Folded::Var)
}

/// Whether folding a CASE stops at a branch whose WHEN folds to `when`
/// (TRUE: that result is the CASE's value) — conservatively, at one of
/// unknown value too.
pub(crate) fn case_stops_after(when: Folded) -> bool {
    matches!(when, Folded::Bool(true) | Folded::Const)
}

/// Whether, folding the arguments of a short-circuiting node in order, the
/// planner may stop before the one after an argument folding to `folded`:
/// for AND (`stop_on == false`) a FALSE one, for OR a TRUE one, and a
/// constant of unknown value for either.
pub(crate) fn stops_after(folded: Folded, stop_on: bool) -> bool {
    matches!(folded, Folded::Const) || folded == Folded::Bool(stop_on)
}

/// Whether, folding COALESCE's arguments in order, the planner stops after
/// one folding to `folded`: a non-NULL constant (or one of unknown value).
pub(crate) fn coalesce_stops_after(folded: Folded) -> bool {
    matches!(folded, Folded::Const | Folded::Bool(_))
}

/// Run `f` for a part of the expression the planner never folds (when
/// `dead`) — no plan-time check applies there.
pub(crate) fn unless_dead<R>(dead: bool, f: impl FnOnce() -> R) -> R {
    if dead {
        super::without_plan_time_checks(f)
    } else {
        f()
    }
}

// ─── EXECUTE's custom plans ─────────────────────────────────────────────────

thread_local! {
    /// While planning an EXECUTE with a custom plan: each parameter's value
    /// when it is a known string (`None` when the planner folds it to a
    /// constant the analyzer doesn't compute).
    static CUSTOM_PLAN: RefCell<Option<HashMap<i32, String>>> = const { RefCell::new(None) };
}

/// Run `f` planning a custom plan of a prepared statement whose
/// parameters `$n` hold the string constants `values` (others constants
/// too, of unknown value).
pub(crate) fn with_custom_plan<R>(values: HashMap<i32, String>, f: impl FnOnce() -> R) -> R {
    let prev = CUSTOM_PLAN.with(|c| c.replace(Some(values)));
    let out = f();
    CUSTOM_PLAN.with(|c| *c.borrow_mut() = prev);
    out
}

fn custom_plan() -> bool {
    CUSTOM_PLAN.with(|c| c.borrow().is_some())
}

/// The string constant a custom plan has for parameter `$number`.
pub(crate) fn bound_param(number: i32) -> Option<String> {
    CUSTOM_PLAN.with(|c| c.borrow().as_ref()?.get(&number).cloned())
}

// ─── Query levels ───────────────────────────────────────────────────────────

/// What a query level's quals tell of the relations the planner estimates.
#[derive(Debug, Clone, Copy, Default)]
struct LevelQuals {
    /// The planner may prove a relation of the level empty (or remove it)
    /// before estimating it: no restriction is estimated, and the
    /// subqueries in FROM planned on their own may never be planned.
    may_be_empty: bool,
    /// The plan of the level may be a dummy one — a relation, or a join
    /// with a constant FALSE qual, proven empty — whose nodes the executor
    /// never starts.
    may_be_dummy: bool,
    /// How many quals (top-level AND arguments of WHERE and of the JOIN ON
    /// clauses) the level has.
    conjuncts: usize,
    /// How many base relations, at least, the level has once the planner
    /// pulled its subqueries up (see [`Group`]).
    rels: usize,
}

/// How the next query level analyzed is planned (see [`with_next_level`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum NextLevel {
    /// A level of its own (a sublink, a CTE's query, a statement): it may
    /// be proven empty with the level around it, to be sure.
    #[default]
    Own,
    /// A FROM subquery the planner pulls up into the level around it,
    /// never null-extended: its quals join that level's, its relations
    /// that level's join tree.
    PulledUp,
    /// A FROM subquery the planner pulls up, on the nullable side of an
    /// outer join: its quals are evaluated at its own level.
    PulledUpNullable,
    /// A FROM subquery (or `security_barrier` view) the planner plans on
    /// its own, once it plans the relation it is: whether the level around
    /// it may push its quals down into it (see [`PUSHABLE`]).
    Separate { pushed_down: bool },
    /// A scalar or ARRAY sublink: a SubPlan (or InitPlan), whose plan the
    /// executor starts even when the plan around it is a dummy one.
    Subplan,
}

thread_local! {
    static LEVELS: RefCell<Vec<LevelQuals>> = const { RefCell::new(Vec::new()) };
    /// Each level's quals over columns that may prove a relation empty
    /// (see [`may_be_empty`]): the planner may push them down into a
    /// subquery it plans on its own (`set_subquery_pathlist`).
    static PUSHABLE: RefCell<Vec<Vec<protobuf::Node>>> = const { RefCell::new(Vec::new()) };
    static NEXT_LEVEL: Cell<NextLevel> = const { Cell::new(NextLevel::Own) };
    /// Aliases of FROM items whose restrictions are not estimated (see
    /// [`skip_restrictions_on`]).
    static SKIPPED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static SKIP_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Run `f`, the analysis of a query level planned as `next`.
pub(crate) fn with_next_level<R>(next: NextLevel, f: impl FnOnce() -> R) -> R {
    let prev = NEXT_LEVEL.with(|n| n.replace(next));
    let out = f();
    NEXT_LEVEL.with(|n| n.set(prev));
    out
}

/// Whether the next query level analyzed is a SubPlan's (see
/// [`NextLevel::Subplan`]).
pub(crate) fn next_level_is_subplan() -> bool {
    NEXT_LEVEL.with(|n| n.get() == NextLevel::Subplan)
}

/// RAII marker for a query level the planner plans (a SELECT, UPDATE,
/// DELETE), with the quals it restricts its relations by.
pub(crate) struct PlanLevel;

impl PlanLevel {
    /// Enter a level whose quals are `where_clause` and the ON clauses of
    /// the joins in `from`. A level inside one that may be proven empty —
    /// a FROM subquery pulled up into it, a sublink — may be proven empty
    /// too: its relations are not estimated. (Its constants are still
    /// folded where the planner plans them — a pulled-up subquery's output
    /// where the outer query reads it, see `resolve::deferred` — as
    /// constant folding precedes relation exclusion.)
    pub(crate) fn enter(
        where_clause: Option<&protobuf::Node>,
        from: &[protobuf::Node],
        snapshot: &PgCatalog,
    ) -> Self {
        Self::enter_level(where_clause, from, None, &[], snapshot)
    }

    /// [`Self::enter`] for SELECT `sel`, whose FROM may read the CTEs
    /// `ctes` (and its own WITH's): a constant HAVING qual is moved into
    /// WHERE, so it too may make the plan a dummy one.
    pub(crate) fn enter_select<'a>(
        sel: &protobuf::SelectStmt,
        ctes: impl IntoIterator<Item = &'a String>,
        snapshot: &PgCatalog,
    ) -> Self {
        let mut names: Vec<&str> = ctes.into_iter().map(String::as_str).collect();
        if let Some(with) = &sel.with_clause {
            for c in &with.ctes {
                if let Some(node::Node::CommonTableExpr(c)) = c.node.as_ref() {
                    names.push(c.ctename.as_str());
                }
            }
        }
        Self::enter_level(
            sel.where_clause.as_deref(),
            &sel.from_clause,
            sel.having_clause.as_deref(),
            &names,
            snapshot,
        )
    }

    fn enter_level(
        where_clause: Option<&protobuf::Node>,
        from: &[protobuf::Node],
        having: Option<&protobuf::Node>,
        ctes: &[&str],
        snapshot: &PgCatalog,
    ) -> Self {
        let next = NEXT_LEVEL.with(|n| n.replace(NextLevel::Own));
        let parent = LEVELS
            .with(|l| l.borrow().last().copied())
            .unwrap_or_default();
        let quals = if super::plan_time_checks() {
            let mut conjuncts = Vec::new();
            if let Some(w) = where_clause {
                flatten_and(w, &mut conjuncts);
            }
            for f in from {
                join_quals(f, &mut conjuncts);
            }
            // subquery_planner moves a HAVING qual without columns (nor
            // aggregates) into WHERE.
            let mut having_quals = Vec::new();
            if let Some(h) = having {
                flatten_and(h, &mut having_quals);
                having_quals.retain(|q| fold(q, snapshot).may_be_const());
            }
            let mut dummy_quals = conjuncts.clone();
            dummy_quals.extend(having_quals.iter().copied());
            let mut group = Group::default();
            group.walk(where_clause, from, ctes, snapshot);
            group.quals.extend(having_quals.into_iter().cloned());
            let (rels, inherited) = match next {
                NextLevel::PulledUp => (group.rels.max(parent.rels), parent.may_be_empty),
                NextLevel::PulledUpNullable | NextLevel::Own | NextLevel::Subplan => {
                    (group.rels, parent.may_be_empty)
                }
                NextLevel::Separate { pushed_down } => {
                    if pushed_down {
                        let pushed = PUSHABLE.with(|p| p.borrow().last().cloned());
                        group.quals.extend(pushed.into_iter().flatten());
                    }
                    (group.rels, false)
                }
            };
            let pushable = conjuncts
                .iter()
                .filter(|c| fold(c, snapshot) == Folded::Var && may_prove_empty(c, snapshot))
                .map(|c| (*c).clone())
                .collect();
            PUSHABLE.with(|p| p.borrow_mut().push(pushable));
            LevelQuals {
                may_be_empty: inherited || group.may_be_empty(rels, snapshot),
                may_be_dummy: (parent.may_be_dummy && next != NextLevel::Subplan)
                    || may_be_empty(&dummy_quals, snapshot),
                conjuncts: conjuncts.len(),
                rels,
            }
        } else {
            PUSHABLE.with(|p| p.borrow_mut().push(Vec::new()));
            LevelQuals::default()
        };
        LEVELS.with(|l| l.borrow_mut().push(quals));
        PlanLevel
    }
}

impl Drop for PlanLevel {
    fn drop(&mut self) {
        LEVELS.with(|l| l.borrow_mut().pop());
        PUSHABLE.with(|p| p.borrow_mut().pop());
    }
}

/// A query level as the planner flattens it (`pull_up_subqueries`): its
/// own quals and base relations, and those of the FROM subqueries and
/// views it pulls up where they are never null-extended. Counted from
/// below: a relation the analyzer can't be sure stays one (a function the
/// planner may inline, a CTE it may inline) counts for none.
#[derive(Default)]
struct Group {
    /// The quals the planner distributes over the level's join tree.
    quals: Vec<protobuf::Node>,
    /// Some quals of a subquery or CTE the planner may pull up are unknown.
    unknown_quals: bool,
    /// A reference to a column may be replaced by an expression, maybe a
    /// constant (a pulled-up subquery's output): a qual over columns may
    /// fold to a constant too.
    substitutes: bool,
    /// How many base relations the join tree has, at least.
    rels: usize,
    /// The relations already counted.
    relids: Vec<crate::oid::PgClassOid>,
    /// The views whose queries are being walked (a view loop is an error
    /// of its own, raised as the rewriter expands it).
    views: Vec<crate::oid::PgClassOid>,
}

impl Group {
    fn walk(
        &mut self,
        where_clause: Option<&protobuf::Node>,
        from: &[protobuf::Node],
        ctes: &[&str],
        snapshot: &PgCatalog,
    ) {
        if let Some(w) = where_clause {
            let mut quals = Vec::new();
            flatten_and(w, &mut quals);
            self.quals.extend(quals.into_iter().cloned());
        }
        for f in from {
            self.item(f, true, ctes, snapshot);
        }
    }

    /// A subquery (or view's query) `sel` in FROM, which may read the CTEs
    /// `ctes`, never null-extended when `kept`.
    fn subquery(
        &mut self,
        sel: &protobuf::SelectStmt,
        kept: bool,
        ctes: &[&str],
        snapshot: &PgCatalog,
    ) {
        if !kept {
            return;
        }
        match crate::resolve::plan_of(sel, snapshot) {
            crate::resolve::Plan::PulledUp => {
                // Only an output that is no column of its own may fold.
                self.substitutes |= !sel.values_lists.is_empty()
                    || sel.target_list.iter().any(|t| {
                        !matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
                            if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
                                Some(node::Node::ColumnRef(_))))
                    });
                self.walk(
                    sel.where_clause.as_deref(),
                    &sel.from_clause,
                    ctes,
                    snapshot,
                );
            }
            crate::resolve::Plan::Separate | crate::resolve::Plan::Whole => self.rels += 1,
            // Maybe pulled up, with its quals.
            crate::resolve::Plan::Uncertain => self.unknown_quals = true,
        }
    }

    /// FROM item `n`, never null-extended when `kept`.
    fn item(&mut self, n: &protobuf::Node, kept: bool, ctes: &[&str], snapshot: &PgCatalog) {
        match n.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => {
                if rv.schemaname.is_empty() && ctes.contains(&rv.relname.as_str()) {
                    // An inlined CTE is a pulled-up subquery.
                    self.unknown_quals |= kept;
                    self.substitutes |= kept;
                    return;
                }
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                let Some(class) = snapshot.resolve_table(schema, &rv.relname) else {
                    return;
                };
                match class.relkind {
                    RelKind::Table
                    | RelKind::Partitioned
                    | RelKind::MaterializedView
                    | RelKind::ForeignTable => {
                        // Self-join elimination may merge two scans of
                        // one table.
                        if kept && !self.relids.contains(&class.oid) {
                            self.relids.push(class.oid);
                            self.rels += 1;
                        }
                    }
                    RelKind::View if snapshot.security_barrier_views.contains(&class.oid) => {
                        if kept {
                            self.rels += 1;
                        }
                    }
                    RelKind::View if !kept => {}
                    RelKind::View if self.views.contains(&class.oid) => self.unknown_quals = true,
                    RelKind::View => match crate::ddl::views::view_query(snapshot, class.oid)
                        .and_then(|q| q.node)
                    {
                        // A view's query sees no CTE of the statement.
                        Some(node::Node::SelectStmt(sel)) => {
                            self.views.push(class.oid);
                            self.subquery(&sel, kept, &[], snapshot);
                            self.views.pop();
                        }
                        _ => self.unknown_quals = true,
                    },
                    _ => self.unknown_quals |= kept,
                }
            }
            Some(node::Node::RangeSubselect(sub)) => {
                if let Some(node::Node::SelectStmt(sel)) =
                    sub.subquery.as_deref().and_then(|q| q.node.as_ref())
                {
                    self.subquery(sel, kept, ctes, snapshot);
                }
            }
            Some(node::Node::JoinExpr(j)) => {
                let (left, right) = match protobuf::JoinType::try_from(j.jointype) {
                    Ok(protobuf::JoinType::JoinInner) => (kept, kept),
                    Ok(protobuf::JoinType::JoinLeft) => (kept, false),
                    Ok(protobuf::JoinType::JoinRight) => (false, kept),
                    _ => (false, false),
                };
                if let Some(l) = j.larg.as_deref() {
                    self.item(l, left, ctes, snapshot);
                }
                if let Some(r) = j.rarg.as_deref() {
                    self.item(r, right, ctes, snapshot);
                }
                if let Some(q) = j.quals.as_deref() {
                    let mut quals = Vec::new();
                    flatten_and(q, &mut quals);
                    self.quals.extend(quals.into_iter().cloned());
                }
            }
            // A function the planner may inline as a subquery it pulls up.
            Some(node::Node::RangeFunction(_)) => self.substitutes |= kept,
            _ => {}
        }
    }

    /// Whether the planner may prove a relation of the level empty: a
    /// qual over a column proves it of that column's relation (`col IS
    /// NULL` over a NOT NULL column, two constants one equivalence class
    /// equates) — any of the level's, to be sure. A qual without columns
    /// that may fold to a constant other than TRUE is distributed to every
    /// relation of the level (`distribute_qual_to_rels`): with a single
    /// one, it is that relation's restriction, which proves it empty
    /// (`relation_excluded_by_constraints`); with several, it is a gating
    /// qual above their join, and each is still estimated.
    fn may_be_empty(&self, rels: usize, snapshot: &PgCatalog) -> bool {
        let constant = self.unknown_quals
            || (self.substitutes && !self.quals.is_empty())
            || self.quals.iter().any(|q| {
                let folded = fold(q, snapshot);
                folded.may_be_const() && folded != Folded::Bool(true)
            });
        let column_quals: Vec<&protobuf::Node> = self
            .quals
            .iter()
            .filter(|q| !fold(q, snapshot).may_be_const())
            .collect();
        (constant && rels < 2) || may_be_empty(&column_quals, snapshot)
    }
}

fn flatten_and<'a>(n: &'a protobuf::Node, out: &mut Vec<&'a protobuf::Node>) {
    if let Some(node::Node::BoolExpr(b)) = n.node.as_ref()
        && b.boolop == protobuf::BoolExprType::AndExpr as i32
    {
        for a in &b.args {
            flatten_and(a, out);
        }
        return;
    }
    out.push(n);
}

fn join_quals<'a>(n: &'a protobuf::Node, out: &mut Vec<&'a protobuf::Node>) {
    if let Some(node::Node::JoinExpr(j)) = n.node.as_ref() {
        for side in [&j.larg, &j.rarg].into_iter().flatten() {
            join_quals(side, out);
        }
        if let Some(q) = j.quals.as_deref() {
            flatten_and(q, out);
        }
    }
}

/// A key identifying a literal's value, for telling two constants apart.
fn literal_key(n: &protobuf::Node) -> Option<String> {
    match n.node.as_ref()? {
        node::Node::AConst(c) if !c.isnull => Some(match c.val.as_ref()? {
            a_const::Val::Ival(i) => format!("i{}", i.ival),
            a_const::Val::Fval(f) => format!("f{}", f.fval),
            a_const::Val::Sval(s) => format!("s{}", s.sval),
            a_const::Val::Boolval(b) => format!("b{}", b.boolval),
            a_const::Val::Bsval(b) => format!("x{}", b.bsval),
        }),
        _ => None,
    }
}

/// Whether `c`, a qual over columns, is one [`may_be_empty`] may prove a
/// relation empty with: `col IS NULL`, or `expr = constant`.
fn may_prove_empty(c: &protobuf::Node, snapshot: &PgCatalog) -> bool {
    match c.node.as_ref() {
        Some(node::Node::NullTest(t)) => {
            t.nulltesttype == protobuf::NullTestType::IsNull as i32
                && matches!(
                    t.arg.as_deref().and_then(|a| a.node.as_ref()),
                    Some(node::Node::ColumnRef(_))
                )
        }
        Some(node::Node::AExpr(e)) if extract_names(&e.name) == ["="] => {
            match protobuf::AExprKind::try_from(e.kind) {
                Ok(protobuf::AExprKind::AexprOp) => [&e.lexpr, &e.rexpr]
                    .into_iter()
                    .flatten()
                    .any(|s| fold(s, snapshot).may_be_const()),
                Ok(protobuf::AExprKind::AexprIn) => matches!(
                    e.rexpr.as_deref().and_then(|r| r.node.as_ref()),
                    Some(node::Node::List(l)) if l.items.len() == 1
                ),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether the planner may prove the level's relations empty from its
/// quals: one folds to a constant other than TRUE, one is `col IS NULL`
/// (FALSE over a NOT NULL column), or two `expr = constant` may put two
/// different constants in one equivalence class.
fn may_be_empty(conjuncts: &[&protobuf::Node], snapshot: &PgCatalog) -> bool {
    let mut equated: Vec<Option<String>> = Vec::new();
    for c in conjuncts {
        let folded = fold(c, snapshot);
        if folded.may_be_const() && folded != Folded::Bool(true) {
            return true;
        }
        match c.node.as_ref() {
            Some(node::Node::NullTest(t))
                if t.nulltesttype == protobuf::NullTestType::IsNull as i32
                    && matches!(
                        t.arg.as_deref().and_then(|a| a.node.as_ref()),
                        Some(node::Node::ColumnRef(_))
                    ) =>
            {
                return true;
            }
            Some(node::Node::AExpr(e)) if extract_names(&e.name) == ["="] => {
                let constant = match protobuf::AExprKind::try_from(e.kind) {
                    Ok(protobuf::AExprKind::AexprOp) => [&e.lexpr, &e.rexpr]
                        .into_iter()
                        .flatten()
                        .find(|s| fold(s, snapshot).may_be_const())
                        .map(|s| &**s),
                    Ok(protobuf::AExprKind::AexprIn) => {
                        match e.rexpr.as_deref().and_then(|r| r.node.as_ref()) {
                            Some(node::Node::List(l)) if l.items.len() == 1 => l.items.first(),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(constant) = constant {
                    equated.push(literal_key(constant));
                }
            }
            _ => {}
        }
    }
    equated.len() >= 2
        && (equated.iter().any(Option::is_none) || equated.windows(2).any(|w| w[0] != w[1]))
}

/// Whether the planner may prove the current level's relations empty.
pub(crate) fn level_may_be_empty() -> bool {
    LEVELS.with(|l| l.borrow().last().is_some_and(|q| q.may_be_empty))
}

/// Whether the plan of the current level may be a dummy one, whose nodes
/// the executor never starts.
pub(crate) fn level_may_be_dummy() -> bool {
    LEVELS.with(|l| l.borrow().last().is_some_and(|q| q.may_be_dummy))
}

/// How many quals the current level has.
pub(crate) fn level_conjuncts() -> usize {
    LEVELS.with(|l| l.borrow().last().map_or(0, |q| q.conjuncts))
}

/// Run `f` (a JOIN's ON clause) knowing the planner may remove the FROM
/// items `aliases` (the nullable side of a LEFT / RIGHT join that may be
/// proven useless, `join_is_removable`) before estimating them.
pub(crate) fn skip_restrictions_on<R>(aliases: Vec<String>, f: impl FnOnce() -> R) -> R {
    let saved = SKIPPED.with(|s| std::mem::replace(&mut *s.borrow_mut(), aliases));
    SKIP_DEPTH.with(|d| d.set(d.get() + 1));
    let out = f();
    SKIP_DEPTH.with(|d| d.set(d.get() - 1));
    SKIPPED.with(|s| *s.borrow_mut() = saved);
    out
}

/// Whether the restrictions of FROM item `alias` of the current level are
/// estimated whenever the statement is planned: it is a table or
/// materialized view — or a partitioned table / inheritance parent whose
/// level has no qual but the restriction (no partition is pruned nor
/// child excluded), with a partition to scan and no CHECK constraint on
/// a member — not removable by the join it is in.
pub(crate) fn restriction_estimated(
    source: &crate::scope::TableSource,
    snapshot: &PgCatalog,
) -> bool {
    if snapshot.planner_settings_changed
        || (SKIP_DEPTH.with(Cell::get) > 0 && SKIPPED.with(|s| s.borrow().contains(&source.alias)))
    {
        return false;
    }
    if !matches!(source.kind, crate::scope::SourceKind::Relation) {
        return false;
    }
    let Some(relid) = source.relid else {
        return false;
    };
    let Some(class) = snapshot.pg_class.get(&relid) else {
        return false;
    };
    let members = |relid| -> Vec<crate::oid::PgClassOid> {
        let mut all = vec![relid];
        all.extend(crate::resolve::inheritance_descendants(snapshot, relid));
        all
    };
    let no_checks = |rels: &[crate::oid::PgClassOid]| {
        !snapshot
            .pg_constraint
            .values()
            .any(|c| c.contype == crate::pg_catalog::ConType::Check && rels.contains(&c.conrelid))
    };
    match class.relkind {
        RelKind::Table | RelKind::MaterializedView => {
            if source.only {
                return true;
            }
            let rels = members(relid);
            rels.len() == 1 || (level_conjuncts() == 1 && no_checks(&rels))
        }
        RelKind::Partitioned => {
            if source.only {
                // ONLY a partitioned table scans nothing.
                return false;
            }
            let rels = members(relid);
            let leaves: Vec<_> = rels
                .iter()
                .copied()
                .filter(|r| {
                    snapshot
                        .pg_class
                        .get(r)
                        .is_some_and(|c| c.relkind != RelKind::Partitioned)
                })
                .collect();
            !leaves.is_empty() && level_conjuncts() == 1 && no_checks(&rels)
        }
        _ => false,
    }
}

/// Whether the restrictions of `relid`, a table a subquery of the current
/// level (FROM item `alias`) the planner pulls up scans, are estimated
/// whenever the statement is planned: a plain table or materialized view
/// without inheritance children, whose subquery its join can't remove.
pub(crate) fn table_restriction_estimated(
    alias: &str,
    relid: crate::oid::PgClassOid,
    snapshot: &PgCatalog,
) -> bool {
    if snapshot.planner_settings_changed
        || (SKIP_DEPTH.with(Cell::get) > 0
            && SKIPPED.with(|s| s.borrow().iter().any(|a| a == alias)))
    {
        return false;
    }
    snapshot
        .pg_class
        .get(&relid)
        .is_some_and(|c| matches!(c.relkind, RelKind::Table | RelKind::MaterializedView))
        && crate::resolve::inheritance_descendants(snapshot, relid).is_empty()
}

/// Whether the FROM item a JOIN's nullable side holds may be removed by
/// `remove_useless_joins` (a relation with a unique index, or a derived
/// one the planner may prove unique).
pub(crate) fn may_be_removed(source: &crate::scope::TableSource, snapshot: &PgCatalog) -> bool {
    let Some(relid) = source.relid else {
        return true;
    };
    !matches!(source.kind, crate::scope::SourceKind::Relation)
        || snapshot
            .pg_index
            .values()
            .any(|i| i.indrelid == relid && i.indisunique)
}
