//! The `redundant_order_by` rule: reports `ORDER BY` keys that can't change
//! the order of the rows.
//!
//! - A key after keys that already determine a unique row (the
//!   [`typedpg_analyzer::order_ties`] proof `unstable_order` uses, run over
//!   the leading keys): no two rows tie on those, so it never breaks a tie.
//! - A plain column `WHERE` equates to a constant or parameter through the
//!   column type's default btree equality (deterministic collation): every
//!   row sorts it the same.
//! - A plain column an earlier key already sorts by: rows tied up to it
//!   have equal values.
//! - The whole `ORDER BY` of a query the analyzer proves returns at most
//!   one row (with `DISTINCT ON`, has at most one row before it: the keys
//!   after its expressions pick the row it keeps, even when it returns a
//!   single group), or of an `EXISTS`, `IN` / `= ANY` or `ALL` subquery without
//!   `LIMIT`, `OFFSET`, `DISTINCT ON` or a locking clause: those only test
//!   membership, so the order of the subquery's rows is never observed.
//!   (`FROM` subqueries, CTEs and `ARRAY(…)` are not judged: an aggregate
//!   or the client may observe their order in practice.)
//!
//! The fix removes the keys — the whole clause when all go — but only keys
//! that are a plain column, an output column's name or an ordinal: removing
//! any other expression could drop an error or a volatile call it
//! evaluates. A `DISTINCT ON`'s leading keys, which must match its
//! expressions, are never removed, and neither is the last key of a query
//! with `LIMIT`, `OFFSET` or `DISTINCT ON` that isn't proven one row (the
//! order is arbitrary then, which `unstable_order` reports).

use std::collections::HashSet;

use typedpg_analyzer::{order_keys, unique_order_prefix};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, Node, SelectStmt, SubLinkType};

use super::limit_without_order::picks_from_at_most_one_row;
use super::{clause_item_spans, list_item_removal};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let unobserved = unobserved_selects(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.sort_clause.is_empty() {
            continue;
        }
        let Some(sorts) = sort_nodes(select) else {
            continue;
        };
        let Some(spans) = clause_item_spans(ctx, &sorts) else {
            continue;
        };
        let safe: Vec<bool> = sorts.iter().map(|node| removable_key(*node)).collect();
        let picks_rows = select.limit_count.is_some()
            || select.limit_offset.is_some()
            || distinct_on_len(select) > 0;
        let pointer: *const SelectStmt = select;
        let whole =
            if unobserved.contains(&pointer) && !picks_rows && select.locking_clause.is_empty() {
                Some("a subquery that only tests membership never observes its rows' order")
            } else if picks_from_at_most_one_row(ctx, select) {
                // With DISTINCT ON, the rows before it: the keys after its
                // expressions pick the row each group keeps, even when only
                // one group is returned.
                Some("the query returns at most one row")
            } else {
                None
            };
        if let Some(why) = whole {
            if let Some(finding) = whole_clause(ctx, &spans, &safe, why) {
                out.push(finding);
            }
            continue;
        }
        let Some(keys) = order_keys(ctx.catalog, ctx.view, select) else {
            continue;
        };
        let prefix = unique_order_prefix(ctx.catalog, ctx.view, select);
        let fixed = distinct_on_len(select);
        let mut reasons: Vec<Option<String>> = Vec::with_capacity(keys.len());
        for (i, key) in keys.iter().enumerate() {
            let reason = if i < fixed {
                None
            } else if key.pinned {
                Some("WHERE pins it to one value, so every row sorts it the same".to_owned())
            } else if key.column.is_some() && keys[..i].iter().any(|k| k.column == key.column) {
                Some("an earlier ORDER BY key already sorts by the same column".to_owned())
            } else if prefix.is_some_and(|k| i >= k) {
                Some(
                    "the keys before it already determine a unique row, so it never breaks a tie"
                        .to_owned(),
                )
            } else {
                None
            };
            reasons.push(reason);
        }
        let removed: Vec<usize> = (0..reasons.len())
            .filter(|&i| reasons[i].is_some())
            .collect();
        if removed.is_empty() {
            continue;
        }
        if removed.len() == reasons.len() {
            // Every key is pinned or repeated: the order is arbitrary. With
            // LIMIT / OFFSET / DISTINCT ON, removing it would only trade
            // this finding for `limit_without_order`'s.
            if !picks_rows
                && let Some(finding) =
                    whole_clause(ctx, &spans, &safe, "WHERE pins every key to one value")
            {
                out.push(finding);
            }
            continue;
        }
        for &i in &removed {
            // Quoted as written: a `$name` parameter stays `$name`.
            let text = ctx.written(spans[i].clone());
            let fix = if safe[i] {
                list_item_removal(&spans, &removed, i)
                    .map(|range| {
                        vec![SqlEdit {
                            range,
                            text: String::new(),
                        }]
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            out.push(RawFinding {
                span: spans[i].clone(),
                message: format!(
                    "the ORDER BY key {text} can't change the order: {}",
                    reasons[i].as_deref().unwrap_or_default()
                ),
                help: Some("remove it".to_owned()),
                fix,
            });
        }
    }
    out
}

/// A finding for the whole `ORDER BY` whose items span `spans`, with a fix removing it
/// when every key is safe to remove.
fn whole_clause(
    ctx: &Ctx<'_>,
    spans: &[std::ops::Range<usize>],
    safe: &[bool],
    why: &str,
) -> Option<RawFinding> {
    let first = spans.first()?.start;
    let last = spans.last()?.end;
    let order_index = ctx.tokens.iter().rposition(|token| {
        token.end <= first && ctx.sql[token.start..token.end].eq_ignore_ascii_case("ORDER")
    })?;
    let order = ctx.tokens[order_index];
    // From the end of the token before `ORDER`, so no blank is left behind.
    let removal_start = order_index
        .checked_sub(1)
        .map_or(order.start, |before| ctx.tokens[before].end);
    let fix = if safe.iter().all(|&safe| safe) {
        vec![SqlEdit {
            range: removal_start..last,
            text: String::new(),
        }]
    } else {
        Vec::new()
    };
    Some(RawFinding {
        span: order.start..last,
        message: format!("ORDER BY has no effect: {why}"),
        help: Some("remove the ORDER BY".to_owned()),
        fix,
    })
}

/// The expression of each `ORDER BY` item, or `None` when one isn't a
/// `SortBy`.
fn sort_nodes(select: &SelectStmt) -> Option<Vec<Option<&Node>>> {
    select
        .sort_clause
        .iter()
        .map(|item| match item.as_ref()? {
            Node::SortBy(sort) => Some(sort.node.as_ref()),
            _ => None,
        })
        .collect()
}

/// Whether removing a sort key can't change what the query evaluates: a
/// plain column (or output column name) or an ordinal, which only refer to
/// values computed anyway.
fn removable_key(node: Option<&Node>) -> bool {
    match node {
        Some(Node::ColumnRef(column)) => column
            .fields
            .iter()
            .all(|field| matches!(field, Some(Node::String(_)))),
        Some(Node::AConst(value)) => matches!(value.val, Some(AConstValue::Integer(_))),
        _ => false,
    }
}

/// How many expressions a `DISTINCT ON` has (0 for none or a plain
/// `DISTINCT`).
fn distinct_on_len(select: &SelectStmt) -> usize {
    match select.distinct_clause.as_slice() {
        [None] => 0,
        list => list.len(),
    }
}

/// The selects of the `EXISTS`, `IN` / `= ANY` and `ALL` subqueries, whose
/// rows' order nothing observes.
fn unobserved_selects(ctx: &Ctx<'_>) -> HashSet<*const SelectStmt> {
    let mut out = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SubLink(sub) = node else {
            continue;
        };
        if !matches!(
            sub.sub_link_type,
            SubLinkType::ExistsSublink | SubLinkType::AnySublink | SubLinkType::AllSublink
        ) {
            continue;
        }
        if let Some(Node::SelectStmt(select)) = sub.subselect.as_ref() {
            let pointer: *const SelectStmt = &**select;
            out.insert(pointer);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{fixed, lint_rule};

    const RULE: Rule = Rule::RedundantOrderBy;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    tenant int NOT NULL,
                    email text NOT NULL,
                    status text NOT NULL,
                    nick text UNIQUE,
                    name text,
                    created_at timestamptz NOT NULL,
                    UNIQUE (tenant, email)
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    created_at timestamptz NOT NULL
                 );",
            )
            .unwrap();
        catalog
    }

    #[track_caller]
    fn check(db: &PgCatalog, sql: &str, expected: &str) {
        let findings = lint_rule(db, sql, RULE);
        assert!(!findings.is_empty(), "{sql}: no finding");
        assert_eq!(fixed(sql, &findings), expected, "{sql}: {findings:?}");
    }

    #[test]
    fn keys_after_a_unique_prefix_are_removed() {
        let db = catalog();
        check(
            &db,
            "SELECT id FROM users ORDER BY id, created_at",
            "SELECT id FROM users ORDER BY id",
        );
        check(
            &db,
            "SELECT id FROM users ORDER BY created_at, id, name DESC NULLS LAST, 1 LIMIT 10",
            "SELECT id FROM users ORDER BY created_at, id LIMIT 10",
        );
        check(
            &db,
            "SELECT id FROM users ORDER BY tenant, email, id",
            "SELECT id FROM users ORDER BY tenant, email",
        );
        check(
            &db,
            "SELECT p.id FROM posts p JOIN users u ON u.id = p.user_id ORDER BY p.id, u.name",
            "SELECT p.id FROM posts p JOIN users u ON u.id = p.user_id ORDER BY p.id",
        );
        // An alias of the key.
        check(
            &db,
            "SELECT id AS k, name FROM users ORDER BY k, name",
            "SELECT id AS k, name FROM users ORDER BY k",
        );
    }

    #[test]
    fn pinned_and_repeated_keys_are_removed() {
        let db = catalog();
        check(
            &db,
            "SELECT id FROM users WHERE status = 'a' ORDER BY status, created_at",
            "SELECT id FROM users WHERE status = 'a' ORDER BY created_at",
        );
        check(
            &db,
            "SELECT id FROM users WHERE tenant = $1 ORDER BY created_at, tenant DESC",
            "SELECT id FROM users WHERE tenant = $1 ORDER BY created_at",
        );
        check(
            &db,
            "SELECT id FROM users ORDER BY created_at, name, created_at DESC",
            "SELECT id FROM users ORDER BY created_at, name",
        );
        // Every key pinned: the whole clause goes.
        check(
            &db,
            "SELECT id FROM users WHERE status = 'a' ORDER BY status",
            "SELECT id FROM users WHERE status = 'a'",
        );
    }

    #[test]
    fn order_nothing_observes_is_removed() {
        let db = catalog();
        check(
            &db,
            "SELECT id FROM users WHERE id = $1 ORDER BY created_at",
            "SELECT id FROM users WHERE id = $1",
        );
        check(
            &db,
            "SELECT count(*) FROM users ORDER BY 1",
            "SELECT count(*) FROM users",
        );
        check(
            &db,
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY created_at)",
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts)",
        );
        check(
            &db,
            "SELECT id FROM users u WHERE EXISTS \
             (SELECT 1 FROM posts p WHERE p.user_id = u.id ORDER BY p.created_at DESC)",
            "SELECT id FROM users u WHERE EXISTS \
             (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
        );
    }

    #[test]
    fn a_key_that_may_raise_is_reported_without_a_fix() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY id, 1 / (tenant - 1)",
            "SELECT id FROM users WHERE id = $1 ORDER BY random()",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].fix.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn keys_that_can_change_the_order_are_kept() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY created_at, id",
            "SELECT id FROM users ORDER BY tenant, email",
            "SELECT id FROM users ORDER BY tenant, id",
            // A nullable unique column: NULLs tie.
            "SELECT id FROM users ORDER BY nick, id",
            // The users' key doesn't determine their posts.
            "SELECT p.id FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.id, p.id",
            // Null-extended rows of an outer join tie on its key.
            "SELECT p.id FROM posts p LEFT JOIN users u ON u.id = p.user_id \
             ORDER BY u.id, p.id",
            // An ON condition pins nothing on the null-extended rows.
            "SELECT p.id FROM posts p LEFT JOIN users u ON u.id = p.user_id AND u.status = 'a' \
             ORDER BY u.status, p.id",
            "SELECT id FROM users WHERE lower(status) = 'a' ORDER BY status, id",
            // Not an equality.
            "SELECT id FROM users WHERE tenant > 3 ORDER BY tenant, id",
            "SELECT id FROM users WHERE tenant = 3 OR tenant = 4 ORDER BY tenant, id",
            // A COLLATE key is another order.
            "SELECT id FROM users ORDER BY name, name COLLATE \"C\"",
            // The DISTINCT ON keys must lead the ORDER BY.
            "SELECT DISTINCT ON (id, created_at) id FROM users ORDER BY id, created_at",
            // Every key pinned under LIMIT: unstable_order's finding.
            "SELECT id FROM users WHERE status = 'a' ORDER BY status LIMIT 10",
            "SELECT id FROM users WHERE status = 'a' ORDER BY status FETCH FIRST ROW ONLY",
            // The order a FROM subquery, CTE, ARRAY or LIMIT observes.
            "SELECT * FROM (SELECT id FROM users ORDER BY created_at) s",
            "WITH s AS (SELECT id FROM users ORDER BY created_at) SELECT * FROM s",
            "SELECT ARRAY(SELECT id FROM users ORDER BY created_at)",
            "SELECT id FROM users WHERE id IN \
             (SELECT user_id FROM posts ORDER BY created_at LIMIT 10)",
            "SELECT id FROM users WHERE id IN \
             (SELECT DISTINCT ON (user_id) id FROM posts ORDER BY user_id, created_at)",
            "SELECT id FROM users WHERE EXISTS \
             (SELECT 1 FROM posts ORDER BY id FOR UPDATE)",
            // A set operation's order.
            "SELECT id FROM users UNION SELECT id FROM posts ORDER BY 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn the_keys_picking_distinct_ons_row_are_kept() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE imgs (
                id int PRIMARY KEY,
                project_id int NOT NULL,
                arch text NOT NULL,
                version int NOT NULL
             );",
        )
        .unwrap();
        // One group (WHERE fixes the DISTINCT ON key), but the keys after it
        // pick which of the group's rows is returned.
        for sql in [
            "SELECT DISTINCT ON (project_id) id FROM imgs WHERE project_id = $p              ORDER BY project_id, version DESC, id",
            "SELECT DISTINCT ON (project_id) id FROM imgs WHERE project_id = $p              ORDER BY project_id, version DESC, id LIMIT 1",
            "SELECT DISTINCT ON (project_id, arch) id FROM imgs              WHERE project_id = $p AND arch = $a ORDER BY project_id, arch, version DESC",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // A pinned key after the DISTINCT ON expressions sorts every row the
        // same; the key that picks the row stays.
        check(
            &db,
            "SELECT DISTINCT ON (project_id) id FROM imgs              WHERE project_id = $p AND arch = 'x' ORDER BY project_id, arch, version DESC",
            "SELECT DISTINCT ON (project_id) id FROM imgs              WHERE project_id = $p AND arch = 'x' ORDER BY project_id, version DESC",
        );
        // At most one row before DISTINCT ON: nothing to pick from.
        check(
            &db,
            "SELECT DISTINCT ON (project_id) id FROM imgs WHERE id = $id              ORDER BY project_id, version DESC",
            "SELECT DISTINCT ON (project_id) id FROM imgs WHERE id = $id",
        );
    }

    #[test]
    fn messages_quote_named_parameters_as_written() {
        let db = catalog();
        let findings = lint_rule(
            &db,
            "SELECT id FROM users WHERE name = $n ORDER BY id, name || $n",
            RULE,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].message,
            "the ORDER BY key name || $n can't change the order: the keys before it already \
             determine a unique row, so it never breaks a tie"
        );
    }
}
