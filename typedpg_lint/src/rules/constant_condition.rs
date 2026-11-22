//! The `constant_condition` rule: reports a `WHERE`, `JOIN … ON` or
//! `HAVING` condition, or an `AND` argument of one, that the planner
//! certainly folds to a constant (`eval_const_expressions` over immutable
//! operators and functions of constants: `1 = 1`, `'a' = 'b'`, `false`):
//! always true, it filters nothing; never true (FALSE or NULL), it keeps no
//! row. See [`typedpg_analyzer::PgCatalog::condition_constant`].
//!
//! The bare `ON true` of a `JOIN LATERAL … ON true` is the syntax's way to
//! join without a condition, and is not reported.
//!
//! The fix removes a whole `WHERE` that is always true from a `SELECT`, and
//! a whole `HAVING` that is always true from a query with a `GROUP BY` (a
//! `HAVING` without one makes the query grouped: one row for the whole
//! input, which removing it would change). An always-true `AND` argument
//! has no fix, nor has a `WHERE` of an `UPDATE` or `DELETE` (without it,
//! `unbounded_write` reports the statement), nor a condition that is never
//! true: what the query meant to do is not what it does.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, BoolExprType, Node, Node as N};

use super::{leftmost_start, scan_until, starts_clause};
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
}

impl Clause {
    fn keyword(self) -> &'static str {
        match self {
            Clause::Where | Clause::WriteWhere => "WHERE",
            Clause::Having { .. } => "HAVING",
            Clause::On => "ON",
        }
    }
}

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
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
            _ => {}
        }
    }
    out
}

fn condition(ctx: &Ctx<'_>, qual: Option<&Node>, clause: Clause, out: &mut Vec<RawFinding>) {
    let Some(qual) = qual else {
        return;
    };
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
                out.push(finding(span, value, clause, Vec::new()));
            }
        }
        return;
    }
    let Some(value) = constant(qual) else {
        return;
    };
    // `JOIN LATERAL … ON true`: the join condition the syntax requires.
    if clause == Clause::On
        && value
        && matches!(qual, N::AConst(c) if matches!(c.val, Some(AConstValue::Boolean(_))))
    {
        return;
    }
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
            Clause::WriteWhere | Clause::On => false,
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
            Clause::Where | Clause::Having { .. } => "remove it",
        };
        ("condition is always true: it filters nothing", help)
    } else {
        let help = match clause {
            Clause::On => "the join matches no row: check the condition",
            Clause::Having { .. } => "no group passes it: check the condition",
            Clause::Where | Clause::WriteWhere => "no row passes it: check the condition",
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
            "SELECT id FROM users WHERE EXISTS (SELECT 1)",
            // The condition `JOIN LATERAL` needs.
            "SELECT u.id FROM users u LEFT JOIN LATERAL (SELECT 1 x) s ON true",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
