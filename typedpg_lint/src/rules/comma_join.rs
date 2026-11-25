//! The `comma_join` rule: reports a `SELECT`'s `FROM` list that joins a
//! relation to an earlier one with a comma (`FROM users u, posts p WHERE
//! u.id = p.user_id`) instead of `JOIN … ON`. A comma is easy to read as a
//! cross join, and the condition that really joins the two can sit far from
//! either in the `WHERE` clause.
//!
//! Not reported:
//! - a single relation: there is no comma join;
//! - a `FROM` function (`FROM t, unnest(t.arr) x`, `jsonb_array_elements(…)`,
//!   `JSON_TABLE(…)`): a comma before a set-returning function is the
//!   idiomatic way to write the lateral join it forms;
//! - a `LATERAL` subquery (`FROM t, LATERAL (SELECT …) s`): the comma is
//!   that lateral join too;
//! - a comma between two functions, or before the first relation of the
//!   list: no two relations are joined by it.
//!
//! A `VALUES` list (`VALUES (1, 2), (3, 4)`) has no `FROM` clause and is not
//! reported; a `(VALUES …)` subquery in `FROM` is a relation like any other.
//!
//! No fix is offered: rewriting the pair as a join means moving the
//! conditions tying them from `WHERE` to `ON`, and which conditions those
//! are is the author's call.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{Node, Node as N};

use super::{location, scan_until, starts_clause};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.from_clause.len() < 2 {
            continue;
        }
        // The first relation of the list, and each later one after it: the
        // commas between them are the joins reported. `None` until a
        // relation is seen.
        let mut first: Option<String> = None;
        for item in select.from_clause.iter().flatten() {
            let Some(display) = relation_display(item) else {
                // A function or a lateral subquery: a comma to it is
                // idiomatic, and it is no relation the next comma joins.
                continue;
            };
            match &first {
                None => first = Some(display),
                Some(anchor) => {
                    let Some(start) = item_start(ctx, item) else {
                        continue;
                    };
                    // A join item runs to the end of its last operand and ON
                    // clause; a plain item to the comma or clause after it.
                    let end = match item {
                        N::JoinExpr(_) => {
                            super::node_span(ctx, item.to_ref()).map_or(start, |span| span.end)
                        }
                        _ => scan_until(ctx, start, |text| text == "," || starts_clause(text)).0,
                    };
                    out.push(RawFinding {
                        span: start..end.max(start + 1),
                        message: format!(
                            "{display} is joined to {anchor} with a comma: write JOIN … ON"
                        ),
                        help: Some(
                            "move the condition tying them into an ON clause (CROSS JOIN when \
                             every combination is meant)"
                                .to_owned(),
                        ),
                        fix: Vec::new(),
                    });
                }
            }
        }
    }
    out
}

/// How the finding names a `FROM` item, or `None` for an item a comma join
/// doesn't report (see the module docs).
fn relation_display(item: &Node) -> Option<String> {
    match item {
        N::RangeVar(range) => Some(relation_name(range)),
        N::RangeTableSample(sample) => match sample.relation.as_ref() {
            Some(N::RangeVar(range)) => Some(relation_name(range)),
            _ => None,
        },
        N::RangeSubselect(sub) if !sub.lateral => Some(match &sub.alias {
            Some(alias) => typedpg_core::quote_identifier(&alias.aliasname),
            None => "a subquery".to_owned(),
        }),
        N::JoinExpr(join) => Some(
            match join.alias.as_ref().or(join.join_using_alias.as_ref()) {
                Some(alias) => typedpg_core::quote_identifier(&alias.aliasname),
                None => "a join".to_owned(),
            },
        ),
        _ => None,
    }
}

fn relation_name(range: &typedpg_pg_query::ast::RangeVar) -> String {
    let name = range
        .alias
        .as_ref()
        .map_or(range.relname.as_str(), |alias| alias.aliasname.as_str());
    typedpg_core::quote_identifier(name)
}

/// The byte `item` starts at.
fn item_start(ctx: &Ctx<'_>, item: &Node) -> Option<usize> {
    match item {
        N::RangeVar(range) => location(range.location),
        N::RangeTableSample(sample) => match sample.relation.as_ref() {
            Some(N::RangeVar(range)) => location(range.location),
            _ => None,
        },
        // A parenthesized subquery starts at its `(`.
        N::RangeSubselect(sub) => {
            super::cartesian_join::subquery_start(ctx, sub.subquery.as_ref()?)
        }
        // A join starts where its left side does.
        N::JoinExpr(join) => item_start(ctx, join.larg.as_ref()?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::CommaJoin;

    #[test]
    fn relations_joined_with_a_comma_are_reported() {
        let db = catalog();
        for (sql, reported) in [
            (
                "SELECT 1 FROM users u, posts p WHERE u.id = p.user_id",
                "posts p",
            ),
            (
                "SELECT u.id, p.id FROM users AS u, posts AS p WHERE u.id = p.user_id",
                "posts AS p",
            ),
            (
                "SELECT 1 FROM users u, (SELECT user_id FROM posts) s",
                "(SELECT user_id FROM posts) s",
            ),
            (
                "SELECT 1 FROM users u, posts p JOIN posts q ON q.id = p.id",
                "posts p JOIN posts q ON q.id = p.id",
            ),
            (
                "SELECT 1 FROM users u CROSS JOIN posts p, users v",
                "users v",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert!(!findings[0].message.is_empty(), "{sql}");
            assert!(findings[0].help.is_some(), "{sql}");
            assert_eq!(fixed(sql, &findings), sql, "{sql}");
        }
    }

    #[test]
    fn each_later_relation_is_reported() {
        let db = catalog();
        let sql = "SELECT 1 FROM users u, posts p, posts q";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "posts p");
        assert_eq!(&sql[findings[1].span.clone()], "posts q");
    }

    #[test]
    fn single_relations_functions_and_lateral_joins_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT 1 FROM users",
            "SELECT 1 FROM users u JOIN posts p ON p.user_id = u.id",
            // A function's comma is the idiomatic lateral join.
            "SELECT 1 FROM users u, unnest(ARRAY[u.name]) n",
            "SELECT 1 FROM users u, generate_series(1, 3) g",
            "SELECT 1 FROM generate_series(1, 3) g, unnest(ARRAY[g]) n",
            "SELECT 1 FROM generate_series(1, 3) g, users u",
            // So is a LATERAL subquery's.
            "SELECT 1 FROM users u, LATERAL (SELECT title FROM posts p WHERE p.user_id = u.id) s",
            "SELECT 1 FROM users u, LATERAL (SELECT 1) s",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
