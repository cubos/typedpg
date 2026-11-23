//! The `redundant_group_by` rule: reports `GROUP BY` keys that can't split
//! a group — a plain column of a table whose other grouped columns cover a
//! unique NOT NULL key of it (the table's row is one per group, so its
//! other columns are too), or one `WHERE` equates to a constant or
//! parameter — and that PostgreSQL still accepts the query without (see
//! [`typedpg_analyzer::redundant_group_keys`]).
//!
//! PostgreSQL only lets the select list, `HAVING`, `ORDER BY` or a window
//! read an ungrouped column whose table's primary key is grouped, so a key
//! read there is only reported when the primary key stays grouped:
//! `GROUP BY u.id, u.name` with `SELECT u.name` becomes `GROUP BY u.id`,
//! but a column determined by another unique key, or pinned by `WHERE`, is
//! kept when the query reads it. One key always stays (no `GROUP BY` would
//! turn no rows into one group), and `GROUPING SETS`, `ROLLUP`, `CUBE` and
//! `GROUP BY DISTINCT` are never touched. The fix removes the keys.

use typedpg_analyzer::redundant_group_keys;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::Node;

use super::{clause_item_spans, list_item_removal};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        let removed = redundant_group_keys(ctx.catalog, ctx.view, select);
        if removed.is_empty() {
            continue;
        }
        let items: Vec<Option<&Node>> = select.group_clause.iter().map(Option::as_ref).collect();
        let Some(spans) = clause_item_spans(ctx, &items) else {
            continue;
        };
        for &i in &removed {
            let Some(range) = list_item_removal(&spans, &removed, i) else {
                continue;
            };
            let text = ctx.sql[spans[i].clone()].trim();
            out.push(RawFinding {
                span: spans[i].clone(),
                message: format!(
                    "the GROUP BY key {text} can't split a group: the other keys determine it, or \
                     WHERE pins it to one value"
                ),
                help: Some("remove it".to_owned()),
                fix: vec![SqlEdit {
                    range,
                    text: String::new(),
                }],
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{fixed, lint_rule};

    const RULE: Rule = Rule::RedundantGroupBy;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    email text NOT NULL UNIQUE,
                    nick text UNIQUE,
                    status text NOT NULL,
                    name text
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    title text NOT NULL
                 );
                 CREATE TABLE tags (name text NOT NULL, post_id bigint NOT NULL);",
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
    fn keys_the_primary_key_determines_are_removed() {
        let db = catalog();
        check(
            &db,
            "SELECT u.id, u.name, count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             GROUP BY u.id, u.name",
            "SELECT u.id, u.name, count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             GROUP BY u.id",
        );
        check(
            &db,
            "SELECT u.name, u.email, count(*) FROM users u JOIN posts p ON p.user_id = u.id \
             GROUP BY u.name, u.email, u.id ORDER BY u.name",
            "SELECT u.name, u.email, count(*) FROM users u JOIN posts p ON p.user_id = u.id \
             GROUP BY u.name, u.email ORDER BY u.name",
        );
        // A `*` reads every column: the primary key is grouped.
        check(
            &db,
            "SELECT * FROM users GROUP BY id, name",
            "SELECT * FROM users GROUP BY id",
        );
        // Not read after grouping: another unique key determines it too.
        check(
            &db,
            "SELECT email, count(*) FROM users GROUP BY email, name",
            "SELECT email, count(*) FROM users GROUP BY email",
        );
        // Keys in parentheses: removed with them.
        check(
            &db,
            "SELECT id, count(*) FROM users GROUP BY (id), name",
            "SELECT id, count(*) FROM users GROUP BY (id)",
        );
    }

    #[test]
    fn pinned_keys_are_removed() {
        let db = catalog();
        check(
            &db,
            "SELECT user_id, count(*) FROM posts WHERE title = $1 GROUP BY title, user_id",
            "SELECT user_id, count(*) FROM posts WHERE title = $1 GROUP BY user_id",
        );
    }

    #[test]
    fn keys_that_split_groups_or_are_needed_are_kept() {
        let db = catalog();
        for sql in [
            "SELECT status, name, count(*) FROM users GROUP BY status, name",
            // A nullable unique column: NULLs share one group.
            "SELECT nick, name, count(*) FROM users GROUP BY nick, name",
            // Read after grouping, and only another unique key determines it:
            // PostgreSQL wants it grouped.
            "SELECT email, name FROM users GROUP BY email, name",
            "SELECT name, count(*) FROM users GROUP BY email, name",
            "SELECT email FROM users GROUP BY email, name HAVING max(name) > ''",
            // Pinned but read.
            "SELECT title, user_id, count(*) FROM posts WHERE title = $1 GROUP BY title, user_id",
            // The last key stays: without GROUP BY no rows are one group.
            "SELECT count(*) FROM posts WHERE title = $1 GROUP BY title",
            // Another table's key.
            "SELECT p.user_id, u.name FROM posts p JOIN users u ON u.id = p.user_id \
             GROUP BY p.user_id, u.name",
            "SELECT name, post_id FROM tags GROUP BY name, post_id",
            "SELECT id, name FROM users GROUP BY ROLLUP (id, name)",
            "SELECT id, name FROM users GROUP BY GROUPING SETS ((id, name), ())",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
