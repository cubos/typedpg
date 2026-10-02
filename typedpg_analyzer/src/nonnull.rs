//! What a condition proves NOT NULL: PG's `find_nonnullable_vars` /
//! `find_nonnullable_rels` (optimizer/util/clauses.c) over the raw AST.
//!
//! A qual that must be TRUE for a row to go on (WHERE, HAVING, an inner
//! join's ON, an aggregate's FILTER, a CASE branch's WHEN) proves every
//! column it is *strict* in non-NULL: had the column been NULL, the qual
//! would have been NULL — or, at the top level, at least not TRUE. The
//! callers turn these facts into narrower nullability ([`Facts`] on the
//! [`crate::nullability::NullabilityContext`]) and into PG's outer-join
//! reduction (`reduce_outer_joins`: a LEFT JOIN whose nullable side a
//! WHERE qual is strict in is an inner join).
//!
//! Strictness comes from the operators and functions the qual resolved to
//! (`pg_proc.proisstrict`, and the casts applied to their arguments): the
//! expression walk records it per node in a [`StrictLog`] while it types
//! the qual, since the raw AST doesn't say which function an operator is.
//! A node the log has no entry for proves nothing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use typedpg_pg_query::protobuf::{self, node};

use crate::scope::Scope;

thread_local! {
    /// Set while a view's columns are read for the rows a DML statement
    /// writes through it (see [`without_narrowing`]).
    static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with no qual proving anything: what a view's columns are over
/// any row of its base relation, not just those its WHERE passes.
pub(crate) fn without_narrowing<R>(f: impl FnOnce() -> R) -> R {
    let before = DISABLED.with(|d| d.replace(true));
    let out = f();
    DISABLED.with(|d| d.set(before));
    out
}

fn disabled() -> bool {
    DISABLED.with(|d| d.get())
}

/// Which kind of node a [`StrictLog`] entry is about (entries are keyed by
/// the node's location; the kind keeps nodes of different kinds apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StrictNode {
    /// An `A_Expr`: every operator it resolved to (BETWEEN and IN resolve
    /// several, all recorded at the A_Expr's location; a simple CASE's
    /// `test = value` is recorded at its WHEN's).
    Op,
    /// A plain function call (not an aggregate, a window call or a
    /// variadic call whose arguments were packed into an array).
    Func,
    /// An explicit cast.
    Cast,
}

/// Per-node strictness recorded while a qual is typed.
#[derive(Debug, Default)]
pub(crate) struct StrictLog(RefCell<HashMap<(i32, StrictNode), bool>>);

impl StrictLog {
    /// Record that the node of `kind` at `location` runs strict code
    /// (`strict`): a NULL input gives a NULL output. Several records for
    /// one node (BETWEEN's two comparisons) must all be strict.
    pub fn note(&self, location: i32, kind: StrictNode, strict: bool) {
        if location < 0 {
            return;
        }
        self.0
            .borrow_mut()
            .entry((location, kind))
            .and_modify(|s| *s &= strict)
            .or_insert(strict);
    }

    pub fn is_strict(&self, location: i32, kind: StrictNode) -> bool {
        location >= 0 && self.0.borrow().get(&(location, kind)) == Some(&true)
    }
}

/// Columns (`(alias, column)`) and FROM entries (`alias`) proven non-NULL.
/// A column proven non-NULL also proves its entry is not null-extended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub columns: HashSet<(String, String)>,
    pub rels: HashSet<String>,
}

impl Facts {
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.rels.is_empty()
    }

    pub fn column(alias: &str, column: &str) -> Facts {
        let mut f = Facts::default();
        f.columns.insert((alias.to_owned(), column.to_owned()));
        f.rels.insert(alias.to_owned());
        f
    }

    pub fn rel(alias: &str) -> Facts {
        let mut f = Facts::default();
        f.rels.insert(alias.to_owned());
        f
    }

    pub fn union(mut self, other: Facts) -> Facts {
        self.columns.extend(other.columns);
        self.rels.extend(other.rels);
        self
    }

    fn intersect(self, other: &Facts) -> Facts {
        Facts {
            columns: self
                .columns
                .into_iter()
                .filter(|c| other.columns.contains(c))
                .collect(),
            rels: self
                .rels
                .into_iter()
                .filter(|r| other.rels.contains(r))
                .collect(),
        }
    }

    /// Only the facts about the FROM entries named in `aliases`.
    pub fn restricted_to(mut self, aliases: &HashSet<String>) -> Facts {
        self.columns.retain(|(a, _)| aliases.contains(a));
        self.rels.retain(|a| aliases.contains(a));
        self
    }
}

/// The aliases of `scope`'s own FROM entries (not the LATERAL / outer
/// tiers): the entries a fact of this query level may speak about.
pub(crate) fn own_aliases(scope: &Scope) -> HashSet<String> {
    scope.sources.iter().map(|s| s.alias.clone()).collect()
}

/// Intersection of the facts of every node (`None` when there is none).
fn intersection<'a>(
    nodes: impl IntoIterator<Item = &'a protobuf::Node>,
    mut f: impl FnMut(&'a protobuf::Node) -> Facts,
) -> Facts {
    let mut acc: Option<Facts> = None;
    for n in nodes {
        let facts = f(n);
        acc = Some(match acc {
            None => facts,
            Some(a) => a.intersect(&facts),
        });
    }
    acc.unwrap_or_default()
}

/// PG's `find_nonnullable_vars(clause)` (with `find_nonnullable_rels`'
/// whole-row entries): what `node` being TRUE (`top_level`) — or merely
/// non-NULL (below the top level) — proves non-NULL. Column references
/// resolve against `scope`, as the qual's own resolution did.
pub(crate) fn nonnullable(
    node: &protobuf::Node,
    top_level: bool,
    scope: &Scope,
    log: &StrictLog,
) -> Facts {
    if disabled() {
        return Facts::default();
    }
    let walk = |n: &protobuf::Node, top: bool| nonnullable(n, top, scope, log);
    let walk_opt = |n: &Option<Box<protobuf::Node>>, top: bool| {
        n.as_deref().map(|n| walk(n, top)).unwrap_or_default()
    };
    let Some(inner) = node.node.as_ref() else {
        return Facts::default();
    };
    match inner {
        node::Node::ColumnRef(c) => column_ref_facts(c, scope),
        node::Node::BoolExpr(b) => match protobuf::BoolExprType::try_from(b.boolop) {
            // At the top level every conjunct must be TRUE. Below it an
            // AND of a NULL and a FALSE is FALSE, so only what every arm
            // proves holds — as for OR at any level.
            Ok(protobuf::BoolExprType::AndExpr) if top_level => b
                .args
                .iter()
                .fold(Facts::default(), |acc, a| acc.union(walk(a, true))),
            Ok(protobuf::BoolExprType::AndExpr | protobuf::BoolExprType::OrExpr) => {
                intersection(&b.args, |a| walk(a, top_level))
            }
            // NOT is strict. At the top level `NOT e` TRUE also means `e`
            // FALSE, so not TRUE: `NOT (x IS NULL)` proves `x`.
            Ok(protobuf::BoolExprType::NotExpr) => {
                b.args.iter().fold(Facts::default(), |acc, a| {
                    let below = walk(a, false);
                    if top_level {
                        acc.union(below)
                            .union(nonnullable_unless_true(a, scope, log))
                    } else {
                        acc.union(below)
                    }
                })
            }
            _ => Facts::default(),
        },
        node::Node::AExpr(e) => {
            use protobuf::AExprKind as K;
            if !log.is_strict(e.location, StrictNode::Op) {
                return Facts::default();
            }
            match K::try_from(e.kind) {
                Ok(K::AexprOp | K::AexprLike | K::AexprIlike | K::AexprSimilar) => {
                    walk_opt(&e.lexpr, false).union(walk_opt(&e.rexpr, false))
                }
                // `x IN (…)` is `x = ANY(ARRAY[…])` or an OR of `x = item`
                // (NOT IN: `<> ALL` over a non-empty array, or an AND of
                // `x <> item`), BETWEEN an AND / OR of comparisons on `x`:
                // all strict in `x` at any level.
                Ok(
                    K::AexprIn
                    | K::AexprBetween
                    | K::AexprNotBetween
                    | K::AexprBetweenSym
                    | K::AexprNotBetweenSym,
                ) => walk_opt(&e.lexpr, false),
                // `is_strict_saop(expr, falseOK)`: `x op ANY(array)` is NULL
                // or FALSE when an input is NULL, enough at the top level.
                // `ALL` over an empty array is TRUE whatever `x` is.
                Ok(K::AexprOpAny) if top_level => {
                    walk_opt(&e.lexpr, false).union(walk_opt(&e.rexpr, false))
                }
                _ => Facts::default(),
            }
        }
        node::Node::FuncCall(f) => {
            if !log.is_strict(f.location, StrictNode::Func) {
                return Facts::default();
            }
            f.args
                .iter()
                .fold(Facts::default(), |acc, a| acc.union(walk(a, false)))
        }
        node::Node::NamedArgExpr(na) => walk_opt(&na.arg, top_level),
        node::Node::TypeCast(tc) => {
            if !log.is_strict(tc.location, StrictNode::Cast) {
                return Facts::default();
            }
            walk_opt(&tc.arg, false)
        }
        node::Node::CollateClause(c) => walk_opt(&c.arg, top_level),
        // A field or an element of a NULL value is NULL.
        node::Node::AIndirection(i) => walk_opt(&i.arg, false),
        node::Node::NullTest(t)
            if top_level
                && protobuf::NullTestType::try_from(t.nulltesttype)
                    == Ok(protobuf::NullTestType::IsNotNull) =>
        {
            let Some(arg) = t.arg.as_deref() else {
                return Facts::default();
            };
            match arg.node.as_ref() {
                // `ROW(a, b) IS NOT NULL`: every field is non-NULL.
                Some(node::Node::RowExpr(r)) => r
                    .args
                    .iter()
                    .fold(Facts::default(), |acc, a| acc.union(walk(a, false))),
                // `t IS NOT NULL` over a whole-row reference: so is every
                // column of `t`.
                _ => match whole_row_source(arg, scope) {
                    Some(src) => src
                        .visible_columns()
                        .fold(Facts::rel(&src.alias), |acc, c| {
                            acc.union(Facts::column(&src.alias, &c.name))
                        }),
                    None => walk(arg, false),
                },
            }
        }
        // Boolean tests that are not TRUE for a NULL input.
        node::Node::BooleanTest(t)
            if top_level
                && matches!(
                    protobuf::BoolTestType::try_from(t.booltesttype),
                    Ok(protobuf::BoolTestType::IsTrue
                        | protobuf::BoolTestType::IsFalse
                        | protobuf::BoolTestType::IsNotUnknown)
                ) =>
        {
            walk_opt(&t.arg, false)
        }
        _ => Facts::default(),
    }
}

/// What `node` *not* being TRUE (FALSE or NULL) proves non-NULL — a CASE
/// branch after `WHEN node`. Only tests that never yield NULL say
/// anything: `x IS NULL` not TRUE is `x` non-NULL, while `x > 0` not TRUE
/// may well be `x` NULL.
pub(crate) fn nonnullable_unless_true(
    node: &protobuf::Node,
    scope: &Scope,
    log: &StrictLog,
) -> Facts {
    if disabled() {
        return Facts::default();
    }
    let Some(inner) = node.node.as_ref() else {
        return Facts::default();
    };
    match inner {
        node::Node::NullTest(t)
            if protobuf::NullTestType::try_from(t.nulltesttype)
                == Ok(protobuf::NullTestType::IsNull) =>
        {
            match t.arg.as_deref() {
                // Some field of the row is non-NULL — which one is unknown.
                Some(a) if matches!(a.node.as_ref(), Some(node::Node::RowExpr(_))) => {
                    Facts::default()
                }
                // A whole row that isn't NULL may still hold NULL columns:
                // only the entry itself is present.
                Some(a) => match whole_row_source(a, scope) {
                    Some(src) => Facts::rel(&src.alias),
                    None => nonnullable(a, false, scope, log),
                },
                None => Facts::default(),
            }
        }
        node::Node::BooleanTest(t) => {
            let Some(arg) = t.arg.as_deref() else {
                return Facts::default();
            };
            match protobuf::BoolTestType::try_from(t.booltesttype) {
                // Not (NOT TRUE): TRUE.
                Ok(protobuf::BoolTestType::IsNotTrue) => nonnullable(arg, true, scope, log),
                // Not UNKNOWN / not (NOT FALSE), i.e. FALSE: non-NULL.
                Ok(protobuf::BoolTestType::IsUnknown | protobuf::BoolTestType::IsNotFalse) => {
                    nonnullable(arg, false, scope, log)
                }
                _ => Facts::default(),
            }
        }
        node::Node::BoolExpr(b) => match protobuf::BoolExprType::try_from(b.boolop) {
            // Not TRUE: no arm is TRUE.
            Ok(protobuf::BoolExprType::OrExpr) => b.args.iter().fold(Facts::default(), |acc, a| {
                acc.union(nonnullable_unless_true(a, scope, log))
            }),
            // Not TRUE: some arm isn't.
            Ok(protobuf::BoolExprType::AndExpr) => {
                intersection(&b.args, |a| nonnullable_unless_true(a, scope, log))
            }
            // `NOT e` not TRUE, for an `e` that is never NULL: `e` TRUE.
            Ok(protobuf::BoolExprType::NotExpr) => match b.args.as_slice() {
                [e] if matches!(
                    e.node.as_ref(),
                    Some(node::Node::NullTest(_) | node::Node::BooleanTest(_))
                ) =>
                {
                    nonnullable(e, true, scope, log)
                }
                _ => Facts::default(),
            },
            _ => Facts::default(),
        },
        _ => Facts::default(),
    }
}

/// The FROM entry a bare `t` / `t.*` reference names as a whole row.
fn whole_row_source<'s>(
    n: &protobuf::Node,
    scope: &'s Scope,
) -> Option<&'s crate::scope::TableSource> {
    let Some(node::Node::ColumnRef(c)) = n.node.as_ref() else {
        return None;
    };
    let parts = crate::expr::extract_string_fields(&c.fields);
    let star = c
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    match (parts.as_slice(), star) {
        ([.., t], true) => scope.find_source(t),
        ([name], false)
            if matches!(
                scope.resolve_column(None, name, None),
                Err(crate::error::AnalyzeError::UndefinedColumn(_))
            ) =>
        {
            scope.find_source(name)
        }
        _ => None,
    }
}

/// A column reference proves its own column non-NULL (a whole-row one,
/// its entry).
fn column_ref_facts(c: &protobuf::ColumnRef, scope: &Scope) -> Facts {
    let star = c
        .fields
        .iter()
        .any(|f| matches!(f.node.as_ref(), Some(node::Node::AStar(_))));
    let parts = crate::expr::extract_string_fields(&c.fields);
    if star {
        return match parts.last() {
            Some(t) => scope
                .find_source(t)
                .map(|s| Facts::rel(&s.alias))
                .unwrap_or_default(),
            None => Facts::default(),
        };
    }
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return Facts::default(),
    };
    match scope.resolve_column(table, column, None) {
        Ok(col) => Facts::column(&col.table_alias, &col.name),
        Err(crate::error::AnalyzeError::UndefinedColumn(_)) if table.is_none() => scope
            .find_source(column)
            .map(|s| Facts::rel(&s.alias))
            .unwrap_or_default(),
        Err(_) => Facts::default(),
    }
}
