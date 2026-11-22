//! The `outer_reference_in_subquery` rule: reports an unqualified column
//! reference in a subquery — a sublink (`IN`, `EXISTS`, `ANY`, a scalar
//! subquery), a derived table or a `LATERAL` item — that resolves to a FROM
//! entry of an enclosing query because no relation of the subquery's own
//! level has that column. `SELECT * FROM a WHERE id IN (SELECT id FROM b)`
//! with no `b.id` filters on `a.id IN (a.id)`: every row of `a` with a
//! non-NULL `id` and a non-empty `b` passes.
//!
//! The analyzer records what each reference bound to (see
//! [`typedpg_analyzer::QueryView::column_ref_binding`]). Only references in
//! the subquery's own clauses are read — its select list, `WHERE`,
//! `GROUP BY`, `HAVING`, `ORDER BY`, window definitions and `JOIN … ON`
//! conditions — of a subquery with a FROM clause: a FROM-less subquery has
//! no relation of its own, so every reference in it is meant to be outer,
//! and the arguments of a FROM item's function (`unnest(tags)`) see only
//! the levels around it. A qualified reference (`a.id`) says which
//! relation it means, and is not reported.

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{Node, Node as N, SelectStmt};

use super::location;
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.from_clause.is_empty() {
            continue;
        }
        let mut refs = Vec::new();
        for clause in own_clauses(select) {
            column_refs(clause, &mut refs);
        }
        for item in select.from_clause.iter().flatten() {
            join_quals(item, &mut refs);
        }
        for location in refs {
            if !seen.insert(location) {
                continue;
            }
            let Some(binding) = ctx.view.column_ref_binding(location) else {
                continue;
            };
            if !binding.outer {
                continue;
            }
            let Some(start) = super::location(location) else {
                continue;
            };
            let Some(token) = ctx.tokens.iter().find(|token| token.start == start) else {
                continue;
            };
            let written = &ctx.sql[token.start..token.end];
            let (message, help) = match binding.source.as_deref() {
                Some(source) => {
                    let source = typedpg_core::quote_identifier(source);
                    (
                        format!(
                            "column {written} is not a column of this subquery's relations: it \
                             is {source}.{written} of the outer query"
                        ),
                        format!(
                            "write {source}.{written} if the outer column is meant, or name a \
                             column of the subquery"
                        ),
                    )
                }
                None => (
                    format!(
                        "column {written} is not a column of this subquery's relations: it is \
                         the outer query's"
                    ),
                    "qualify it if the outer column is meant, or name a column of the subquery"
                        .to_owned(),
                ),
            };
            out.push(RawFinding {
                span: token.start..token.end,
                message,
                help: Some(help),
                fix: Vec::new(),
            });
        }
    }
    out
}

/// The clauses of `select` its own level's names resolve in, outside its
/// FROM clause.
fn own_clauses(select: &SelectStmt) -> impl Iterator<Item = &Node> {
    select
        .target_list
        .iter()
        .chain(&select.group_clause)
        .chain(&select.sort_clause)
        .chain(&select.window_clause)
        .chain(&select.distinct_clause)
        .flatten()
        .chain(select.where_clause.as_ref())
        .chain(select.having_clause.as_ref())
}

/// The `ON` conditions of the joins of FROM item `item`, not entering
/// subqueries or function calls.
fn join_quals(item: &Node, out: &mut Vec<i32>) {
    if let N::JoinExpr(join) = item {
        if let Some(quals) = join.quals.as_ref() {
            column_refs(quals, out);
        }
        for side in [join.larg.as_ref(), join.rarg.as_ref()]
            .into_iter()
            .flatten()
        {
            join_quals(side, out);
        }
    }
}

/// Every unqualified column reference in `node`, not entering a nested
/// `SELECT`: a subquery's references are its own level's.
fn column_refs(node: &Node, out: &mut Vec<i32>) {
    let mut stack = vec![node.to_ref()];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::SelectStmt(_) => continue,
            NodeRef::ColumnRef(column) => {
                if let [Some(N::String(_))] = column.fields.as_slice()
                    && location(column.location).is_some()
                {
                    out.push(column.location);
                }
                continue;
            }
            _ => {}
        }
        stack.extend(node.children());
    }
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, lint_rule};

    const RULE: Rule = Rule::OuterReferenceInSubquery;

    #[test]
    fn unqualified_references_to_the_outer_query_are_reported() {
        let db = catalog();
        for (sql, reported, message) in [
            (
                // posts has no `name`: it is users.name.
                "SELECT id FROM users WHERE name IN (SELECT name FROM posts)",
                "name",
                "column name is not a column of this subquery's relations: it is users.name of \
                 the outer query",
            ),
            (
                "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts WHERE title = email)",
                "email",
                "column email is not a column of this subquery's relations: it is u.email of the \
                 outer query",
            ),
            (
                "SELECT (SELECT max(title) FROM posts p WHERE p.user_id = code::bigint) FROM users",
                "code",
                "column code is not a column of this subquery's relations: it is users.code of \
                 the outer query",
            ),
            (
                "SELECT id FROM users u, LATERAL (SELECT title FROM posts WHERE title = name) s",
                "name",
                "column name is not a column of this subquery's relations: it is u.name of the \
                 outer query",
            ),
            (
                "SELECT id FROM users u WHERE EXISTS \
                 (SELECT 1 FROM posts p JOIN posts q ON q.id = email::bigint)",
                "email",
                "column email is not a column of this subquery's relations: it is u.email of the \
                 outer query",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let start = sql.rfind(reported).unwrap();
            assert_eq!(findings[0].span, start..start + reported.len(), "{sql}");
            assert_eq!(findings[0].message, message, "{sql}");
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn references_the_subquery_resolves_itself_are_not_reported() {
        let db = catalog();
        for sql in [
            // Qualified: the outer column on purpose.
            "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
            // `id` is posts.id.
            "SELECT id FROM users WHERE id IN (SELECT id FROM posts)",
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts)",
            // A FROM-less subquery has no relation of its own.
            "SELECT (SELECT name) FROM users",
            "SELECT id FROM users WHERE EXISTS (SELECT 1 WHERE name = 'x')",
            // A FROM function's arguments only see the outer levels.
            "SELECT (SELECT count(*) FROM unnest(ARRAY[name]) n) FROM users",
            // The top level has no outer query.
            "SELECT name FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
