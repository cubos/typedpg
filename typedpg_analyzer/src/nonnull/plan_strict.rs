//! The relations a qual forces non-NULL as the planner's
//! `reduce_outer_joins` sees it: PG's `find_nonnullable_rels`
//! (optimizer/util/clauses.c) over the qual once `eval_const_expressions`
//! folded it — and nothing more.
//!
//! [`super::nonnullable`] proves much more than this (what a CASE or a
//! COALESCE evaluates to for a NULL column, whole-row tests, CHECK
//! constraints and foreign keys on top): sound for nullability, but the
//! plan-time errors that depend on a join still being outer (`FULL JOIN is
//! only supported with merge-joinable or hash-joinable join conditions`,
//! `FOR UPDATE cannot be applied to the nullable side of an outer join`)
//! need the planner's own, narrower, reduction.
//!
//! The result is the relations PG's test *may* return, for some plan of
//! the statement: where the analyzer can't tell (a strictness it doesn't
//! know, a parameter a custom plan replaces with a constant, what a pulled-up
//! subquery's column stands for), it assumes the relation forced. So a join
//! this module leaves outer is one every plan leaves outer.

use std::collections::HashSet;

use typedpg_pg_query::ast;
use typedpg_pg_query::ast::{AExprKind as K, BoolExprType, BoolTestType, NullTestType};

use super::StrictLog;
use crate::expr::plan_fold::{self, Folded};
use crate::pg_catalog::PgCatalog;

/// What a column reference of the qual's level is once planned.
#[derive(Debug, Default)]
pub(crate) struct PlanVar {
    /// The FROM entries of the level whose relations the Var (or what the
    /// planner replaces it by) belongs to: none for an outer level's
    /// column, nor for a FULL JOIN's merged column (a COALESCE).
    pub rels: Vec<String>,
    /// Certainly of a composite type: a NullTest over it tests a row
    /// (`argisrow`), which `find_nonnullable_rels` skips.
    pub row: bool,
}

/// How the qual's names resolve.
pub(crate) trait PlanVars {
    /// What column reference `c` is (a whole-row one too).
    fn column(&self, c: &ast::ColumnRef) -> PlanVar;
    /// `(x).f` over a whole-row reference to a FROM entry: the column the
    /// parser makes of it.
    fn field_column(&self, i: &ast::AIndirection) -> Option<ast::Node>;
    /// What an `EXISTS` sublink that is an argument of the qual's top-level
    /// AND forces: `pull_up_sublinks` makes it a semi-join above this
    /// level's relations, whose quals (the subquery's WHERE) count like an
    /// inner join's.
    fn exists(&self, sub: &ast::SubLink) -> HashSet<String>;
}

/// The strictness the analyzer knows of the qual's nodes.
pub(crate) struct Strictness<'a> {
    /// The qual's [`StrictLog`], when it was typed (a view's or a
    /// subquery's stored query read from its text has none: every call is
    /// then taken to be strict).
    pub log: Option<&'a StrictLog>,
    pub snapshot: &'a PgCatalog,
}

impl Strictness<'_> {
    /// Whether the function behind the operator or call at `location` may
    /// be strict — or inlined (a SQL function's body replaces the call in
    /// `eval_const_expressions`, and may be strict where the function
    /// isn't).
    fn may_be_strict(&self, location: i32, kind: super::StrictNode) -> bool {
        let Some(log) = self.log else {
            return true;
        };
        let proc = match kind {
            super::StrictNode::Op => log
                .operator_at(location)
                .and_then(|op| self.snapshot.pg_operator.get(&op.oid))
                .and_then(|op| op.oprcode),
            super::StrictNode::Func => log.called_proc(location),
            _ => None,
        };
        match proc.and_then(|p| self.snapshot.pg_proc.get(&p)) {
            Some(p) => p.proisstrict || p.prolang == crate::pg_catalog::SQL_LANGUAGE,
            // Several operators (a row comparison, BETWEEN), or none
            // recorded: only a record of a non-strict one says no.
            None => log.strictness(location, kind) != Some(false),
        }
    }

    /// Whether the explicit cast at `location` passes a NULL through
    /// (binary-coercible, an I/O conversion, a strict cast function).
    fn cast_may_be_strict(&self, location: i32) -> bool {
        self.log
            .is_none_or(|log| log.strictness(location, super::StrictNode::Cast) != Some(false))
    }
}

/// Marks a column key among the entries a qual forces (see
/// [`column_key`]); no SQL identifier holds it.
const COLUMN_MARK: char = '\u{2}';

/// The key of column `column` of FROM entry `alias` (`*`: its whole row),
/// recorded beside the entry itself: what a pulled-up subquery or view
/// hands down to its own query depends on which of its columns are forced
/// (see [`Reach`]). Never the name of an entry, so it reduces no join of
/// the level.
pub(crate) fn column_key(alias: &str, column: &str) -> String {
    format!("{alias}{COLUMN_MARK}{column}")
}

/// `key` (an entry or a [`column_key`]) with entry `from` renamed `to`.
pub(crate) fn renamed_key(key: String, from: &str, to: &str) -> String {
    match key.split_once(COLUMN_MARK) {
        Some((a, c)) if a == from => column_key(to, c),
        None if key == from => to.to_owned(),
        _ => key,
    }
}

/// What the constraints handed down to a FROM entry (a subquery or view)
/// force of its output columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reach {
    Nothing,
    /// These columns (by name).
    Columns(HashSet<String>),
    /// Its whole row, or columns the analyzer can't name.
    All,
}

impl Reach {
    /// What `forced` (entries and [`column_key`]s) forces of entry `alias`.
    pub(crate) fn of(alias: &str, forced: &HashSet<String>) -> Reach {
        if !forced.contains(alias) {
            return Reach::Nothing;
        }
        let mut cols = HashSet::new();
        for k in forced {
            if let Some((a, c)) = k.split_once(COLUMN_MARK)
                && a == alias
            {
                if c == "*" {
                    return Reach::All;
                }
                cols.insert(c.to_owned());
            }
        }
        if cols.is_empty() {
            Reach::All
        } else {
            Reach::Columns(cols)
        }
    }
}

/// The output columns of stored query `sel` (a view's, a FROM subquery's)
/// with their target expressions, renamed by `out_names` (a view's
/// columns, a subquery alias's column list). `None` when the analyzer can't
/// name them all here (a `*`, a set operation).
fn outputs<'a>(
    sel: &'a ast::SelectStmt,
    out_names: &[String],
) -> Option<Vec<(String, Option<&'a ast::Node>)>> {
    if sel.op != ast::SetOperation::SetopNone || sel.target_list.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for (i, t) in sel.target_list.iter().enumerate() {
        let Some(ast::Node::ResTarget(rt)) = t.as_ref() else {
            return None;
        };
        let expr = rt.val.as_ref();
        let star = expr.is_some_and(|e| {
            e.nodes()
                .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::AStar(_)))
        });
        if star {
            return None;
        }
        let name = match out_names.get(i) {
            Some(n) => n.clone(),
            None if !rt.name.is_empty() => rt.name.clone(),
            None => match expr {
                Some(ast::Node::ColumnRef(c)) => crate::expr::extract_string_fields(&c.fields)
                    .last()
                    .cloned()
                    .unwrap_or_default(),
                // Another expression's derived name: unknown here.
                _ => return None,
            },
        };
        out.push((name, expr));
    }
    Some(out)
}

/// What output expression `expr` of stored query `sel`, replacing a
/// reference to its column (`pullup_replace_vars`), forces where the
/// reference was strict: its Vars' entries — and when the query is on the
/// nullable side of an outer join of the level around (`under_outer`), an
/// expression that is no plain Var is wrapped in a PlaceHolderVar, which
/// also forces the query's one relation when it has only one (a PHV whose
/// syntactic scope is a single relation behaves like its Var).
fn output_forces(
    sel: &ast::SelectStmt,
    expr: Option<&ast::Node>,
    under_outer: bool,
    vars: &NamedVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    let mut out = rels(expr, true, vars, strict);
    if under_outer && !matches!(expr, Some(ast::Node::ColumnRef(_))) {
        match sel.from_clause.as_slice() {
            [
                Some(
                    ast::Node::RangeVar(_)
                    | ast::Node::RangeSubselect(_)
                    | ast::Node::RangeFunction(_),
                ),
            ] => {
                out.extend(vars.entries.iter().map(|(a, _)| a.clone()));
            }
            // A FROM-less query: one RTE_RESULT.
            [] => {
                out.insert(String::new());
            }
            _ => {}
        }
    }
    out
}

/// The entries of stored query `sel` (a view's, a FROM subquery's) the
/// constraints `reach` hands down to it force: a forced output column's
/// Vars, once the planner replaced it by its target expression — pulled up
/// (`pullup_replace_vars`) or pushed down into it (`subquery_push_qual`);
/// see [`output_forces`]. `out_names` renames the outputs.
pub(crate) fn forced_by_outputs(
    sel: &ast::SelectStmt,
    out_names: &[String],
    reach: &Reach,
    under_outer: bool,
    snapshot: &PgCatalog,
) -> HashSet<String> {
    let vars = NamedVars::of_from(&sel.from_clause, snapshot);
    let all = || -> HashSet<String> { vars.entries.iter().map(|(a, _)| a.clone()).collect() };
    let cols = match reach {
        Reach::Nothing => return HashSet::new(),
        Reach::All => return all(),
        Reach::Columns(cols) => cols,
    };
    let Some(outputs) = outputs(sel, out_names) else {
        return all();
    };
    let strict = Strictness {
        log: None,
        snapshot,
    };
    let mut out = HashSet::new();
    for (name, expr) in outputs {
        if cols.contains(&name) {
            out.extend(output_forces(sel, expr, under_outer, &vars, &strict));
        }
    }
    out
}

/// Whether a strict reference to column `column` of FROM entry `alias` —
/// stored query `sel`, on the nullable side of an outer join of its level —
/// forces the entry: a Var of it, unless the planner certainly pulls the
/// query up ([`crate::resolve::deferred::pulled_up_in`]) and the column's
/// expression forces none of its relations (see [`output_forces`]).
pub(crate) fn nulled_output_forces(
    sel: &ast::SelectStmt,
    out_names: &[String],
    column: &str,
    snapshot: &PgCatalog,
) -> bool {
    if !crate::resolve::deferred::pulled_up_in(sel, snapshot) {
        return true;
    }
    let Some(outputs) = outputs(sel, out_names) else {
        return true;
    };
    let vars = NamedVars::of_from(&sel.from_clause, snapshot);
    let strict = Strictness {
        log: None,
        snapshot,
    };
    let mut named = outputs
        .into_iter()
        .filter(|(name, _)| name == column)
        .peekable();
    if named.peek().is_none() {
        return true;
    }
    named.any(|(_, expr)| !output_forces(sel, expr, true, &vars, &strict).is_empty())
}

/// `find_nonnullable_rels(qual)` for a WHERE / JOIN ON / HAVING qual.
pub(crate) fn qual_rels(
    qual: Option<&ast::Node>,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    rels(qual, true, vars, strict)
}

/// Whether a custom plan may fold `n` to a constant: it reads a parameter,
/// or the analyzer can't tell what it folds to.
fn may_fold(n: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
    plan_fold::fold(n, snapshot) != Folded::Var || has_param(n)
}

fn has_param(n: Option<&ast::Node>) -> bool {
    n.is_some_and(|n| {
        n.nodes()
            .any(|(r, _)| matches!(r, typedpg_pg_query::NodeRef::ParamRef(_)))
    })
}

fn union_all<'a>(
    nodes: impl IntoIterator<Item = Option<&'a ast::Node>>,
    mut f: impl FnMut(Option<&'a ast::Node>) -> HashSet<String>,
) -> HashSet<String> {
    let mut out = HashSet::new();
    for n in nodes {
        out.extend(f(n));
    }
    out
}

/// The intersection of every node's set (empty for no node).
fn intersect_all<'a>(
    nodes: impl IntoIterator<Item = Option<&'a ast::Node>>,
    mut f: impl FnMut(Option<&'a ast::Node>) -> HashSet<String>,
) -> HashSet<String> {
    let mut acc: Option<HashSet<String>> = None;
    for n in nodes {
        let s = f(n);
        acc = Some(match acc {
            None => s,
            Some(a) => a.intersection(&s).cloned().collect(),
        });
    }
    acc.unwrap_or_default()
}

/// `find_nonnullable_rels_walker(node, top_level)`.
fn rels(
    node: Option<&ast::Node>,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    let snapshot = strict.snapshot;
    let walk = |n: Option<&ast::Node>, t: bool| rels(n, t, vars, strict);
    let Some(n) = node else {
        return HashSet::new();
    };
    // A node folding to a constant reads no Var.
    if matches!(
        plan_fold::fold(Some(n), snapshot),
        Folded::Bool(_) | Folded::Null
    ) {
        return HashSet::new();
    }
    match n {
        ast::Node::ColumnRef(c) => vars.column(c).rels.into_iter().collect(),
        ast::Node::BoolExpr(b) => match b.boolop {
            // An implicit-AND list: each argument must be TRUE.
            BoolExprType::AndExpr if top => {
                union_all(b.args.iter().map(Option::as_ref), |a| walk(a, true))
            }
            // Strict when every argument is: the intersection — once
            // `eval_const_expressions` dropped the arguments folding to
            // TRUE (AND) / FALSE (OR), in a qual NULL too
            // (`canonicalize_qual`). An argument a custom plan may fold so
            // may be dropped.
            BoolExprType::AndExpr | BoolExprType::OrExpr => {
                let kept: Vec<Option<&ast::Node>> = b
                    .args
                    .iter()
                    .map(Option::as_ref)
                    .filter(|a| !may_fold(*a, snapshot))
                    .collect();
                if kept.is_empty() {
                    // Every argument may fold away: what's left may be any
                    // one of them.
                    return union_all(b.args.iter().map(Option::as_ref), |a| walk(a, top));
                }
                intersect_all(kept, |a| walk(a, top))
            }
            BoolExprType::NotExpr => {
                negated(b.args.first().and_then(Option::as_ref), top, vars, strict)
            }
        },
        ast::Node::AExpr(e) => aexpr(e, top, vars, strict),
        ast::Node::FuncCall(f) => {
            if f.agg_star
                || f.over.is_some()
                || !strict.may_be_strict(f.location, super::StrictNode::Func)
            {
                return HashSet::new();
            }
            union_all(f.args.iter().map(Option::as_ref), |a| walk(a, false))
        }
        ast::Node::NamedArgExpr(na) => walk(na.arg.as_ref(), top),
        // RelabelType / CoerceViaIO / ArrayCoerceExpr pass `top_level` on;
        // a strict cast function is a FuncExpr (below the top level).
        ast::Node::TypeCast(tc) => {
            if strict.cast_may_be_strict(tc.location) {
                walk(tc.arg.as_ref(), top)
            } else {
                HashSet::new()
            }
        }
        ast::Node::CollateClause(c) => walk(c.arg.as_ref(), top),
        // A field of a FROM entry's whole row is a Var of its column; any
        // other field or subscript (FieldSelect, SubscriptingRef) proves
        // nothing.
        ast::Node::AIndirection(i) => match vars.field_column(i) {
            Some(col) => walk(Some(&col), top),
            None => HashSet::new(),
        },
        ast::Node::CaseExpr(c) => case(c, top, vars, strict),
        ast::Node::CoalesceExpr(c) => {
            // `eval_const_expressions` drops the NULL constants and the
            // arguments after the first non-NULL one: a single argument
            // left is the COALESCE. A custom plan may fold an argument over
            // a parameter to NULL, and the analyzer may not know whether
            // a constant is.
            use plan_fold::Certain;
            let mut live: Vec<Option<&ast::Node>> = Vec::new();
            let mut may_vanish = false;
            for a in c.args.iter().map(Option::as_ref) {
                match plan_fold::certainly(a, snapshot) {
                    Certain::Null => continue,
                    Certain::Bool(_) | Certain::Value => {
                        live.push(a);
                        break;
                    }
                    Certain::UnknownValue => may_vanish = true,
                    Certain::Not => {
                        may_vanish |= has_param(a) || plan_fold::fold(a, snapshot) == Folded::Const
                    }
                }
                live.push(a);
            }
            match live.as_slice() {
                [only] => walk(*only, top),
                args if may_vanish => union_all(args.iter().copied(), |a| walk(a, top)),
                _ => HashSet::new(),
            }
        }
        ast::Node::NullTest(t) => null_test(t, t.nulltesttype, top, vars, strict),
        ast::Node::BooleanTest(t) => bool_test(t.arg.as_ref(), t.booltesttype, top, vars, strict),
        ast::Node::SubLink(sub) => sublink(sub, top, vars, strict),
        _ => HashSet::new(),
    }
}

/// `NOT arg`, as `eval_const_expressions` leaves it (`negate_clause`): a
/// double NOT cancels out, a NullTest or a BooleanTest flips, De Morgan's
/// laws push the NOT into an AND / OR, an operator becomes its negator
/// (strict in the same arguments); otherwise NOT is strict in its argument.
fn negated(
    arg: Option<&ast::Node>,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    let walk = |n: Option<&ast::Node>, t: bool| rels(n, t, vars, strict);
    match arg {
        Some(ast::Node::BoolExpr(b)) if b.boolop == BoolExprType::NotExpr => {
            walk(b.args.first().and_then(Option::as_ref), top)
        }
        // NOT (a AND b) is (NOT a) OR (NOT b): strict in what both are —
        // once the arguments folding to constants were dropped (or may be).
        Some(ast::Node::BoolExpr(b)) if b.boolop == BoolExprType::AndExpr => {
            let snapshot = strict.snapshot;
            let kept: Vec<Option<&ast::Node>> = b
                .args
                .iter()
                .map(Option::as_ref)
                .filter(|a| !may_fold(*a, snapshot))
                .collect();
            if kept.is_empty() {
                return union_all(b.args.iter().map(Option::as_ref), |a| {
                    negated(a, top, vars, strict)
                });
            }
            intersect_all(kept, |a| negated(a, top, vars, strict))
        }
        // NOT (a OR b) is (NOT a) AND (NOT b): at the top level, a list.
        Some(ast::Node::BoolExpr(b)) if b.boolop == BoolExprType::OrExpr => {
            if top {
                union_all(b.args.iter().map(Option::as_ref), |a| {
                    negated(a, true, vars, strict)
                })
            } else {
                let snapshot = strict.snapshot;
                let kept: Vec<Option<&ast::Node>> = b
                    .args
                    .iter()
                    .map(Option::as_ref)
                    .filter(|a| !may_fold(*a, snapshot))
                    .collect();
                if kept.is_empty() {
                    return union_all(b.args.iter().map(Option::as_ref), |a| {
                        negated(a, false, vars, strict)
                    });
                }
                intersect_all(kept, |a| negated(a, false, vars, strict))
            }
        }
        Some(ast::Node::NullTest(t)) => {
            let flipped = match t.nulltesttype {
                NullTestType::IsNull => NullTestType::IsNotNull,
                NullTestType::IsNotNull => NullTestType::IsNull,
            };
            // A row test isn't negated (`NOT (r IS NULL)` differs from
            // `r IS NOT NULL`): NOT over a NullTest, which proves nothing
            // below the top level.
            let row = match t.arg.as_ref() {
                Some(ast::Node::RowExpr(_)) => true,
                Some(ast::Node::ColumnRef(c)) => vars.column(c).row,
                _ => false,
            };
            if row {
                HashSet::new()
            } else {
                null_test(t, flipped, top, vars, strict)
            }
        }
        Some(ast::Node::BooleanTest(t)) => {
            let flipped = match t.booltesttype {
                BoolTestType::IsTrue => BoolTestType::IsNotTrue,
                BoolTestType::IsNotTrue => BoolTestType::IsTrue,
                BoolTestType::IsFalse => BoolTestType::IsNotFalse,
                BoolTestType::IsNotFalse => BoolTestType::IsFalse,
                BoolTestType::IsUnknown => BoolTestType::IsNotUnknown,
                BoolTestType::IsNotUnknown => BoolTestType::IsUnknown,
            };
            bool_test(t.arg.as_ref(), flipped, top, vars, strict)
        }
        // `NOT (x BETWEEN a AND b)` is `x NOT BETWEEN a AND b` (and so
        // on): the negated AND / OR of the comparisons.
        Some(ast::Node::AExpr(e))
            if matches!(
                e.kind,
                K::AexprBetween | K::AexprNotBetween | K::AexprBetweenSym | K::AexprNotBetweenSym
            ) =>
        {
            let flipped = ast::AExpr {
                kind: match e.kind {
                    K::AexprBetween => K::AexprNotBetween,
                    K::AexprNotBetween => K::AexprBetween,
                    K::AexprBetweenSym => K::AexprNotBetweenSym,
                    _ => K::AexprBetweenSym,
                },
                ..(**e).clone()
            };
            aexpr(&flipped, top, vars, strict)
        }
        // `NOT (x IN (…))`: `x <> ALL (…)`, or an AND of `x <> item`.
        Some(ast::Node::AExpr(e)) if e.kind == K::AexprIn => {
            in_list(e, !is_not_in(e), top, vars, strict)
        }
        other => walk(other, false),
    }
}

fn is_not_in(e: &ast::AExpr) -> bool {
    crate::expr::extract_string_fields(&e.name).join(".") == "<>"
}

/// IS NOT NULL is strict at the top level, unless it tests a row: a row
/// constructor's test is split into one per field (`eval_const_expressions`),
/// a composite value's is skipped.
fn null_test(
    t: &ast::NullTest,
    kind: NullTestType,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    if !top || kind != NullTestType::IsNotNull {
        return HashSet::new();
    }
    match t.arg.as_ref() {
        Some(ast::Node::RowExpr(r)) => union_all(r.args.iter().map(Option::as_ref), |a| match a {
            Some(ast::Node::RowExpr(_)) => HashSet::new(),
            Some(ast::Node::ColumnRef(c)) if vars.column(c).row => HashSet::new(),
            a => rels(a, false, vars, strict),
        }),
        Some(ast::Node::ColumnRef(c)) => {
            let v = vars.column(c);
            if v.row {
                HashSet::new()
            } else {
                v.rels.into_iter().collect()
            }
        }
        a => rels(a, false, vars, strict),
    }
}

/// IS TRUE / IS FALSE / IS NOT UNKNOWN reject a NULL at the top level.
fn bool_test(
    arg: Option<&ast::Node>,
    kind: BoolTestType,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    if top
        && matches!(
            kind,
            BoolTestType::IsTrue | BoolTestType::IsFalse | BoolTestType::IsNotUnknown
        )
    {
        rels(arg, false, vars, strict)
    } else {
        HashSet::new()
    }
}

/// A CASE folds to the result of its first WHEN folding to TRUE, once the
/// ones folding to FALSE or NULL are dropped (its ELSE when all are);
/// otherwise it stays a CASE, which proves nothing. A WHEN a custom plan may
/// fold may select its result, or be dropped.
fn case(
    c: &ast::CaseExpr,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    let snapshot = strict.snapshot;
    let test = c.arg.as_ref().map(|a| plan_fold::fold(Some(a), snapshot));
    let test_param = has_param(c.arg.as_ref());
    let mut out = HashSet::new();
    for w in &c.args {
        let Some(ast::Node::CaseWhen(w)) = w.as_ref() else {
            return out;
        };
        let folded = plan_fold::case_when(test, w.expr.as_ref(), snapshot);
        match folded {
            Folded::Bool(false) | Folded::Null => {}
            Folded::Bool(true) => {
                out.extend(rels(w.result.as_ref(), top, vars, strict));
                return out;
            }
            Folded::Const => out.extend(rels(w.result.as_ref(), top, vars, strict)),
            Folded::Var if test_param || has_param(w.expr.as_ref()) => {
                out.extend(rels(w.result.as_ref(), top, vars, strict))
            }
            // A WHEN that stays: so does the CASE.
            Folded::Var => return out,
        }
    }
    out.extend(rels(c.defresult.as_ref(), top, vars, strict));
    out
}

/// What the `A_Expr` transforms into.
fn aexpr(
    e: &ast::AExpr,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    let walk = |n: Option<&ast::Node>, t: bool| rels(n, t, vars, strict);
    let (l, r) = (e.lexpr.as_ref(), e.rexpr.as_ref());
    let op_strict = || strict.may_be_strict(e.location, super::StrictNode::Op);
    match e.kind {
        K::AexprOp if super::is_row(l) && super::is_row(r) => {
            // `make_row_comparison_op`: `=` an AND of the pairs' `=`, `<>`
            // an OR of their `<>`, an ordering comparison a RowCompareExpr
            // (which `find_nonnullable_rels` doesn't look into).
            let (Some(lf), Some(rf)) = (super::plain_row_fields(l), super::plain_row_fields(r))
            else {
                return HashSet::new();
            };
            if lf.len() != rf.len() || !op_strict() {
                return HashSet::new();
            }
            let pairs = lf
                .iter()
                .map(Option::as_ref)
                .zip(rf.iter().map(Option::as_ref));
            let pair = |(a, b): (Option<&ast::Node>, Option<&ast::Node>)| {
                let mut s = walk(a, false);
                s.extend(walk(b, false));
                s
            };
            let op = crate::expr::extract_string_fields(&e.name).join(".");
            match op.as_str() {
                "=" if top => pairs.fold(HashSet::new(), |mut acc, p| {
                    acc.extend(pair(p));
                    acc
                }),
                "=" | "<>" => {
                    let sets: Vec<HashSet<String>> = pairs.map(pair).collect();
                    let mut it = sets.into_iter();
                    let first = it.next().unwrap_or_default();
                    it.fold(first, |a, s| a.intersection(&s).cloned().collect())
                }
                _ => HashSet::new(),
            }
        }
        K::AexprOp | K::AexprLike | K::AexprIlike | K::AexprSimilar => {
            if !op_strict() {
                return HashSet::new();
            }
            let mut s = walk(l, false);
            s.extend(walk(r, false));
            s
        }
        // `x op ANY / ALL (array)`: `is_strict_saop` — ANY at the top level
        // is enough, otherwise the array must surely have an element.
        K::AexprOpAny | K::AexprOpAll => {
            let any = e.kind == K::AexprOpAny;
            if !op_strict() || !((any && top) || array_may_be_nonempty(r)) {
                return HashSet::new();
            }
            let mut s = walk(l, false);
            s.extend(walk(r, false));
            s
        }
        K::AexprIn => in_list(e, is_not_in(e), top, vars, strict),
        // `x BETWEEN a AND b` is `x >= a AND x <= b`, NOT BETWEEN
        // `x < a OR x > b`; SYMMETRIC an OR / AND of both orders.
        K::AexprBetween | K::AexprNotBetween | K::AexprBetweenSym | K::AexprNotBetweenSym => {
            if !op_strict() {
                return HashSet::new();
            }
            let Some(ast::Node::List(bounds)) = r else {
                return HashSet::new();
            };
            let (a, b) = match bounds.items.as_slice() {
                [a, b] => (a.as_ref(), b.as_ref()),
                _ => return HashSet::new(),
            };
            let x = walk(l, false);
            let (ra, rb) = (walk(a, false), walk(b, false));
            let and_top = top && matches!(e.kind, K::AexprBetween | K::AexprBetweenSym);
            let mut out = x;
            if and_top {
                out.extend(ra);
                out.extend(rb);
            } else {
                out.extend(ra.intersection(&rb).cloned());
            }
            out
        }
        // `x IS DISTINCT FROM NULL` (a NULL once folded) is the NullTest
        // `x IS NOT NULL` with `argisrow` false; NOT DISTINCT FROM NULL is
        // IS NULL. Otherwise a DistinctExpr, which `find_nonnullable_rels`
        // skips — PostgreSQL 19 makes it `<>` / `=` only over operands
        // proven non-NULL, whose relations no outer join nulls.
        K::AexprDistinct | K::AexprNotDistinct => {
            let null = |n: Option<&ast::Node>| plan_fold::fold(n, strict.snapshot) == Folded::Null;
            let other = if null(r) {
                l
            } else if null(l) {
                r
            } else {
                return HashSet::new();
            };
            if e.kind == K::AexprDistinct && top {
                walk(other, false)
            } else {
                HashSet::new()
            }
        }
        // NullIfExpr: skipped.
        K::AexprNullif => HashSet::new(),
    }
}

/// `x [NOT] IN (…)`: `x = ANY (ARRAY[…])` / `x <> ALL (ARRAY[…])` over a
/// non-empty array, or an OR of `x = item` / an AND of `x <> item`
/// (`transformAExprIn`). Strict in `x` either way; in an item only through
/// the AND (all of them) or the OR (what every item shares).
fn in_list(
    e: &ast::AExpr,
    not_in: bool,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    if !strict.may_be_strict(e.location, super::StrictNode::Op) {
        return HashSet::new();
    }
    let walk = |n: Option<&ast::Node>| rels(n, false, vars, strict);
    let mut out = walk(e.lexpr.as_ref());
    let items: Vec<Option<&ast::Node>> = match e.rexpr.as_ref() {
        Some(ast::Node::List(l)) => l.items.iter().map(Option::as_ref).collect(),
        other => vec![other],
    };
    if not_in && top {
        out.extend(union_all(items, walk));
    } else {
        out.extend(intersect_all(items, walk));
    }
    out
}

/// Whether the array of an `op ALL / ANY (array)` may be a non-empty
/// constant or `ARRAY[…]` (what `is_strict_saop` accepts below the top
/// level): not an array literal `'{}'`, an empty `ARRAY[]`, a
/// multidimensional `ARRAY[ARRAY[…]]`, or anything else (a column, a
/// function's result).
fn array_may_be_nonempty(n: Option<&ast::Node>) -> bool {
    use typedpg_pg_query::ast::AConstValue;
    match n {
        Some(ast::Node::TypeCast(tc)) => array_may_be_nonempty(tc.arg.as_ref()),
        Some(ast::Node::AArrayExpr(a)) => {
            !a.elements.is_empty()
                && !a
                    .elements
                    .iter()
                    .any(|e| matches!(e, Some(ast::Node::AArrayExpr(_))))
        }
        Some(ast::Node::AConst(c)) => match c.val.as_ref() {
            Some(AConstValue::String(s)) => {
                let body = s.sval.trim();
                let inner = body.rsplit_once('=').map_or(body, |(_, rest)| rest).trim();
                inner != "{}"
            }
            _ => !c.isnull,
        },
        // A parameter a custom plan replaces with a constant array.
        Some(ast::Node::ParamRef(_)) => true,
        _ => false,
    }
}

/// A sublink once preprocessed: an `EXISTS` at the top level a semi-join
/// (see [`PlanVars::exists`]); `x op ANY (SELECT …)` (`x IN (SELECT …)`) a
/// semi-join or a SubPlan strict in `x` at the top level; a row comparison
/// `(a, b) = (SELECT …)` strict in its row at any level. Anything else
/// proves nothing.
fn sublink(
    sub: &ast::SubLink,
    top: bool,
    vars: &dyn PlanVars,
    strict: &Strictness<'_>,
) -> HashSet<String> {
    use ast::SubLinkType as S;
    let tested = || match sub.testexpr.as_ref() {
        Some(ast::Node::RowExpr(r)) => union_all(r.args.iter().map(Option::as_ref), |a| {
            rels(a, false, vars, strict)
        }),
        t => rels(t, false, vars, strict),
    };
    match sub.sub_link_type {
        S::ExistsSublink if top => vars.exists(sub),
        S::AnySublink if top => tested(),
        S::RowcompareSublink => tested(),
        _ => HashSet::new(),
    }
}

/// Resolution by name alone, for a stored query (a view's, a FROM
/// subquery's read from its text) analyzed without a scope: a qualified
/// column belongs to the entry it names; an unqualified one, to every
/// entry of the level that may have it.
pub(crate) struct NamedVars {
    /// The level's FROM entries, with the columns each is known to have
    /// (`None`: unknown).
    pub entries: Vec<(String, Option<HashSet<String>>)>,
    /// The merged columns of the `JOIN USING` / `NATURAL` joins, outermost
    /// first: the entries the Var an unqualified reference to one is
    /// belongs to (the left constituent of an inner or LEFT join, the right
    /// one of a RIGHT join, none for a FULL join's COALESCE).
    pub merged: Vec<(String, Vec<String>)>,
}

impl PlanVars for NamedVars {
    fn column(&self, c: &ast::ColumnRef) -> PlanVar {
        let star = c
            .fields
            .iter()
            .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))));
        let parts = crate::expr::extract_string_fields(&c.fields);
        let entry = |name: &str| self.entries.iter().find(|(a, _)| a == name);
        match (parts.as_slice(), star) {
            // A table's whole row is a composite Var.
            ([.., t], true) => PlanVar {
                rels: entry(t)
                    .map(|(a, _)| [a.clone(), column_key(a, "*")])
                    .into_iter()
                    .flatten()
                    .collect(),
                row: entry(t).is_some_and(|(_, cols)| cols.is_some()),
            },
            ([col], false) if self.merged.iter().any(|(m, _)| m == col) => {
                let (_, rels) = self
                    .merged
                    .iter()
                    .find(|(m, _)| m == col)
                    .expect("a merged column");
                let keys: Vec<String> = rels.iter().map(|a| column_key(a, col)).collect();
                PlanVar {
                    rels: rels.iter().cloned().chain(keys).collect(),
                    row: false,
                }
            }
            ([col], false) => {
                let has = |cols: &Option<HashSet<String>>| {
                    cols.as_ref().is_none_or(|cs| cs.contains(col))
                };
                let mut rels: Vec<String> = self
                    .entries
                    .iter()
                    .filter(|(_, cols)| has(cols))
                    .map(|(a, _)| a.clone())
                    .collect();
                let mut row = false;
                let mut column = col.as_str();
                // A whole-row reference to an entry — or a column of an
                // entry the analyzer doesn't know the columns of.
                if rels.is_empty() {
                    match entry(col) {
                        Some((a, cols)) => {
                            rels.push(a.clone());
                            row = cols.is_some();
                            column = "*";
                        }
                        None => rels = self.entries.iter().map(|(a, _)| a.clone()).collect(),
                    }
                }
                let keys: Vec<String> = rels.iter().map(|a| column_key(a, column)).collect();
                rels.extend(keys);
                PlanVar { rels, row }
            }
            ([.., t, c], false) => PlanVar {
                rels: entry(t)
                    .map(|(a, _)| [a.clone(), column_key(a, c)])
                    .into_iter()
                    .flatten()
                    .collect(),
                row: false,
            },
            _ => PlanVar::default(),
        }
    }

    fn field_column(&self, i: &ast::AIndirection) -> Option<ast::Node> {
        let ast::Node::ColumnRef(c) = i.arg.as_ref()? else {
            return None;
        };
        let [Some(ast::Node::String(field))] = i.indirection.as_slice() else {
            return None;
        };
        let parts = crate::expr::extract_string_fields(&c.fields);
        let [name] = parts.as_slice() else {
            return None;
        };
        self.entries.iter().any(|(a, _)| a == name).then(|| {
            ast::Node::ColumnRef(typedpg_pg_query::NodeBox::new(ast::ColumnRef {
                fields: vec![
                    Some(ast::Node::String(typedpg_pg_query::NodeBox::new(
                        ast::String { sval: name.clone() },
                    ))),
                    Some(ast::Node::String(field.clone())),
                ],
                location: c.location,
            }))
        })
    }

    fn exists(&self, sub: &ast::SubLink) -> HashSet<String> {
        // Any entry the subquery may read.
        let Some(sq) = sub.subselect.as_ref() else {
            return HashSet::new();
        };
        let mut out = HashSet::new();
        for (n, _) in sq.nodes() {
            if let typedpg_pg_query::NodeRef::ColumnRef(c) = n {
                let parts = crate::expr::extract_string_fields(&c.fields);
                match parts.as_slice() {
                    [_] => out.extend(self.entries.iter().map(|(a, _)| a.clone())),
                    [.., t, _] if self.entries.iter().any(|(a, _)| a == t) => {
                        out.insert(t.clone());
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

impl NamedVars {
    /// The entries of the FROM list `from` (joins searched), with the
    /// columns of the tables among them.
    pub(crate) fn of_from(from: &[Option<ast::Node>], snapshot: &PgCatalog) -> NamedVars {
        fn walk(
            n: Option<&ast::Node>,
            snapshot: &PgCatalog,
            out: &mut Vec<(String, Option<HashSet<String>>)>,
        ) {
            match n {
                Some(ast::Node::RangeVar(rv)) => {
                    let alias = rv
                        .alias
                        .as_ref()
                        .map_or(rv.relname.clone(), |a| a.aliasname.clone());
                    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                    let cols = rv
                        .alias
                        .as_ref()
                        .filter(|a| !a.colnames.is_empty())
                        .map_or_else(
                            || {
                                snapshot.resolve_table(schema, &rv.relname).map(|c| {
                                    snapshot
                                        .attributes_of(c.oid)
                                        .iter()
                                        .map(|a| a.attname.clone())
                                        .collect::<HashSet<String>>()
                                })
                            },
                            |_| None,
                        );
                    out.push((alias, cols));
                }
                Some(ast::Node::RangeTableSample(ts)) => walk(ts.relation.as_ref(), snapshot, out),
                Some(ast::Node::JoinExpr(j)) => {
                    walk(j.larg.as_ref(), snapshot, out);
                    walk(j.rarg.as_ref(), snapshot, out);
                    if let Some(a) = j.alias.as_ref() {
                        out.push((a.aliasname.clone(), None));
                    }
                }
                Some(n) => {
                    if let Some(alias) = item_alias(n) {
                        out.push((alias, None));
                    }
                }
                None => {}
            }
        }
        let mut entries = Vec::new();
        for item in from {
            walk(item.as_ref(), snapshot, &mut entries);
        }
        // The entries of side `n` having column `col` (as far as known),
        // through the merged column of a USING join there.
        fn side_rels(n: Option<&ast::Node>, col: &str, snapshot: &PgCatalog) -> Vec<String> {
            if let Some(ast::Node::JoinExpr(j)) = n
                && let Some(names) = using_names(j, snapshot)
                && names.iter().any(|c| c == col)
            {
                return merged_rels(j, col, snapshot);
            }
            let mut out = Vec::new();
            walk(n, snapshot, &mut out);
            out.into_iter()
                .filter(|(_, cols)| cols.as_ref().is_none_or(|cs| cs.contains(col)))
                .map(|(a, _)| a)
                .collect()
        }
        fn merged_rels(j: &ast::JoinExpr, col: &str, snapshot: &PgCatalog) -> Vec<String> {
            match j.jointype {
                ast::JoinType::JoinFull => Vec::new(),
                ast::JoinType::JoinRight => side_rels(j.rarg.as_ref(), col, snapshot),
                _ => side_rels(j.larg.as_ref(), col, snapshot),
            }
        }
        // A USING join's column names; a NATURAL join's, when both sides'
        // columns are known.
        fn using_names(j: &ast::JoinExpr, snapshot: &PgCatalog) -> Option<Vec<String>> {
            if !j.using_clause.is_empty() {
                return Some(crate::expr::extract_string_fields(&j.using_clause));
            }
            if !j.is_natural {
                return None;
            }
            let cols = |n: Option<&ast::Node>| -> Option<HashSet<String>> {
                let mut out = Vec::new();
                walk(n, snapshot, &mut out);
                out.into_iter().try_fold(HashSet::new(), |mut acc, (_, c)| {
                    acc.extend(c?);
                    Some(acc)
                })
            };
            let (l, r) = (cols(j.larg.as_ref())?, cols(j.rarg.as_ref())?);
            Some(l.intersection(&r).cloned().collect())
        }
        fn collect(
            n: Option<&ast::Node>,
            snapshot: &PgCatalog,
            out: &mut Vec<(String, Vec<String>)>,
        ) {
            if let Some(ast::Node::JoinExpr(j)) = n {
                if let Some(names) = using_names(j, snapshot) {
                    for c in names {
                        if !out.iter().any(|(m, _)| *m == c) {
                            out.push((c.clone(), merged_rels(j, &c, snapshot)));
                        }
                    }
                }
                collect(j.larg.as_ref(), snapshot, out);
                collect(j.rarg.as_ref(), snapshot, out);
            }
        }
        let mut merged = Vec::new();
        for item in from {
            collect(item.as_ref(), snapshot, &mut merged);
        }
        NamedVars { entries, merged }
    }
}

/// The alias of a FROM item that isn't a table or a join.
fn item_alias(n: &ast::Node) -> Option<String> {
    match n {
        ast::Node::RangeSubselect(rs) => rs.alias.as_ref().map(|a| a.aliasname.clone()),
        ast::Node::RangeFunction(rf) => rf.alias.as_ref().map(|a| a.aliasname.clone()),
        _ => None,
    }
}

/// Resolution in the scope a qual was typed in, the join aliases expanded
/// as the planner does (see
/// [`crate::nullability::NullabilityContext::plan_var_rels`]).
pub(crate) struct ScopeVars<'a> {
    pub scope: &'a crate::scope::Scope,
    /// The FROM items the qual's level reads (its subqueries' queries).
    pub from: Vec<&'a ast::Node>,
    pub null_ctx: &'a crate::nullability::NullabilityContext,
    pub log: &'a StrictLog,
    pub snapshot: &'a PgCatalog,
}

impl ScopeVars<'_> {
    /// A whole-row reference to FROM entry `alias`: a Var of a table's row
    /// (a composite value); a pulled-up view's or subquery's is a row
    /// constructor of its outputs, a join's of its columns.
    fn whole_row(&self, alias: &str) -> PlanVar {
        let Some(src) = self.scope.find_source(alias) else {
            return PlanVar::default();
        };
        let table = matches!(src.kind, crate::scope::SourceKind::Relation)
            && src
                .relid
                .and_then(|r| self.snapshot.pg_class.get(&r))
                .is_none_or(|c| c.relkind != crate::pg_catalog::RelKind::View);
        PlanVar {
            rels: self.null_ctx.plan_entry_rels(&src.alias),
            row: table,
        }
    }

    /// Whether a strict reference to column `column` of FROM entry `alias`
    /// forces the entry: unless it is a view or subquery on the nullable
    /// side of an outer join of the level whose column the planner replaces
    /// by an expression forcing none of its relations (see
    /// [`nulled_output_forces`]).
    fn output_forces(&self, alias: &str, column: &str) -> bool {
        use crate::scope::SourceKind;
        if !self.null_ctx.syntactically_nulled(alias) {
            return true;
        }
        let Some(src) = self.scope.find_source(alias) else {
            return true;
        };
        match src.kind {
            SourceKind::Subquery { .. } => match find_subselect(&self.from, alias) {
                Some((sel, names)) => nulled_output_forces(sel, &names, column, self.snapshot),
                None => true,
            },
            SourceKind::Relation => {
                let Some(relid) = src.relid else {
                    return true;
                };
                match crate::ddl::views::view_query(self.snapshot, relid) {
                    Some(ast::Node::SelectStmt(sel)) => {
                        let names: Vec<String> = self
                            .snapshot
                            .attributes_of(relid)
                            .iter()
                            .filter(|a| a.attnum > 0)
                            .map(|a| a.attname.clone())
                            .collect();
                        nulled_output_forces(&sel, &names, column, self.snapshot)
                    }
                    _ => true,
                }
            }
            _ => true,
        }
    }
}

/// FROM subquery `alias` among `items` (joins searched): its query and the
/// column names its alias gives.
fn find_subselect<'a>(
    items: &[&'a ast::Node],
    alias: &str,
) -> Option<(&'a ast::SelectStmt, Vec<String>)> {
    fn find<'a>(n: &'a ast::Node, alias: &str) -> Option<(&'a ast::SelectStmt, Vec<String>)> {
        match n {
            ast::Node::RangeSubselect(rs)
                if rs.alias.as_ref().is_some_and(|a| a.aliasname == alias) =>
            {
                match rs.subquery.as_ref()? {
                    ast::Node::SelectStmt(s) => Some((
                        s,
                        rs.alias.as_ref().map_or_else(Vec::new, |a| {
                            crate::expr::extract_string_fields(&a.colnames)
                        }),
                    )),
                    _ => None,
                }
            }
            ast::Node::JoinExpr(j) => j
                .larg
                .as_ref()
                .and_then(|l| find(l, alias))
                .or_else(|| j.rarg.as_ref().and_then(|r| find(r, alias))),
            _ => None,
        }
    }
    items.iter().find_map(|n| find(n, alias))
}

impl PlanVars for ScopeVars<'_> {
    fn column(&self, c: &ast::ColumnRef) -> PlanVar {
        let star = c
            .fields
            .iter()
            .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))));
        let parts = crate::expr::extract_string_fields(&c.fields);
        if star {
            return match parts.last() {
                Some(t) => self.whole_row(t),
                None => PlanVar::default(),
            };
        }
        let (table, column) = match parts.as_slice() {
            [col] => (None, col.as_str()),
            [tbl, col] => (Some(tbl.as_str()), col.as_str()),
            [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
            _ => return PlanVar::default(),
        };
        match self.scope.resolve_column(table, column, None) {
            Ok(col) => {
                let mut rels = self
                    .null_ctx
                    .plan_var_rels(&(col.table_alias.clone(), col.name.clone()));
                if !self.output_forces(&col.table_alias, &col.name) {
                    rels.retain(|r| r != &col.table_alias);
                }
                PlanVar {
                    rels,
                    row: crate::coerce::is_complex(col.type_oid, self.snapshot)
                        || col.type_oid == crate::pg_catalog::oid::RECORD,
                }
            }
            Err(crate::error::AnalyzeError::UndefinedColumn(_)) if table.is_none() => {
                self.whole_row(column)
            }
            Err(_) => PlanVar::default(),
        }
    }

    fn field_column(&self, i: &ast::AIndirection) -> Option<ast::Node> {
        crate::expr::whole_row_field_column(i, self.scope)
    }

    fn exists(&self, sub: &ast::SubLink) -> HashSet<String> {
        // What the subquery's WHERE proves of this level (captured as it
        // was analyzed), and any entry of this level it names.
        let mut out: HashSet<String> = HashSet::new();
        for a in self.log.correlated(sub.location).rels {
            out.extend(self.null_ctx.plan_entry_rels(&a));
        }
        if let Some(sq) = sub.subselect.as_ref() {
            for (n, _) in sq.nodes() {
                if let typedpg_pg_query::NodeRef::ColumnRef(c) = n {
                    let parts = crate::expr::extract_string_fields(&c.fields);
                    if let [.., t, _] = parts.as_slice()
                        && let Some(src) = self.scope.find_source(t)
                    {
                        out.extend(self.null_ctx.plan_entry_rels(&src.alias));
                    }
                }
            }
        }
        out
    }
}
