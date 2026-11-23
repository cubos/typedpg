//! The `unstable_order` rule: reports an `ORDER BY` whose keys two rows
//! can share, under a `LIMIT`, `OFFSET` / `FETCH` or `DISTINCT ON` that
//! picks rows by that order. PG returns tied rows in no particular order,
//! and not the same one each time: a page cut through a run of ties may
//! repeat or skip rows on the next page, `LIMIT 1` may return either row,
//! and `DISTINCT ON` may keep either row of a group.
//!
//! The keys are unique when, with the columns `WHERE` equates to
//! constants, parameters or columns of an enclosing query level (one value
//! per evaluation), they cover a unique key (NOT NULL, or `NULLS NOT
//! DISTINCT`; `col COLLATE c` counts as `col` under a deterministic `c`)
//! of every table — or of the tables the others determine
//! through equalities to a key (a join on a primary key) — or, for a
//! grouped query, every grouping column (see
//! [`typedpg_analyzer::order_ties`]). A query returning at most one row is
//! not reported (with `DISTINCT ON`, one with at most one row before it: a
//! single group still picks among its rows), nor `FETCH … WITH TIES` (it returns every tie), a
//! `SKIP LOCKED` queue, an `EXISTS` subquery or a batching `IN` in an
//! `UPDATE` / `DELETE` (which don't care which rows), nor a select whose
//! output is the same for every row. Nothing is reported when the proof
//! can't see the whole FROM clause (a subquery, a CTE, a function, a
//! partitioned or inheritance table, a partial or expression unique index):
//! the keys may be unique there.
//!
//! No `ORDER BY` at all is `limit_without_order`'s finding. No fix is
//! offered: which tiebreaker to add is the author's choice.

use typedpg_analyzer::{OrderTies, order_ties, select_at_most_one_row};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, LimitOption, Node, SelectStmt};

use super::limit_without_order::{
    batched_subqueries, constant_target, distinct_on_span, has_skip_locked, invariant_target,
    limit_is_zero, limit_span, offset_span, picks_from_at_most_one_row,
};
use super::{leftmost_start, scan_until, starts_clause};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let batched = batched_subqueries(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.sort_clause.is_empty() || select.limit_option == LimitOption::WithTies {
            continue;
        }
        let picks = if distinct_on_span(ctx, select).is_some() {
            "DISTINCT ON"
        } else if let Some(span) = limit_span(ctx, select) {
            if ctx.sql[span].to_ascii_uppercase().starts_with("FETCH") {
                "FETCH"
            } else {
                "LIMIT"
            }
        } else if offset_span(ctx, select).is_some() {
            "OFFSET"
        } else {
            continue;
        };
        let pointer: *const SelectStmt = select;
        if batched.contains(&pointer)
            || has_skip_locked(select)
            || limit_is_zero(select)
            || constant_target(ctx, select)
            || invariant_target(ctx, select)
            || if picks == "DISTINCT ON" {
                picks_from_at_most_one_row(ctx, select)
            } else {
                select_at_most_one_row(ctx.catalog, ctx.view, select)
            }
        {
            continue;
        }
        let OrderTies::Ties(undetermined) = order_ties(ctx.catalog, ctx.view, select) else {
            continue;
        };
        let Some(span) = order_by_span(ctx, select) else {
            continue;
        };
        let names: Vec<String> = undetermined
            .iter()
            .map(|(name, _)| typedpg_core::quote_identifier(name))
            .collect();
        let what = if picks == "DISTINCT ON" {
            "DISTINCT ON keeps an arbitrary row of the tied ones in each group"
        } else if limit_is_one(select) && offset_span(ctx, select).is_none() {
            "which of the tied rows comes back is arbitrary"
        } else {
            "rows that tie come back in an arbitrary order, so pages may skip or repeat rows"
        };
        let message = format!(
            "the ORDER BY keys can tie: they don't determine a unique row of {} — under {picks}, \
             {what}",
            names.join(", ")
        );
        let tiebreakers: Vec<String> = undetermined
            .iter()
            .filter(|(_, pk)| !pk.is_empty())
            .map(|(name, pk)| {
                let table = typedpg_core::quote_identifier(name);
                pk.iter()
                    .map(|c| format!("{table}.{}", typedpg_core::quote_identifier(c)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect();
        let help = if tiebreakers.is_empty() {
            "add a unique column as the last ORDER BY key".to_owned()
        } else {
            format!(
                "add a unique tiebreaker as the last ORDER BY key, such as the primary key: {}",
                tiebreakers.join(", ")
            )
        };
        out.push(RawFinding {
            span,
            message,
            help: Some(help),
            fix: Vec::new(),
        });
    }
    out
}

/// Whether the row count is the literal 1 (`LIMIT 1`, `FETCH FIRST 1 ROW
/// ONLY`).
fn limit_is_one(select: &SelectStmt) -> bool {
    matches!(
        select.limit_count.as_ref(),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(AConstValue::Integer(ref count)) if count.ival == 1)
    )
}

/// The `ORDER BY …` clause of `select`.
fn order_by_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let first = match select.sort_clause.first()?.as_ref()? {
        Node::SortBy(sort) => sort.node.as_ref().and_then(leftmost_start)?,
        _ => return None,
    };
    let order = ctx
        .tokens
        .iter()
        .filter(|token| token.end <= first)
        .rfind(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case("ORDER"))?;
    let (end, _) = scan_until(ctx, first, starts_clause);
    Some(order.start..end.max(first + 1))
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::UnstableOrder;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    tenant int NOT NULL,
                    email text NOT NULL,
                    nick text UNIQUE,
                    name text,
                    created_at timestamptz NOT NULL,
                    UNIQUE (tenant, email)
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    created_at timestamptz NOT NULL
                 );
                 CREATE TABLE logs (at timestamptz NOT NULL, msg text);
                 CREATE TABLE slugs (id int PRIMARY KEY, slug text NOT NULL);
                 CREATE UNIQUE INDEX ON slugs (lower(slug));
                 CREATE TABLE src (id int PRIMARY KEY);
                 CREATE TABLE ver (
                    source_id int NOT NULL REFERENCES src,
                    version int NOT NULL,
                    digest text,
                    PRIMARY KEY (source_id, version)
                 );
                 CREATE TABLE mig (name text PRIMARY KEY);
                 CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', \
                    deterministic = false);",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn keys_that_can_tie_are_reported() {
        let db = catalog();
        for (sql, help) in [
            (
                "SELECT id FROM users ORDER BY created_at LIMIT 10",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY name OFFSET 20",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY created_at FETCH FIRST 5 ROWS ONLY",
                Some("users.id"),
            ),
            // A nullable UNIQUE column: NULLs tie.
            (
                "SELECT id FROM users ORDER BY nick LIMIT 10",
                Some("users.id"),
            ),
            // Part of a key.
            (
                "SELECT id FROM users ORDER BY tenant LIMIT 10",
                Some("users.id"),
            ),
            // The users' key doesn't determine their posts.
            (
                "SELECT p.id FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.id LIMIT 10",
                Some("p.id"),
            ),
            (
                "SELECT DISTINCT ON (user_id) id FROM posts ORDER BY user_id, created_at",
                Some("posts.id"),
            ),
            // No primary key to suggest.
            ("SELECT msg FROM logs ORDER BY at LIMIT 10", None),
            // A group per (tenant, name): ordered by tenant alone.
            (
                "SELECT tenant, name, count(*) FROM users GROUP BY tenant, name \
                 ORDER BY tenant LIMIT 10",
                Some("users.id"),
            ),
            (
                "SELECT id FROM users ORDER BY 1 + 1, created_at LIMIT 1",
                Some("users.id"),
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                sql[findings[0].span.clone()].starts_with("ORDER BY"),
                "{sql}: {:?}",
                &sql[findings[0].span.clone()]
            );
            let h = findings[0].help.as_deref().unwrap();
            match help {
                Some(pk) => assert!(h.contains(pk), "{sql}: {h}"),
                None => assert_eq!(h, "add a unique column as the last ORDER BY key"),
            }
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn distinct_on_picking_from_one_group_is_reported() {
        let db = catalog();
        // WHERE fixes the DISTINCT ON key: one row comes back, but which of
        // the group's rows is arbitrary.
        let sql = "SELECT DISTINCT ON (tenant) id FROM users WHERE tenant = $1 ORDER BY tenant";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0]
                .message
                .contains("DISTINCT ON keeps an arbitrary row"),
            "{findings:?}"
        );
        // One row before DISTINCT ON: nothing to pick from.
        let sql = "SELECT DISTINCT ON (tenant) id FROM users WHERE id = $1 ORDER BY tenant";
        assert!(lint_rule(&db, sql, RULE).is_empty());
    }

    #[test]
    fn a_single_row_limit_says_which_row_is_arbitrary() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY created_at DESC LIMIT 1",
            "SELECT id FROM users ORDER BY created_at DESC FETCH FIRST 1 ROW ONLY",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                findings[0]
                    .message
                    .ends_with("which of the tied rows comes back is arbitrary"),
                "{sql}: {findings:?}"
            );
        }
        // A page past the first still pages.
        let findings = lint_rule(
            &db,
            "SELECT id FROM users ORDER BY created_at LIMIT 1 OFFSET $1",
            RULE,
        );
        assert!(
            findings[0]
                .message
                .ends_with("pages may skip or repeat rows")
        );
    }

    #[test]
    fn outer_columns_fix_a_key_like_parameters() {
        let db = catalog();
        for sql in [
            "SELECT s.id, (SELECT v.digest FROM ver v WHERE v.source_id = s.id              ORDER BY v.version DESC LIMIT 1) FROM src s",
            "SELECT s.id, l.digest FROM src s CROSS JOIN LATERAL              (SELECT v.digest FROM ver v WHERE v.source_id = s.id              ORDER BY v.version DESC LIMIT 1) l",
            "SELECT s.id, l.digest FROM src s LEFT JOIN LATERAL              (SELECT v.digest FROM ver v WHERE s.id = v.source_id              ORDER BY v.version DESC LIMIT 1) l ON true",
            "SELECT (SELECT v.digest FROM ver v WHERE v.source_id = $1              ORDER BY v.version DESC LIMIT 1)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // Not an equality: the outer column fixes nothing.
        let sql = "SELECT s.id, (SELECT v.digest FROM ver v WHERE v.source_id > s.id \
                   ORDER BY v.version DESC LIMIT 1) FROM src s";
        assert_eq!(lint_rule(&db, sql, RULE).len(), 1, "{sql}");
    }

    #[test]
    fn a_deterministic_collate_keeps_a_unique_key_unique() {
        let db = catalog();
        for sql in [
            "SELECT name FROM mig ORDER BY name COLLATE \"C\" LIMIT 10",
            "SELECT name FROM mig ORDER BY name COLLATE \"C\" DESC LIMIT 10",
            "SELECT name FROM mig ORDER BY name COLLATE pg_catalog.\"POSIX\" LIMIT 10",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // A nondeterministic collation compares distinct names equal.
        let sql = "SELECT name FROM mig ORDER BY name COLLATE ci LIMIT 10";
        assert_eq!(lint_rule(&db, sql, RULE).len(), 1, "{sql}");
    }

    #[test]
    fn unique_keys_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY created_at, id LIMIT 10",
            "SELECT id FROM users ORDER BY id DESC LIMIT 10",
            "SELECT id AS k FROM users ORDER BY k LIMIT 10",
            "SELECT id FROM users ORDER BY 1 LIMIT 10",
            "SELECT id FROM users ORDER BY tenant, email LIMIT 10",
            // A nullable unique column a condition may prove non-NULL.
            "SELECT id FROM users WHERE nick IS NOT NULL ORDER BY nick LIMIT 10",
            "SELECT id FROM users WHERE nick > 'm' ORDER BY nick LIMIT 10",
            // The rest of the key is pinned by WHERE.
            "SELECT id FROM users WHERE tenant = $1 ORDER BY email LIMIT 10",
            "SELECT id FROM users WHERE tenant = 3 ORDER BY email LIMIT 10",
            // A join on the key: the post determines its user.
            "SELECT p.id FROM posts p JOIN users u ON u.id = p.user_id \
             ORDER BY p.created_at, p.id LIMIT 10",
            // Grouped: one row per group key.
            "SELECT tenant, count(*) FROM users GROUP BY tenant ORDER BY tenant LIMIT 10",
            "SELECT tenant, name FROM users GROUP BY tenant, name ORDER BY name, tenant LIMIT 10",
            "SELECT DISTINCT tenant FROM users ORDER BY tenant LIMIT 10",
            "SELECT DISTINCT ON (user_id) id FROM posts ORDER BY user_id, id",
            // At most one row.
            "SELECT id FROM users WHERE id = $1 ORDER BY created_at LIMIT 1",
            "SELECT count(*) FROM users ORDER BY 1 LIMIT 1",
            // Ties are all returned.
            "SELECT id FROM users ORDER BY created_at FETCH FIRST 5 ROWS WITH TIES",
            // Without LIMIT / OFFSET the order of ties picks nothing.
            "SELECT id FROM users ORDER BY created_at",
            // A queue.
            "SELECT id FROM users ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED",
            // Existence only.
            "SELECT 1 FROM posts WHERE EXISTS \
             (SELECT 1 FROM users ORDER BY created_at LIMIT 1)",
            // What the proof can't see: an expression unique index, a
            // subquery, a CTE.
            "SELECT id FROM slugs ORDER BY lower(slug) LIMIT 10",
            "SELECT id FROM (SELECT id, created_at FROM users) s ORDER BY created_at LIMIT 10",
            "WITH s AS (SELECT id, created_at FROM users) \
             SELECT id FROM s ORDER BY created_at LIMIT 10",
            // The same output for every row.
            "SELECT 1 FROM users ORDER BY created_at LIMIT 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
