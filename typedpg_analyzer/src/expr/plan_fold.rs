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
//! - `pull_up_sublinks` first turns an IN or [NOT] EXISTS sublink that is a
//!   top-level AND argument of WHERE into a join: its subquery is planned
//!   with the query, whatever the WHERE then folds to;
//! - a relation is estimated only when it isn't proven empty first
//!   (`relation_excluded_by_constraints`: a constant FALSE / NULL
//!   restriction, an equivalence class equating two different constants
//!   in a join domain of that relation alone, `col IS NULL` over a NOT
//!   NULL column — which prove only that column's relation empty;
//!   partition pruning; an inheritance child's constraints), nor removed
//!   (`remove_useless_joins`, which needs a relation it can prove distinct
//!   and none of whose columns is read above the join).
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
                if operator_may_be_volatile(&e.name, snapshot) {
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

/// Whether an operator named `name` may be a volatile one (or one whose
/// function the analyzer can't find).
fn operator_may_be_volatile(name: &[protobuf::Node], snapshot: &PgCatalog) -> bool {
    let names = extract_names(name);
    names.last().is_some_and(|name| {
        snapshot.pg_operator.values().any(|o| {
            &o.oprname == name
                && o.oprcode
                    .and_then(|c| snapshot.pg_proc.get(&c))
                    .is_none_or(|p| p.provolatile == ProVolatile::Volatile)
        })
    })
}

/// Whether `pull_up_sublinks` turns `sub`, a top-level AND argument of a
/// WHERE clause, into a semi-join (`convert_ANY_sublink_to_join`) — before
/// the clause is folded: an `x IN (SELECT …)` (or `= ANY`) whose tested
/// expression reads a column of the level (as `is_level_column` tells)
/// and calls nothing volatile. Its subquery then joins the level, a FROM
/// subquery never null-extended.
pub(crate) fn joined_any_sublink(
    sub: &protobuf::SubLink,
    snapshot: &PgCatalog,
    is_level_column: &dyn Fn(&protobuf::ColumnRef) -> bool,
) -> bool {
    if sub.sub_link_type != protobuf::SubLinkType::AnySublink as i32 {
        return false;
    }
    let Some(test) = sub.testexpr.as_deref() else {
        return false;
    };
    if may_be_volatile(test, snapshot)
        || (!sub.oper_name.is_empty() && operator_may_be_volatile(&sub.oper_name, snapshot))
    {
        return false;
    }
    let Some(inner) = test.node.as_ref() else {
        return false;
    };
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        match n {
            // A sublink's columns are its own query's.
            typedpg_pg_query::NodeRef::SubLink(_) => continue,
            typedpg_pg_query::NodeRef::ColumnRef(cr) if is_level_column(cr) => return true,
            _ => {}
        }
        todo.extend(n.children());
    }
    false
}

/// Whether `pull_up_sublinks` turns `sub`, an EXISTS sublink that is (or
/// whose NOT is) a top-level AND argument of a WHERE clause, into a semi-
/// (anti-) join before the clause is folded
/// (`convert_EXISTS_sublink_to_join`): `simplify_EXISTS_query` can throw
/// its select list away (no aggregate, window or set-returning function,
/// grouping set, HAVING, OFFSET, LIMIT but a positive or NULL one,
/// locking, set operation or WITH) with its GROUP BY, WINDOW, DISTINCT
/// and ORDER BY clauses, its
/// FROM reads plain relations only (maybe joined on their own columns),
/// and its WHERE reads a column of the
/// level (`is_level_column`, of a column named by no FROM item of the
/// subquery) and calls nothing volatile. Its FROM items then join the
/// level, its WHERE becomes the join's qual.
pub(crate) fn joined_exists_sublink(
    sub: &protobuf::SubLink,
    snapshot: &PgCatalog,
    is_level_column: &dyn Fn(&protobuf::ColumnRef) -> bool,
) -> bool {
    if sub.sub_link_type != protobuf::SubLinkType::ExistsSublink as i32 {
        return false;
    }
    let Some(node::Node::SelectStmt(sel)) = sub.subselect.as_deref().and_then(|s| s.node.as_ref())
    else {
        return false;
    };
    if sel.op != protobuf::SetOperation::SetopNone as i32
        || !sel.values_lists.is_empty()
        || sel.with_clause.is_some()
        // GROUP BY (WINDOW, DISTINCT, ORDER BY) is thrown away; grouping
        // sets are not.
        || sel
            .group_clause
            .iter()
            .filter_map(|g| g.node.as_ref())
            .any(|g| {
                g.nodes()
                    .into_iter()
                    .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::GroupingSet(_)))
            })
        || sel.having_clause.is_some()
        || sel.limit_offset.is_some()
        // A positive or NULL (ALL) constant LIMIT is dropped.
        || sel.limit_count.as_deref().is_some_and(|l| {
            !matches!(l.node.as_ref(), Some(node::Node::AConst(c)) if c.isnull)
                && int_literal(l).is_none_or(|n| n <= 0)
        })
        || !sel.locking_clause.is_empty()
    {
        return false;
    }
    // Plain relations, maybe joined (without an alias for the join).
    fn plain<'a>(
        n: &'a protobuf::Node,
        inner: &mut Vec<&'a str>,
        on: &mut Vec<&'a protobuf::Node>,
    ) -> bool {
        match n.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => {
                inner.push(
                    rv.alias
                        .as_ref()
                        .map_or(rv.relname.as_str(), |a| &a.aliasname),
                );
                true
            }
            Some(node::Node::JoinExpr(j)) if j.alias.is_none() && j.join_using_alias.is_none() => {
                on.extend(j.quals.as_deref());
                [&j.larg, &j.rarg]
                    .into_iter()
                    .all(|s| s.as_deref().is_some_and(|s| plain(s, inner, on)))
            }
            _ => false,
        }
    }
    let mut inner = Vec::new();
    let mut on = Vec::new();
    for f in &sel.from_clause {
        if !plain(f, &mut inner, &mut on) {
            return false;
        }
    }
    // Nothing but the WHERE clause may read the level
    // (`contain_vars_of_level`): every column of a join's ON clause is
    // certainly one of the subquery's own, qualified by one of its FROM
    // items, and it holds no sublink.
    let own_columns = |q: &protobuf::Node| {
        q.node.as_ref().is_some_and(|q| {
            q.nodes().into_iter().all(|(n, _)| match n {
                typedpg_pg_query::NodeRef::SubLink(_) => false,
                typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                    let names = extract_names(&cr.fields);
                    matches!(names.as_slice(), [t, _]
                        if names.len() == cr.fields.len() && inner.contains(&t.as_str()))
                }
                _ => true,
            })
        })
    };
    if !on.into_iter().all(own_columns) {
        return false;
    }
    // No aggregate, window or set-returning function in what the planner
    // throws away (nor anything the analyzer can't tell of).
    let mut todo: Vec<typedpg_pg_query::NodeRef<'_>> = sel
        .target_list
        .iter()
        .chain(&sel.sort_clause)
        .filter_map(|n| n.node.as_ref().map(node::Node::to_ref))
        .collect();
    while let Some(n) = todo.pop() {
        match n {
            typedpg_pg_query::NodeRef::SubLink(_) => continue,
            typedpg_pg_query::NodeRef::FuncCall(fc) => {
                let names = extract_names(&fc.funcname);
                let (schema, name) = match names.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => return false,
                };
                let candidates = snapshot.find_functions(schema, name);
                if fc.over.is_some()
                    || fc.agg_star
                    || candidates.is_empty()
                    || candidates.iter().any(|p| {
                        p.proretset
                            || matches!(
                                p.prokind,
                                crate::pg_catalog::ProKind::Aggregate
                                    | crate::pg_catalog::ProKind::Window
                            )
                    })
                {
                    return false;
                }
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    let Some(qual) = sel.where_clause.as_deref() else {
        return false;
    };
    if may_be_volatile(qual, snapshot) {
        return false;
    }
    let Some(qual) = qual.node.as_ref() else {
        return false;
    };
    let mut todo = vec![qual.to_ref()];
    while let Some(n) = todo.pop() {
        match n {
            typedpg_pg_query::NodeRef::SubLink(_) => continue,
            typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                let names = extract_names(&cr.fields);
                if let [t, _] = names.as_slice()
                    && names.len() == cr.fields.len()
                    && !inner.contains(&t.as_str())
                    && is_level_column(cr)
                {
                    return true;
                }
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    false
}

/// The sublink `pull_up_sublinks` may turn into a join a top-level AND
/// argument `n` of a WHERE clause holds: `n` itself, or the EXISTS a NOT
/// negates.
pub(crate) fn conjunct_sublink(n: &protobuf::Node) -> Option<&protobuf::SubLink> {
    match n.node.as_ref()? {
        node::Node::SubLink(sub) => Some(sub),
        node::Node::BoolExpr(b) if b.boolop == protobuf::BoolExprType::NotExpr as i32 => {
            match b.args.as_slice() {
                [a] => match a.node.as_ref()? {
                    node::Node::SubLink(sub)
                        if sub.sub_link_type == protobuf::SubLinkType::ExistsSublink as i32 =>
                    {
                        Some(sub)
                    }
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

thread_local! {
    /// The WHERE clause being analyzed, at the innermost level analyzing
    /// one (see [`with_where_clause`]) — or the ON clause of an inner join
    /// never null-extended, whose sublinks `pull_up_sublinks` pulls up
    /// alike (reading only the join's sides, which is all the ON clause
    /// sees).
    static WHERE_ROOT: Cell<*const protobuf::Node> = const { Cell::new(std::ptr::null()) };
    /// The FROM items (by alias) a sublink of [`WHERE_ROOT`] may read to
    /// be pulled up, when that is the ON clause of a join a semi-join
    /// below it joins only part of (see [`with_nullable_on_clause`]);
    /// `None` for any of the level's.
    static AVAILABLE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Run `f`, the analysis of WHERE clause `n` (or of such an ON clause).
pub(crate) fn with_where_clause<R>(n: &protobuf::Node, f: impl FnOnce() -> R) -> R {
    with_root(n, None, f)
}

/// Run `f`, the analysis of ON clause `n` of a join whose sublinks
/// `pull_up_sublinks` turns into semi-joins on the nullable side of an
/// outer join (`pull_up_sublinks_qual_recurse`): a LEFT (RIGHT) join's,
/// into its right (left) side — those reading only the FROM items
/// `available` of that side — or an inner join's below the nullable side
/// of one, into the join (`available`, its two sides).
pub(crate) fn with_nullable_on_clause<R>(
    n: &protobuf::Node,
    available: Vec<String>,
    f: impl FnOnce() -> R,
) -> R {
    with_root(n, Some(available), f)
}

fn with_root<R>(n: &protobuf::Node, available: Option<Vec<String>>, f: impl FnOnce() -> R) -> R {
    let prev = WHERE_ROOT.with(|w| w.replace(std::ptr::from_ref(n)));
    let prev_available = AVAILABLE.with(|a| a.replace(available));
    let out = f();
    WHERE_ROOT.with(|w| w.set(prev));
    AVAILABLE.with(|a| *a.borrow_mut() = prev_available);
    out
}

/// The FROM items a sublink of the clause [`is_where_clause`] tells of
/// may read to be pulled up, when not any of the level's (see
/// [`with_nullable_on_clause`]).
pub(crate) fn where_available() -> Option<Vec<String>> {
    AVAILABLE.with(|a| a.borrow().clone())
}

/// Whether `n` is the WHERE clause being analyzed.
pub(crate) fn is_where_clause(n: *const protobuf::Node) -> bool {
    !n.is_null() && WHERE_ROOT.with(|w| w.get() == n)
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

/// What qual `n` — a WHERE or JOIN ON clause, a HAVING one — folds to
/// once the planner preprocessed it.
pub(crate) fn fold_qual(n: &protobuf::Node, snapshot: &PgCatalog) -> Folded {
    qual_with(n, snapshot, &|_| Folded::Var, false)
}

/// [`fold_qual`] for a part of a qual's AND / OR / NOT tree, under an odd
/// number of NOTs when `negated`, in the scope of `ctx`: a column the
/// planner replaces by a pulled-up subquery's output is what that folds to
/// (see [`crate::scope::Substitution`]).
pub(crate) fn fold_qual_in(n: &protobuf::Node, ctx: super::Ctx<'_>, negated: bool) -> Folded {
    qual_with(n, ctx.snapshot, &|cr| column_in(cr, ctx), negated)
}

/// `eval_const_expressions` pushes a NOT down an AND / OR (`negate_clause`,
/// De Morgan's laws), then `canonicalize_qual` (`find_duplicate_ors`), at
/// any depth of the AND / OR tree, reduces an AND with a FALSE or NULL
/// argument to FALSE and drops a FALSE or NULL argument of an OR: in a
/// qual, the NULL constant is FALSE (`rt.s ~ '(' AND NULL` is FALSE, its
/// regex never estimated). Never [`Folded::Null`].
fn qual_with(
    n: &protobuf::Node,
    snapshot: &PgCatalog,
    col: &dyn Fn(&protobuf::ColumnRef) -> Folded,
    negated: bool,
) -> Folded {
    if let Some(node::Node::BoolExpr(b)) = n.node.as_ref() {
        match protobuf::BoolExprType::try_from(b.boolop) {
            Ok(protobuf::BoolExprType::NotExpr) => {
                if let Some(a) = b.args.first() {
                    return qual_with(a, snapshot, col, !negated);
                }
            }
            Ok(op) => {
                let or = (op == protobuf::BoolExprType::OrExpr) != negated;
                let (mut var, mut unknown) = (false, false);
                for a in &b.args {
                    match qual_with(a, snapshot, col, negated) {
                        Folded::Bool(v) if v == or => return Folded::Bool(or),
                        Folded::Bool(_) => {}
                        Folded::Const => unknown = true,
                        Folded::Var | Folded::Null => var = true,
                    }
                }
                return if unknown {
                    Folded::Const
                } else if var {
                    Folded::Var
                } else {
                    Folded::Bool(!or)
                };
            }
            Err(_) => {}
        }
    }
    match fold_with(n, snapshot, col) {
        Folded::Null => Folded::Bool(false),
        Folded::Bool(v) => Folded::Bool(v != negated),
        other => other,
    }
}

/// The expression the planner replaces column `cr` by, in the scope of
/// `ctx`, before folding: the output of a subquery it certainly pulls up
/// where it is never null-extended (see [`crate::scope::Substitution`]).
pub(crate) fn substituted_column(
    cr: &protobuf::ColumnRef,
    ctx: super::Ctx<'_>,
) -> Option<std::sync::Arc<protobuf::Node>> {
    let parts = extract_names(&cr.fields);
    let (table, column) = match parts.as_slice() {
        [c] if parts.len() == cr.fields.len() => (None, c.as_str()),
        [t, c] if parts.len() == cr.fields.len() => (Some(t.as_str()), c.as_str()),
        _ => return None,
    };
    let col = ctx.scope.resolve_column(table, column, None).ok()?;
    match col.deferred.substitution {
        crate::scope::Substitution::Certain => col.deferred.expr.clone(),
        _ => None,
    }
}

/// What column `cr` folds to in the scope of `ctx` (see [`fold_qual_in`]).
fn column_in(cr: &protobuf::ColumnRef, ctx: super::Ctx<'_>) -> Folded {
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
#[derive(Debug, Clone, Default)]
struct LevelQuals {
    /// The planner may prove any relation of the level empty (or remove
    /// it) before estimating it: no restriction is estimated, and the
    /// subqueries in FROM planned on their own may never be planned.
    may_be_empty: bool,
    /// The FROM items of the level (by alias) the planner may prove
    /// empty, beside: a qual over a column proves only that column's
    /// relation empty (`relation_excluded_by_constraints` reads a
    /// relation's own restrictions); the others are still estimated.
    excluded: Vec<String>,
    /// The aliases of the level's own FROM items: one [`Self::excluded`]
    /// can't tell of (a join's alias) may be any.
    items: Vec<String>,
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
    /// The level is a FROM subquery the planner pulls up: what it doesn't
    /// output (its WINDOW clause, which no window call can use there) is
    /// never planned.
    pulled_up: bool,
    /// The level's SELECT, when it is one (see [`read_above`]): the address
    /// of a node the analysis borrows while the level is entered.
    select: Option<*const protobuf::SelectStmt>,
    /// Its FROM clause (an UPDATE's or DELETE's, with its target),
    /// borrowed alike.
    from: Option<*const [protobuf::Node]>,
    /// Every relation of the level has a row mark (`preprocess_rowmarks`):
    /// an UPDATE, a DELETE, a SELECT with a locking clause, or a level
    /// pulled up into one. Its row identity is needed above every join.
    row_marks: bool,
    /// How its select list is used.
    outputs: Outputs,
    /// The alias of the FROM item it is in the level around it.
    item: Option<String>,
    /// The CTEs it may read, by name.
    ctes: Vec<String>,
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
    /// The alias of the FROM item the next level analyzed is, in the
    /// level around it (see [`with_next_item`]).
    static NEXT_ITEM: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Aliases of FROM items whose restrictions are not estimated (see
    /// [`skip_restrictions_on`]).
    static SKIPPED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static SKIP_DEPTH: Cell<usize> = const { Cell::new(0) };
    /// What estimating the skipped FROM items' restrictions raises, while
    /// [`record_unless_removed`] collects it.
    static UNLESS_REMOVED: RefCell<Option<Vec<super::PlanTimeError>>> =
        const { RefCell::new(None) };
}

/// Run `f`, the analysis of a query level planned as `next`.
pub(crate) fn with_next_level<R>(next: NextLevel, f: impl FnOnce() -> R) -> R {
    let prev = NEXT_LEVEL.with(|n| n.replace(next));
    let out = f();
    NEXT_LEVEL.with(|n| n.set(prev));
    out
}

/// [`with_next_level`] for a level that is FROM item `alias` of the level
/// around it: what that level's quals prove of `alias` alone holds for it.
pub(crate) fn with_next_item<R>(next: NextLevel, alias: &str, f: impl FnOnce() -> R) -> R {
    let prev = NEXT_ITEM.with(|n| n.replace(Some(alias.to_owned())));
    let out = with_next_level(next, f);
    NEXT_ITEM.with(|n| *n.borrow_mut() = prev);
    out
}

/// Whether the next query level analyzed is a SubPlan's (see
/// [`NextLevel::Subplan`]).
pub(crate) fn next_level_is_subplan() -> bool {
    NEXT_LEVEL.with(|n| n.get() == NextLevel::Subplan)
}

/// The statement a query level is (see [`PlanLevel`]).
#[derive(Clone, Copy)]
enum Level<'a> {
    Select(&'a protobuf::SelectStmt),
    /// An UPDATE or DELETE, whose target is at this location.
    Dml {
        target: Option<i32>,
    },
}

/// RAII marker for a query level the planner plans (a SELECT, UPDATE,
/// DELETE), with the quals it restricts its relations by.
pub(crate) struct PlanLevel;

impl PlanLevel {
    /// Enter SELECT `sel`, whose quals are its WHERE clause and the ON
    /// clauses of the joins in its FROM, which may read the CTEs `ctes`
    /// (and its own WITH's). A level inside one that may be proven empty —
    /// a FROM subquery pulled up into it, a sublink — may be proven empty
    /// too: its relations are not estimated. (Its constants are still
    /// folded where the planner plans them — a pulled-up subquery's output
    /// where the outer query reads it, see `resolve::deferred` — as
    /// constant folding precedes relation exclusion.) A constant HAVING
    /// qual is moved into WHERE, so it too may make the plan a dummy one.
    pub(crate) fn enter_select<'a>(
        sel: &'a protobuf::SelectStmt,
        ctes: impl IntoIterator<Item = &'a String>,
        snapshot: &PgCatalog,
    ) -> Self {
        let names = cte_list(
            ctes,
            sel.with_clause.as_ref(),
            typedpg_pg_query::NodeRef::SelectStmt(sel),
            None,
            snapshot,
        );
        Self::enter_level(
            sel.where_clause.as_deref(),
            &sel.from_clause,
            &sel.from_clause,
            sel.having_clause.as_deref(),
            &names,
            Level::Select(sel),
            snapshot,
        )
    }

    /// [`Self::enter_select`] for an UPDATE or DELETE, `stmt`, of target
    /// `target` (one of `from`, a table whatever the CTEs), whose FROM /
    /// USING items may read the CTEs `ctes` and those of its own WITH,
    /// `with`.
    pub(crate) fn enter_dml<'a>(
        where_clause: Option<&protobuf::Node>,
        from: &[protobuf::Node],
        target: Option<&protobuf::RangeVar>,
        with: Option<&'a protobuf::WithClause>,
        stmt: typedpg_pg_query::NodeRef<'_>,
        ctes: impl IntoIterator<Item = &'a String>,
        snapshot: &PgCatalog,
    ) -> Self {
        let names = cte_list(ctes, with, stmt, target, snapshot);
        let level = Level::Dml {
            target: target.map(|t| t.location),
        };
        // The read checks look for the joins the analysis walks: the
        // statement's own FROM / USING items.
        let own_from = match stmt {
            typedpg_pg_query::NodeRef::UpdateStmt(u) => u.from_clause.as_slice(),
            typedpg_pg_query::NodeRef::DeleteStmt(d) => d.using_clause.as_slice(),
            _ => from,
        };
        Self::enter_level(where_clause, from, own_from, None, &names, level, snapshot)
    }

    /// Enter the source `source` of a MERGE, while it is analyzed: of the
    /// level, only its row marks are modeled (every relation of the
    /// source has one, `preprocess_rowmarks`), which keep its joins'
    /// nullable sides (see [`read_above`]).
    pub(crate) fn enter_merge_source(source: &protobuf::Node) -> Self {
        let quals = if super::plan_time_checks() {
            LevelQuals {
                from: Some(std::ptr::from_ref(std::slice::from_ref(source))),
                row_marks: true,
                ..LevelQuals::default()
            }
        } else {
            LevelQuals::default()
        };
        PUSHABLE.with(|p| p.borrow_mut().push(Vec::new()));
        LEVELS.with(|l| l.borrow_mut().push(quals));
        PlanLevel
    }

    fn enter_level(
        where_clause: Option<&protobuf::Node>,
        from: &[protobuf::Node],
        own_from: &[protobuf::Node],
        having: Option<&protobuf::Node>,
        ctes: &[Cte<'_>],
        level: Level<'_>,
        snapshot: &PgCatalog,
    ) -> Self {
        let next = NEXT_LEVEL.with(|n| n.replace(NextLevel::Own));
        let item = NEXT_ITEM.with(|n| n.replace(None));
        let pending = NEXT_OUTPUTS.with(|o| o.replace(None));
        let semijoin = NEXT_SEMIJOIN.with(|o| o.replace(false));
        let parent = LEVELS
            .with(|l| l.borrow().last().cloned())
            .unwrap_or_default();
        let quals = if super::plan_time_checks() {
            let pulled_up = matches!(next, NextLevel::PulledUp | NextLevel::PulledUpNullable);
            let row_marks = match level {
                Level::Dml { .. } => true,
                Level::Select(sel) => !sel.locking_clause.is_empty(),
            } || (parent.row_marks && pulled_up);
            // SAFETY: the level around this one is on the stack.
            let parent_select = parent.select.map(|s| unsafe { &*s });
            // A set operation's arms are used as it is.
            let arm = match level {
                Level::Select(sel) => parent_select.is_some_and(|p| {
                    p.op != protobuf::SetOperation::SetopNone as i32
                        && [&p.larg, &p.rarg]
                            .into_iter()
                            .flatten()
                            .any(|a| std::ptr::eq(&**a, sel))
                }),
                Level::Dml { .. } => false,
            };
            let outputs = match pending {
                Some(o) => o,
                None if arm && next == NextLevel::Own => parent.outputs,
                None => Outputs::Needed,
            };
            // `simplify_EXISTS_query` keeps the select list of a set
            // operation, or of a query with HAVING, OFFSET, a locking
            // clause or a window function.
            let outputs = match level {
                Level::Select(sel)
                    if outputs == Outputs::Exists
                        && (sel.op != protobuf::SetOperation::SetopNone as i32
                            || sel.having_clause.is_some()
                            || sel.limit_offset.is_some()
                            || !sel.locking_clause.is_empty()
                            || sel.target_list.iter().any(|t| {
                                t.node.as_ref().is_some_and(|t| {
                                    t.nodes().into_iter().any(|(n, _)| {
                                        matches!(n, typedpg_pg_query::NodeRef::FuncCall(f)
                                            if f.over.is_some())
                                    })
                                })
                            })) =>
                {
                    Outputs::Needed
                }
                _ => outputs,
            };
            // DISTINCT and a set operation but UNION ALL read every output,
            // and so does a row mark of the level around (the whole row).
            let outputs = match level {
                Level::Select(sel)
                    if outputs == Outputs::Trimmed
                        && (parent.row_marks
                            || sel.distinct_clause.iter().any(|d| d.node.is_none())
                            || (sel.op != protobuf::SetOperation::SetopNone as i32
                                && !sel.all)) =>
                {
                    Outputs::Needed
                }
                _ => outputs,
            };
            // A level of its own (a statement, a scalar sublink's), whose
            // outer joins no qual of a level around may reduce.
            let own_level =
                pending.is_none() && !arm && matches!(next, NextLevel::Own | NextLevel::Subplan);
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
                having_quals.retain(|q| fold_qual(q, snapshot).may_be_const());
            }
            let mut dummy_quals = conjuncts.clone();
            dummy_quals.extend(having_quals.iter().copied());
            let mut group = Group {
                target: match level {
                    Level::Dml { target } => target,
                    Level::Select(_) => None,
                },
                ..Group::default()
            };
            group.walk(where_clause, from, ctes, snapshot, &Owner::Level, true);
            group.own_level = own_level;
            // The relations on the nullable side of an outer join that
            // are certainly kept are distributed the top join domain's
            // constant quals too (`jd_relids`).
            let cte_names: Vec<String> = ctes.iter().map(|c| c.name.to_owned()).collect();
            group.rels += Reads {
                select: match level {
                    Level::Select(sel) => Some(sel),
                    Level::Dml { .. } => None,
                },
                from: own_from,
                row_marks,
                outputs,
                pulled_up,
                item: item.as_deref(),
                parent: Some(&parent),
                ctes: &cte_names,
                snapshot,
            }
            .held_relations();
            group.quals.extend(
                having_quals
                    .into_iter()
                    .map(|q| (q.clone(), Owner::Level, true)),
            );
            // A level inside one any relation of which may be proven
            // empty may be, to be sure — a FROM subquery pulled up, only
            // when its relations may be.
            let parent_empty = match &item {
                Some(alias)
                    if matches!(next, NextLevel::PulledUp | NextLevel::PulledUpNullable) =>
                {
                    parent.excludes(alias)
                }
                // A semi-join's relations are none of the level's: only
                // what proves any of them empty may prove its.
                None if semijoin => parent.may_be_empty,
                _ => parent.may_be_empty || !parent.excluded.is_empty(),
            };
            let (rels, inherited) = match next {
                NextLevel::PulledUp => (group.rels.max(parent.rels), parent_empty),
                NextLevel::PulledUpNullable | NextLevel::Own | NextLevel::Subplan => {
                    (group.rels, parent_empty)
                }
                NextLevel::Separate { pushed_down } => {
                    if pushed_down {
                        let pushed = PUSHABLE.with(|p| p.borrow().last().cloned());
                        group.quals.extend(
                            pushed
                                .into_iter()
                                .flatten()
                                .map(|q| (q, Owner::Level, true)),
                        );
                    }
                    (group.rels, false)
                }
            };
            let pushable = conjuncts
                .iter()
                .filter(|c| fold_qual(c, snapshot) == Folded::Var && may_prove_empty(c, snapshot))
                .map(|c| (*c).clone())
                .collect();
            PUSHABLE.with(|p| p.borrow_mut().push(pushable));
            let proven = group.may_be_empty(rels, snapshot);
            LevelQuals {
                may_be_empty: inherited || proven.whole,
                excluded: proven.aliases,
                items: std::mem::take(&mut group.aliases),
                may_be_dummy: (parent.may_be_dummy && next != NextLevel::Subplan)
                    || may_be_empty(&dummy_quals, snapshot),
                conjuncts: conjuncts.len(),
                rels,
                pulled_up,
                select: match level {
                    Level::Select(sel) => Some(std::ptr::from_ref(sel)),
                    Level::Dml { .. } => None,
                },
                from: Some(std::ptr::from_ref(own_from)),
                row_marks,
                outputs,
                item,
                ctes: cte_names,
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
    /// The quals the planner distributes over the level's join tree, each
    /// with where it comes from (see [`Owner`]) and whether it is one of
    /// the top join domain's (WHERE, an inner join's ON, never below the
    /// nullable side of an outer join nor in a semi-join's condition).
    quals: Vec<(protobuf::Node, Owner, bool)>,
    /// The aliases of the level's own FROM items (tables, subqueries,
    /// functions — not joins), which a column proves empty.
    aliases: Vec<String>,
    /// The tables among them.
    tables: Vec<crate::oid::PgClassOid>,
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
    /// The location of the target of the UPDATE or DELETE the level is
    /// (one of its FROM items): a table, never a CTE of its name.
    target: Option<i32>,
    /// The top-level AND arguments of the ON clauses of the level's own
    /// inner joins never null-extended, whose sublinks `pull_up_sublinks`
    /// may turn into semi-joins (see [`Self::walk`]).
    on_quals: Vec<protobuf::Node>,
    /// The aliases of the level's own FROM items on the nullable side of
    /// an outer join.
    nullable: Vec<String>,
    /// The level is one of its own, whose outer joins no qual of a level
    /// around may reduce (see [`Self::null_extended`]).
    own_level: bool,
}

impl Group {
    /// The quals `where_clause` and FROM items `from` of a query level (or
    /// of a subquery pulled up into it), in the top join domain when
    /// `top`.
    fn walk(
        &mut self,
        where_clause: Option<&protobuf::Node>,
        from: &[protobuf::Node],
        ctes: &[Cte<'_>],
        snapshot: &PgCatalog,
        owner: &Owner,
        top: bool,
    ) {
        if let Some(w) = where_clause {
            let mut quals = Vec::new();
            flatten_and(w, &mut quals);
            self.quals
                .extend(quals.into_iter().map(|q| (q.clone(), owner.clone(), top)));
        }
        let on_quals = self.on_quals.len();
        for f in from {
            self.item(f, true, ctes, snapshot, owner, top);
        }
        if !matches!(owner, Owner::Level) {
            return;
        }
        // The IN sublinks `pull_up_sublinks` turns into semi-joins: their
        // subqueries join the level (any qual in them may prove empty
        // what can't be told).
        let mut quals = Vec::new();
        if let Some(w) = where_clause {
            flatten_and(w, &mut quals);
        }
        let mut joined: Vec<protobuf::Node> = quals.into_iter().cloned().collect();
        // Those of a kept inner join's ON clause too, above that join —
        // whose own quals don't reach their relations: they are not
        // counted for the level.
        let on_start = joined.len();
        joined.extend(self.on_quals.drain(on_quals..));
        let owner = Owner::SemiJoin;
        for (i, q) in joined.iter().enumerate() {
            let Some(sub) = conjunct_sublink(q) else {
                continue;
            };
            let Some(node::Node::SelectStmt(sel)) =
                sub.subselect.as_deref().and_then(|s| s.node.as_ref())
            else {
                continue;
            };
            let rels = self.rels;
            let level = |cr: &protobuf::ColumnRef| self.reads_level(cr, snapshot);
            if joined_any_sublink(sub, snapshot, &level) {
                self.subquery(sel, true, ctes, snapshot, &owner, false);
            } else if joined_exists_sublink(sub, snapshot, &level) {
                // Its FROM items join the level, its WHERE is their
                // join's qual.
                self.walk(
                    sel.where_clause.as_deref(),
                    &sel.from_clause,
                    ctes,
                    snapshot,
                    &owner,
                    false,
                );
            }
            if i >= on_start {
                self.rels = rels;
            }
        }
    }

    /// Whether column `cr` certainly is one of the level's: qualified by
    /// one of its FROM items, or (unqualified) a column of one of its
    /// tables.
    fn reads_level(&self, cr: &protobuf::ColumnRef, snapshot: &PgCatalog) -> bool {
        let names = extract_names(&cr.fields);
        if names.len() != cr.fields.len() {
            return false;
        }
        match names.as_slice() {
            [t, _] => self.aliases.contains(t),
            [c] => self.tables.iter().any(|r| {
                snapshot
                    .attributes_of(*r)
                    .iter()
                    .any(|a| &a.attname == c && a.attnum > 0)
            }),
            _ => false,
        }
    }

    /// A subquery (or view's query, inlined CTE's) `sel` in FROM, which
    /// may read the CTEs `ctes`, never null-extended when `kept`; its
    /// quals are `owner`'s, in the top join domain when `top`.
    fn subquery(
        &mut self,
        sel: &protobuf::SelectStmt,
        kept: bool,
        ctes: &[Cte<'_>],
        snapshot: &PgCatalog,
        owner: &Owner,
        top: bool,
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
                    owner,
                    top,
                );
            }
            crate::resolve::Plan::Separate | crate::resolve::Plan::Whole => self.rels += 1,
            // Maybe pulled up, with its quals.
            crate::resolve::Plan::Uncertain => self.unknown_quals = true,
        }
    }

    /// FROM item `n`, never null-extended when `kept`, below the FROM
    /// item of the level `owner` says (one of the level's own when
    /// [`Owner::Level`]); kept, it is in the top join domain when `top`.
    fn item(
        &mut self,
        n: &protobuf::Node,
        kept: bool,
        ctes: &[Cte<'_>],
        snapshot: &PgCatalog,
        owner: &Owner,
        top: bool,
    ) {
        let level_own = matches!(owner, Owner::Level);
        // The quals below one of the level's own FROM items are about it.
        let own = |alias: &str, lateral: bool| match owner {
            Owner::Level => Owner::Item {
                alias: alias.to_owned(),
                lateral,
            },
            other => other.clone(),
        };
        let mut record = |alias: &str| {
            if matches!(owner, Owner::Level) {
                self.aliases.push(alias.to_owned());
                if !kept {
                    self.nullable.push(alias.to_owned());
                }
            }
        };
        match n.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => {
                let alias = rv
                    .alias
                    .as_ref()
                    .map_or(rv.relname.as_str(), |a| &a.aliasname);
                record(alias);
                let owner = &own(alias, false);
                if rv.schemaname.is_empty()
                    && self.target != Some(rv.location)
                    && let Some(cte) = ctes.iter().rev().find(|c| c.name == rv.relname)
                {
                    let body = match cte.def.and_then(|c| c.ctequery.as_deref()?.node.as_ref()) {
                        Some(node::Node::SelectStmt(sel)) => Some(sel),
                        _ => None,
                    };
                    if cte.inlined
                        && let Some(sel) = body
                        && crate::resolve::plan_of(sel, snapshot) == crate::resolve::Plan::PulledUp
                    {
                        // Inlined, a FROM subquery the planner pulls up
                        // (its body sees the CTEs before it, by name).
                        let names: Vec<Cte<'_>> = ctes
                            .iter()
                            .map(|c| Cte {
                                name: c.name,
                                def: None,
                                inlined: false,
                                scanned: false,
                            })
                            .collect();
                        self.subquery(sel, kept, &names, snapshot, owner, top);
                    } else if cte.scanned || cte.def.is_some_and(|c| scanned_alone(c, snapshot)) {
                        if kept {
                            self.rels += 1;
                        }
                    } else {
                        // An inlined CTE may be a pulled-up subquery.
                        self.unknown_quals |= kept;
                        self.substitutes |= kept;
                    }
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
                        if level_own {
                            self.tables.push(class.oid);
                        }
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
                            self.subquery(&sel, kept, &[], snapshot, owner, top);
                            self.views.pop();
                        }
                        _ => self.unknown_quals = true,
                    },
                    _ => self.unknown_quals |= kept,
                }
            }
            Some(node::Node::RangeSubselect(sub)) => {
                // (An unaliased subquery is named by nothing.)
                let alias = sub.alias.as_ref().map_or("", |a| a.aliasname.as_str());
                record(alias);
                let owner = &own(alias, sub.lateral);
                if let Some(node::Node::SelectStmt(sel)) =
                    sub.subquery.as_deref().and_then(|q| q.node.as_ref())
                {
                    self.subquery(sel, kept, ctes, snapshot, owner, top);
                }
            }
            Some(node::Node::JoinExpr(j)) => {
                let inner = j.jointype == protobuf::JoinType::JoinInner as i32;
                let (left, right) = match protobuf::JoinType::try_from(j.jointype) {
                    Ok(protobuf::JoinType::JoinInner) => (kept, kept),
                    Ok(protobuf::JoinType::JoinLeft) => (kept, false),
                    Ok(protobuf::JoinType::JoinRight) => (false, kept),
                    _ => (false, false),
                };
                if let Some(l) = j.larg.as_deref() {
                    self.item(l, left, ctes, snapshot, owner, top);
                }
                if let Some(r) = j.rarg.as_deref() {
                    self.item(r, right, ctes, snapshot, owner, top);
                }
                if let Some(q) = j.quals.as_deref() {
                    let mut quals = Vec::new();
                    flatten_and(q, &mut quals);
                    // An outer join's quals are its nullable side's join
                    // domain's.
                    let in_top = top && kept && inner;
                    if inner && kept && level_own {
                        self.on_quals.extend(quals.iter().map(|q| (*q).clone()));
                    }
                    self.quals.extend(
                        quals
                            .into_iter()
                            .map(|q| (q.clone(), owner.clone(), in_top)),
                    );
                }
            }
            // A function the planner may inline as a subquery it pulls up.
            Some(node::Node::RangeFunction(rf)) => {
                if let Some(a) = rf.alias.as_ref() {
                    record(&a.aliasname);
                }
                self.substitutes |= kept;
            }
            _ => {}
        }
    }

    /// Which relations of the level the planner may prove empty: a qual
    /// over a column proves it of that column's relation only (`col IS
    /// NULL` over a NOT NULL column, two constants one equivalence class
    /// equates — of a single relation, or else a gating qual). A qual
    /// without columns that may fold to a constant other than TRUE is
    /// distributed to every relation of the level
    /// (`distribute_qual_to_rels`): with a single one, it is that
    /// relation's restriction, which proves it empty
    /// (`relation_excluded_by_constraints`); with several, it is a gating
    /// qual above their join, and each is still estimated.
    fn may_be_empty(&self, rels: usize, snapshot: &PgCatalog) -> Proven {
        // An outer join's ON qual (or one below its nullable side) is
        // attached to that join (`distribute_qual_to_rels`, `ojscope`):
        // a constant there gates the join, never the level's relations.
        let level_wide = |owner: &Owner, top: bool| top || matches!(owner, Owner::SemiJoin);
        let constant = self.unknown_quals
            || (self.substitutes && self.quals.iter().any(|(_, o, top)| level_wide(o, *top)))
            || self.quals.iter().any(|(q, o, top)| {
                let folded = fold_qual(q, snapshot);
                level_wide(o, *top) && folded.may_be_const() && folded != Folded::Bool(true)
            });
        let column_quals: Vec<(&protobuf::Node, &Owner, bool)> = self
            .quals
            .iter()
            .filter(|(q, _, _)| !fold_qual(q, snapshot).may_be_const())
            .map(|(q, o, top)| (q, o, *top))
            .collect();
        let mut proven = Proven::default();
        let mut equalities = Vec::new();
        for (q, owner, top) in &column_quals {
            match q.node.as_ref() {
                Some(node::Node::NullTest(t))
                    if t.nulltesttype == protobuf::NullTestType::IsNull as i32
                        && matches!(
                            t.arg.as_deref().and_then(|a| a.node.as_ref()),
                            Some(node::Node::ColumnRef(_))
                        ) =>
                {
                    if !(*top && matches!(owner, Owner::Level) && self.null_extended(t)) {
                        self.prove_about(q, owner, &mut proven);
                    }
                }
                // Two constants an equivalence class of the top join domain
                // equates make a variable-free qual applied over the
                // domain (`generate_base_implied_equalities_const`,
                // `process_implied_equality`): of several relations, a
                // gating qual above their join, which proves none of them
                // empty.
                Some(node::Node::AExpr(e))
                    if extract_names(&e.name) == ["="] && !(*top && rels >= 2) =>
                {
                    equalities.push((*q, *owner));
                }
                _ => {}
            }
        }
        // An equivalence class's members may be any equality's columns.
        let quals: Vec<&protobuf::Node> = equalities.iter().map(|(q, _)| *q).collect();
        if equates_constants(&quals, snapshot) {
            for (q, owner) in equalities {
                self.prove_about(q, owner, &mut proven);
            }
        }
        proven.whole |= constant && rels < 2;
        proven
    }

    /// Whether `t`, an `alias.col IS NULL` qual of the level's top join
    /// domain, tests a column of a FROM item certainly on the nullable
    /// side of an outer join there: then it is no restriction of that
    /// item's relation (the column, null-extended, may be NULL), and
    /// proves nothing empty. Not when another qual of the domain reads
    /// such an item (or an unqualified column): strict, it may reduce the
    /// outer join to an inner one (`reduce_outer_joins`).
    fn null_extended(&self, t: &protobuf::NullTest) -> bool {
        let Some(node::Node::ColumnRef(cr)) = t.arg.as_deref().and_then(|a| a.node.as_ref()) else {
            return false;
        };
        let names = extract_names(&cr.fields);
        let [alias, _] = names.as_slice() else {
            return false;
        };
        if !self.own_level || names.len() != cr.fields.len() || !self.nullable.contains(alias) {
            return false;
        }
        let tested = |q: &protobuf::Node| {
            matches!(q.node.as_ref(), Some(node::Node::NullTest(t))
                if t.nulltesttype == protobuf::NullTestType::IsNull as i32
                    && matches!(t.arg.as_deref().and_then(|a| a.node.as_ref()),
                        Some(node::Node::ColumnRef(_))))
        };
        !self.quals.iter().any(|(q, _, top)| {
            *top && !tested(q)
                && q.node.as_ref().is_some_and(|q| {
                    q.nodes().into_iter().any(|(n, _)| match n {
                        typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                            let names = extract_names(&cr.fields);
                            match names.as_slice() {
                                [t, _] if names.len() == cr.fields.len() => {
                                    self.nullable.contains(t)
                                }
                                _ => true,
                            }
                        }
                        _ => false,
                    })
                })
        })
    }

    /// Record in `proven` that a qual `n` from `owner` may prove the
    /// relations of the columns it reads empty: below one of the level's
    /// FROM items, that item (and, under LATERAL, the level's items it
    /// names); in the level's own quals, the item a column names — any,
    /// when one can't be told (an unqualified column, a join's alias).
    fn prove_about(&self, n: &protobuf::Node, owner: &Owner, proven: &mut Proven) {
        let (outer, lateral) = match owner {
            Owner::Level => (true, false),
            Owner::Item { alias, lateral } => {
                // An unaliased subquery's columns are named by no alias.
                if alias.is_empty() {
                    proven.whole = true;
                    return;
                }
                proven.aliases.push(alias.clone());
                (false, *lateral)
            }
            // Only what it reads of the level's FROM items.
            Owner::SemiJoin => (false, true),
        };
        if !outer && !lateral {
            return;
        }
        let Some(inner) = n.node.as_ref() else {
            return;
        };
        let mut todo = vec![inner.to_ref()];
        while let Some(n) = todo.pop() {
            if let typedpg_pg_query::NodeRef::ColumnRef(cr) = n {
                let names = extract_names(&cr.fields);
                match names.as_slice() {
                    [t, _] if names.len() == cr.fields.len() && self.aliases.contains(t) => {
                        proven.aliases.push(t.clone());
                    }
                    // Below a LATERAL item, a column naming none of the
                    // level's items is the item's own.
                    [_, _] if names.len() == cr.fields.len() && !outer => {}
                    // A join's alias, or an unqualified column.
                    _ => proven.whole = true,
                }
            }
            todo.extend(n.children());
        }
    }
}

/// The CTEs a level may read: those of an outer WITH `outer` (known by
/// name only), and those of `with`, the WITH clause of statement `root`
/// (whose target, when a data-modifying one, is `target`).
fn cte_list<'a>(
    outer: impl IntoIterator<Item = &'a String>,
    with: Option<&'a protobuf::WithClause>,
    root: typedpg_pg_query::NodeRef<'_>,
    target: Option<&protobuf::RangeVar>,
    snapshot: &PgCatalog,
) -> Vec<Cte<'a>> {
    let mut names: Vec<Cte<'a>> = outer
        .into_iter()
        .map(|n| Cte {
            name: n.as_str(),
            def: None,
            inlined: false,
            scanned: false,
        })
        .collect();
    let Some(with) = with else {
        return names;
    };
    for c in &with.ctes {
        let Some(node::Node::CommonTableExpr(c)) = c.node.as_ref() else {
            continue;
        };
        // Referenced once (`cterefcount`, counted over the statement
        // owning the WITH, but a data-modifying one's target), it is
        // inlined.
        let target_named =
            target.is_some_and(|t| t.schemaname.is_empty() && t.relname == c.ctename);
        let references = crate::resolve::references_in(root, Some(with), &c.ctename)
            .map(|n| n.saturating_sub(usize::from(target_named)));
        let inlined = crate::resolve::inlined(c, false, references, snapshot);
        names.push(Cte {
            name: c.ctename.as_str(),
            def: Some(c),
            inlined: !with.recursive && inlined == Some(true),
            scanned: inlined == Some(false),
        });
    }
    names
}

/// A CTE a level may read.
#[derive(Clone, Copy)]
struct Cte<'a> {
    name: &'a str,
    /// Its definition, when known.
    def: Option<&'a protobuf::CommonTableExpr>,
    /// The planner certainly inlines it (see [`crate::resolve::inlined`]).
    inlined: bool,
    /// The planner certainly doesn't: a reference to it is a CTE scan.
    scanned: bool,
}

/// Whether a reference to CTE `cte` is a relation of its own, never pulled
/// up: a CTE scan (MATERIALIZED, data-modifying), or a subquery the planner
/// plans on its own once inlined.
fn scanned_alone(cte: &protobuf::CommonTableExpr, snapshot: &PgCatalog) -> bool {
    match cte.ctequery.as_deref().and_then(|q| q.node.as_ref()) {
        Some(node::Node::SelectStmt(sel)) => {
            cte.ctematerialized == protobuf::CteMaterialize::Always as i32
                || crate::resolve::plan_of(sel, snapshot) == crate::resolve::Plan::Separate
        }
        Some(_) => true,
        None => false,
    }
}

/// Where a qual of a level comes from.
#[derive(Debug, Clone, Default)]
enum Owner {
    /// The level's own WHERE or JOIN ON clauses.
    #[default]
    Level,
    /// A FROM subquery or view of the level, `alias`, pulled up into it
    /// (`lateral` when it may read the level's other items).
    Item { alias: String, lateral: bool },
    /// A sublink of the level `pull_up_sublinks` turns into a semi-join:
    /// its relations are named by nothing of the level, and its own level
    /// tells what its quals prove of them.
    SemiJoin,
}

/// What a level's quals may prove empty.
#[derive(Debug, Default)]
struct Proven {
    /// Any of its relations.
    whole: bool,
    /// These FROM items.
    aliases: Vec<String>,
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
    for c in conjuncts {
        let folded = fold_qual(c, snapshot);
        if folded.may_be_const() && folded != Folded::Bool(true) {
            return true;
        }
        if let Some(node::Node::NullTest(t)) = c.node.as_ref()
            && t.nulltesttype == protobuf::NullTestType::IsNull as i32
            && matches!(
                t.arg.as_deref().and_then(|a| a.node.as_ref()),
                Some(node::Node::ColumnRef(_))
            )
        {
            return true;
        }
    }
    equates_constants(conjuncts, snapshot)
}

/// Whether two `expr = constant` of `conjuncts` may put two different
/// constants in one equivalence class.
fn equates_constants(conjuncts: &[&protobuf::Node], snapshot: &PgCatalog) -> bool {
    let mut equated: Vec<Option<String>> = Vec::new();
    for c in conjuncts {
        match c.node.as_ref() {
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

/// Whether the planner may prove a relation of the current level empty.
pub(crate) fn level_may_be_empty() -> bool {
    LEVELS.with(|l| {
        l.borrow()
            .last()
            .is_some_and(|q| q.may_be_empty || !q.excluded.is_empty())
    })
}

/// Whether the planner may prove the current level's FROM item `alias`
/// empty (or its level's every relation).
pub(crate) fn level_may_exclude(alias: &str) -> bool {
    LEVELS.with(|l| l.borrow().last().is_some_and(|q| q.excludes(alias)))
}

impl LevelQuals {
    /// Whether the planner may prove the level's FROM item `alias` empty.
    fn excludes(&self, alias: &str) -> bool {
        self.may_be_empty
            || self.excluded.iter().any(|a| a == alias)
            || (!self.excluded.is_empty() && !self.items.iter().any(|a| a == alias))
    }
}

/// Whether the current level certainly reads a column of its FROM item
/// `alias` above `join`, the outer join whose nullable side it is:
/// `join_is_removable` then keeps the join (the column is needed above
/// it, `attr_needed`), whose nullable side is planned and its
/// restrictions estimated. See [`Reads::above`].
pub(crate) fn read_above(alias: &str, join: &protobuf::JoinExpr, snapshot: &PgCatalog) -> bool {
    LEVELS.with(|l| {
        let l = l.borrow();
        let Some((last, rest)) = l.split_last() else {
            return false;
        };
        // SAFETY: the level's nodes are borrowed by the analysis while its
        // `PlanLevel` lives, and it is on the stack.
        let Some(reads) = (unsafe { last.reads(rest.last(), snapshot) }) else {
            return false;
        };
        reads.above(alias, join, 0)
    })
}

/// Whether join `j` certainly equates its sides: NATURAL, USING, or an
/// `=` in its ON clause. Without one, nothing proves a table on its
/// nullable side unique for it (`rel_is_distinct_for` needs a
/// mergejoinable equality, of the join or of the relation's own
/// restrictions — both in the ON clause here).
pub(crate) fn join_equates(j: &protobuf::JoinExpr) -> bool {
    j.is_natural
        || !j.using_clause.is_empty()
        || j.quals
            .as_deref()
            .and_then(|q| q.node.as_ref())
            .is_some_and(|q| {
                q.nodes().into_iter().any(|(n, _)| {
                    matches!(n, typedpg_pg_query::NodeRef::AExpr(e)
                        if extract_names(&e.name).last().is_some_and(|o| o == "="))
                })
            })
}

/// Whether join `j` may prove table `relid`, its nullable side `alias`,
/// unique for it (`rel_is_distinct_for`): a unique index each key column
/// of which the join equates (`alias.col = …`, an unqualified `col = …`,
/// USING or NATURAL), or one over an expression.
pub(crate) fn join_may_prove_unique(
    j: &protobuf::JoinExpr,
    alias: &str,
    relid: crate::oid::PgClassOid,
    snapshot: &PgCatalog,
) -> bool {
    let using = extract_names(&j.using_clause);
    let mut equated: Vec<String> = Vec::new();
    if let Some(q) = j.quals.as_deref().and_then(|q| q.node.as_ref()) {
        for (n, _) in q.nodes() {
            let typedpg_pg_query::NodeRef::AExpr(e) = n else {
                continue;
            };
            if extract_names(&e.name).last().is_none_or(|o| o != "=") {
                continue;
            }
            for side in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                if let Some(node::Node::ColumnRef(cr)) = side.node.as_ref() {
                    let names = extract_names(&cr.fields);
                    if names.len() != cr.fields.len() {
                        continue;
                    }
                    match names.as_slice() {
                        [t, c] if t == alias => equated.push(c.clone()),
                        [c] => equated.push(c.clone()),
                        _ => {}
                    }
                }
            }
        }
    }
    let attrs = snapshot.attributes_of(relid);
    snapshot
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid && i.indisunique)
        .any(|i| {
            i.indkey
                .iter()
                .take(usize::try_from(i.indnkeyatts).unwrap_or(0))
                .all(|&k| {
                    k == 0
                        || j.is_natural
                        || attrs.iter().find(|a| a.attnum == k).is_none_or(|a| {
                            using.contains(&a.attname) || equated.contains(&a.attname)
                        })
                })
        })
}

/// How the select list of a query level is used by what reads it (see
/// [`with_next_outputs`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Outputs {
    /// Every output is needed: a statement's, a scalar, ARRAY or IN
    /// sublink's, a CTE planned whole (a CTE scan).
    #[default]
    Needed,
    /// A FROM subquery's (a view's, an inlined CTE's): an output the level
    /// around it doesn't read is needed by nothing — pulled up, nothing
    /// references it; planned on its own, `remove_unused_subquery_outputs`
    /// replaces it with a NULL.
    Trimmed,
    /// An EXISTS sublink's: `simplify_EXISTS_query` throws its select list
    /// away, with its GROUP BY, WINDOW, DISTINCT and ORDER BY clauses.
    Exists,
}

thread_local! {
    /// How the next query level's select list is used (see
    /// [`with_next_outputs`]).
    static NEXT_OUTPUTS: Cell<Option<Outputs>> = const { Cell::new(None) };
}

thread_local! {
    /// The next query level is a sublink's `pull_up_sublinks` turns into a
    /// semi-join (see [`with_next_semijoin`]).
    static NEXT_SEMIJOIN: Cell<bool> = const { Cell::new(false) };
}

/// Run `f`, the analysis of the query of a sublink `pull_up_sublinks`
/// turns into a semi-join of the level.
pub(crate) fn with_next_semijoin<R>(f: impl FnOnce() -> R) -> R {
    let prev = NEXT_SEMIJOIN.with(|o| o.replace(true));
    let out = f();
    NEXT_SEMIJOIN.with(|o| o.set(prev));
    out
}

/// Run `f`, the analysis of a query level whose select list is used as
/// `outputs` says.
pub(crate) fn with_next_outputs<R>(outputs: Outputs, f: impl FnOnce() -> R) -> R {
    let prev = NEXT_OUTPUTS.with(|o| o.replace(Some(outputs)));
    let out = f();
    NEXT_OUTPUTS.with(|o| o.set(prev));
    out
}

/// A column reference the read checks look for: `alias.column` (any
/// column of `alias` when `column` is `None`), or `alias.*`.
#[derive(Clone, Copy)]
struct Target<'a> {
    alias: &'a str,
    column: Option<&'a str>,
}

impl Target<'_> {
    fn matches(&self, cr: &protobuf::ColumnRef) -> bool {
        let [a, c] = cr.fields.as_slice() else {
            return false;
        };
        matches!(a.node.as_ref(), Some(node::Node::String(a)) if a.sval == self.alias)
            && match c.node.as_ref() {
                Some(node::Node::String(c)) => self.column.is_none_or(|col| c.sval == col),
                Some(node::Node::AStar(_)) => true,
                _ => false,
            }
    }
}

/// Whether expression `n` certainly reads `target` once the planner
/// folded it (`eval_const_expressions`): a reference no constant folding
/// can drop — none below an AND / OR beside an argument that may fold to
/// a constant (but the one that leaves the others as they are), in a CASE,
/// in a COALESCE after an argument that may fold to a constant other than
/// NULL, or beside a NULL argument (or a parameter, which a custom plan
/// may make a NULL constant) of an operator or function
/// (which may be strict, folded to NULL). In a sublink, a reference to a
/// level outside it counts where the sublink keeps it (see
/// [`sublink_reads`]); none in a FROM subquery, whose outputs may be
/// trimmed.
fn certainly_reads(n: &protobuf::Node, target: &Target<'_>, snapshot: &PgCatalog) -> bool {
    let Some(inner) = n.node.as_ref() else {
        return false;
    };
    !fold(n, snapshot).known() && reads_ref(inner.to_ref(), target, snapshot)
}

fn reads_ref(n: typedpg_pg_query::NodeRef<'_>, target: &Target<'_>, snapshot: &PgCatalog) -> bool {
    use typedpg_pg_query::NodeRef as R;
    match n {
        R::ColumnRef(cr) => target.matches(cr),
        R::SubLink(sub) => {
            sub.testexpr
                .as_deref()
                .is_some_and(|t| certainly_reads(t, target, snapshot))
                || matches!(sub.subselect.as_deref().and_then(|s| s.node.as_ref()),
                Some(node::Node::SelectStmt(sel)) if sublink_reads(
                    sel,
                    sub.sub_link_type == protobuf::SubLinkType::ExistsSublink as i32,
                    target,
                    snapshot,
                ))
        }
        R::SelectStmt(_) | R::RangeSubselect(_) | R::CaseExpr(_) => false,
        // A NULL argument is dropped, and so is every one after an
        // argument that may fold to another constant.
        R::CoalesceExpr(c) => {
            for a in &c.args {
                match fold(a, snapshot) {
                    Folded::Null => {}
                    Folded::Var => {
                        if certainly_reads(a, target, snapshot) {
                            return true;
                        }
                    }
                    _ => return false,
                }
            }
            false
        }
        R::BoolExpr(b) => {
            let identity = match protobuf::BoolExprType::try_from(b.boolop) {
                Ok(protobuf::BoolExprType::AndExpr) => Some(Folded::Bool(true)),
                Ok(protobuf::BoolExprType::OrExpr) => Some(Folded::Bool(false)),
                Ok(protobuf::BoolExprType::NotExpr) => None,
                _ => return false,
            };
            if identity.is_some()
                && b.args.iter().any(|a| {
                    let f = fold(a, snapshot);
                    (f != Folded::Var && Some(f) != identity) || params_only(a)
                })
            {
                return false;
            }
            b.args.iter().any(|a| certainly_reads(a, target, snapshot))
        }
        R::AExpr(e) => {
            let mut operands: Vec<&protobuf::Node> = Vec::new();
            for side in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                match side.node.as_ref() {
                    Some(node::Node::List(l)) => operands.extend(l.items.iter()),
                    _ => operands.push(side),
                }
            }
            !operands
                .iter()
                .any(|o| fold(o, snapshot) == Folded::Null || params_only(o))
                && operands
                    .iter()
                    .any(|o| certainly_reads(o, target, snapshot))
        }
        R::FuncCall(fc) => {
            !fc.args
                .iter()
                .any(|a| fold(a, snapshot) == Folded::Null || params_only(a))
                && n.children()
                    .into_iter()
                    .any(|c| reads_ref(c, target, snapshot))
        }
        _ => n
            .children()
            .into_iter()
            .any(|c| reads_ref(c, target, snapshot)),
    }
}

/// Whether `n` reads parameters but no column nor sublink: a custom plan
/// has them as constants (NULL ones, maybe), which it folds.
fn params_only(n: &protobuf::Node) -> bool {
    let Some(inner) = n.node.as_ref() else {
        return false;
    };
    let nodes = inner.nodes();
    nodes
        .iter()
        .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::ParamRef(_)))
        && !nodes.iter().any(|(n, _)| {
            matches!(
                n,
                typedpg_pg_query::NodeRef::ColumnRef(_) | typedpg_pg_query::NodeRef::SubLink(_)
            )
        })
}

/// Whether the query of a sublink, `sel` (an EXISTS's when `exists`),
/// certainly reads `target`, of a level outside it: in its WHERE, HAVING,
/// the ON clause of a join never removed, or — but an EXISTS's, which
/// `simplify_EXISTS_query` throws away — its select list. Not when one of
/// its FROM items may be named `target.alias` (hiding the outer one).
fn sublink_reads(
    sel: &protobuf::SelectStmt,
    exists: bool,
    target: &Target<'_>,
    snapshot: &PgCatalog,
) -> bool {
    if sel.op != protobuf::SetOperation::SetopNone as i32 || !sel.values_lists.is_empty() {
        return false;
    }
    fn hides(n: &protobuf::Node, alias: &str) -> bool {
        match n.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => {
                rv.alias
                    .as_ref()
                    .map_or(rv.relname.as_str(), |a| &a.aliasname)
                    == alias
            }
            Some(node::Node::RangeSubselect(s)) => {
                s.alias.as_ref().is_some_and(|a| a.aliasname == alias)
            }
            Some(node::Node::RangeFunction(f)) => {
                f.alias.as_ref().is_none_or(|a| a.aliasname == alias)
            }
            Some(node::Node::JoinExpr(j)) => {
                j.alias.as_ref().is_some_and(|a| a.aliasname == alias)
                    || [&j.larg, &j.rarg]
                        .into_iter()
                        .flatten()
                        .any(|s| hides(s, alias))
            }
            // Anything else may be named so.
            _ => true,
        }
    }
    if sel.from_clause.iter().any(|f| hides(f, target.alias)) {
        return false;
    }
    // The ON clauses of its joins no join removal drops.
    fn on_reads(n: &protobuf::Node, target: &Target<'_>, snapshot: &PgCatalog) -> bool {
        let Some(node::Node::JoinExpr(j)) = n.node.as_ref() else {
            return false;
        };
        let (quals, left, right) = match protobuf::JoinType::try_from(j.jointype) {
            Ok(protobuf::JoinType::JoinInner | protobuf::JoinType::JoinFull) => (true, true, true),
            Ok(protobuf::JoinType::JoinLeft) => (false, true, false),
            Ok(protobuf::JoinType::JoinRight) => (false, false, true),
            _ => return false,
        };
        (quals
            && j.quals
                .as_deref()
                .is_some_and(|q| certainly_reads(q, target, snapshot)))
            || (left
                && j.larg
                    .as_deref()
                    .is_some_and(|s| on_reads(s, target, snapshot)))
            || (right
                && j.rarg
                    .as_deref()
                    .is_some_and(|s| on_reads(s, target, snapshot)))
    }
    sel.where_clause
        .as_deref()
        .into_iter()
        .chain(sel.having_clause.as_deref())
        .chain(sel.target_list.iter().filter(|_| !exists))
        .any(|c| certainly_reads(c, target, snapshot))
        || sel
            .from_clause
            .iter()
            .any(|f| on_reads(f, target, snapshot))
}

/// What a query level's read checks look at (see [`LevelQuals::reads`]).
#[derive(Clone, Copy)]
struct Reads<'a> {
    /// The level's SELECT, when it is one.
    select: Option<&'a protobuf::SelectStmt>,
    /// Its FROM clause (an UPDATE's or DELETE's with its target).
    from: &'a [protobuf::Node],
    row_marks: bool,
    outputs: Outputs,
    pulled_up: bool,
    /// The alias of the FROM item it is in the level around it, `parent`.
    item: Option<&'a str>,
    parent: Option<&'a LevelQuals>,
    /// The CTEs it may read, by name.
    ctes: &'a [String],
    snapshot: &'a PgCatalog,
}

impl LevelQuals {
    /// The level's [`Reads`], under the level around it, `parent`.
    ///
    /// # Safety
    /// The level's `select` and `from` must still be borrowed by the
    /// analysis (its `PlanLevel` lives), and so must `parent`'s.
    unsafe fn reads<'a>(
        &'a self,
        parent: Option<&'a LevelQuals>,
        snapshot: &'a PgCatalog,
    ) -> Option<Reads<'a>> {
        // SAFETY: the caller's.
        let from = unsafe { &*self.from? };
        let select = self.select.map(|s| unsafe { &*s });
        Some(Reads {
            select,
            from,
            row_marks: self.row_marks,
            outputs: self.outputs,
            pulled_up: self.pulled_up,
            item: self.item.as_deref(),
            parent,
            ctes: &self.ctes,
            snapshot,
        })
    }
}

impl Reads<'_> {
    /// Whether the level certainly reads a column of its FROM item `alias`
    /// above `join`, the outer join whose nullable side it is (see
    /// [`read_above`]): every relation, when it has row marks (their row
    /// identity is needed, `preprocess_targetlist`); else a reference to
    /// it no constant folding drops (see [`certainly_reads`]) in its WHERE,
    /// HAVING, GROUP BY, ORDER BY or DISTINCT ON, in its select list or
    /// WINDOW clause where those are kept (see [`Outputs`]), or in the ON
    /// clause of another join that is never removed — an inner join, or an
    /// outer join whose nullable side is kept (see [`Self::side_kept`]).
    /// Only a reference qualified by the alias counts (`alias.col`,
    /// `alias.*`), or a `*` of the select list. `depth` bounds the
    /// recursion through other joins' nullable sides.
    fn above(&self, alias: &str, join: *const protobuf::JoinExpr, depth: usize) -> bool {
        fn holds(n: &protobuf::Node, join: *const protobuf::JoinExpr) -> bool {
            let Some(node::Node::JoinExpr(j)) = n.node.as_ref() else {
                return false;
            };
            std::ptr::eq(&**j, join)
                || [&j.larg, &j.rarg]
                    .into_iter()
                    .flatten()
                    .any(|s| holds(s, join))
        }
        // A join the level's FROM clause doesn't hold (a view's, a CTE's):
        // nothing is known.
        if !self.from.iter().any(|f| holds(f, join)) {
            return false;
        }
        if self.row_marks {
            return true;
        }
        let target = Target {
            alias,
            column: None,
        };
        self.from
            .iter()
            .any(|f| self.join_reads(f, join, &target, depth))
            || self
                .select
                .is_some_and(|sel| self.clauses_read(sel, &target))
    }

    /// Whether the ON clause of a join in FROM item `n` — but `join` and
    /// what is below it — certainly reads `target`, the join being never
    /// removed.
    fn join_reads(
        &self,
        n: &protobuf::Node,
        join: *const protobuf::JoinExpr,
        target: &Target<'_>,
        depth: usize,
    ) -> bool {
        let Some(node::Node::JoinExpr(j)) = n.node.as_ref() else {
            return false;
        };
        if std::ptr::eq(&**j, join) {
            return false;
        }
        let reads = || {
            j.quals
                .as_deref()
                .is_some_and(|q| certainly_reads(q, target, self.snapshot))
        };
        let kept = || match protobuf::JoinType::try_from(j.jointype) {
            Ok(protobuf::JoinType::JoinInner | protobuf::JoinType::JoinFull) => true,
            Ok(protobuf::JoinType::JoinLeft) => self.side_kept(j, j.rarg.as_deref(), depth),
            Ok(protobuf::JoinType::JoinRight) => self.side_kept(j, j.larg.as_deref(), depth),
            _ => false,
        };
        (reads() && kept())
            || [&j.larg, &j.rarg]
                .into_iter()
                .flatten()
                .any(|s| self.join_reads(s, join, target, depth))
    }

    /// Whether `remove_useless_joins` certainly keeps outer join `j` of
    /// the level, whose nullable side is `side`: a table the join can't
    /// prove unique (see [`join_may_prove_unique`]), a subquery planned
    /// on its own whose rows may not be distinct, or a FROM item read
    /// above the join.
    fn side_kept(
        &self,
        j: &protobuf::JoinExpr,
        side: Option<&protobuf::Node>,
        depth: usize,
    ) -> bool {
        let alias = match side.and_then(|s| s.node.as_ref()) {
            Some(node::Node::RangeVar(rv)) => {
                let alias = rv
                    .alias
                    .as_ref()
                    .map_or(rv.relname.as_str(), |a| a.aliasname.as_str());
                if self
                    .plain_table(rv)
                    .is_some_and(|relid| !join_may_prove_unique(j, alias, relid, self.snapshot))
                {
                    return true;
                }
                alias
            }
            Some(node::Node::RangeSubselect(s)) => {
                // Planned on its own, a subquery is proven unique only by
                // what makes its rows distinct (`query_is_distinct_for`).
                if let Some(node::Node::SelectStmt(sel)) =
                    s.subquery.as_deref().and_then(|q| q.node.as_ref())
                    && crate::resolve::plan_of(sel, self.snapshot) == crate::resolve::Plan::Separate
                    && !may_be_distinct(sel, self.snapshot)
                {
                    return true;
                }
                match s.alias.as_ref() {
                    Some(a) => a.aliasname.as_str(),
                    None => return false,
                }
            }
            _ => return false,
        };
        depth < 3 && self.above(alias, j, depth + 1)
    }

    /// The relation `rv` names when it certainly is a table (no CTE of
    /// its name).
    fn plain_table(&self, rv: &protobuf::RangeVar) -> Option<crate::oid::PgClassOid> {
        if rv.schemaname.is_empty() && self.ctes.iter().any(|c| c == &rv.relname) {
            return None;
        }
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        let class = self.snapshot.resolve_table(schema, &rv.relname)?;
        matches!(
            class.relkind,
            RelKind::Table
                | RelKind::Partitioned
                | RelKind::MaterializedView
                | RelKind::ForeignTable
        )
        .then_some(class.oid)
    }

    /// Whether the clauses of `sel` (the level's SELECT) certainly read
    /// `target`. The select list counts as [`Outputs`] says: a trimmed
    /// one's entry only where the level around it reads that output. A
    /// WINDOW clause no window call uses is dropped with a subquery
    /// pulled up.
    fn clauses_read(&self, sel: &protobuf::SelectStmt, target: &Target<'_>) -> bool {
        let reads = |n: &protobuf::Node| certainly_reads(n, target, self.snapshot);
        let kept_clauses: &[&[protobuf::Node]] = match self.outputs {
            Outputs::Exists => &[],
            Outputs::Trimmed if self.pulled_up => {
                &[&sel.group_clause, &sel.sort_clause, &sel.distinct_clause]
            }
            _ => &[
                &sel.group_clause,
                &sel.sort_clause,
                &sel.distinct_clause,
                &sel.window_clause,
            ],
        };
        if sel
            .where_clause
            .as_deref()
            .into_iter()
            .chain(sel.having_clause.as_deref())
            .chain(kept_clauses.iter().flat_map(|c| c.iter()))
            .any(reads)
        {
            return true;
        }
        match self.outputs {
            Outputs::Needed => {
                sel.target_list.iter().any(|t| {
                    matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
                        if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
                            Some(node::Node::ColumnRef(cr)) if matches!(cr.fields.as_slice(),
                                [f] if matches!(f.node, Some(node::Node::AStar(_))))))
                }) || sel.target_list.iter().any(reads)
            }
            Outputs::Trimmed => sel
                .target_list
                .iter()
                .enumerate()
                .any(|(i, t)| reads(t) && self.output_read(sel, i)),
            Outputs::Exists => false,
        }
    }

    /// Whether the level around this one certainly reads output `i` of
    /// `sel`, the FROM item [`Self::item`] it is there (named as that
    /// item's column aliases, or the CTE's, say), in a clause that keeps
    /// it.
    fn output_read(&self, sel: &protobuf::SelectStmt, i: usize) -> bool {
        let (Some(item), Some(parent)) = (self.item, self.parent) else {
            return false;
        };
        // SAFETY: the level around this one is on the stack, its nodes
        // borrowed by the analysis.
        let Some(outer) = (unsafe { parent.reads(None, self.snapshot) }) else {
            return false;
        };
        let Some(psel) = outer.select else {
            return false;
        };
        // Entry `i` is output `i` when no `*` comes before it.
        if item.is_empty()
            || sel.target_list[..i].iter().any(|t| {
                t.node.as_ref().is_some_and(|t| {
                    t.nodes()
                        .into_iter()
                        .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::AStar(_)))
                })
            })
        {
            return false;
        }
        let Some(node::Node::ResTarget(rt)) = sel.target_list[i].node.as_ref() else {
            return false;
        };
        fn find<'n>(n: &'n protobuf::Node, item: &str) -> Option<&'n protobuf::Node> {
            match n.node.as_ref()? {
                node::Node::RangeVar(rv)
                    if rv
                        .alias
                        .as_ref()
                        .map_or(rv.relname.as_str(), |a| &a.aliasname)
                        == item =>
                {
                    Some(n)
                }
                node::Node::RangeSubselect(s)
                    if s.alias.as_ref().is_some_and(|a| a.aliasname == item) =>
                {
                    Some(n)
                }
                node::Node::JoinExpr(j) => [&j.larg, &j.rarg]
                    .into_iter()
                    .flatten()
                    .find_map(|s| find(s, item)),
                _ => None,
            }
        }
        let strings = |names: &[protobuf::Node]| -> Vec<String> { extract_names(names) };
        let renames = match psel
            .from_clause
            .iter()
            .find_map(|f| find(f, item))
            .and_then(|n| n.node.as_ref())
        {
            Some(node::Node::RangeSubselect(s)) => s
                .alias
                .as_ref()
                .map(|a| strings(&a.colnames))
                .unwrap_or_default(),
            Some(node::Node::RangeVar(rv)) => {
                let mut names = rv
                    .alias
                    .as_ref()
                    .map(|a| strings(&a.colnames))
                    .unwrap_or_default();
                if names.len() <= i {
                    // A CTE of the level's own WITH (any other's is unknown).
                    let cte = psel.with_clause.as_ref().and_then(|w| {
                        w.ctes.iter().find_map(|c| match c.node.as_ref() {
                            Some(node::Node::CommonTableExpr(c)) if c.ctename == rv.relname => {
                                Some(c)
                            }
                            _ => None,
                        })
                    });
                    let Some(cte) = cte else {
                        return false;
                    };
                    let own = strings(&cte.aliascolnames);
                    names.extend(own.into_iter().skip(names.len()));
                }
                names
            }
            _ => return false,
        };
        let name = if let Some(n) = renames.get(i) {
            n.clone()
        } else if !rt.name.is_empty() {
            rt.name.clone()
        } else if let Some(node::Node::ColumnRef(cr)) =
            rt.val.as_deref().and_then(|v| v.node.as_ref())
            && let Some(node::Node::String(s)) = cr.fields.last().and_then(|f| f.node.as_ref())
        {
            s.sval.clone()
        } else {
            return false;
        };
        let target = Target {
            alias: item,
            column: Some(&name),
        };
        outer.clauses_read(psel, &target)
            || psel
                .from_clause
                .iter()
                .any(|f| outer.join_reads(f, std::ptr::null(), &target, 0))
    }

    /// How many base relations on the nullable side of the level's outer
    /// joins are certainly kept: a table (or a subquery with a FROM
    /// clause, pulled up or planned on its own) that is a whole side never
    /// removed (see [`Self::side_kept`]), or a side of a FULL join.
    fn held_relations(&self) -> usize {
        fn count(r: &Reads<'_>, n: &protobuf::Node) -> usize {
            let Some(node::Node::JoinExpr(j)) = n.node.as_ref() else {
                return 0;
            };
            let one = |side: Option<&protobuf::Node>| -> usize {
                match side.and_then(|s| s.node.as_ref()) {
                    Some(node::Node::RangeVar(rv)) => usize::from(r.plain_table(rv).is_some()),
                    Some(node::Node::RangeSubselect(s)) => usize::from(matches!(
                        s.subquery.as_deref().and_then(|q| q.node.as_ref()),
                        Some(node::Node::SelectStmt(sel)) if !sel.from_clause.is_empty()
                            && matches!(crate::resolve::plan_of(sel, r.snapshot),
                                crate::resolve::Plan::PulledUp | crate::resolve::Plan::Separate)
                    )),
                    _ => 0,
                }
            };
            let (l, rr) = (j.larg.as_deref(), j.rarg.as_deref());
            let held = match protobuf::JoinType::try_from(j.jointype) {
                Ok(protobuf::JoinType::JoinLeft) if r.side_kept(j, rr, 0) => one(rr),
                Ok(protobuf::JoinType::JoinRight) if r.side_kept(j, l, 0) => one(l),
                Ok(protobuf::JoinType::JoinFull) => one(l) + one(rr),
                _ => 0,
            };
            held + l.map_or(0, |s| count(r, s)) + rr.map_or(0, |s| count(r, s))
        }
        self.from.iter().map(|f| count(self, f)).sum()
    }
}

/// Whether the plan of the current level may be a dummy one, whose nodes
/// the executor never starts.
pub(crate) fn level_may_be_dummy() -> bool {
    LEVELS.with(|l| l.borrow().last().is_some_and(|q| q.may_be_dummy))
}

/// Whether the current level has a qual the planner may push down into a
/// subquery it plans on its own and prove it empty there (see
/// [`PUSHABLE`]).
pub(crate) fn level_may_push_down() -> bool {
    PUSHABLE.with(|p| p.borrow().last().is_some_and(|q| !q.is_empty()))
}

/// Whether the current level is a FROM subquery the planner pulls up
/// (see [`NextLevel::PulledUp`]).
pub(crate) fn level_pulled_up() -> bool {
    LEVELS.with(|l| l.borrow().last().is_some_and(|q| q.pulled_up))
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

/// [`skip_restrictions_on`], collecting what estimating the restrictions
/// of `aliases` raises (see [`record_skipped`]) rather than dropping it:
/// what a reference raises unless the join it is on removes it.
pub(crate) fn record_unless_removed<R>(
    aliases: Vec<String>,
    f: impl FnOnce() -> R,
) -> (R, Vec<super::PlanTimeError>) {
    let prev = UNLESS_REMOVED.with(|s| s.replace(Some(Vec::new())));
    let out = skip_restrictions_on(aliases, f);
    let errors = UNLESS_REMOVED.with(|s| s.replace(prev)).unwrap_or_default();
    (out, errors)
}

/// Whether FROM item `alias`'s restrictions are skipped while
/// [`record_unless_removed`] collects them: `estimated` tells whether
/// they would be estimated otherwise.
pub(crate) fn records_skipped(alias: &str, estimated: impl FnOnce() -> bool) -> bool {
    if UNLESS_REMOVED.with(|s| s.borrow().is_none())
        || SKIP_DEPTH.with(Cell::get) == 0
        || !SKIPPED.with(|s| s.borrow().iter().any(|a| a == alias))
    {
        return false;
    }
    let depth = SKIP_DEPTH.with(|d| d.replace(0));
    let out = estimated();
    SKIP_DEPTH.with(|d| d.set(depth));
    out
}

/// Collect error `e` estimating a skipped restriction (see
/// [`records_skipped`]).
pub(crate) fn record_skipped(location: Option<i32>, e: crate::error::AnalyzeError) {
    UNLESS_REMOVED.with(|s| {
        if let Some(errors) = s.borrow_mut().as_mut() {
            errors.push((location, e));
        }
    });
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
/// one the planner may prove unique: `join_is_removable` needs
/// `rel_supports_distinctness`).
pub(crate) fn may_be_removed(source: &crate::scope::TableSource, snapshot: &PgCatalog) -> bool {
    if let crate::scope::SourceKind::Subquery { removable, .. } = source.kind {
        return removable;
    }
    let Some(relid) = source.relid else {
        return true;
    };
    !matches!(source.kind, crate::scope::SourceKind::Relation)
        || snapshot
            .pg_index
            .values()
            .any(|i| i.indrelid == relid && i.indisunique)
}

/// Whether `query_supports_distinctness` may hold for `sel`, a subquery
/// the planner plans on its own: it has DISTINCT, GROUP BY, HAVING, a set
/// operation — or may call an aggregate (a function some candidate of
/// which is one, or one the analyzer can't find, outside sublinks).
pub(crate) fn may_be_distinct(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> bool {
    if sel.op != protobuf::SetOperation::SetopNone as i32
        || !sel.distinct_clause.is_empty()
        || !sel.group_clause.is_empty()
        || sel.having_clause.is_some()
    {
        return true;
    }
    let mut todo: Vec<typedpg_pg_query::NodeRef<'_>> = sel
        .target_list
        .iter()
        .chain(&sel.sort_clause)
        .filter_map(|n| n.node.as_ref().map(node::Node::to_ref))
        .collect();
    while let Some(n) = todo.pop() {
        match n {
            // A sublink's aggregates are its own query's.
            typedpg_pg_query::NodeRef::SubLink(_) => continue,
            typedpg_pg_query::NodeRef::FuncCall(fc) => {
                let names = extract_names(&fc.funcname);
                let (schema, name) = match names.as_slice() {
                    [n] => (None, n.as_str()),
                    [s, n] => (Some(s.as_str()), n.as_str()),
                    _ => return true,
                };
                let candidates = snapshot.find_functions(schema, name);
                if fc.agg_star
                    || candidates.is_empty()
                    || candidates
                        .iter()
                        .any(|p| p.prokind == crate::pg_catalog::ProKind::Aggregate)
                {
                    return true;
                }
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    false
}
