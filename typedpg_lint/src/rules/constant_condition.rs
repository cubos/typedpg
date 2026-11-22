//! The `constant_condition` rule: reports a condition that is always or
//! never true — a `WHERE` (of a `SELECT`, `UPDATE` or `DELETE`), `JOIN …
//! ON`, `HAVING` or aggregate `FILTER (WHERE …)` condition, any `AND` / `OR`
//! / `NOT` argument inside one, and the parts of a `CASE WHEN` condition.
//! Always true, it filters nothing; never true (FALSE or NULL), it keeps no
//! row (or, as an `OR` argument, is dead code).
//!
//! Two proofs report:
//!
//! - The planner's: a condition (a whole one, or an argument of its
//!   top-level `AND`) `eval_const_expressions` certainly folds to a
//!   constant — immutable operators and functions of constants: `1 = 1`,
//!   `'a' = 'b'`, `false`. See
//!   [`typedpg_analyzer::PgCatalog::condition_constant`].
//! - The analyzer's: a built-in comparison or `IN` list decided by what is
//!   known of its operands where it is evaluated — a column's CHECK
//!   constraints, domain or enum, and the conditions holding there (an
//!   earlier `AND` argument, an enclosing `WHERE`): `status = 'actve'` when
//!   `status` is one of `'active'`, `'inactive'`; `n < 0` under `CHECK (n >=
//!   0)`; `a = 2` after `a = 1 AND`; `x = x` on a column never NULL. See
//!   [`typedpg_analyzer::QueryView::condition_fact`]. An `EXISTS` whose
//!   subquery always returns a row (an aggregate without `GROUP BY`, no
//!   `FROM`, `VALUES`) is always true. The message says what decides it.
//!
//! A `CASE WHEN` condition decided as a whole is
//! `unreachable_case_branch`'s finding (the branch, or the ones after it,
//! is never taken); this rule reports the decided parts of one that isn't.
//! Comparisons with a `NULL` literal are `null_comparison`'s, and `IS [NOT]
//! NULL` tests `redundant_null_check`'s.
//!
//! The bare `ON true` of a `JOIN LATERAL … ON true` is the syntax's way to
//! join without a condition, and is not reported.
//!
//! The fix removes a whole `WHERE` the planner folds to true from a
//! `SELECT`, and a whole `HAVING` folded to true from a query with a `GROUP
//! BY` (a `HAVING` without one makes the query grouped: one row for the
//! whole input, which removing it would change). Nothing else has a fix —
//! an always-true `AND` argument, a `WHERE` of an `UPDATE` or `DELETE`
//! (without it, `unbounded_write` reports the statement), a condition that
//! is never true (what the query meant is not what it does), and what only
//! the analyzer's facts decide.

use std::collections::HashSet;

use typedpg_analyzer::{ConditionReason, ProKind};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{
    AConstValue, AExprKind, BoolExprType, CaseWhen, LimitOption, Node, Node as N, SelectStmt,
    SetOperation, SubLinkType,
};

use super::{
    condition_span, expression_end, leftmost_start, operator_name, scan_until, starts_clause,
};
use crate::{Ctx, RawFinding, SqlEdit};

/// The clause a condition is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    /// A `SELECT`'s `WHERE`.
    Where,
    /// A `HAVING`, with whether the query has a `GROUP BY`.
    Having { grouped: bool },
    /// An `UPDATE`'s or `DELETE`'s `WHERE`.
    WriteWhere,
    /// A `JOIN … ON`.
    On,
    /// An aggregate's `FILTER (WHERE …)`.
    Filter,
    /// A `CASE WHEN` condition.
    When,
}

impl Clause {
    fn keyword(self) -> &'static str {
        match self {
            Clause::Where | Clause::WriteWhere | Clause::Filter => "WHERE",
            Clause::Having { .. } => "HAVING",
            Clause::On => "ON",
            Clause::When => "WHEN",
        }
    }
}

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let searched = searched_whens(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::SelectStmt(sel) => {
                condition(ctx, sel.where_clause.as_ref(), Clause::Where, &mut out);
                condition(
                    ctx,
                    sel.having_clause.as_ref(),
                    Clause::Having {
                        grouped: !sel.group_clause.is_empty(),
                    },
                    &mut out,
                );
            }
            NodeRef::UpdateStmt(stmt) => {
                condition(
                    ctx,
                    stmt.where_clause.as_ref(),
                    Clause::WriteWhere,
                    &mut out,
                );
            }
            NodeRef::DeleteStmt(stmt) => {
                condition(
                    ctx,
                    stmt.where_clause.as_ref(),
                    Clause::WriteWhere,
                    &mut out,
                );
            }
            NodeRef::JoinExpr(join) => {
                condition(ctx, join.quals.as_ref(), Clause::On, &mut out);
            }
            NodeRef::FuncCall(call) => {
                condition(ctx, call.agg_filter.as_ref(), Clause::Filter, &mut out);
            }
            NodeRef::CaseWhen(when) => {
                let pointer: *const CaseWhen = when;
                // A WHEN decided as a whole is `unreachable_case_branch`'s.
                if let Some(expr) = when.expr.as_ref()
                    && searched.contains(&pointer)
                    && condition_value(ctx, expr).is_none()
                    && ctx.view.case_when_reached(when.location) != Some(false)
                    && ctx.view.case_when_passed(when.location) != Some(false)
                {
                    semantic(ctx, expr, Clause::When, &HashSet::new(), &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

/// The `WHEN`s of searched `CASE`s (`CASE WHEN cond …`), not of simple
/// ones (`CASE x WHEN v …`, whose `WHEN` holds a value).
fn searched_whens(ctx: &Ctx<'_>) -> HashSet<*const CaseWhen> {
    let mut out = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CaseExpr(case) = node
            && case.arg.is_none()
        {
            for arg in &case.args {
                if let Some(N::CaseWhen(when)) = arg {
                    out.insert(&**when as *const CaseWhen);
                }
            }
        }
    }
    out
}

fn condition(ctx: &Ctx<'_>, qual: Option<&Node>, clause: Clause, out: &mut Vec<RawFinding>) {
    let Some(qual) = qual else {
        return;
    };
    // The nodes the planner's proof reported: the analyzer's says nothing
    // more inside them.
    let mut reported: HashSet<*const Node> = HashSet::new();
    let constant = |node: &Node| {
        // A comparison with a NULL literal is `null_comparison`'s.
        if compares_with_null(ctx, node) {
            return None;
        }
        ctx.catalog.condition_constant(node)
    };
    // Each argument of a top-level AND on its own: one constant argument
    // among others is reported, not the whole condition.
    if let N::BoolExpr(and) = qual
        && and.boolop == BoolExprType::AndExpr
    {
        for arg in and.args.iter().flatten() {
            if let Some(value) = constant(arg)
                && let Some(span) = conjunct_span(ctx, arg)
            {
                reported.insert(arg);
                out.push(finding(span, value, clause, Vec::new()));
            }
        }
        semantic(ctx, qual, clause, &reported, out);
        return;
    }
    let value = constant(qual);
    // `JOIN LATERAL … ON true`: the join condition the syntax requires.
    let lateral_true = clause == Clause::On
        && value == Some(true)
        && matches!(qual, N::AConst(c) if matches!(c.val, Some(AConstValue::Boolean(_))));
    if lateral_true {
        return;
    }
    let Some(value) = value else {
        semantic(ctx, qual, clause, &reported, out);
        return;
    };
    let Some(start) = leftmost_start(qual) else {
        return;
    };
    // The clause's keyword, before the condition and any parentheses
    // around it.
    let keyword = ctx
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| token.end <= start)
        .rfind(|(_, token)| ctx.sql[token.start..token.end].eq_ignore_ascii_case(clause.keyword()));
    // Scanned from the keyword, so parentheses around the condition are
    // part of it.
    let from = keyword.map_or(start, |(_, token)| token.end);
    let (end, _) = scan_until(ctx, from, starts_clause);
    let span = keyword.map_or(start, |(_, token)| token.start)..end.max(start + 1);
    let removable = value
        && match clause {
            Clause::Where => true,
            Clause::Having { grouped } => grouped,
            Clause::WriteWhere | Clause::On | Clause::Filter | Clause::When => false,
        };
    let fix = match keyword {
        // From the end of the token before the keyword: ` WHERE true` goes,
        // what follows stays.
        Some((index, _)) if removable && index > 0 => vec![SqlEdit {
            range: ctx.tokens[index - 1].end..end,
            text: String::new(),
        }],
        _ => Vec::new(),
    };
    out.push(finding(span, value, clause, fix));
}

/// What the analyzer's proof says of the parts of `qual`: each
/// comparison, `IN` list and `EXISTS` under its `AND` / `OR` / `NOT`, but
/// not inside a node `reported` already.
fn semantic(
    ctx: &Ctx<'_>,
    qual: &Node,
    clause: Clause,
    reported: &HashSet<*const Node>,
    out: &mut Vec<RawFinding>,
) {
    let mut stack = vec![(qual, true)];
    while let Some((node, whole)) = stack.pop() {
        if reported.contains(&(node as *const Node)) {
            continue;
        }
        if let N::BoolExpr(b) = node {
            stack.extend(b.args.iter().flatten().rev().map(|a| (a, false)));
            continue;
        }
        // What the planner folds is the other proof's (reported at the top
        // level, or a constant expression written on purpose).
        if ctx.catalog.condition_constant(node).is_some() {
            continue;
        }
        let Some((value, reason)) = leaf_value(ctx, node) else {
            continue;
        };
        let Some(span) = condition_span(ctx, node, whole) else {
            continue;
        };
        let help = match (value, clause) {
            (true, Clause::WriteWhere) => {
                "remove it; to write every row on purpose, drop the WHERE and allow it with \
                 -- typedpg-lint: allow(unbounded_write)"
            }
            (true, _) => "remove it, or fix what it was meant to test",
            (false, _) => "fix the condition (a mistyped value?), or remove it",
        };
        out.push(RawFinding {
            span,
            message: format!(
                "{reason}, so this condition is {}",
                if value { "always true" } else { "never true" }
            ),
            help: Some(help.to_owned()),
            fix: Vec::new(),
        });
    }
}

/// What the analyzer proved of the comparison, `IN` list or `EXISTS`
/// `node`: always true (`true`) or never true, and why, in words.
fn leaf_value(ctx: &Ctx<'_>, node: &Node) -> Option<(bool, String)> {
    match node {
        N::AExpr(expr) if matches!(expr.kind, AExprKind::AexprOp | AExprKind::AexprIn) => {
            // A comparison with a NULL literal is `null_comparison`'s.
            if compares_with_null(ctx, node) {
                return None;
            }
            let fact = ctx.view.condition_fact(expr.location)?;
            let reason = match &fact.reason {
                ConditionReason::SelfComparison => {
                    let subject = operand_text(ctx, expr.lexpr.as_ref()?)?;
                    if fact.value {
                        format!("{subject} is compared with itself and is never NULL here")
                    } else {
                        format!("{subject} is compared with itself")
                    }
                }
                ConditionReason::Known {
                    left,
                    known,
                    source,
                } => {
                    let operand = if *left { &expr.lexpr } else { &expr.rexpr };
                    let subject = operand_text(ctx, operand.as_ref()?)?;
                    match source {
                        Some(source) => format!("{subject} is {known} ({source})"),
                        None => format!("{subject} is {known} where this is evaluated"),
                    }
                }
            };
            Some((fact.value, reason))
        }
        N::SubLink(sub) if sub.sub_link_type == SubLinkType::ExistsSublink => {
            let Some(N::SelectStmt(select)) = sub.subselect.as_ref() else {
                return None;
            };
            always_returns_a_row(ctx, select)
                .then(|| (true, "the EXISTS subquery always returns a row".to_owned()))
        }
        _ => None,
    }
}

/// Whether condition `node` is always (`true`) or never true — and why,
/// when the analyzer's facts decide it: the planner folds it, or its parts
/// decide it (each part is judged knowing what the ones before it say) —
/// an `AND` with a part never true or every part always true, an `OR` with
/// a part always true or every part never true, a `NOT` of a part always
/// true.
pub(super) fn condition_value(ctx: &Ctx<'_>, node: &Node) -> Option<(bool, Option<String>)> {
    if let Some(value) = ctx.catalog.condition_constant(node) {
        return Some((value, None));
    }
    match node {
        N::BoolExpr(b) => {
            let parts: Vec<Option<(bool, Option<String>)>> = b
                .args
                .iter()
                .flatten()
                .map(|a| condition_value(ctx, a))
                .collect();
            if b.boolop == BoolExprType::NotExpr {
                return match parts.as_slice() {
                    [Some((true, reason))] => Some((false, reason.clone())),
                    _ => None,
                };
            }
            let decisive = b.boolop == BoolExprType::OrExpr;
            if let Some(Some((value, reason))) = parts
                .iter()
                .find(|p| matches!(p, Some((v, _)) if *v == decisive))
            {
                return Some((*value, reason.clone()));
            }
            if !parts.is_empty()
                && parts
                    .iter()
                    .all(|p| matches!(p, Some((v, _)) if *v != decisive))
            {
                let reason = parts.iter().flatten().find_map(|(_, r)| r.clone());
                return Some((!decisive, reason));
            }
            None
        }
        _ => leaf_value(ctx, node).map(|(value, reason)| (value, Some(reason))),
    }
}

/// The text of an operand, for a message: as written, in backquotes.
fn operand_text(ctx: &Ctx<'_>, node: &Node) -> Option<String> {
    let start = leftmost_start(node)?;
    let end = expression_end(ctx, start);
    let text = ctx.sql.get(start..end)?.trim();
    (!text.is_empty()).then(|| format!("`{text}`"))
}

/// Whether `select`, an `EXISTS` subquery, returns a row whatever the
/// data: one `VALUES` row or more, no `FROM` and no `WHERE`, or an
/// aggregate of its own level without `GROUP BY` or `HAVING` — with no
/// set-returning call that may make none, and no `LIMIT` or `OFFSET`
/// that may drop it.
fn always_returns_a_row(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    if select.op != SetOperation::SetopNone
        || select.limit_offset.is_some()
        || select.limit_option == LimitOption::WithTies
        || select.into_clause.is_some()
        || !select.locking_clause.is_empty()
    {
        return false;
    }
    let limited = select.limit_count.as_ref().is_some_and(|count| {
        !matches!(count, N::AConst(c)
            if !c.isnull && matches!(&c.val, Some(AConstValue::Integer(i)) if i.ival >= 1))
    });
    if limited || may_return_a_set(ctx, NodeRef::SelectStmt(select)) {
        return false;
    }
    if !select.values_lists.is_empty() {
        return true;
    }
    if select.having_clause.is_some() || !select.group_clause.is_empty() {
        return false;
    }
    // Without FROM, a WHERE may drop the one row; an aggregate's one group
    // is there whatever WHERE drops.
    (select.from_clause.is_empty() && select.where_clause.is_none())
        || (!select.from_clause.is_empty() && own_aggregate(ctx, select))
}

/// The names of a function or operator name list.
fn name_parts(nodes: &[Option<Node>]) -> Vec<&str> {
    nodes
        .iter()
        .filter_map(|n| match n {
            Some(N::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect()
}

/// Whether a call under `node` may return a set (any overload of the name,
/// an unresolved one too), outside nested queries.
fn may_return_a_set(ctx: &Ctx<'_>, node: NodeRef<'_>) -> bool {
    let mut stack: Vec<NodeRef<'_>> = node.children().collect();
    while let Some(n) = stack.pop() {
        match n {
            NodeRef::SelectStmt(_) | NodeRef::SubLink(_) => continue,
            NodeRef::FuncCall(call) => {
                let names = name_parts(&call.funcname);
                let procs = match names.as_slice() {
                    [name] => ctx.catalog.find_functions(None, name),
                    [schema, name] => ctx.catalog.find_functions(Some(schema), name),
                    _ => return true,
                };
                if procs.is_empty() || procs.iter().any(|p| p.proretset) {
                    return true;
                }
            }
            NodeRef::AExpr(e) => {
                if let Some(name) = operator_name(e) {
                    let set_returning = ctx.catalog.pg_operator().values().any(|o| {
                        o.oprname == name
                            && o.oprcode
                                .and_then(|code| ctx.catalog.pg_proc().get(&code))
                                .is_none_or(|p| p.proretset)
                    });
                    if set_returning {
                        return true;
                    }
                }
            }
            _ => {}
        }
        stack.extend(n.children());
    }
    false
}

/// Whether `select`'s target list calls an aggregate of `select`'s own
/// level (no `OVER`, every overload of the name an aggregate, its
/// arguments reading no outer query's column): the query then yields one
/// row per group, and one group without `GROUP BY`.
fn own_aggregate(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = select
        .target_list
        .iter()
        .flatten()
        .map(|n| n.to_ref())
        .collect();
    while let Some(n) = stack.pop() {
        match n {
            NodeRef::SelectStmt(_) | NodeRef::SubLink(_) => continue,
            NodeRef::FuncCall(call) if call.over.is_none() => {
                let names = name_parts(&call.funcname);
                let procs = match names.as_slice() {
                    [name] => ctx.catalog.find_functions(None, name),
                    [schema, name] => ctx.catalog.find_functions(Some(schema), name),
                    _ => Vec::new(),
                };
                let aggregate =
                    !procs.is_empty() && procs.iter().all(|p| p.prokind == ProKind::Aggregate);
                if aggregate && !reads_outer(ctx, NodeRef::FuncCall(call)) {
                    return true;
                }
            }
            _ => {}
        }
        stack.extend(n.children());
    }
    false
}

/// Whether a column reference under `node` binds to an enclosing query
/// level, or isn't known to bind.
fn reads_outer(ctx: &Ctx<'_>, node: NodeRef<'_>) -> bool {
    if let NodeRef::ColumnRef(c) = node {
        return ctx
            .view
            .column_ref_binding(c.location)
            .is_none_or(|b| b.outer);
    }
    node.children().any(|c| reads_outer(ctx, c))
}

/// Whether `node` holds an operator call or `IN` list with a `NULL`
/// literal operand, which `null_comparison` reports.
fn compares_with_null(ctx: &Ctx<'_>, node: &Node) -> bool {
    let is_null = |n: Option<&Node>| n.is_some_and(|n| super::is_null_literal(ctx, n));
    match node {
        N::AExpr(expr) => {
            is_null(expr.lexpr.as_ref())
                || is_null(expr.rexpr.as_ref())
                || matches!(expr.rexpr.as_ref(), Some(N::List(list))
                    if list.items.iter().any(|item| is_null(item.as_ref())))
                || expr
                    .lexpr
                    .as_ref()
                    .is_some_and(|n| compares_with_null(ctx, n))
                || expr
                    .rexpr
                    .as_ref()
                    .is_some_and(|n| compares_with_null(ctx, n))
        }
        N::BoolExpr(expr) => expr
            .args
            .iter()
            .flatten()
            .any(|n| compares_with_null(ctx, n)),
        _ => false,
    }
}

/// The span of an `AND` argument: from its first token to the `AND` / `OR`
/// (outside a `BETWEEN … AND`), clause keyword or closing parenthesis that
/// ends it.
fn conjunct_span(ctx: &Ctx<'_>, arg: &Node) -> Option<std::ops::Range<usize>> {
    let start = leftmost_start(arg)?;
    let mut between = false;
    let (end, _) = scan_until(ctx, start, |text| {
        if text.eq_ignore_ascii_case("BETWEEN") {
            between = true;
            return false;
        }
        if text.eq_ignore_ascii_case("AND") && between {
            between = false;
            return false;
        }
        text.eq_ignore_ascii_case("AND") || text.eq_ignore_ascii_case("OR") || starts_clause(text)
    });
    Some(start..end.max(start + 1))
}

fn finding(
    span: std::ops::Range<usize>,
    value: bool,
    clause: Clause,
    fix: Vec<SqlEdit>,
) -> RawFinding {
    let (message, help) = if value {
        let help = match clause {
            Clause::WriteWhere => {
                "remove it; to write every row on purpose, drop the WHERE and allow it with \
                 -- typedpg-lint: allow(unbounded_write)"
            }
            Clause::On => "remove it, or use a CROSS JOIN",
            Clause::Where | Clause::Having { .. } | Clause::Filter | Clause::When => "remove it",
        };
        ("condition is always true: it filters nothing", help)
    } else {
        let help = match clause {
            Clause::On => "the join matches no row: check the condition",
            Clause::Having { .. } => "no group passes it: check the condition",
            Clause::Where | Clause::WriteWhere | Clause::Filter | Clause::When => {
                "no row passes it: check the condition"
            }
        };
        ("condition is never true", help)
    };
    RawFinding {
        span,
        message: message.to_owned(),
        help: Some(help.to_owned()),
        fix,
    }
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::ConstantCondition;

    #[test]
    fn a_where_that_is_always_true_is_removed() {
        let db = catalog();
        for (sql, expected) in [
            ("SELECT id FROM users WHERE true", "SELECT id FROM users"),
            (
                "SELECT id FROM users WHERE 1 = 1 ORDER BY id",
                "SELECT id FROM users ORDER BY id",
            ),
            (
                "SELECT id FROM users WHERE (2 > 1) LIMIT 1",
                "SELECT id FROM users LIMIT 1",
            ),
            (
                "SELECT id FROM users WHERE 'a' <> 'b' GROUP BY id",
                "SELECT id FROM users GROUP BY id",
            ),
            (
                "SELECT id FROM (SELECT id FROM users WHERE true) s",
                "SELECT id FROM (SELECT id FROM users) s",
            ),
            (
                "SELECT count(*) FROM users GROUP BY name HAVING 1 < 2",
                "SELECT count(*) FROM users GROUP BY name",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(
                findings[0].message,
                "condition is always true: it filters nothing"
            );
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn what_has_no_fix() {
        let db = catalog();
        for sql in [
            // One argument of an AND.
            "SELECT id FROM users WHERE name = 'x' AND 1 = 1",
            // A HAVING without GROUP BY groups the query.
            "SELECT 1 FROM users HAVING true",
            // UPDATE / DELETE: without WHERE, unbounded_write reports.
            "UPDATE users SET name = 'x' WHERE true",
            "DELETE FROM users WHERE 1 = 1",
            // A JOIN's ON.
            "SELECT u.id FROM users u JOIN users v ON 1 = 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].fix.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_condition_that_is_never_true_is_reported() {
        let db = catalog();
        for (sql, reported) in [
            ("SELECT id FROM users WHERE false", "WHERE false"),
            ("SELECT id FROM users WHERE 1 = 2", "WHERE 1 = 2"),
            ("SELECT id FROM users WHERE NULL", "WHERE NULL"),
            ("SELECT id FROM users WHERE name = 'x' AND 1 > 2", "1 > 2"),
            (
                "SELECT id FROM users WHERE id BETWEEN 1 AND 2 AND 1 > 2",
                "1 > 2",
            ),
            (
                "SELECT u.id FROM users u LEFT JOIN users v ON false",
                "ON false",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].message, "condition is never true", "{sql}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert!(findings[0].fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn conditions_that_may_vary_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id = 1",
            "SELECT id FROM users WHERE $1",
            "SELECT id FROM users WHERE $1 = 1",
            "SELECT id FROM users WHERE random() > 0.5",
            "SELECT id FROM users WHERE now() > '2020-01-01'",
            "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM posts)",
            // The condition `JOIN LATERAL` needs.
            "SELECT u.id FROM users u LEFT JOIN LATERAL (SELECT 1 x) s ON true",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    // ── What the analyzer's facts decide ──────────────────────────────────

    fn refined() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
                 CREATE DOMAIN pct AS int CHECK (VALUE BETWEEN 0 AND 100);
                 CREATE TABLE orders (
                    id bigint PRIMARY KEY,
                    status text NOT NULL,
                    kind text,
                    qty int NOT NULL CHECK (qty >= 0),
                    price numeric CHECK (price > 0),
                    feeling mood NOT NULL,
                    discount pct,
                    note text,
                    legacy text,
                    CONSTRAINT orders_status_check CHECK (status IN ('open', 'paid', 'void')),
                    CHECK (kind IN ('a', 'b'))
                 );
                 CREATE TABLE old (
                    id int PRIMARY KEY,
                    state text NOT NULL
                 );
                 ALTER TABLE old ADD CONSTRAINT old_state CHECK (state IN ('x', 'y')) NOT VALID;
                 CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', \
                    deterministic = false);
                 CREATE TABLE tags (
                    label text COLLATE ci NOT NULL CHECK (label IN ('a', 'b'))
                 );",
            )
            .unwrap();
        catalog
    }

    /// The single finding of `sql`, its reported text and message.
    fn one(db: &PgCatalog, sql: &str) -> (String, String) {
        let findings = lint_rule(db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        assert!(findings[0].fix.is_empty(), "{sql}");
        (
            sql[findings[0].span.clone()].to_owned(),
            findings[0].message.clone(),
        )
    }

    #[test]
    fn a_value_the_column_cannot_have_is_never_true() {
        let db = refined();
        for (sql, reported, message) in [
            (
                "SELECT id FROM orders WHERE status = 'opne'",
                "status = 'opne'",
                "`status` is always one of 'open', 'paid', 'void' (CHECK constraint \
                 \"orders_status_check\"), so this condition is never true",
            ),
            (
                "SELECT id FROM orders WHERE status IN ('closd', 'shipped')",
                "status IN ('closd', 'shipped')",
                "`status` is always one of 'open', 'paid', 'void' (CHECK constraint \
                 \"orders_status_check\"), so this condition is never true",
            ),
            (
                "SELECT id FROM orders WHERE qty < 0",
                "qty < 0",
                "`qty` is always at least 0 (CHECK constraint \"orders_qty_check\"), so this \
                 condition is never true",
            ),
            (
                "SELECT id FROM orders WHERE discount > 200",
                "discount > 200",
                "`discount` is always between 0 and 100 (domain \"pct\"), so this condition is \
                 never true",
            ),
            ("SELECT id FROM orders WHERE price <= 0", "price <= 0", ""),
            // Reversed operands.
            (
                "SELECT id FROM orders WHERE 'opne' = status",
                "'opne' = status",
                "",
            ),
            // In an OR: dead code.
            (
                "SELECT id FROM orders WHERE id = $1 OR kind = 'c'",
                "kind = 'c'",
                "",
            ),
            // An ON clause, a HAVING, a FILTER, an UPDATE.
            (
                "SELECT o.id FROM orders o JOIN orders p ON p.id = o.id AND p.kind = 'z'",
                "p.kind = 'z'",
                "",
            ),
            (
                "SELECT kind FROM orders GROUP BY kind HAVING kind = 'z'",
                "kind = 'z'",
                "",
            ),
            (
                "SELECT count(*) FILTER (WHERE status = 'x') FROM orders",
                "status = 'x'",
                "",
            ),
            (
                "UPDATE orders SET note = 'n' WHERE status = 'x'",
                "status = 'x'",
                "",
            ),
            // The parts of a CASE WHEN not decided as a whole.
            (
                "SELECT CASE WHEN id = $1 OR status = 'x' THEN 1 ELSE 2 END FROM orders",
                "status = 'x'",
                "",
            ),
        ] {
            let (text, msg) = one(&db, sql);
            assert_eq!(text, reported, "{sql}");
            assert!(
                msg.ends_with("so this condition is never true"),
                "{sql}: {msg}"
            );
            if !message.is_empty() {
                assert_eq!(msg, message, "{sql}");
            }
        }
    }

    #[test]
    fn what_earlier_conditions_settle_is_reported() {
        let db = refined();
        for (sql, reported, value) in [
            (
                "SELECT id FROM orders WHERE note = 'a' AND note = 'b'",
                "note = 'b'",
                "never true",
            ),
            (
                "SELECT id FROM orders WHERE qty > 5 AND qty > 3",
                "qty > 3",
                "always true",
            ),
            (
                "SELECT id FROM orders WHERE id = $1 AND qty = qty",
                "qty = qty",
                "always true",
            ),
            (
                "SELECT id FROM orders WHERE status IN ('open', 'paid', 'void')",
                "status IN ('open', 'paid', 'void')",
                "always true",
            ),
            (
                "SELECT id FROM orders WHERE qty >= 0",
                "qty >= 0",
                "always true",
            ),
            (
                "SELECT id FROM orders WHERE qty <> qty",
                "qty <> qty",
                "never true",
            ),
            (
                "SELECT id FROM orders WHERE EXISTS (SELECT count(*) FROM orders o WHERE o.id = 1)",
                "EXISTS (SELECT count(*) FROM orders o WHERE o.id = 1)",
                "always true",
            ),
            (
                "SELECT id FROM orders WHERE NOT EXISTS (SELECT 1)",
                "EXISTS (SELECT 1)",
                "always true",
            ),
        ] {
            let (text, msg) = one(&db, sql);
            assert_eq!(text, reported, "{sql}");
            assert!(msg.ends_with(value), "{sql}: {msg}");
        }
        let (_, msg) = one(&db, "SELECT id FROM orders WHERE note = 'a' AND note = 'b'");
        assert_eq!(
            msg,
            "`note` is always 'a' where this is evaluated, so this condition is never true"
        );
    }

    #[test]
    fn what_may_vary_or_others_own_is_not_reported() {
        let db = refined();
        for sql in [
            "SELECT id FROM orders WHERE status = 'open'",
            "SELECT id FROM orders WHERE status IN ('open', 'paid')",
            "SELECT id FROM orders WHERE status = $1",
            "SELECT id FROM orders WHERE qty > 0",
            "SELECT id FROM orders WHERE qty < 5",
            "SELECT id FROM orders WHERE price > 1",
            "SELECT id FROM orders WHERE note = 'x'",
            // Nullable: TRUE or NULL is not always true.
            "SELECT id FROM orders WHERE kind IN ('a', 'b')",
            "SELECT id FROM orders WHERE price > 0",
            "SELECT id FROM orders WHERE note = note",
            // The earlier condition doesn't settle it.
            "SELECT id FROM orders WHERE qty > 3 AND qty > 5",
            "SELECT id FROM orders WHERE note = 'a' OR note = 'b'",
            // A NOT VALID constraint: old rows may hold any value.
            "SELECT id FROM old WHERE state = 'z'",
            // A nondeterministic collation: 'A' passes `IN ('a', 'b')`.
            "SELECT label FROM tags WHERE label = 'A'",
            // Others' findings.
            "SELECT id FROM orders WHERE status = NULL",
            "SELECT id FROM orders WHERE status IS NULL",
            "SELECT id FROM orders WHERE status IS NOT NULL",
            // An enum label the type has (all of them: the analyzer's
            // refinement of an enum).
            "SELECT id FROM orders WHERE feeling = 'ok'",
            // A subquery that may return no row.
            "SELECT id FROM orders WHERE EXISTS (SELECT 1 FROM orders o WHERE o.qty > 3)",
            "SELECT id FROM orders WHERE EXISTS (SELECT count(*) FROM orders HAVING count(*) > 1)",
            "SELECT id FROM orders WHERE EXISTS (SELECT generate_series(1, 0))",
            "SELECT id FROM orders WHERE EXISTS (SELECT 1 LIMIT $1)",
            // An outer query's aggregate.
            "SELECT o.id FROM orders o GROUP BY o.id \
             HAVING EXISTS (SELECT max(o.qty) FROM orders p WHERE p.id = 1)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_when_decided_as_a_whole_is_left_to_unreachable_case_branch() {
        let db = refined();
        for sql in [
            "SELECT CASE WHEN status = 'x' THEN 1 ELSE 2 END FROM orders",
            "SELECT CASE WHEN qty >= 0 THEN 1 ELSE 2 END FROM orders",
            "SELECT CASE WHEN id = $1 AND status = 'x' THEN 1 ELSE 2 END FROM orders",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
            let branch = lint_rule(&db, sql, Rule::UnreachableCaseBranch);
            assert_eq!(branch.len(), 1, "{sql}: {branch:?}");
        }
    }
}
