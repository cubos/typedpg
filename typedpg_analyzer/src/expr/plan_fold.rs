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
use std::rc::Rc;

use typedpg_pg_query::ast;

use crate::node_key::NodeKey;
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

/// What an expression certainly folds to (see [`certainly`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Certain {
    /// A boolean constant.
    Bool(bool),
    /// The NULL constant.
    Null,
    /// A constant other than NULL, of a value the analyzer doesn't compute.
    Value,
    /// A constant whose value — NULL or not — the analyzer doesn't compute:
    /// an immutable function or operator `evaluate_function` folds without
    /// knowing what it returns.
    UnknownValue,
    /// Not certainly a constant: it may stay for execution.
    Not,
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

fn extract_names(nodes: &[Option<ast::Node>]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// Whether every function `fc` may resolve to is one the planner doesn't
/// fold (stable or volatile).
pub(crate) fn never_folded(fc: &ast::FuncCall, snapshot: &PgCatalog) -> bool {
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
pub(crate) fn may_be_volatile(n: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
    let Some(inner) = n else {
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
fn operator_may_be_volatile(name: &[Option<ast::Node>], snapshot: &PgCatalog) -> bool {
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
    sub: &ast::SubLink,
    snapshot: &PgCatalog,
    is_level_column: &dyn Fn(&ast::ColumnRef) -> bool,
) -> bool {
    if sub.sub_link_type != ast::SubLinkType::AnySublink {
        return false;
    }
    let test @ Some(_) = &sub.testexpr else {
        return false;
    };
    if may_be_volatile(test.as_ref(), snapshot)
        || (!sub.oper_name.is_empty() && operator_may_be_volatile(&sub.oper_name, snapshot))
    {
        return false;
    }
    let Some(inner) = test.as_ref() else {
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
    sub: &ast::SubLink,
    snapshot: &PgCatalog,
    is_level_column: &dyn Fn(&ast::ColumnRef) -> bool,
) -> bool {
    if sub.sub_link_type != ast::SubLinkType::ExistsSublink {
        return false;
    }
    let Some(ast::Node::SelectStmt(sel)) = sub.subselect.as_ref() else {
        return false;
    };
    if sel.op != ast::SetOperation::SetopNone
        || !sel.values_lists.is_empty()
        || sel.with_clause.is_some()
        // GROUP BY (WINDOW, DISTINCT, ORDER BY) is thrown away; grouping
        // sets are not.
        || sel
            .group_clause
            .iter()
            .filter_map(|g| g.as_ref())
            .any(|g| {
                g.nodes()
                    .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::GroupingSet(_)))
            })
        || sel.having_clause.is_some()
        || sel.limit_offset.is_some()
        // A positive or NULL (ALL) constant LIMIT is dropped.
        || (sel.limit_count.is_some()
            && !matches!(sel.limit_count.as_ref(), Some(ast::Node::AConst(c)) if c.isnull)
            && int_literal(sel.limit_count.as_ref()).is_none_or(|n| n <= 0))
        || !sel.locking_clause.is_empty()
    {
        return false;
    }
    // Plain relations, maybe joined (without an alias for the join).
    fn plain<'a>(
        n: Option<&'a ast::Node>,
        inner: &mut Vec<&'a str>,
        on: &mut Vec<&'a ast::Node>,
    ) -> bool {
        match n {
            Some(ast::Node::RangeVar(rv)) => {
                inner.push(
                    rv.alias
                        .as_ref()
                        .map_or(rv.relname.as_str(), |a| &a.aliasname),
                );
                true
            }
            Some(ast::Node::JoinExpr(j)) if j.alias.is_none() && j.join_using_alias.is_none() => {
                on.extend(j.quals.as_ref());
                [&j.larg, &j.rarg]
                    .into_iter()
                    .all(|s| s.as_ref().is_some_and(|s| plain(Some(s), inner, on)))
            }
            _ => false,
        }
    }
    let mut inner = Vec::new();
    let mut on = Vec::new();
    for f in &sel.from_clause {
        if !plain(f.as_ref(), &mut inner, &mut on) {
            return false;
        }
    }
    // Nothing but the WHERE clause may read the level
    // (`contain_vars_of_level`): every column of a join's ON clause is
    // certainly one of the subquery's own, qualified by one of its FROM
    // items, and it holds no sublink.
    let own_columns = |q: &ast::Node| {
        q.nodes().all(|(n, _)| match n {
            typedpg_pg_query::NodeRef::SubLink(_) => false,
            typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                let names = extract_names(&cr.fields);
                matches!(names.as_slice(), [t, _]
                    if names.len() == cr.fields.len() && inner.contains(&t.as_str()))
            }
            _ => true,
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
        .filter_map(|n| n.as_ref().map(ast::Node::to_ref))
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
    let qual @ Some(_) = &sel.where_clause else {
        return false;
    };
    if may_be_volatile(qual.as_ref(), snapshot) {
        return false;
    }
    let Some(qual) = qual.as_ref() else {
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

/// Whether the query of sublink `sub` certainly reads no FROM item of the
/// level around it that `pull_up_sublinks` doesn't make available where
/// the sublink is (`available_rels`, see
/// [`with_nullable_on_clause`]): a column may be told the query's own when
/// it is qualified by a FROM item of the query's, or (unqualified) a column
/// of one of its tables; any other qualified by a name `other_alias`
/// accepts, or unqualified and `other_column`, may be one of those items'.
/// `outer_cte` tells the CTE names the query sees from around it.
pub(crate) fn subquery_reads_only(
    sub: &ast::SubLink,
    snapshot: &PgCatalog,
    outer_cte: &dyn Fn(&str) -> bool,
    other_alias: &dyn Fn(&str) -> bool,
    other_column: &dyn Fn(&str) -> bool,
) -> bool {
    let Some(query) = sub.subselect.as_ref() else {
        return false;
    };
    let ast::Node::SelectStmt(sel) = query else {
        return false;
    };
    // The query's own FROM items, and the columns of its tables (`None`
    // when one of them isn't a table the catalog tells the columns of).
    let mut aliases: Vec<&str> = Vec::new();
    let mut columns: Option<Vec<String>> = Some(Vec::new());
    let cte_names: Vec<&str> = sel
        .with_clause
        .iter()
        .flat_map(|w| &w.ctes)
        .filter_map(|c| match c.as_ref() {
            Some(ast::Node::CommonTableExpr(c)) => Some(c.ctename.as_str()),
            _ => None,
        })
        .collect();
    let mut todo: Vec<Option<&ast::Node>> = sel.from_clause.iter().map(Option::as_ref).collect();
    while let Some(f) = todo.pop() {
        match f {
            Some(ast::Node::RangeVar(rv)) => {
                aliases.push(
                    rv.alias
                        .as_ref()
                        .map_or(rv.relname.as_str(), |a| &a.aliasname),
                );
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                let cte = schema.is_none()
                    && (cte_names.contains(&rv.relname.as_str()) || outer_cte(&rv.relname));
                match snapshot.resolve_table(schema, &rv.relname) {
                    Some(class)
                        if !cte && rv.alias.as_ref().is_none_or(|a| a.colnames.is_empty()) =>
                    {
                        if let Some(cols) = columns.as_mut() {
                            cols.extend(
                                snapshot
                                    .attributes_of(class.oid)
                                    .iter()
                                    .filter(|a| a.attnum > 0)
                                    .map(|a| a.attname.clone()),
                            );
                        }
                    }
                    _ => columns = None,
                }
            }
            Some(ast::Node::RangeSubselect(r)) => {
                aliases.extend(r.alias.as_ref().map(|a| a.aliasname.as_str()));
                columns = None;
            }
            Some(ast::Node::RangeFunction(r)) => {
                aliases.extend(r.alias.as_ref().map(|a| a.aliasname.as_str()));
                columns = None;
            }
            Some(ast::Node::JoinExpr(j)) => {
                if j.alias.is_some()
                    || j.join_using_alias.is_some()
                    || !j.using_clause.is_empty()
                    || j.is_natural
                {
                    columns = None;
                }
                aliases.extend(j.alias.as_ref().map(|a| a.aliasname.as_str()));
                todo.extend(j.larg.as_ref().map(Some));
                todo.extend(j.rarg.as_ref().map(Some));
            }
            _ => columns = None,
        }
    }
    let own_column = |c: &str| {
        columns
            .as_ref()
            .is_some_and(|cols| cols.iter().any(|n| n == c))
    };
    let other_alias = |t: &str| !aliases.contains(&t) && other_alias(t);
    query.nodes().all(|(n, _)| match n {
        typedpg_pg_query::NodeRef::ColumnRef(cr) => {
            let parts = extract_names(&cr.fields);
            match parts.as_slice() {
                // A column, or a whole-row reference.
                [c] if parts.len() == cr.fields.len() => {
                    own_column(c) || !(other_alias(c) || other_column(c))
                }
                [] => true,
                [t, ..] => !other_alias(t),
            }
        }
        _ => true,
    })
}

/// The sublink `pull_up_sublinks` may turn into a join a top-level AND
/// argument `n` of a WHERE clause holds: `n` itself, or the EXISTS a NOT
/// negates.
pub(crate) fn conjunct_sublink(n: Option<&ast::Node>) -> Option<&ast::SubLink> {
    conjunct_sublink_node(n)
}

/// [`conjunct_sublink`] of a node held by reference.
fn conjunct_sublink_node(n: Option<&ast::Node>) -> Option<&ast::SubLink> {
    match n? {
        ast::Node::SubLink(sub) => Some(sub),
        ast::Node::BoolExpr(b) if b.boolop == ast::BoolExprType::NotExpr => {
            match b.args.as_slice() {
                [a] => match a.as_ref()? {
                    ast::Node::SubLink(sub)
                        if sub.sub_link_type == ast::SubLinkType::ExistsSublink =>
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
    static WHERE_ROOT: Cell<Option<NodeKey<ast::Node>>> = const { Cell::new(None) };
    /// The FROM items (by alias) a sublink of [`WHERE_ROOT`] may read to
    /// be pulled up, when that is the ON clause of a join a semi-join
    /// below it joins only part of (see [`with_nullable_on_clause`]);
    /// `None` for any of the level's.
    static AVAILABLE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Run `f`, the analysis of WHERE clause `n` (or of such an ON clause).
pub(crate) fn with_where_clause<R>(n: Option<&ast::Node>, f: impl FnOnce() -> R) -> R {
    with_root(n, None, f)
}

/// Run `f`, the analysis of ON clause `n` of a join whose sublinks
/// `pull_up_sublinks` turns into semi-joins on the nullable side of an
/// outer join (`pull_up_sublinks_qual_recurse`): a LEFT (RIGHT) join's,
/// into its right (left) side — those reading only the FROM items
/// `available` of that side — or an inner join's below the nullable side
/// of one, into the join (`available`, its two sides).
pub(crate) fn with_nullable_on_clause<R>(
    n: Option<&ast::Node>,
    available: Vec<String>,
    f: impl FnOnce() -> R,
) -> R {
    with_root(n, Some(available), f)
}

fn with_root<R>(n: Option<&ast::Node>, available: Option<Vec<String>>, f: impl FnOnce() -> R) -> R {
    // A missing clause is never one being analyzed (see `is_where_clause`).
    let root = n.map(NodeKey::of);
    let prev = WHERE_ROOT.with(|w| w.replace(root));
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
pub(crate) fn is_where_clause(n: &ast::Node) -> bool {
    WHERE_ROOT.with(|w| w.get() == Some(NodeKey::of(n)))
}

/// The value of a whole-number literal.
fn int_literal(n: Option<&ast::Node>) -> Option<i64> {
    match n? {
        ast::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            ast::AConstValue::Integer(i) => Some(i64::from(i.ival)),
            _ => None,
        },
        _ => None,
    }
}

/// What `n` folds to.
pub(crate) fn fold(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Folded {
    fold_node(n, snapshot)
}

/// [`fold`] of a node held by reference.
fn fold_node(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Folded {
    fold_with(n, snapshot, &|_| Folded::Var)
}

/// What qual `n` — a WHERE or JOIN ON clause, a HAVING one — folds to
/// once the planner preprocessed it.
pub(crate) fn fold_qual(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Folded {
    fold_qual_node(n, snapshot)
}

/// [`fold_qual`] of a node held by reference.
fn fold_qual_node(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Folded {
    qual_with(n, snapshot, &|_| Folded::Var, &|_| false, false)
}

/// [`fold_qual`] for a part of a qual's AND / OR / NOT tree, under an odd
/// number of NOTs when `negated`, in the scope of `ctx`: a column the
/// planner replaces by a pulled-up subquery's output is what that folds to
/// (see [`crate::scope::Substitution`]).
pub(crate) fn fold_qual_in(n: Option<&ast::Node>, ctx: super::Ctx<'_>, negated: bool) -> Folded {
    qual_with(
        n,
        ctx.snapshot,
        &|cr| column_in(cr, ctx),
        &|e| super::foldable::planner_nonnullable(e, ctx),
        negated,
    )
}

/// `eval_const_expressions` pushes a NOT down an AND / OR (`negate_clause`,
/// De Morgan's laws), then `canonicalize_qual` (`find_duplicate_ors`), at
/// any depth of the AND / OR tree, reduces an AND with a FALSE or NULL
/// argument to FALSE and drops a FALSE or NULL argument of an OR: in a
/// qual, the NULL constant is FALSE (`rt.s ~ '(' AND NULL` is FALSE, its
/// regex never estimated). Never [`Folded::Null`]. PostgreSQL 19 also
/// folds `x IS [NOT] NULL` of an `x` proven non-NULL (`nonnull`, see
/// [`super::foldable::planner_nonnullable`]): `t.id IS NULL` over a
/// primary key is FALSE.
fn qual_with(
    n: Option<&ast::Node>,
    snapshot: &PgCatalog,
    col: &dyn Fn(&ast::ColumnRef) -> Folded,
    nonnull: &dyn Fn(Option<&ast::Node>) -> bool,
    negated: bool,
) -> Folded {
    if let Some(ast::Node::NullTest(t)) = n
        && !matches!(t.arg.as_ref(), Some(ast::Node::RowExpr(_)))
        && fold_with(t.arg.as_ref(), snapshot, col) == Folded::Var
        && nonnull(t.arg.as_ref())
    {
        let is_null = t.nulltesttype == ast::NullTestType::IsNull;
        return Folded::Bool(!is_null != negated);
    }
    if let Some(ast::Node::BoolExpr(b)) = n {
        match b.boolop {
            ast::BoolExprType::NotExpr => {
                if let Some(a) = b.args.first() {
                    return qual_with(a.as_ref(), snapshot, col, nonnull, !negated);
                }
            }
            op => {
                let or = (op == ast::BoolExprType::OrExpr) != negated;
                let (mut var, mut unknown) = (false, false);
                for a in &b.args {
                    match qual_with(a.as_ref(), snapshot, col, nonnull, negated) {
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
    cr: &ast::ColumnRef,
    ctx: super::Ctx<'_>,
) -> Option<std::sync::Arc<ast::Node>> {
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
fn column_in(cr: &ast::ColumnRef, ctx: super::Ctx<'_>) -> Folded {
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
            Some(e) => fold_with(Some(e), ctx.snapshot, &|_| Folded::Var),
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
    n: Option<&ast::Node>,
    snapshot: &PgCatalog,
    col: &dyn Fn(&ast::ColumnRef) -> Folded,
) -> Folded {
    let Some(inner) = n else {
        return Folded::Const;
    };
    let all = |args: &[Option<ast::Node>]| -> Folded {
        if args
            .iter()
            .any(|a| fold_with(a.as_ref(), snapshot, col) == Folded::Var)
        {
            Folded::Var
        } else {
            Folded::Const
        }
    };
    match inner {
        ast::Node::AConst(c) => {
            if c.isnull {
                return Folded::Null;
            }
            match c.val.as_ref() {
                Some(ast::AConstValue::Boolean(b)) => Folded::Bool(b.boolval),
                Some(ast::AConstValue::String(s)) => {
                    parse_bool(&s.sval).map_or(Folded::Const, Folded::Bool)
                }
                _ => Folded::Const,
            }
        }
        ast::Node::TypeCast(c) => match c.arg.as_ref().map(|a| fold_with(Some(a), snapshot, col)) {
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
        ast::Node::CollateClause(c) => c
            .arg
            .as_ref()
            .map_or(Folded::Const, |a| fold_with(Some(a), snapshot, col)),
        ast::Node::ColumnRef(cr) => col(cr),
        // A custom plan of an EXECUTE has its arguments as constants.
        ast::Node::ParamRef(_) => {
            if custom_plan() {
                Folded::Const
            } else {
                Folded::Var
            }
        }
        ast::Node::SubLink(_)
        | ast::Node::SqlValueFunction(_)
        | ast::Node::GroupingFunc(_)
        | ast::Node::CurrentOfExpr(_)
        | ast::Node::SetToDefault(_) => Folded::Var,
        ast::Node::BoolExpr(b) => match b.boolop {
            ast::BoolExprType::NotExpr => {
                match b.args.first().map(|a| fold_with(a.as_ref(), snapshot, col)) {
                    Some(Folded::Bool(v)) => Folded::Bool(!v),
                    Some(other) => other,
                    None => Folded::Const,
                }
            }
            op => {
                let or = op == ast::BoolExprType::OrExpr;
                let (mut var, mut unknown, mut null) = (false, false, false);
                for a in &b.args {
                    match fold_with(a.as_ref(), snapshot, col) {
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
        },
        ast::Node::AExpr(e) => {
            let mut operands: Vec<Option<&ast::Node>> = Vec::new();
            for side in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                match side {
                    ast::Node::List(l) => operands.extend(l.items.iter().map(Option::as_ref)),
                    _ => operands.push(Some(side)),
                }
            }
            let folded: Vec<Folded> = operands
                .iter()
                .map(|o| fold_with(*o, snapshot, col))
                .collect();
            let kind = e.kind;
            if matches!(
                kind,
                ast::AExprKind::AexprDistinct | ast::AExprKind::AexprNotDistinct
            ) {
                return if folded.contains(&Folded::Var) {
                    Folded::Var
                } else {
                    Folded::Const
                };
            }
            // A (strict) operator over a NULL constant folds to NULL.
            if folded.contains(&Folded::Null) {
                return if kind == ast::AExprKind::AexprOp {
                    Folded::Null
                } else {
                    Folded::Const
                };
            }
            if folded.contains(&Folded::Var) {
                return Folded::Var;
            }
            if kind == ast::AExprKind::AexprOp
                && let (l @ Some(_), r @ Some(_)) = (&e.lexpr, &e.rexpr)
                && let (Some(l), Some(r)) = (int_literal(l.as_ref()), int_literal(r.as_ref()))
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
        ast::Node::NullTest(t) => {
            let is_null = t.nulltesttype == ast::NullTestType::IsNull;
            match t.arg.as_ref().map(|a| fold_with(Some(a), snapshot, col)) {
                Some(Folded::Var) => Folded::Var,
                Some(Folded::Null) => Folded::Bool(is_null),
                Some(Folded::Bool(_)) => Folded::Bool(!is_null),
                _ => Folded::Const,
            }
        }
        ast::Node::BooleanTest(t) => {
            match t.arg.as_ref().map(|a| fold_with(Some(a), snapshot, col)) {
                Some(Folded::Var) => Folded::Var,
                _ => Folded::Const,
            }
        }
        ast::Node::CoalesceExpr(c) => {
            let mut var = false;
            for a in &c.args {
                match fold_with(a.as_ref(), snapshot, col) {
                    Folded::Null => {}
                    Folded::Var => var = true,
                    v => return if var { Folded::Var } else { v },
                }
            }
            if var { Folded::Var } else { Folded::Null }
        }
        ast::Node::CaseExpr(c) => {
            let test = c.arg.as_ref().map(|a| fold_with(Some(a), snapshot, col));
            let mut var = false;
            for w in &c.args {
                let Some(ast::Node::CaseWhen(w)) = w.as_ref() else {
                    return Folded::Const;
                };
                match case_when_with(test, w.expr.as_ref(), snapshot, col) {
                    Folded::Bool(false) | Folded::Null => {}
                    Folded::Bool(true) => {
                        return if var {
                            Folded::Var
                        } else {
                            w.result
                                .as_ref()
                                .map_or(Folded::Const, |r| fold_with(Some(r), snapshot, col))
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
                    .as_ref()
                    .map_or(Folded::Null, |d| fold_with(Some(d), snapshot, col))
            }
        }
        ast::Node::FuncCall(fc) => {
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
        ast::Node::NamedArgExpr(a) => a
            .arg
            .as_ref()
            .map_or(Folded::Const, |a| fold_with(Some(a), snapshot, col)),
        ast::Node::MinMaxExpr(m) => all(&m.args),
        ast::Node::RowExpr(r) => all(&r.args),
        ast::Node::AArrayExpr(a) => all(&a.elements),
        ast::Node::AIndirection(i) => i.arg.as_ref().map_or(Folded::Const, |a| {
            if fold_with(Some(a), snapshot, col) == Folded::Var {
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
    cond: Option<&ast::Node>,
    snapshot: &PgCatalog,
) -> Folded {
    case_when_with(test, cond, snapshot, &|_| Folded::Var)
}

fn case_when_with(
    test: Option<Folded>,
    cond: Option<&ast::Node>,
    snapshot: &PgCatalog,
    col: &dyn Fn(&ast::ColumnRef) -> Folded,
) -> Folded {
    let cond = cond.map_or(Folded::Const, |e| fold_with(Some(e), snapshot, col));
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

/// The functions and operators a stored expression (`expression_planner`
/// folds it before execution) calls, split by whether the planner certainly
/// evaluates them. A call in code `eval_const_expressions` drops never runs:
/// its `pg_depend` routine needs no EXECUTE when the expression executes.
#[derive(Default)]
pub(crate) struct Callees {
    pub(crate) live: std::collections::HashSet<String>,
    pub(crate) dead: std::collections::HashSet<String>,
    /// The `(name, argument count)` of the live function calls — the only
    /// thing a `pg_depend` row tells apart without resolving names again
    /// (see [`Callees::runs_routine`]).
    pub(crate) live_arities: std::collections::HashSet<(String, usize)>,
    /// Likewise, of the calls in code the planner drops.
    pub(crate) dead_arities: std::collections::HashSet<(String, usize)>,
}

impl Callees {
    /// Whether a routine (or operator's function) named `name` may run: it
    /// appears in live code, or nowhere this walk classified (a cast
    /// function, say).
    pub(crate) fn runs(&self, name: &str) -> bool {
        self.live.contains(name) || !self.dead.contains(name)
    }

    /// Whether the walk saw `name` anywhere, live or dead.
    pub(crate) fn mentions(&self, name: &str) -> bool {
        self.live.contains(name) || self.dead.contains(name)
    }

    /// [`Self::runs`] over the exact expression the walk covered: a name the
    /// walk never saw is not part of it. A policy's `pg_depend` rows cover
    /// every clause of the policy, but a statement builds only the clauses
    /// its command and tables use (a WITH CHECK on a read, a USING on an
    /// INSERT): a dependency named by no built clause is dead for it.
    pub(crate) fn runs_mentioned(&self, name: &str) -> bool {
        self.live.contains(name)
    }

    /// Whether the routine `name` with `nargs` input parameters is called by
    /// live code. The stored expression's `pg_depend` rows name the exact
    /// OIDs the expression resolved to at creation, and resolving its names
    /// again under a later `search_path` may pick another schema's function:
    /// only the call sites the folding walk finds dead for *the same*
    /// expression may prune a row. A function with defaults or `VARIADIC`
    /// parameters is called with another argument count than its `pronargs`:
    /// when no call site of the name has this count, every live call of the
    /// name keeps it.
    pub(crate) fn runs_routine(&self, name: &str, nargs: usize) -> bool {
        let key = (name.to_owned(), nargs);
        if self.live_arities.contains(&key) {
            return true;
        }
        if self.dead_arities.contains(&key) {
            return false;
        }
        self.runs(name)
    }

    /// [`Self::runs_routine`] with [`Self::runs_mentioned`]'s rule: a name
    /// no call site of the walk mentions is dead.
    pub(crate) fn runs_routine_mentioned(&self, name: &str, nargs: usize) -> bool {
        let key = (name.to_owned(), nargs);
        if self.live_arities.contains(&key) {
            return true;
        }
        if self.dead_arities.contains(&key) {
            return false;
        }
        self.runs_mentioned(name)
    }
}

/// The callees of `n`, dead branches classified (see [`Callees`]).
pub(crate) fn callees(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Callees {
    let mut out = Callees::default();
    walk_node(n, snapshot, false, &mut out);
    out
}

/// [`walk_callees`] for a node the tree holds by reference.
fn walk_node(n: Option<&ast::Node>, snapshot: &PgCatalog, dead: bool, out: &mut Callees) {
    if let Some(inner) = n {
        walk_callees(inner.to_ref(), snapshot, dead, out);
    }
}

fn walk_callees(
    n: typedpg_pg_query::NodeRef<'_>,
    snapshot: &PgCatalog,
    dead: bool,
    out: &mut Callees,
) {
    use typedpg_pg_query::NodeRef;
    if dead {
        collect_callees(n, out);
        return;
    }
    match n {
        NodeRef::FuncCall(fc) => {
            if let Some(name) = extract_names(&fc.funcname).last() {
                out.live_arities.insert((name.clone(), fc.args.len()));
            }
            out.live.extend(extract_names(&fc.funcname).last().cloned());
        }
        NodeRef::AExpr(e) => out.live.extend(extract_names(&e.name).last().cloned()),
        NodeRef::BoolExpr(b) => {
            let deciding = match b.boolop {
                ast::BoolExprType::AndExpr => Some(false),
                ast::BoolExprType::OrExpr => Some(true),
                _ => None,
            };
            if let Some(value) = deciding
                && b.args
                    .iter()
                    .any(|a| certainly(a.as_ref(), snapshot) == Certain::Bool(value))
            {
                for a in &b.args {
                    if certainly(a.as_ref(), snapshot) == Certain::Bool(value) {
                        continue;
                    }
                    walk_node(a.as_ref(), snapshot, true, out);
                }
                return;
            }
        }
        NodeRef::CoalesceExpr(c) => {
            let mut stopped = false;
            for a in &c.args {
                if stopped {
                    walk_node(a.as_ref(), snapshot, true, out);
                    continue;
                }
                walk_node(a.as_ref(), snapshot, false, out);
                stopped = certainly_non_null_constant(a.as_ref(), snapshot);
            }
            return;
        }
        NodeRef::CaseExpr(c) => {
            let mut stopped = false;
            for w in &c.args {
                let Some(ast::Node::CaseWhen(w)) = w.as_ref() else {
                    break;
                };
                if stopped {
                    if let Some(cond) = w.expr.as_ref() {
                        walk_node(Some(cond), snapshot, true, out);
                    }
                    if let Some(r) = w.result.as_ref() {
                        walk_node(Some(r), snapshot, true, out);
                    }
                    continue;
                }
                if c.arg.is_some() {
                    // A simple CASE: compare its test with each WHEN. The
                    // planner folds only `CASE const WHEN const` (see
                    // `certain_case`); the walker's generic names still catch
                    // the live calls.
                    if let Some(cond) = w.expr.as_ref() {
                        walk_node(Some(cond), snapshot, false, out);
                    }
                    if let Some(r) = w.result.as_ref() {
                        walk_node(Some(r), snapshot, false, out);
                    }
                    continue;
                }
                if let Some(cond) = w.expr.as_ref() {
                    walk_node(Some(cond), snapshot, false, out);
                }
                match w.expr.as_ref().map(|e| certainly(Some(e), snapshot)) {
                    Some(Certain::Bool(false) | Certain::Null) => {
                        if let Some(r) = w.result.as_ref() {
                            walk_node(Some(r), snapshot, true, out);
                        }
                    }
                    Some(Certain::Bool(true)) => {
                        if let Some(r) = w.result.as_ref() {
                            walk_node(Some(r), snapshot, false, out);
                        }
                        stopped = true;
                    }
                    _ => {
                        if let Some(r) = w.result.as_ref() {
                            walk_node(Some(r), snapshot, false, out);
                        }
                    }
                }
            }
            if let Some(d) = c.defresult.as_ref() {
                walk_node(Some(d), snapshot, stopped, out);
            }
            return;
        }
        _ => {}
    }
    for child in n.children() {
        walk_callees(child, snapshot, false, out);
    }
}

/// The names and call arities of the functions and operators anywhere in
/// `n`, live or dead.
fn collect_callees(n: typedpg_pg_query::NodeRef<'_>, out: &mut Callees) {
    use typedpg_pg_query::NodeRef;
    match n {
        NodeRef::FuncCall(fc) => {
            if let Some(name) = extract_names(&fc.funcname).last() {
                out.dead_arities.insert((name.clone(), fc.args.len()));
                out.dead.insert(name.clone());
            }
        }
        NodeRef::AExpr(e) => out.dead.extend(extract_names(&e.name).last().cloned()),
        _ => {}
    }
    for child in n.children() {
        collect_callees(child, out);
    }
}

/// Run `f` for a part of the expression the rewriter / planner certainly
/// drops, so execution never checks it (a privilege check the dropped code
/// would need is not made). The code is still analyzed: parse analysis
/// resolved its names and types before the planner ran.
pub(crate) fn unless_dropped<R>(dropped: bool, f: impl FnOnce() -> R) -> R {
    if dropped && !crate::privileges::checks_dropped_code() {
        crate::privileges::not_executed(f)
    } else {
        f()
    }
}

/// Whether `n` is a condition the planner certainly folds to a known
/// boolean: what `eval_const_expressions` reduces an `IF` / `CASE WHEN` /
/// `AND` / `OR` operand to. `Some(true)` / `Some(false)` when certain (a
/// `NULL` condition is not true, so `Some(false)`); `None` when it isn't
/// certainly a constant.
pub(crate) fn certainly_true(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Option<bool> {
    match certainly(n, snapshot) {
        Certain::Bool(v) => Some(v),
        Certain::Null => Some(false),
        Certain::Value | Certain::UnknownValue | Certain::Not => None,
    }
}

/// Whether `n` certainly folds to a non-NULL constant (COALESCE stops after
/// it: the arguments after it are never evaluated).
pub(crate) fn certainly_non_null_constant(n: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
    matches!(certainly(n, snapshot), Certain::Bool(_) | Certain::Value)
}

thread_local! {
    /// The scope `certainly` resolves column references in (see
    /// [`certainly_in`]): the output of a subquery the planner certainly
    /// pulls up replaces the reference before folding.
    static CERTAIN_SCOPE: Cell<Option<*const crate::scope::Scope>> = const { Cell::new(None) };
    /// How deep in substituted output expressions `certainly` is.
    static CERTAIN_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Restores [`CERTAIN_SCOPE`] when dropped.
struct CertainScopeGuard(Option<*const crate::scope::Scope>);

impl Drop for CertainScopeGuard {
    fn drop(&mut self) {
        CERTAIN_SCOPE.with(|c| c.set(self.0));
    }
}

/// [`certainly`] in the scope of `ctx`: a column reference the planner
/// replaces by a certainly known expression — the output of a subquery it
/// certainly pulls up (`pull_up_simple_subquery`), a view's, an inlined
/// CTE's — counts as that expression. A reference to a column that stays a
/// Var (a table's, a subquery the planner keeps, one of an outer join's
/// nullable side) is no constant.
pub(crate) fn certainly_in(n: Option<&ast::Node>, ctx: super::Ctx<'_>) -> Certain {
    let outer = CERTAIN_SCOPE.with(|c| c.replace(Some(std::ptr::from_ref(ctx.scope))));
    let _restore = CertainScopeGuard(outer);
    certainly(n, ctx.snapshot)
}

/// [`certainly`] of column `cr`: what the expression the planner
/// substitutes for it folds to (see [`certainly_in`]).
fn certain_column(cr: &ast::ColumnRef, snapshot: &PgCatalog) -> Certain {
    let Some(scope) = CERTAIN_SCOPE.with(Cell::get) else {
        return Certain::Not;
    };
    // SAFETY: the scope outlives the guard that installed it on this thread
    // (see [`certainly_in`]); `certainly` only walks the current level's
    // expression, never a subquery's.
    let scope = unsafe { &*scope };
    let parts = extract_names(&cr.fields);
    let (table, column) = match parts.as_slice() {
        [c] if parts.len() == cr.fields.len() => (None, c.as_str()),
        [t, c] if parts.len() == cr.fields.len() => (Some(t.as_str()), c.as_str()),
        _ => return Certain::Not,
    };
    let Ok(col) = scope.resolve_column(table, column, None) else {
        return Certain::Not;
    };
    match col.deferred.substitution {
        crate::scope::Substitution::Certain => match col.deferred.expr.as_deref() {
            Some(e) => {
                // A substituted expression is usually a constant already; a
                // chain of pulls ends in one. Bound the walk anyway.
                let depth = CERTAIN_DEPTH.with(Cell::get);
                if depth >= 64 {
                    return Certain::Not;
                }
                CERTAIN_DEPTH.with(|c| c.set(depth + 1));
                let out = certainly(Some(e), snapshot);
                CERTAIN_DEPTH.with(|c| c.set(depth));
                out
            }
            // A table's column passed through stays a Var.
            None => Certain::Not,
        },
        _ => Certain::Not,
    }
}

/// What `eval_const_expressions` makes of `n`, *certainly*: a boolean or
/// NULL constant it computes, a non-NULL constant it folds whose value the
/// analyzer doesn't compute, or [`Certain::Not`] when the expression may stay
/// (or when the analyzer can't tell). A column reference counts only in
/// [`certainly_in`]'s scope, as the expression the planner substitutes for
/// it. Unlike [`fold`], which errs towards treating an expression as
/// foldable, this errs towards `Not`: code the planner may keep is code that
/// runs, and its privileges must be checked.
pub(crate) fn certainly(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Certain {
    let Some(inner) = n else {
        return Certain::Not;
    };
    match inner {
        ast::Node::AConst(c) => {
            if c.isnull {
                return Certain::Null;
            }
            match c.val.as_ref() {
                Some(ast::AConstValue::Boolean(b)) => Certain::Bool(b.boolval),
                // An untyped literal: in a boolean context the parser has
                // coerced it to boolean (`parse_bool`); otherwise it is a
                // non-NULL constant.
                Some(ast::AConstValue::String(s)) => match parse_bool(&s.sval) {
                    Some(v) => Certain::Bool(v),
                    None => Certain::Value,
                },
                _ => Certain::Value,
            }
        }
        ast::Node::TypeCast(tc) => certain_cast(tc, snapshot),
        ast::Node::ColumnRef(cr) => certain_column(cr, snapshot),
        ast::Node::CollateClause(c) => c
            .arg
            .as_ref()
            .map_or(Certain::Not, |a| certainly(Some(a), snapshot)),
        // A custom plan substitutes the parameter's value.
        ast::Node::ParamRef(_) => {
            if custom_plan() {
                Certain::UnknownValue
            } else {
                Certain::Not
            }
        }
        ast::Node::BoolExpr(b) => certain_bool_expr(b, snapshot),
        ast::Node::AExpr(e) => certain_a_expr(e, snapshot),
        ast::Node::NullTest(t) => {
            let is_null = t.nulltesttype == ast::NullTestType::IsNull;
            match t.arg.as_ref().map(|a| certainly(Some(a), snapshot)) {
                Some(Certain::Null) => Certain::Bool(is_null),
                Some(Certain::Bool(_) | Certain::Value) => Certain::Bool(!is_null),
                // A folded constant may be NULL.
                _ => Certain::Not,
            }
        }
        ast::Node::BooleanTest(t) => certain_boolean_test(t, snapshot),
        ast::Node::CoalesceExpr(c) => {
            for a in &c.args {
                match certainly(a.as_ref(), snapshot) {
                    Certain::Null => {}
                    v @ (Certain::Bool(_) | Certain::Value) => return v,
                    // A folded constant may be NULL: the next argument may
                    // still run.
                    Certain::UnknownValue => return Certain::Not,
                    Certain::Not => return Certain::Not,
                }
            }
            Certain::Null
        }
        ast::Node::CaseExpr(c) => certain_case(c, snapshot),
        ast::Node::FuncCall(fc) => certain_func_call(fc, snapshot),
        ast::Node::MinMaxExpr(m) => {
            let mut all_null = true;
            for a in &m.args {
                match certainly(a.as_ref(), snapshot) {
                    Certain::Null => {}
                    Certain::Bool(_) | Certain::Value => all_null = false,
                    Certain::UnknownValue | Certain::Not => return Certain::Not,
                }
            }
            if all_null {
                Certain::Null
            } else {
                Certain::Value
            }
        }
        ast::Node::AArrayExpr(a) => {
            if a.elements.iter().all(|e| {
                matches!(
                    certainly(e.as_ref(), snapshot),
                    Certain::Bool(_) | Certain::Null | Certain::Value
                )
            }) {
                Certain::Value
            } else {
                Certain::Not
            }
        }
        ast::Node::RowExpr(r) => {
            if r.args.iter().all(|e| {
                matches!(
                    certainly(e.as_ref(), snapshot),
                    Certain::Bool(_) | Certain::Null | Certain::Value
                )
            }) {
                Certain::Value
            } else {
                Certain::Not
            }
        }
        ast::Node::NamedArgExpr(a) => a
            .arg
            .as_ref()
            .map_or(Certain::Not, |a| certainly(Some(a), snapshot)),
        ast::Node::AIndirection(i) => {
            let base = i.arg.as_ref().map(|a| certainly(Some(a), snapshot));
            let indirection = i
                .indirection
                .iter()
                .all(|x| certainly(x.as_ref(), snapshot) != Certain::Not);
            match base {
                Some(Certain::Bool(_) | Certain::Value | Certain::UnknownValue) if indirection => {
                    Certain::UnknownValue
                }
                _ => Certain::Not,
            }
        }
        _ => Certain::Not,
    }
}

/// [`certainly`] for a cast: `eval_const_expressions` folds a cast of a
/// constant when its target is no domain (a `CoerceToDomain` is never
/// folded, whatever its argument) and no cast function it may run is mutable.
/// A cast of an untyped string literal (`'now'::timestamptz`) runs the
/// target's input function at parse time: it is a Const whatever that
/// function's volatility.
fn certain_cast(tc: &ast::TypeCast, snapshot: &PgCatalog) -> Certain {
    let Some(target) = cast_target(tc, snapshot) else {
        return Certain::Not;
    };
    let Some(ty) = snapshot.pg_type.get(&target) else {
        return Certain::Not;
    };
    let arg_node = tc.arg.as_ref();
    // `transformRowExpr`: a row constructor cast to a composite type is a
    // `RowExpr` of that type, not a coercion — as constant as its fields
    // (`ROW(1)::ct` is a `Const`-like argument `eval_const_expressions`
    // folds a call over).
    if ty.typtype == crate::pg_catalog::TypType::Composite
        && let Some(ast::Node::RowExpr(row)) = arg_node
    {
        return if row.args.iter().all(|e| {
            matches!(
                certainly(e.as_ref(), snapshot),
                Certain::Bool(_) | Certain::Null | Certain::Value
            )
        }) {
            Certain::Value
        } else {
            Certain::Not
        };
    }
    // coerce_type turns an untyped constant into a Const of the target's
    // *base* type (`parse_coerce.c`), and wraps it in a `CoerceToDomain`
    // when the target is a domain. That node is never folded: its constraint
    // expressions run when it is evaluated — even for a NULL value
    // (`EEOP_DOMAIN_CHECK` evaluates them), so `NULL::d` is not the NULL
    // constant. `eval_const_expressions` replaces a constraint-less domain's
    // coercion with a RelabelType (`DomainHasConstraints`), so only a domain
    // with constraints keeps the node.
    if arg_node.is_some_and(|a| {
        matches!(
            Some(a),
            Some(ast::Node::AConst(c)) if c.isnull
        )
    }) {
        // `DomainHasConstraints` counts the whole domain stack: its own
        // checks and NOT NULL, and every parent domain's (the typcache's
        // `load_domaintype_info` applies them).
        if ty.typtype == crate::pg_catalog::TypType::Domain
            && domain_has_constraints(snapshot, target)
        {
            return Certain::Not;
        }
        return Certain::Null;
    }
    // A boolean cast keeps the value: a value already boolean only takes a
    // RelabelType, and the parser evaluates an untyped literal through
    // `boolin` (`'false'::boolean` is the false constant).
    if target == crate::pg_catalog::oid::BOOL
        && let Some(arg) = arg_node.map(|a| certainly(Some(a), snapshot))
    {
        match arg {
            Certain::Bool(v) => return Certain::Bool(v),
            Certain::Null => return Certain::Null,
            _ => {}
        }
    }
    if ty.typtype == crate::pg_catalog::TypType::Domain {
        return Certain::Not;
    }
    let arg = arg_node.map(|a| certainly(Some(a), snapshot));
    // The parser turns an untyped literal into a Const when it coerces it
    // (stringTypeDatum), before the planner runs.
    let untyped_literal = arg_node.is_some_and(|a| {
        matches!(
            Some(a),
            Some(ast::Node::AConst(c))
                if !c.isnull && matches!(c.val.as_ref(), Some(ast::AConstValue::String(_)))
        )
    });
    let source = arg_node.and_then(|a| constant_type(Some(a), snapshot));
    let cast = source.and_then(|source| {
        snapshot
            .pg_cast
            .values()
            .find(|c| c.castsource == source && c.casttarget == target)
    });
    let function = |cast: Option<&crate::pg_catalog::PgCast>| {
        cast.and_then(|c| c.castfunc)
            .and_then(|f| snapshot.pg_proc.get(&f))
    };
    // No `pg_cast` row: a CoerceViaIO through the types' output and input
    // functions, which `eval_const_expressions` folds only when both are
    // immutable — and the catalog doesn't keep their volatility here. Stay
    // conservative (`1::text` may fold, but `'now'::timestamptz::text`'s
    // timestamptz output is stable).
    let Some(cast) = cast else {
        return if untyped_literal {
            Certain::Value
        } else {
            Certain::Not
        };
    };
    match arg {
        Some(Certain::Not) | None => Certain::Not,
        // A NULL through a strict cast function is NULL.
        Some(Certain::Null) => {
            if function(Some(cast)).is_none_or(|p| p.proisstrict) {
                Certain::Null
            } else {
                Certain::Not
            }
        }
        Some(Certain::Bool(v)) if target == crate::pg_catalog::oid::BOOL => Certain::Bool(v),
        Some(Certain::Bool(_) | Certain::Value | Certain::UnknownValue) => {
            match function(Some(cast)) {
                // A relabel / binary coercion keeps the value.
                None => Certain::Value,
                Some(p) if p.provolatile == ProVolatile::Immutable => {
                    // The cast function runs, and may return NULL.
                    Certain::UnknownValue
                }
                Some(_) => Certain::Not,
            }
        }
    }
}

/// Whether a domain (or a domain it is over) has any constraint:
/// `DomainHasConstraints` (typcache.c), which counts NOT NULL too.
fn domain_has_constraints(snapshot: &PgCatalog, domain: crate::oid::PgTypeOid) -> bool {
    let mut current = Some(domain);
    while let Some(ty) = current {
        let Some(row) = snapshot.pg_type.get(&ty) else {
            return true;
        };
        if row.typtype != crate::pg_catalog::TypType::Domain {
            return false;
        }
        if row.typnotnull
            || snapshot
                .domain_constraints
                .get(&ty)
                .is_some_and(|constraints| !constraints.is_empty())
        {
            return true;
        }
        current = row.typbasetype;
    }
    false
}

/// The type a constant expression certainly has, for finding its cast:
/// an integer / boolean literal, or the target of a cast.
fn constant_type(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Option<crate::oid::PgTypeOid> {
    match n? {
        ast::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            ast::AConstValue::Integer(_) => Some(crate::pg_catalog::oid::INT4),
            ast::AConstValue::Boolean(_) => Some(crate::pg_catalog::oid::BOOL),
            _ => None,
        },
        ast::Node::TypeCast(tc) => cast_target(tc, snapshot),
        _ => None,
    }
}

/// The type a `TypeCast`'s `TypeName` names, array bounds included.
fn cast_target(tc: &ast::TypeCast, snapshot: &PgCatalog) -> Option<crate::oid::PgTypeOid> {
    let type_name = tc.type_name.as_ref()?;
    let names = extract_names(&type_name.names);
    let ty = match names.as_slice() {
        [name] => snapshot.resolve_type_by_name(None, name),
        [schema, name] => snapshot.resolve_type_by_name(Some(schema), name),
        _ => None,
    }?;
    if type_name.array_bounds.is_empty() {
        Some(ty.oid)
    } else {
        snapshot.array_type_of(ty.oid)
    }
}

fn certain_bool_expr(b: &ast::BoolExpr, snapshot: &PgCatalog) -> Certain {
    let op = b.boolop;
    match op {
        ast::BoolExprType::NotExpr => {
            match b.args.first().map(|a| certainly(a.as_ref(), snapshot)) {
                Some(Certain::Bool(v)) => Certain::Bool(!v),
                Some(Certain::Null) => Certain::Null,
                _ => Certain::Not,
            }
        }
        op @ (ast::BoolExprType::AndExpr | ast::BoolExprType::OrExpr) => {
            let or = op == ast::BoolExprType::OrExpr;
            let mut null = false;
            for a in &b.args {
                match certainly(a.as_ref(), snapshot) {
                    // simplify_and_arguments / simplify_or_arguments: this
                    // argument is the result, the others are dropped.
                    Certain::Bool(v) if v == or => return Certain::Bool(or),
                    Certain::Bool(_) => {}
                    Certain::Null => null = true,
                    Certain::Value | Certain::UnknownValue | Certain::Not => {
                        return Certain::Not;
                    }
                }
            }
            if null {
                Certain::Null
            } else {
                Certain::Bool(!or)
            }
        }
    }
}

fn certain_boolean_test(t: &ast::BooleanTest, snapshot: &PgCatalog) -> Certain {
    use typedpg_pg_query::ast::BoolTestType;
    let arg = t.arg.as_ref().map(|a| certainly(Some(a), snapshot));
    let kind = t.booltesttype;
    match (kind, arg) {
        (BoolTestType::IsTrue, Some(Certain::Bool(v))) => Certain::Bool(v),
        (BoolTestType::IsTrue, Some(Certain::Null)) => Certain::Bool(false),
        (BoolTestType::IsNotTrue, Some(Certain::Bool(v))) => Certain::Bool(!v),
        (BoolTestType::IsNotTrue, Some(Certain::Null)) => Certain::Bool(true),
        (BoolTestType::IsFalse, Some(Certain::Bool(v))) => Certain::Bool(!v),
        (BoolTestType::IsFalse, Some(Certain::Null)) => Certain::Bool(false),
        (BoolTestType::IsNotFalse, Some(Certain::Bool(v))) => Certain::Bool(v),
        (BoolTestType::IsNotFalse, Some(Certain::Null)) => Certain::Bool(true),
        (BoolTestType::IsUnknown, Some(Certain::Null)) => Certain::Bool(true),
        (BoolTestType::IsUnknown, Some(Certain::Bool(_) | Certain::Value)) => Certain::Bool(false),
        (BoolTestType::IsNotUnknown, Some(Certain::Null)) => Certain::Bool(false),
        (BoolTestType::IsNotUnknown, Some(Certain::Bool(_) | Certain::Value)) => {
            Certain::Bool(true)
        }
        _ => Certain::Not,
    }
}

/// The literal an expression is, for comparing a simple CASE's test with a
/// WHEN: an integer or a (plain) string literal.
enum Literal {
    Int(i64),
    Str(String),
    Bool(bool),
}

fn certain_literal(n: Option<&ast::Node>) -> Option<Literal> {
    match n? {
        ast::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            ast::AConstValue::Integer(i) => Some(Literal::Int(i64::from(i.ival))),
            ast::AConstValue::String(s) => Some(Literal::Str(s.sval.clone())),
            ast::AConstValue::Boolean(b) => Some(Literal::Bool(b.boolval)),
            _ => None,
        },
        ast::Node::TypeCast(tc) => {
            // A boolean cast runs the input function at parse time: a plain
            // string literal becomes the boolean it spells.
            if cast_target_is_bool(tc) {
                return match tc.arg.as_ref()? {
                    ast::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
                        ast::AConstValue::String(s) => parse_bool(&s.sval).map(Literal::Bool),
                        ast::AConstValue::Boolean(b) => Some(Literal::Bool(b.boolval)),
                        _ => None,
                    },
                    _ => None,
                };
            }
            // Only a cast that keeps the text of a plain string literal
            // (`'x'::text`), never a typed conversion (`'1'::int`).
            if !cast_target_is_text(tc) {
                return None;
            }
            match tc.arg.as_ref()? {
                ast::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
                    ast::AConstValue::String(s) => Some(Literal::Str(s.sval.clone())),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether the cast's target is plain `boolean` (not a domain, no array).
fn cast_target_is_bool(tc: &ast::TypeCast) -> bool {
    let Some(type_name) = tc.type_name.as_ref() else {
        return false;
    };
    if !type_name.array_bounds.is_empty() || !type_name.typmods.is_empty() {
        return false;
    }
    let names = extract_names(&type_name.names);
    matches!(names.as_slice(), [n] | [_, n] if n == "bool" || n == "boolean")
}

fn cast_target_is_text(tc: &ast::TypeCast) -> bool {
    let Some(names) = tc.type_name.as_ref().map(|t| extract_names(&t.names)) else {
        return false;
    };
    matches!(
        names.as_slice(),
        [n] | [_, n] if n == "text" || n == "varchar"
    )
}

fn certain_case(c: &ast::CaseExpr, snapshot: &PgCatalog) -> Certain {
    // A simple CASE compares its test with each WHEN; a searched CASE tests
    // each WHEN directly.
    let test = c.arg.as_ref();
    for w in &c.args {
        let Some(ast::Node::CaseWhen(w)) = w.as_ref() else {
            return Certain::Not;
        };
        let cond = match test {
            None => match w.expr.as_ref().map(|e| certainly(Some(e), snapshot)) {
                Some(Certain::Bool(v)) => Some(v),
                Some(Certain::Null) => Some(false),
                Some(Certain::Value | Certain::UnknownValue) => return Certain::Not,
                Some(Certain::Not) | None => return Certain::Not,
            },
            Some(test) => match (
                certain_literal(Some(test)),
                certain_literal(w.expr.as_ref()),
            ) {
                (Some(a), Some(b)) => Some(equal_literals(&a, &b)),
                _ => return Certain::Not,
            },
        };
        match cond {
            Some(true) => {
                return w
                    .result
                    .as_ref()
                    .map_or(Certain::Not, |r| certainly(Some(r), snapshot));
            }
            Some(false) => {}
            None => return Certain::Not,
        }
    }
    match c.defresult.as_ref() {
        Some(d) => certainly(Some(d), snapshot),
        None => Certain::Null,
    }
}

/// Whether a simple CASE's test `test` certainly equals its WHEN value
/// `when` (`CASE x WHEN v …`): `None` when either isn't a known literal.
pub(crate) fn simple_case_equal(
    test: Option<&ast::Node>,
    when: Option<&ast::Node>,
) -> Option<bool> {
    match (certain_literal(test), certain_literal(when)) {
        (Some(a), Some(b)) => Some(equal_literals(&a, &b)),
        _ => None,
    }
}

fn equal_literals(a: &Literal, b: &Literal) -> bool {
    match (a, b) {
        (Literal::Int(a), Literal::Int(b)) => a == b,
        (Literal::Str(a), Literal::Str(b)) => a == b,
        (Literal::Bool(a), Literal::Bool(b)) => a == b,
        // `transformCaseExpr` builds `subject = when` and lets the operator
        // resolution coerce: an unknown string literal takes the other side's
        // type, so `CASE 1 WHEN '1'` compares integers. `int4in` accepts the
        // digits `str::parse` does for the values that reach here (a literal
        // it rejects fails the parse, before folding).
        (Literal::Int(a), Literal::Str(b)) | (Literal::Str(b), Literal::Int(a)) => {
            b.trim().parse::<i64>() == Ok(*a)
        }
        (Literal::Bool(a), Literal::Str(b)) | (Literal::Str(b), Literal::Bool(a)) => {
            parse_bool(b) == Some(*a)
        }
        _ => false,
    }
}

fn certain_a_expr(e: &ast::AExpr, snapshot: &PgCatalog) -> Certain {
    let kind = e.kind;
    let operands: Vec<Option<&ast::Node>> = [e.lexpr.as_ref(), e.rexpr.as_ref()]
        .into_iter()
        .flatten()
        .flat_map(|n| match n {
            ast::Node::List(l) => l.items.iter().map(Option::as_ref).collect(),
            _ => vec![Some(n)],
        })
        .collect();
    let folded: Vec<Certain> = operands.iter().map(|o| certainly(*o, snapshot)).collect();
    let names = extract_names(&e.name);
    let operator = names.last().map(String::as_str);
    // An operator all of whose implementations are strict folds a NULL
    // operand to NULL (`NULL + x`). Only a plain operator call: `x IS
    // DISTINCT FROM NULL` and `NULLIF(x, NULL)` name the strict `=` but are
    // never NULL / are `x`, an `IN` list with a NULL item is TRUE when
    // another item matches, and `NULL = ANY ('{}')` is false.
    if folded.contains(&Certain::Null)
        && kind == ast::AExprKind::AexprOp
        && operator.is_some_and(|op| operator_certain(op, snapshot, ProVolatile::Volatile, true))
    {
        return Certain::Null;
    }
    if folded.contains(&Certain::Not) || folded.contains(&Certain::Null) {
        return Certain::Not;
    }
    // A comparison of two constants the analyzer can compute.
    if kind == ast::AExprKind::AexprOp
        && let (Some(l), Some(r)) = (e.lexpr.as_ref(), e.rexpr.as_ref())
    {
        if let (Some(l), Some(r)) = (int_literal(Some(l)), int_literal(Some(r))) {
            let v = match operator {
                Some("=") => Some(l == r),
                Some("<>" | "!=") => Some(l != r),
                Some("<") => Some(l < r),
                Some(">") => Some(l > r),
                Some("<=") => Some(l <= r),
                Some(">=") => Some(l >= r),
                _ => None,
            };
            if let Some(v) = v {
                return Certain::Bool(v);
            }
        }
        if let (Some(a), Some(b)) = (certain_literal(Some(l)), certain_literal(Some(r))) {
            match (operator, &a, &b) {
                (Some("="), Literal::Str(a), Literal::Str(b)) => return Certain::Bool(a == b),
                (Some("<>" | "!="), Literal::Str(a), Literal::Str(b)) => {
                    return Certain::Bool(a != b);
                }
                _ => {}
            }
        }
    }
    // Any other operator over constants folds to a constant, if every
    // implementation is immutable; its value (and NULL-ness) is unknown.
    if operator.is_some_and(|op| operator_certain(op, snapshot, ProVolatile::Immutable, false)) {
        Certain::UnknownValue
    } else {
        Certain::Not
    }
}

/// Whether every operator named `op`'s implementation is at most
/// `volatility` (and, when `strict`, strict).
fn operator_certain(op: &str, snapshot: &PgCatalog, volatility: ProVolatile, strict: bool) -> bool {
    let mut any = false;
    for o in snapshot.pg_operator.values().filter(|o| o.oprname == op) {
        any = true;
        let Some(p) = o.oprcode.and_then(|c| snapshot.pg_proc.get(&c)) else {
            return false;
        };
        if !at_most(p.provolatile, volatility) || (strict && !p.proisstrict) {
            return false;
        }
    }
    any
}

/// Whether volatility `v` is at most `limit`.
fn at_most(v: ProVolatile, limit: ProVolatile) -> bool {
    match limit {
        ProVolatile::Immutable => v == ProVolatile::Immutable,
        ProVolatile::Stable => v != ProVolatile::Volatile,
        ProVolatile::Volatile => true,
    }
}

fn certain_func_call(fc: &ast::FuncCall, snapshot: &PgCatalog) -> Certain {
    if fc.over.is_some() || fc.agg_filter.is_some() || fc.agg_star || !fc.agg_order.is_empty() {
        return Certain::Not;
    }
    let names = extract_names(&fc.funcname);
    let (schema, name) = match names.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Certain::Not,
    };
    let candidates = snapshot.find_functions(schema, name);
    if candidates.is_empty() || candidates.iter().any(|p| p.proretset) {
        return Certain::Not;
    }
    let args: Vec<Certain> = fc
        .args
        .iter()
        .map(|a| certainly(a.as_ref(), snapshot))
        .collect();
    if args.contains(&Certain::Null) && candidates.iter().all(|p| p.proisstrict) {
        return Certain::Null;
    }
    if args.contains(&Certain::Not) {
        return Certain::Not;
    }
    if candidates
        .iter()
        .all(|p| p.provolatile == ProVolatile::Immutable)
    {
        // evaluate_function folds the call to a Const: an immutable
        // function may still return NULL.
        Certain::UnknownValue
    } else {
        Certain::Not
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
    select: Option<*const ast::SelectStmt>,
    /// Its FROM clause (an UPDATE's or DELETE's, with its target),
    /// borrowed alike.
    from: Option<*const [Option<ast::Node>]>,
    /// Every relation of the level has a row mark (`preprocess_rowmarks`):
    /// an UPDATE, a DELETE, a SELECT with a locking clause, or a level
    /// pulled up into one. Its row identity is needed above every join.
    row_marks: bool,
    /// How its select list is used.
    outputs: Outputs,
    /// The alias of the FROM item it is in the level around it.
    item: Option<String>,
    /// A level of its own (a statement, a scalar sublink's), whose outer
    /// joins no qual of a level around may reduce.
    own_level: bool,
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
    /// subquery it plans on its own (`set_subquery_pathlist`). Shared, so
    /// a level below borrows them without cloning the trees.
    static PUSHABLE: RefCell<Vec<Rc<[Option<ast::Node>]>>> = const { RefCell::new(Vec::new()) };
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
    Select(&'a ast::SelectStmt),
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
        sel: &'a ast::SelectStmt,
        ctes: impl IntoIterator<Item = &'a String>,
        snapshot: &PgCatalog,
    ) -> Self {
        let names = cte_list(
            ctes,
            sel.with_clause.as_deref(),
            typedpg_pg_query::NodeRef::SelectStmt(sel),
            None,
            snapshot,
        );
        Self::enter_level(
            sel.where_clause.as_ref(),
            &sel.from_clause,
            &sel.from_clause,
            sel.having_clause.as_ref(),
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
        where_clause: Option<&ast::Node>,
        from: &[Option<ast::Node>],
        target: Option<&ast::RangeVar>,
        with: Option<&'a ast::WithClause>,
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
    pub(crate) fn enter_merge_source(source: &Option<ast::Node>) -> Self {
        let quals = if super::plan_time_checks() {
            LevelQuals {
                from: Some(std::ptr::from_ref(std::slice::from_ref(source))),
                row_marks: true,
                ..LevelQuals::default()
            }
        } else {
            LevelQuals::default()
        };
        PUSHABLE.with(|p| p.borrow_mut().push(Rc::new([])));
        LEVELS.with(|l| l.borrow_mut().push(quals));
        PlanLevel
    }

    fn enter_level(
        where_clause: Option<&ast::Node>,
        from: &[Option<ast::Node>],
        own_from: &[Option<ast::Node>],
        having: Option<&ast::Node>,
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
                    p.op != ast::SetOperation::SetopNone
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
                        && (sel.op != ast::SetOperation::SetopNone
                            || sel.having_clause.is_some()
                            || sel.limit_offset.is_some()
                            || !sel.locking_clause.is_empty()
                            || sel.target_list.iter().any(|t| {
                                t.as_ref().is_some_and(|t| {
                                    t.nodes().any(|(n, _)| {
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
                            || sel.distinct_clause.iter().any(|d| d.is_none())
                            || (sel.op != ast::SetOperation::SetopNone && !sel.all)) =>
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
                flatten_and(Some(w), &mut conjuncts);
            }
            for f in from {
                join_quals(f.as_ref(), &mut conjuncts);
            }
            // subquery_planner moves a HAVING qual without columns (nor
            // aggregates) into WHERE.
            let mut having_quals = Vec::new();
            if let Some(h) = having {
                flatten_and(Some(h), &mut having_quals);
                having_quals.retain(|q| fold_qual_node(*q, snapshot).may_be_const());
            }
            let mut dummy_quals = conjuncts.clone();
            dummy_quals.extend(having_quals.iter().copied());
            // The quals the level around pushes down into this one, which
            // it plans on its own.
            let pushed = match next {
                NextLevel::Separate { pushed_down: true } => {
                    PUSHABLE.with(|p| p.borrow().last().cloned())
                }
                _ => None,
            };
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
            group
                .quals
                .extend(having_quals.into_iter().map(|q| (q, Owner::Level, true)));
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
                NextLevel::Separate { .. } => {
                    group.quals.extend(
                        pushed
                            .iter()
                            .flat_map(|p| p.iter())
                            .map(|q| (q.as_ref(), Owner::Level, true)),
                    );
                    (group.rels, false)
                }
            };
            let pushable = conjuncts
                .iter()
                .filter(|c| {
                    fold_qual_node(**c, snapshot) == Folded::Var && may_prove_empty(**c, snapshot)
                })
                .map(|c| c.cloned())
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
                own_level,
                ctes: cte_names,
            }
        } else {
            PUSHABLE.with(|p| p.borrow_mut().push(Rc::new([])));
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
struct Group<'a> {
    /// The quals the planner distributes over the level's join tree, each
    /// with where it comes from (see [`Owner`]) and whether it is one of
    /// the top join domain's (WHERE, an inner join's ON, never below the
    /// nullable side of an outer join nor in a semi-join's condition).
    quals: Vec<(Option<&'a ast::Node>, Owner, bool)>,
    /// The aliases of the level's own FROM items (tables, subqueries,
    /// functions — not joins), which a column proves empty.
    aliases: Vec<String>,
    /// The tables among them.
    tables: Vec<crate::oid::PgClassOid>,
    /// The level's own tables by alias, and whether the scan also reads
    /// inheritance children (of a table that isn't partitioned).
    table_aliases: Vec<(String, crate::oid::PgClassOid, bool)>,
    /// The outputs of the level's own pulled-up subqueries (inlined CTEs,
    /// views) that pass a table's column through, by `(alias, output)`:
    /// the planner reads that table column instead.
    passthrough: Vec<((String, String), crate::oid::PgClassOid, String)>,
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
    on_quals: Vec<Option<&'a ast::Node>>,
    /// The aliases of the level's own FROM items on the nullable side of
    /// an outer join.
    nullable: Vec<String>,
    /// The level is one of its own, whose outer joins no qual of a level
    /// around may reduce (see [`Self::null_extended`]).
    own_level: bool,
}

impl<'a> Group<'a> {
    /// The quals `where_clause` and FROM items `from` of a query level (or
    /// of a subquery pulled up into it), in the top join domain when
    /// `top`.
    fn walk(
        &mut self,
        where_clause: Option<&'a ast::Node>,
        from: &'a [Option<ast::Node>],
        ctes: &[Cte<'a>],
        snapshot: &'a PgCatalog,
        owner: &Owner,
        top: bool,
    ) {
        let mut quals = Vec::new();
        if let Some(w) = where_clause {
            flatten_and(Some(w), &mut quals);
        }
        self.quals
            .extend(quals.iter().map(|&q| (q, owner.clone(), top)));
        let on_quals = self.on_quals.len();
        for f in from {
            self.item(f.as_ref(), true, ctes, snapshot, owner, top);
        }
        if !matches!(owner, Owner::Level) {
            return;
        }
        // The IN sublinks `pull_up_sublinks` turns into semi-joins: their
        // subqueries join the level (any qual in them may prove empty
        // what can't be told).
        let mut joined = quals;
        // Those of a kept inner join's ON clause too, above that join —
        // whose own quals don't reach their relations: they are not
        // counted for the level.
        let on_start = joined.len();
        joined.extend(self.on_quals.drain(on_quals..));
        let owner = Owner::SemiJoin;
        for (i, q) in joined.iter().enumerate() {
            let Some(sub) = conjunct_sublink(*q) else {
                continue;
            };
            let Some(ast::Node::SelectStmt(sel)) = sub.subselect.as_ref() else {
                continue;
            };
            let rels = self.rels;
            let level = |cr: &ast::ColumnRef| self.reads_level(cr, snapshot);
            if joined_any_sublink(sub, snapshot, &level) {
                self.subquery(sel, true, ctes, snapshot, &owner, false);
            } else if joined_exists_sublink(sub, snapshot, &level) {
                // Its FROM items join the level, its WHERE is their
                // join's qual.
                self.walk(
                    sel.where_clause.as_ref(),
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
    fn reads_level(&self, cr: &ast::ColumnRef, snapshot: &PgCatalog) -> bool {
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
        sel: &'a ast::SelectStmt,
        kept: bool,
        ctes: &[Cte<'a>],
        snapshot: &'a PgCatalog,
        owner: &Owner,
        top: bool,
    ) {
        if !kept {
            return;
        }
        match crate::resolve::plan_of(sel, snapshot) {
            crate::resolve::Plan::PulledUp => {
                // An output passing a column of its one table through.
                if let Owner::Item { alias, .. } = owner
                    && let [Some(ast::Node::RangeVar(rv))] = sel.from_clause.as_slice()
                    && let Some(class) = snapshot.resolve_table(
                        (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str()),
                        &rv.relname,
                    )
                    && (class.relkind == RelKind::Partitioned
                        || !rv.inh
                        || crate::ddl::tables::inherit::children_of(snapshot, class.oid).is_empty())
                {
                    for t in &sel.target_list {
                        let Some(ast::Node::ResTarget(rt)) = t.as_ref() else {
                            continue;
                        };
                        let Some(ast::Node::ColumnRef(cr)) = rt.val.as_ref() else {
                            continue;
                        };
                        let names = extract_names(&cr.fields);
                        if names.len() != cr.fields.len() {
                            continue;
                        }
                        let Some(column) = names.last() else {
                            continue;
                        };
                        let output = if rt.name.is_empty() {
                            column.clone()
                        } else {
                            rt.name.clone()
                        };
                        self.passthrough
                            .push(((alias.clone(), output), class.oid, column.clone()));
                    }
                }
                // Only an output that is no column of its own may fold.
                self.substitutes |= !sel.values_lists.is_empty()
                    || sel.target_list.iter().any(|t| {
                        !matches!(t.as_ref(), Some(ast::Node::ResTarget(rt))
                            if matches!(rt.val.as_ref(),
                                Some(ast::Node::ColumnRef(_))))
                    });
                self.walk(
                    sel.where_clause.as_ref(),
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
        n: Option<&'a ast::Node>,
        kept: bool,
        ctes: &[Cte<'a>],
        snapshot: &'a PgCatalog,
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
        match n {
            Some(ast::Node::RangeVar(rv)) => {
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
                    let body = match cte.def.and_then(|c| c.ctequery.as_ref()) {
                        Some(ast::Node::SelectStmt(sel)) => Some(sel),
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
                            let alias = rv
                                .alias
                                .as_ref()
                                .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
                            let children = rv.inh
                                && class.relkind != RelKind::Partitioned
                                && !crate::ddl::tables::inherit::children_of(snapshot, class.oid)
                                    .is_empty();
                            self.table_aliases.push((alias, class.oid, children));
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
                    RelKind::View => {
                        match crate::ddl::views::stored_view_query(snapshot, class.oid) {
                            // A view's query sees no CTE of the statement.
                            Some(ast::Node::SelectStmt(sel)) => {
                                self.views.push(class.oid);
                                // The stored query's names were resolved
                                // when the view was defined: looking into it
                                // again requires no schema's USAGE.
                                crate::privileges::suspended(|| {
                                    self.subquery(sel, kept, &[], snapshot, owner, top);
                                });
                                self.views.pop();
                            }
                            _ => self.unknown_quals = true,
                        }
                    }
                    _ => self.unknown_quals |= kept,
                }
            }
            Some(ast::Node::RangeSubselect(sub)) => {
                // (An unaliased subquery is named by nothing.)
                let alias = sub.alias.as_ref().map_or("", |a| a.aliasname.as_str());
                record(alias);
                let owner = &own(alias, sub.lateral);
                if let Some(ast::Node::SelectStmt(sel)) = sub.subquery.as_ref() {
                    self.subquery(sel, kept, ctes, snapshot, owner, top);
                }
            }
            Some(ast::Node::JoinExpr(j)) => {
                let inner = j.jointype == ast::JoinType::JoinInner;
                let (left, right) = match j.jointype {
                    ast::JoinType::JoinInner => (kept, kept),
                    ast::JoinType::JoinLeft => (kept, false),
                    ast::JoinType::JoinRight => (false, kept),
                    _ => (false, false),
                };
                if let Some(l) = j.larg.as_ref() {
                    self.item(Some(l), left, ctes, snapshot, owner, top);
                }
                if let Some(r) = j.rarg.as_ref() {
                    self.item(Some(r), right, ctes, snapshot, owner, top);
                }
                if let Some(q) = j.quals.as_ref() {
                    let mut quals = Vec::new();
                    flatten_and(Some(q), &mut quals);
                    // An outer join's quals are its nullable side's join
                    // domain's.
                    let in_top = top && kept && inner;
                    if inner && kept && level_own {
                        self.on_quals.extend(quals.iter().copied());
                    }
                    self.quals
                        .extend(quals.into_iter().map(|q| (q, owner.clone(), in_top)));
                }
            }
            // A function the planner may inline as a subquery it pulls up.
            Some(ast::Node::RangeFunction(rf)) => {
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
        // PostgreSQL 19 folds `col IS NULL` over a column proven non-NULL
        // to FALSE (expr_is_nonnullable) before distributing the quals: a
        // constant like any other, no relation's restriction.
        let constant = self.unknown_quals
            || (self.substitutes && self.quals.iter().any(|(_, o, top)| level_wide(o, *top)))
            || self.quals.iter().any(|(q, o, top)| {
                let folded = fold_qual(*q, snapshot);
                level_wide(o, *top)
                    && ((folded.may_be_const() && folded != Folded::Bool(true))
                        || self.folded_null_test(*q, snapshot))
            });
        let column_quals: Vec<(Option<&ast::Node>, &Owner, bool)> = self
            .quals
            .iter()
            .filter(|(q, _, _)| !fold_qual(*q, snapshot).may_be_const())
            .map(|(q, o, top)| (*q, o, *top))
            .collect();
        let mut proven = Proven::default();
        let mut equalities = Vec::new();
        for (q, owner, top) in &column_quals {
            match q.as_ref() {
                // `col IS NULL` over a column proven non-NULL is folded
                // (above). One the folding left alone — on an outer join's
                // nullable side — proves its table empty
                // (`restriction_is_always_false`) once another qual reduces
                // the join to an inner one.
                Some(ast::Node::NullTest(t))
                    if t.nulltesttype == ast::NullTestType::IsNull
                        && !self.folded_null_test(*q, snapshot)
                        && self.not_null_table_column(t, snapshot) =>
                {
                    if !(*top && matches!(owner, Owner::Level) && self.null_extended(t)) {
                        self.prove_about(*q, owner, &mut proven);
                    }
                }
                // Two constants an equivalence class of the top join domain
                // equates make a variable-free qual applied over the
                // domain (`generate_base_implied_equalities_const`,
                // `process_implied_equality`): of several relations, a
                // gating qual above their join, which proves none of them
                // empty.
                Some(ast::Node::AExpr(e))
                    if extract_names(&e.name) == ["="] && !(*top && rels >= 2) =>
                {
                    equalities.push((*q, *owner));
                }
                _ => {}
            }
        }
        // An equivalence class's members may be any equality's columns.
        let quals: Vec<Option<&ast::Node>> = equalities.iter().map(|(q, _)| *q).collect();
        if equates_constants(&quals, snapshot) {
            for (q, owner) in equalities {
                self.prove_about(q, owner, &mut proven);
            }
        }
        proven.whole |= constant && rels < 2;
        proven
    }

    /// Whether qual `q` is `col IS NULL` over a column PostgreSQL 19's
    /// constant folding proves non-NULL (`var_is_nonnullable`): of one of
    /// the level's tables (not an inheritance parent scanning children),
    /// with NOT NULL, and on no outer join's nullable side.
    fn folded_null_test(&self, q: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
        let Some(ast::Node::NullTest(t)) = q else {
            return false;
        };
        t.nulltesttype == ast::NullTestType::IsNull && self.null_test_column(t, snapshot, false)
    }

    /// Whether NullTest `t` tests a NOT NULL column of one of the level's
    /// tables (not an inheritance parent scanning children), wherever it
    /// is (`restriction_is_always_false`'s `NOTNULL_SOURCE_RELOPT`).
    fn not_null_table_column(&self, t: &ast::NullTest, snapshot: &PgCatalog) -> bool {
        self.null_test_column(t, snapshot, true)
    }

    /// [`Self::folded_null_test`] / [`Self::not_null_table_column`]: the
    /// column NullTest `t` tests is a NOT NULL one of the level's tables,
    /// on no outer join's nullable side unless `nullable_ok`.
    fn null_test_column(&self, t: &ast::NullTest, snapshot: &PgCatalog, nullable_ok: bool) -> bool {
        let Some(ast::Node::ColumnRef(cr)) = t.arg.as_ref() else {
            return false;
        };
        let names = extract_names(&cr.fields);
        if names.len() != cr.fields.len() {
            return false;
        }
        let not_null = |relid: crate::oid::PgClassOid, col: &str| {
            snapshot
                .attributes_of(relid)
                .iter()
                .any(|a| a.attname == col && a.attnum > 0 && a.attnotnull)
        };
        match names.as_slice() {
            [alias, col] => {
                (nullable_ok || !self.nullable.contains(alias))
                    && (self
                        .table_aliases
                        .iter()
                        .find(|(a, ..)| a == alias)
                        .is_some_and(|(_, relid, children)| !children && not_null(*relid, col))
                        || self
                            .passthrough
                            .iter()
                            .find(|((a, o), ..)| a == alias && o == col)
                            .is_some_and(|(_, relid, column)| not_null(*relid, column)))
            }
            [col] => {
                let mut owners = self.table_aliases.iter().filter(|(_, relid, _)| {
                    snapshot
                        .attributes_of(*relid)
                        .iter()
                        .any(|a| &a.attname == col && a.attnum > 0)
                });
                match (owners.next(), owners.next()) {
                    (Some((alias, relid, children)), None) => {
                        (nullable_ok || !self.nullable.contains(alias))
                            && !children
                            && not_null(*relid, col)
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Whether `t`, an `alias.col IS NULL` qual of the level's top join
    /// domain, tests a column of a FROM item certainly on the nullable
    /// side of an outer join there: then it is no restriction of that
    /// item's relation (the column, null-extended, may be NULL), and
    /// proves nothing empty. Not when another qual of the domain reads
    /// such an item (or an unqualified column): strict, it may reduce the
    /// outer join to an inner one (`reduce_outer_joins`).
    fn null_extended(&self, t: &ast::NullTest) -> bool {
        let Some(ast::Node::ColumnRef(cr)) = t.arg.as_ref() else {
            return false;
        };
        let names = extract_names(&cr.fields);
        let [alias, _] = names.as_slice() else {
            return false;
        };
        if !self.own_level || names.len() != cr.fields.len() || !self.nullable.contains(alias) {
            return false;
        }
        let tested = |q: Option<&ast::Node>| {
            matches!(q, Some(ast::Node::NullTest(t))
                if t.nulltesttype == ast::NullTestType::IsNull
                    && matches!(t.arg.as_ref(),
                        Some(ast::Node::ColumnRef(_))))
        };
        !self.quals.iter().any(|(q, _, top)| {
            *top && !tested(*q)
                && q.is_some_and(|q| {
                    q.nodes().any(|(n, _)| match n {
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
    fn prove_about(&self, n: Option<&ast::Node>, owner: &Owner, proven: &mut Proven) {
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
        let Some(inner) = n else {
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
    with: Option<&'a ast::WithClause>,
    root: typedpg_pg_query::NodeRef<'_>,
    target: Option<&ast::RangeVar>,
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
        let Some(ast::Node::CommonTableExpr(c)) = c.as_ref() else {
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
    def: Option<&'a ast::CommonTableExpr>,
    /// The planner certainly inlines it (see [`crate::resolve::inlined`]).
    inlined: bool,
    /// The planner certainly doesn't: a reference to it is a CTE scan.
    scanned: bool,
}

/// Whether a reference to CTE `cte` is a relation of its own, never pulled
/// up: a CTE scan (MATERIALIZED, data-modifying), or a subquery the planner
/// plans on its own once inlined.
fn scanned_alone(cte: &ast::CommonTableExpr, snapshot: &PgCatalog) -> bool {
    match cte.ctequery.as_ref() {
        Some(ast::Node::SelectStmt(sel)) => {
            cte.ctematerialized == ast::CteMaterialize::Always
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

fn flatten_and<'a>(n: Option<&'a ast::Node>, out: &mut Vec<Option<&'a ast::Node>>) {
    if let Some(ast::Node::BoolExpr(b)) = n
        && b.boolop == ast::BoolExprType::AndExpr
    {
        for a in &b.args {
            flatten_and(a.as_ref(), out);
        }
        return;
    }
    out.push(n);
}

fn join_quals<'a>(n: Option<&'a ast::Node>, out: &mut Vec<Option<&'a ast::Node>>) {
    if let Some(ast::Node::JoinExpr(j)) = n {
        for side in [&j.larg, &j.rarg].into_iter().flatten() {
            join_quals(Some(side), out);
        }
        if let Some(q) = j.quals.as_ref() {
            flatten_and(Some(q), out);
        }
    }
}

/// A key identifying a literal's value, for telling two constants apart.
fn literal_key(n: Option<&ast::Node>) -> Option<String> {
    match n? {
        ast::Node::AConst(c) if !c.isnull => Some(match c.val.as_ref()? {
            ast::AConstValue::Integer(i) => format!("i{}", i.ival),
            ast::AConstValue::Float(f) => format!("f{}", f.fval),
            ast::AConstValue::String(s) => format!("s{}", s.sval),
            ast::AConstValue::Boolean(b) => format!("b{}", b.boolval),
            ast::AConstValue::BitString(b) => format!("x{}", b.bsval),
        }),
        _ => None,
    }
}

/// Whether `c`, a qual over columns, is one [`may_be_empty`] may prove a
/// relation empty with: `col IS NULL`, or `expr = constant`.
fn may_prove_empty(c: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
    match c {
        Some(ast::Node::NullTest(t)) => {
            t.nulltesttype == ast::NullTestType::IsNull
                && matches!(t.arg.as_ref(), Some(ast::Node::ColumnRef(_)))
        }
        Some(ast::Node::AExpr(e)) if extract_names(&e.name) == ["="] => match e.kind {
            ast::AExprKind::AexprOp => [&e.lexpr, &e.rexpr]
                .into_iter()
                .flatten()
                .any(|s| fold_node(Some(s), snapshot).may_be_const()),
            ast::AExprKind::AexprIn => matches!(
                e.rexpr.as_ref(),
                Some(ast::Node::List(l)) if l.items.len() == 1
            ),
            _ => false,
        },
        _ => false,
    }
}

/// Whether the planner may prove the level's relations empty from its
/// quals: one folds to a constant other than TRUE, one is `col IS NULL`
/// (FALSE over a NOT NULL column), or two `expr = constant` may put two
/// different constants in one equivalence class.
fn may_be_empty(conjuncts: &[Option<&ast::Node>], snapshot: &PgCatalog) -> bool {
    for c in conjuncts {
        let folded = fold_qual_node(*c, snapshot);
        if folded.may_be_const() && folded != Folded::Bool(true) {
            return true;
        }
        if let Some(ast::Node::NullTest(t)) = c
            && t.nulltesttype == ast::NullTestType::IsNull
            && matches!(t.arg.as_ref(), Some(ast::Node::ColumnRef(_)))
        {
            return true;
        }
    }
    equates_constants(conjuncts, snapshot)
}

/// Whether two `expr = constant` of `conjuncts` may put two different
/// constants in one equivalence class.
fn equates_constants(conjuncts: &[Option<&ast::Node>], snapshot: &PgCatalog) -> bool {
    let mut equated: Vec<Option<String>> = Vec::new();
    for c in conjuncts {
        match c {
            Some(ast::Node::AExpr(e)) if extract_names(&e.name) == ["="] => {
                let constant = match e.kind {
                    ast::AExprKind::AexprOp => [&e.lexpr, &e.rexpr]
                        .into_iter()
                        .flatten()
                        .find(|s| fold_node(Some(s), snapshot).may_be_const())
                        .map(Some),
                    ast::AExprKind::AexprIn => match e.rexpr.as_ref() {
                        Some(ast::Node::List(l)) if l.items.len() == 1 => {
                            l.items.first().map(Option::as_ref)
                        }
                        _ => None,
                    },
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
pub(crate) fn read_above(alias: &str, join: &ast::JoinExpr, snapshot: &PgCatalog) -> bool {
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
        reads.above(alias, NodeKey::of(join), 0)
    })
}

/// Whether join `j` certainly equates its sides: NATURAL, USING, or an
/// `=` in its ON clause. Without one, nothing proves a table on its
/// nullable side unique for it (`rel_is_distinct_for` needs a
/// mergejoinable equality, of the join or of the relation's own
/// restrictions — both in the ON clause here).
pub(crate) fn join_equates(j: &ast::JoinExpr) -> bool {
    j.is_natural
        || !j.using_clause.is_empty()
        || j.quals.as_ref().is_some_and(|q| {
            q.nodes().any(|(n, _)| {
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
    j: &ast::JoinExpr,
    alias: &str,
    relid: crate::oid::PgClassOid,
    snapshot: &PgCatalog,
) -> bool {
    let using = extract_names(&j.using_clause);
    let mut equated: Vec<String> = Vec::new();
    if let Some(q) = j.quals.as_ref() {
        for (n, _) in q.nodes() {
            let typedpg_pg_query::NodeRef::AExpr(e) = n else {
                continue;
            };
            if extract_names(&e.name).last().is_none_or(|o| o != "=") {
                continue;
            }
            for side in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                if let ast::Node::ColumnRef(cr) = side {
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
    fn matches(&self, cr: &ast::ColumnRef) -> bool {
        let [a, c] = cr.fields.as_slice() else {
            return false;
        };
        matches!(a.as_ref(), Some(ast::Node::String(a)) if a.sval == self.alias)
            && match c.as_ref() {
                Some(ast::Node::String(c)) => self.column.is_none_or(|col| c.sval == col),
                Some(ast::Node::AStar(_)) => true,
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
fn certainly_reads(n: Option<&ast::Node>, target: &Target<'_>, snapshot: &PgCatalog) -> bool {
    let Some(inner) = n else {
        return false;
    };
    !fold_node(n, snapshot).known() && reads_ref(inner.to_ref(), target, snapshot)
}

fn reads_ref(n: typedpg_pg_query::NodeRef<'_>, target: &Target<'_>, snapshot: &PgCatalog) -> bool {
    use typedpg_pg_query::NodeRef as R;
    match n {
        R::ColumnRef(cr) => target.matches(cr),
        R::SubLink(sub) => {
            certainly_reads(sub.testexpr.as_ref(), target, snapshot)
                || matches!(sub.subselect.as_ref(),
                Some(ast::Node::SelectStmt(sel)) if sublink_reads(
                    sel,
                    sub.sub_link_type == ast::SubLinkType::ExistsSublink,
                    target,
                    snapshot,
                ))
        }
        R::SelectStmt(_) | R::RangeSubselect(_) | R::CaseExpr(_) => false,
        // A NULL argument is dropped, and so is every one after an
        // argument that may fold to another constant.
        R::CoalesceExpr(c) => {
            for a in &c.args {
                match fold(a.as_ref(), snapshot) {
                    Folded::Null => {}
                    Folded::Var => {
                        if certainly_reads(a.as_ref(), target, snapshot) {
                            return true;
                        }
                    }
                    _ => return false,
                }
            }
            false
        }
        R::BoolExpr(b) => {
            let identity = match b.boolop {
                ast::BoolExprType::AndExpr => Some(Folded::Bool(true)),
                ast::BoolExprType::OrExpr => Some(Folded::Bool(false)),
                ast::BoolExprType::NotExpr => None,
            };
            if identity.is_some()
                && b.args.iter().any(|a| {
                    let f = fold(a.as_ref(), snapshot);
                    (f != Folded::Var && Some(f) != identity) || params_only(a.as_ref())
                })
            {
                return false;
            }
            b.args
                .iter()
                .any(|a| certainly_reads(a.as_ref(), target, snapshot))
        }
        R::AExpr(e) => {
            let mut operands: Vec<Option<&ast::Node>> = Vec::new();
            for side in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                match side {
                    ast::Node::List(l) => operands.extend(l.items.iter().map(Option::as_ref)),
                    _ => operands.push(Some(side)),
                }
            }
            !operands
                .iter()
                .any(|o| fold_node(*o, snapshot) == Folded::Null || params_only(*o))
                && operands
                    .iter()
                    .any(|o| certainly_reads(*o, target, snapshot))
        }
        R::FuncCall(fc) => {
            !fc.args
                .iter()
                .any(|a| fold(a.as_ref(), snapshot) == Folded::Null || params_only(a.as_ref()))
                && n.children().any(|c| reads_ref(c, target, snapshot))
        }
        _ => n.children().any(|c| reads_ref(c, target, snapshot)),
    }
}

/// Whether `n` reads parameters but no column nor sublink: a custom plan
/// has them as constants (NULL ones, maybe), which it folds.
fn params_only(n: Option<&ast::Node>) -> bool {
    let Some(inner) = n else {
        return false;
    };
    let nodes: Vec<_> = inner.nodes().collect();
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
    sel: &ast::SelectStmt,
    exists: bool,
    target: &Target<'_>,
    snapshot: &PgCatalog,
) -> bool {
    if sel.op != ast::SetOperation::SetopNone || !sel.values_lists.is_empty() {
        return false;
    }
    fn hides(n: Option<&ast::Node>, alias: &str) -> bool {
        match n {
            Some(ast::Node::RangeVar(rv)) => {
                rv.alias
                    .as_ref()
                    .map_or(rv.relname.as_str(), |a| &a.aliasname)
                    == alias
            }
            Some(ast::Node::RangeSubselect(s)) => {
                s.alias.as_ref().is_some_and(|a| a.aliasname == alias)
            }
            Some(ast::Node::RangeFunction(f)) => {
                f.alias.as_ref().is_none_or(|a| a.aliasname == alias)
            }
            Some(ast::Node::JoinExpr(j)) => {
                j.alias.as_ref().is_some_and(|a| a.aliasname == alias)
                    || [&j.larg, &j.rarg]
                        .into_iter()
                        .flatten()
                        .any(|s| hides(Some(s), alias))
            }
            // Anything else may be named so.
            _ => true,
        }
    }
    if sel
        .from_clause
        .iter()
        .any(|f| hides(f.as_ref(), target.alias))
    {
        return false;
    }
    // The ON clauses of its joins no join removal drops.
    fn on_reads(n: Option<&ast::Node>, target: &Target<'_>, snapshot: &PgCatalog) -> bool {
        let Some(ast::Node::JoinExpr(j)) = n else {
            return false;
        };
        let (quals, left, right) = match j.jointype {
            ast::JoinType::JoinInner | ast::JoinType::JoinFull => (true, true, true),
            ast::JoinType::JoinLeft => (false, true, false),
            ast::JoinType::JoinRight => (false, false, true),
            _ => return false,
        };
        (quals && certainly_reads(j.quals.as_ref(), target, snapshot))
            || (left && on_reads(j.larg.as_ref(), target, snapshot))
            || (right && on_reads(j.rarg.as_ref(), target, snapshot))
    }
    sel.where_clause
        .as_ref()
        .into_iter()
        .chain(sel.having_clause.as_ref())
        .map(Some)
        .chain(
            sel.target_list
                .iter()
                .filter(|_| !exists)
                .map(Option::as_ref),
        )
        .any(|c| certainly_reads(c, target, snapshot))
        || sel
            .from_clause
            .iter()
            .any(|f| on_reads(f.as_ref(), target, snapshot))
}

/// What a query level's read checks look at (see [`LevelQuals::reads`]).
#[derive(Clone, Copy)]
struct Reads<'a> {
    /// The level's SELECT, when it is one.
    select: Option<&'a ast::SelectStmt>,
    /// Its FROM clause (an UPDATE's or DELETE's with its target).
    from: &'a [Option<ast::Node>],
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
    fn above(&self, alias: &str, join: NodeKey<ast::JoinExpr>, depth: usize) -> bool {
        fn holds(n: Option<&ast::Node>, join: NodeKey<ast::JoinExpr>) -> bool {
            let Some(ast::Node::JoinExpr(j)) = n else {
                return false;
            };
            NodeKey::of(&**j) == join
                || [&j.larg, &j.rarg]
                    .into_iter()
                    .flatten()
                    .any(|s| holds(Some(s), join))
        }
        // A join the level's FROM clause doesn't hold (a view's, a CTE's):
        // nothing is known.
        if !self.from.iter().any(|f| holds(f.as_ref(), join)) {
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
            .any(|f| self.join_reads(f.as_ref(), Some(join), &target, depth))
            || self
                .select
                .is_some_and(|sel| self.clauses_read(sel, &target))
    }

    /// Whether the ON clause of a join in FROM item `n` — but `join` (if any)
    /// and what is below it — certainly reads `target`, the join being never
    /// removed.
    fn join_reads(
        &self,
        n: Option<&ast::Node>,
        join: Option<NodeKey<ast::JoinExpr>>,
        target: &Target<'_>,
        depth: usize,
    ) -> bool {
        let Some(ast::Node::JoinExpr(j)) = n else {
            return false;
        };
        if Some(NodeKey::of(&**j)) == join {
            return false;
        }
        let reads = || certainly_reads(j.quals.as_ref(), target, self.snapshot);
        let kept = || match j.jointype {
            ast::JoinType::JoinInner | ast::JoinType::JoinFull => true,
            ast::JoinType::JoinLeft => self.side_kept(j, j.rarg.as_ref(), depth),
            ast::JoinType::JoinRight => self.side_kept(j, j.larg.as_ref(), depth),
            _ => false,
        };
        (reads() && kept())
            || [&j.larg, &j.rarg]
                .into_iter()
                .flatten()
                .any(|s| self.join_reads(Some(s), join, target, depth))
    }

    /// Whether `remove_useless_joins` certainly keeps outer join `j` of
    /// the level, whose nullable side is `side`: a table the join can't
    /// prove unique (see [`join_may_prove_unique`]), a subquery planned
    /// on its own whose rows may not be distinct, a FROM item read above
    /// the join, or one a sublink of its ON clause is pulled up into (see
    /// [`Self::pulls_sublink_into`]).
    fn side_kept(&self, j: &ast::JoinExpr, side: Option<&ast::Node>, depth: usize) -> bool {
        let alias = match side {
            Some(ast::Node::RangeVar(rv)) => {
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
            Some(ast::Node::RangeSubselect(s)) => {
                // Planned on its own, a subquery is proven unique only by
                // what makes its rows distinct (`query_is_distinct_for`).
                if let Some(ast::Node::SelectStmt(sel)) = s.subquery.as_ref()
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
        self.pulls_sublink_into(j, alias)
            || (depth < 3 && self.above(alias, NodeKey::of(j), depth + 1))
    }

    /// Whether `pull_up_sublinks` certainly turns a sublink of outer join
    /// `j`'s ON clause into a semi-join in its nullable side, FROM item
    /// `alias` (see [`with_nullable_on_clause`]): the side is then a join,
    /// which `join_is_removable` never removes (nor would it, the
    /// semi-join reading the side above the join). Only references
    /// qualified by `alias` count as the side's, and the sublink's query
    /// must certainly read nothing else of the level.
    fn pulls_sublink_into(&self, j: &ast::JoinExpr, alias: &str) -> bool {
        let Some(quals) = j.quals.as_ref() else {
            return false;
        };
        let side_column = |cr: &ast::ColumnRef| {
            let names = extract_names(&cr.fields);
            names.len() == cr.fields.len() && matches!(names.as_slice(), [t, _] if t == alias)
        };
        // The tested expression reads the side alone.
        let test_reads_side = |sub: &ast::SubLink| {
            sub.testexpr.as_ref().is_none_or(|t| {
                t.nodes().all(|(n, _)| match n {
                    typedpg_pg_query::NodeRef::SubLink(_) => false,
                    typedpg_pg_query::NodeRef::ColumnRef(cr) => side_column(cr),
                    _ => true,
                })
            })
        };
        let mut conjuncts = Vec::new();
        flatten_and(Some(quals), &mut conjuncts);
        conjuncts.into_iter().any(|q| {
            conjunct_sublink_node(q).is_some_and(|sub| {
                (joined_any_sublink(sub, self.snapshot, &side_column)
                    || joined_exists_sublink(sub, self.snapshot, &side_column))
                    && test_reads_side(sub)
                    && subquery_reads_only(
                        sub,
                        self.snapshot,
                        &|name| self.ctes.iter().any(|c| c == name),
                        &|t| t != alias,
                        &|_| true,
                    )
            })
        })
    }

    /// The relation `rv` names when it certainly is a table (no CTE of
    /// its name).
    fn plain_table(&self, rv: &ast::RangeVar) -> Option<crate::oid::PgClassOid> {
        plain_table(rv, self.snapshot, &|name| {
            self.ctes.iter().any(|c| c == name)
        })
    }

    /// Whether the clauses of `sel` (the level's SELECT) certainly read
    /// `target`. The select list counts as [`Outputs`] says: a trimmed
    /// one's entry only where the level around it reads that output. A
    /// WINDOW clause no window call uses is dropped with a subquery
    /// pulled up.
    fn clauses_read(&self, sel: &ast::SelectStmt, target: &Target<'_>) -> bool {
        let reads = |n: Option<&ast::Node>| certainly_reads(n, target, self.snapshot);
        let kept_clauses: &[&[Option<ast::Node>]] = match self.outputs {
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
            .as_ref()
            .into_iter()
            .chain(sel.having_clause.as_ref())
            .map(Some)
            .chain(
                kept_clauses
                    .iter()
                    .flat_map(|c| c.iter())
                    .map(Option::as_ref),
            )
            .any(reads)
        {
            return true;
        }
        match self.outputs {
            Outputs::Needed => {
                sel.target_list.iter().any(|t| {
                    matches!(t.as_ref(), Some(ast::Node::ResTarget(rt))
                        if matches!(rt.val.as_ref(),
                            Some(ast::Node::ColumnRef(cr)) if matches!(cr.fields.as_slice(),
                                [f] if matches!(f, Some(ast::Node::AStar(_))))))
                }) || sel.target_list.iter().any(|t| reads(t.as_ref()))
            }
            Outputs::Trimmed => sel
                .target_list
                .iter()
                .enumerate()
                .any(|(i, t)| reads(t.as_ref()) && self.output_read(sel, i)),
            Outputs::Exists => false,
        }
    }

    /// Whether the level around this one certainly reads output `i` of
    /// `sel`, the FROM item [`Self::item`] it is there (named as that
    /// item's column aliases, or the CTE's, say), in a clause that keeps
    /// it.
    fn output_read(&self, sel: &ast::SelectStmt, i: usize) -> bool {
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
                t.as_ref().is_some_and(|t| {
                    t.nodes()
                        .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::AStar(_)))
                })
            })
        {
            return false;
        }
        let Some(ast::Node::ResTarget(rt)) = sel.target_list[i].as_ref() else {
            return false;
        };
        fn find<'n>(n: Option<&'n ast::Node>, item: &str) -> Option<&'n ast::Node> {
            match n? {
                ast::Node::RangeVar(rv)
                    if rv
                        .alias
                        .as_ref()
                        .map_or(rv.relname.as_str(), |a| &a.aliasname)
                        == item =>
                {
                    n
                }
                ast::Node::RangeSubselect(s)
                    if s.alias.as_ref().is_some_and(|a| a.aliasname == item) =>
                {
                    n
                }
                ast::Node::JoinExpr(j) => [&j.larg, &j.rarg]
                    .into_iter()
                    .flatten()
                    .find_map(|s| find(Some(s), item)),
                _ => None,
            }
        }
        let strings = |names: &[Option<ast::Node>]| -> Vec<String> { extract_names(names) };
        let renames = match psel.from_clause.iter().find_map(|f| find(f.as_ref(), item)) {
            Some(ast::Node::RangeSubselect(s)) => s
                .alias
                .as_ref()
                .map(|a| strings(&a.colnames))
                .unwrap_or_default(),
            Some(ast::Node::RangeVar(rv)) => {
                let mut names = rv
                    .alias
                    .as_ref()
                    .map(|a| strings(&a.colnames))
                    .unwrap_or_default();
                if names.len() <= i {
                    // A CTE of the level's own WITH (any other's is unknown).
                    let cte = psel.with_clause.as_ref().and_then(|w| {
                        w.ctes.iter().find_map(|c| match c.as_ref() {
                            Some(ast::Node::CommonTableExpr(c)) if c.ctename == rv.relname => {
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
        } else if let Some(ast::Node::ColumnRef(cr)) = rt.val.as_ref()
            && let Some(ast::Node::String(s)) = cr.fields.last().and_then(|f| f.as_ref())
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
                .any(|f| outer.join_reads(f.as_ref(), None, &target, 0))
    }

    /// How many base relations on the nullable side of the level's outer
    /// joins are certainly kept: a table (or a subquery with a FROM
    /// clause, pulled up or planned on its own) that is a whole side never
    /// removed (see [`Self::side_kept`]), or a side of a FULL join — and
    /// the distinct tables of a side that is an inner join of two or more
    /// (`join_is_removable` needs a single relation on the nullable side,
    /// and an inner join's are all in its `min_righthand`; self-join
    /// elimination merges only scans of one table).
    fn held_relations(&self) -> usize {
        fn count(r: &Reads<'_>, n: Option<&ast::Node>) -> usize {
            let Some(ast::Node::JoinExpr(j)) = n else {
                return 0;
            };
            let one = |side: Option<&ast::Node>| -> usize {
                match side {
                    Some(ast::Node::RangeVar(rv)) => usize::from(r.plain_table(rv).is_some()),
                    Some(ast::Node::RangeSubselect(s)) => usize::from(matches!(
                        s.subquery.as_ref(),
                        Some(ast::Node::SelectStmt(sel)) if !sel.from_clause.is_empty()
                            && matches!(crate::resolve::plan_of(sel, r.snapshot),
                                crate::resolve::Plan::PulledUp | crate::resolve::Plan::Separate)
                    )),
                    _ => 0,
                }
            };
            let inner_tables = |side: Option<&ast::Node>| {
                side.map_or(0, |side| {
                    inner_join_tables_node(Some(side), r.snapshot, &|name| {
                        r.ctes.iter().any(|c| c == name)
                    })
                })
            };
            let joined = |side: Option<&ast::Node>| matches!(side, Some(ast::Node::JoinExpr(_)));
            let (l, rr) = (j.larg.as_ref(), j.rarg.as_ref());
            let held = match j.jointype {
                ast::JoinType::JoinLeft if joined(rr) => inner_tables(rr),
                ast::JoinType::JoinRight if joined(l) => inner_tables(l),
                ast::JoinType::JoinLeft if r.side_kept(j, rr, 0) => one(rr),
                ast::JoinType::JoinRight if r.side_kept(j, l, 0) => one(l),
                ast::JoinType::JoinFull => one(l) + one(rr) + inner_tables(l) + inner_tables(rr),
                _ => 0,
            };
            held + l.map_or(0, |s| count(r, Some(s))) + rr.map_or(0, |s| count(r, Some(s)))
        }
        self.from.iter().map(|f| count(self, f.as_ref())).sum()
    }
}

/// Whether a qual of the level around may reduce the outer joins of the
/// current one (`reduce_outer_joins` over the query it is pulled up into,
/// or over its own query with the quals `set_subquery_pathlist` pushes
/// down into it): the level is a FROM subquery, a view or a CTE there, and
/// a qual there (WHERE, a JOIN's ON) may read it — any qual, when its name
/// there isn't known; when the level around is no SELECT, to be sure.
pub(crate) fn outer_quals_may_reduce(snapshot: &PgCatalog) -> bool {
    LEVELS.with(|l| {
        let l = l.borrow();
        let Some((current, rest)) = l.split_last() else {
            return false;
        };
        let Some(parent) = rest.last() else {
            return false;
        };
        if current.own_level {
            return false;
        }
        // SAFETY: the level around this one is on the stack, its nodes
        // borrowed by the analysis.
        let Some(psel) = parent.select.map(|s| unsafe { &*s }) else {
            return true;
        };
        let mut quals = Vec::new();
        if let Some(w) = psel.where_clause.as_ref() {
            quals.push(w);
        }
        // A USING / NATURAL join's equalities, which aren't written.
        let mut using = false;
        fn join_quals<'n>(
            n: Option<&'n ast::Node>,
            out: &mut Vec<&'n ast::Node>,
            using: &mut bool,
        ) {
            if let Some(ast::Node::JoinExpr(j)) = n {
                out.extend(j.quals.as_ref());
                *using |= j.is_natural || !j.using_clause.is_empty();
                join_quals(j.larg.as_ref(), out, using);
                join_quals(j.rarg.as_ref(), out, using);
            }
        }
        for f in &psel.from_clause {
            join_quals(f.as_ref(), &mut quals, &mut using);
        }
        // A qual the planner's strictness test may find forcing it (see
        // `nonnull::plan_strict`), judged by names alone.
        let vars = crate::nonnull::plan_strict::NamedVars::of_from(&psel.from_clause, snapshot);
        let strict = crate::nonnull::plan_strict::Strictness {
            log: None,
            snapshot,
        };
        let may_force = |q: &ast::Node, alias: &str| {
            crate::nonnull::plan_strict::qual_rels(Some(q), &vars, &strict).contains(alias)
        };
        match current.item.as_deref() {
            Some(alias) if !alias.is_empty() => using || quals.iter().any(|q| may_force(q, alias)),
            _ => using || !quals.is_empty(),
        }
    })
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

/// The relation `rv` names when it certainly is a table (no CTE of its
/// name, as `is_cte` tells).
fn plain_table(
    rv: &ast::RangeVar,
    snapshot: &PgCatalog,
    is_cte: &dyn Fn(&str) -> bool,
) -> Option<crate::oid::PgClassOid> {
    if rv.schemaname.is_empty() && is_cte(&rv.relname) {
        return None;
    }
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    let class = snapshot.resolve_table(schema, &rv.relname)?;
    matches!(
        class.relkind,
        RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView | RelKind::ForeignTable
    )
    .then_some(class.oid)
}

/// How many distinct tables `side`, the nullable side of an outer join,
/// certainly has when it is an inner join (of inner joins) of two or more
/// — 0 otherwise. Such a side is never removed: `join_is_removable` needs
/// a single relation there, and an inner join's are all in the outer
/// join's `min_righthand` (self-join elimination merges only scans of one
/// table). `is_cte` tells the CTE names the level sees.
pub(crate) fn inner_join_tables(
    side: Option<&ast::Node>,
    snapshot: &PgCatalog,
    is_cte: &dyn Fn(&str) -> bool,
) -> usize {
    inner_join_tables_node(side, snapshot, is_cte)
}

/// [`inner_join_tables`] of a node held by reference.
fn inner_join_tables_node(
    side: Option<&ast::Node>,
    snapshot: &PgCatalog,
    is_cte: &dyn Fn(&str) -> bool,
) -> usize {
    if !matches!(side, Some(ast::Node::JoinExpr(_))) {
        return 0;
    }
    let mut relids = Vec::new();
    let mut todo = vec![side];
    while let Some(n) = todo.pop() {
        match n {
            Some(ast::Node::JoinExpr(j)) if j.jointype == ast::JoinType::JoinInner => {
                todo.extend(j.larg.as_ref().map(Some));
                todo.extend(j.rarg.as_ref().map(Some));
            }
            Some(ast::Node::RangeVar(rv)) => {
                if let Some(relid) = plain_table(rv, snapshot, is_cte)
                    && !relids.contains(&relid)
                {
                    relids.push(relid);
                }
            }
            _ => {}
        }
    }
    if relids.len() >= 2 { relids.len() } else { 0 }
}

/// Whether `query_supports_distinctness` may hold for `sel`, a subquery
/// the planner plans on its own: it has DISTINCT, GROUP BY, HAVING, a set
/// operation — or may call an aggregate (a function some candidate of
/// which is one, or one the analyzer can't find, outside sublinks).
pub(crate) fn may_be_distinct(sel: &ast::SelectStmt, snapshot: &PgCatalog) -> bool {
    if sel.op != ast::SetOperation::SetopNone
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
        .filter_map(|n| n.as_ref().map(ast::Node::to_ref))
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

// ─── Estimation of restrictions (`estimate_expression_value`) ───────────────

thread_local! {
    /// The expressions the planner estimates while planning the statement
    /// (`estimate_expression_value`): the pseudoconstant side of a
    /// restriction or join comparison, a `LIMIT` / `OFFSET` bound. A STABLE
    /// (or IMMUTABLE) call over constant arguments there runs while planning
    /// (`evaluate_function`'s estimation mode).
    ///
    /// One frame per query level (or per expression): the innermost one is
    /// the only one that applies, so a sublink's own level doesn't inherit
    /// its enclosing level's marks.
    static ESTIMABLE_SIDES: RefCell<Vec<std::collections::HashSet<NodeKey<ast::Node>>>> =
        const { RefCell::new(Vec::new()) };
    /// How deep in such an expression the analysis is.
    static ESTIMATING: Cell<u32> = const { Cell::new(0) };
}

/// The guard of [`ESTIMATING`]: the expression being analyzed is one the
/// planner estimates.
struct EstimatedGuard;

impl EstimatedGuard {
    fn enter() -> Self {
        ESTIMATING.with(|c| c.set(c.get() + 1));
        EstimatedGuard
    }
}

impl Drop for EstimatedGuard {
    fn drop(&mut self) {
        ESTIMATING.with(|c| c.set(c.get() - 1));
    }
}

/// Whether the expression being analyzed is one the planner estimates.
pub(crate) fn estimating() -> bool {
    ESTIMATING.with(Cell::get) > 0
}

/// The guard of [`enter_estimated`].
pub(crate) struct EstimatedScope {
    _guard: Option<EstimatedGuard>,
}

/// Enter the estimation frame of the expression `node`, if it is one the
/// planner estimates (see [`with_estimated_qual`]): everything inside it may
/// run a constant-argument STABLE call while planning.
pub(crate) fn enter_estimated(node: Option<&ast::Node>) -> EstimatedScope {
    let hit = ESTIMABLE_SIDES.with(|sides| {
        sides
            .borrow()
            .last()
            .is_some_and(|sides| node.is_some_and(|n| sides.contains(&NodeKey::of(n))))
    });
    EstimatedScope {
        _guard: hit.then(EstimatedGuard::enter),
    }
}

fn with_estimated<R>(sides: Vec<NodeKey<ast::Node>>, f: impl FnOnce() -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            ESTIMABLE_SIDES.with(|sides| {
                sides.borrow_mut().pop();
            });
        }
    }
    ESTIMABLE_SIDES.with(|current| current.borrow_mut().push(sides.into_iter().collect()));
    let _pop = Pop;
    f()
}

/// Run `f`, the analysis of a `WHERE` / `JOIN ... ON` clause of `scope`'s
/// level: the planner estimates the pseudoconstant side of each of its
/// restriction comparisons (`selfuncs.c`'s `get_restriction_variable`,
/// `scalararraysel` and the BooleanTest fallback), which runs a constant
/// argument STABLE call there while planning.
pub(crate) fn with_estimated_qual<R>(
    qual: Option<&ast::Node>,
    scope: &crate::scope::Scope,
    snapshot: &PgCatalog,
    f: impl FnOnce() -> R,
) -> R {
    let sides = estimated_sides(qual, scope, snapshot);
    with_estimated(sides, f)
}

/// Run `f`, the analysis of a `LIMIT` / `OFFSET` expression: the planner
/// estimates its value while planning every query with a Limit node
/// (`preprocess_limit`), running a constant argument STABLE call there.
pub(crate) fn with_estimated_limit<R>(expr: Option<&ast::Node>, f: impl FnOnce() -> R) -> R {
    with_estimated(expr.map(NodeKey::of).into_iter().collect(), f)
}

/// Run `f`, the analysis of a `TABLESAMPLE` method's first parameter: a
/// sample scan's size estimate (`set_tablesample_rel_size` →
/// `SampleScanGetSampleSize`) `estimate_expression_value`s it, running a
/// constant argument STABLE call there. The `REPEATABLE` seed is no part of
/// the estimate: only `eval_const_expressions` folds it.
pub(crate) fn with_estimated_sample<R>(arg: Option<&ast::Node>, f: impl FnOnce() -> R) -> R {
    with_estimated(arg.map(NodeKey::of).into_iter().collect(), f)
}

/// The sub-expressions of a `WHERE` / `ON` clause that the planner reduces
/// to constants while estimating the clause's selectivity: for each
/// comparison with a relation column on one side and none on the other, the
/// side without columns is `estimate_expression_value`'d. The walk follows
/// `clause_selectivity`: AND / OR arguments, NOT, a BooleanTest other than
/// UNKNOWN (whose fallback recurses into the argument), and gives up on a
/// clause `eval_const_expressions` certainly replaces by a constant.
fn estimated_sides(
    qual: Option<&ast::Node>,
    scope: &crate::scope::Scope,
    snapshot: &PgCatalog,
) -> Vec<NodeKey<ast::Node>> {
    let mut out = Vec::new();
    if let Some(inner) = qual {
        estimated_sides_here(inner.to_ref(), scope, snapshot, &mut out);
    }
    out
}

fn estimated_sides_here(
    n: typedpg_pg_query::NodeRef<'_>,
    scope: &crate::scope::Scope,
    snapshot: &PgCatalog,
    out: &mut Vec<NodeKey<ast::Node>>,
) {
    use typedpg_pg_query::NodeRef;
    match n {
        NodeRef::BoolExpr(b) => {
            let op = Some(b.boolop);
            for arg in &b.args {
                let folded = fold_qual(arg.as_ref(), snapshot);
                match (op, folded) {
                    // A constant FALSE decides an AND: the clause is gone.
                    (Some(ast::BoolExprType::AndExpr), Folded::Bool(false) | Folded::Null) => {
                        return;
                    }
                    // Likewise TRUE for an OR.
                    (Some(ast::BoolExprType::OrExpr), Folded::Bool(true)) => return,
                    (Some(ast::BoolExprType::AndExpr), Folded::Bool(true))
                    | (Some(ast::BoolExprType::OrExpr), Folded::Bool(false) | Folded::Null) => {
                        continue;
                    }
                    _ => {
                        if let Some(inner) = arg.as_ref() {
                            estimated_sides_here(inner.to_ref(), scope, snapshot, out);
                        }
                    }
                }
            }
        }
        NodeRef::BooleanTest(t) => {
            // `booltestsel` falls back to `clause_selectivity` of the
            // argument for IS [NOT] TRUE / FALSE; IS UNKNOWN doesn't.
            if matches!(
                t.booltesttype,
                ast::BoolTestType::IsTrue
                    | ast::BoolTestType::IsFalse
                    | ast::BoolTestType::IsNotTrue
                    | ast::BoolTestType::IsNotFalse
            ) && let Some(arg) = t.arg.as_ref()
            {
                estimated_sides_here(arg.to_ref(), scope, snapshot, out);
            }
        }
        NodeRef::AExpr(e) => {
            let kind = Some(e.kind);
            match kind {
                Some(
                    ast::AExprKind::AexprOp
                    | ast::AExprKind::AexprDistinct
                    | ast::AExprKind::AexprNotDistinct,
                ) => {
                    // `get_restriction_variable`: the side without a Var of
                    // the relation being estimated is reduced with
                    // `estimate_expression_value`. Which relation is the one
                    // estimated depends on the paths the planner costs: a
                    // parameterized path for either relation of a join
                    // compares the other side as a pseudoconstant
                    // (`get_baserel_parampathinfo` → `get_parameterized_
                    // baserel_size`). So a side is estimated when the other
                    // reads a relation it doesn't — for a join clause with
                    // columns on both sides that is both sides. The planner
                    // chooses those paths by cost (an index, the statistics);
                    // treating both as run keeps a denial without depending
                    // on the plan.
                    let left = level_column_sources(e.lexpr.as_ref(), scope);
                    let right = level_column_sources(e.rexpr.as_ref(), scope);
                    let left_estimated = right.iter().any(|s| !left.contains(s));
                    let right_estimated = left.iter().any(|s| !right.contains(s));
                    if left_estimated {
                        mark_side(e.lexpr.as_ref(), out);
                    }
                    if right_estimated {
                        mark_side(e.rexpr.as_ref(), out);
                    }
                }
                Some(
                    ast::AExprKind::AexprIn
                    | ast::AExprKind::AexprOpAny
                    | ast::AExprKind::AexprOpAll,
                ) => {
                    // `scalararraysel` estimates both operands whatever
                    // relations they read (the clause returned early only
                    // when it has no Var at all).
                    let left = level_column_sources(e.lexpr.as_ref(), scope);
                    let right = level_column_sources(e.rexpr.as_ref(), scope);
                    if !left.is_empty() || !right.is_empty() {
                        mark_side(e.lexpr.as_ref(), out);
                        mark_side(e.rexpr.as_ref(), out);
                    }
                }
                _ => {}
            }
        }
        NodeRef::TypeCast(tc) => {
            // `clause_selectivity` looks through a no-op relabeling
            // (`RelabelType`, `CoerceToDomain`), which a cast to plain
            // boolean becomes.
            if cast_target_is_bool(tc)
                && let Some(arg) = tc.arg.as_ref()
            {
                estimated_sides_here(arg.to_ref(), scope, snapshot, out);
            }
        }
        // A NullTest's selectivity never recurses into its argument
        // (`nulltestsel`), and anything else isn't a comparison.
        _ => {}
    }
}

/// Mark the sub-expressions of one operand of a comparison the planner
/// estimates: the operand itself, or the elements of an `IN` / `= ANY`
/// array (which is what the analysis reaches).
fn mark_side(side: Option<&ast::Node>, out: &mut Vec<NodeKey<ast::Node>>) {
    let Some(side) = side else {
        return;
    };
    if contains_sublink(Some(side)) {
        return;
    }
    match side {
        ast::Node::List(list) => {
            for item in list.items.iter().flatten() {
                if !contains_sublink(Some(item)) {
                    out.push(NodeKey::of(item));
                }
            }
        }
        ast::Node::AArrayExpr(array) => {
            for element in array.elements.iter().flatten() {
                if !contains_sublink(Some(element)) {
                    out.push(NodeKey::of(element));
                }
            }
        }
        _ => out.push(NodeKey::of(side)),
    }
}

/// The aliases of `scope`'s own FROM items `node` reads columns of: the
/// relations of the Vars it holds. A sublink's columns are its own query's.
fn level_column_sources(node: Option<&ast::Node>, scope: &crate::scope::Scope) -> Vec<String> {
    use typedpg_pg_query::NodeRef;
    let mut out: Vec<String> = Vec::new();
    let Some(node) = node else {
        return out;
    };
    let Some(inner) = Some(node) else {
        return out;
    };
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        match n {
            NodeRef::SubLink(_) => continue,
            NodeRef::ColumnRef(cr) => {
                let names: Vec<&str> = cr
                    .fields
                    .iter()
                    .filter_map(|f| match f.as_ref() {
                        Some(ast::Node::String(s)) => Some(s.sval.as_str()),
                        _ => None,
                    })
                    .collect();
                let stars = cr
                    .fields
                    .iter()
                    .filter(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))))
                    .count();
                let sources: Vec<&str> = if stars > 0 {
                    // `*` is every column of the level; `t.*` is t's row.
                    match names.as_slice() {
                        [] => scope
                            .sources
                            .iter()
                            .map(|s| s.alias.as_str())
                            .collect::<Vec<_>>(),
                        [table] if scope.is_level_source(table) => vec![table],
                        _ => Vec::new(),
                    }
                } else {
                    match names.as_slice() {
                        [column] => scope.level_column_sources(None, column),
                        [table, column] => scope.level_column_sources(Some(table), column),
                        _ => Vec::new(),
                    }
                };
                for source in sources {
                    if !out.iter().any(|s| s == source) {
                        out.push(source.to_owned());
                    }
                }
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    out
}

fn contains_sublink(node: Option<&ast::Node>) -> bool {
    use typedpg_pg_query::NodeRef;
    let Some(inner) = node else {
        return false;
    };
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        if matches!(n, NodeRef::SubLink(_)) {
            return true;
        }
        todo.extend(n.children());
    }
    false
}
