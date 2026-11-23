//! The `left_join_nullified` rule: reports a `WHERE` condition (an `AND`
//! argument of one) that is never true for a row whose entry on the
//! nullable side of an outer join is null-extended — `LEFT JOIN b … WHERE
//! b.status = 'x'` — so the join keeps none of the rows it was written to
//! keep, and behaves as an inner join (a `FULL` one as a `LEFT` or
//! `RIGHT` join).
//!
//! What a condition rejects is the analyzer's strictness reasoning — PG's
//! `reduce_outer_joins` (see
//! [`typedpg_analyzer::QueryView::nullified_joins`]): a strict operator or
//! function of the entry's column, `IS NOT NULL`, an `OR` only when every
//! arm rejects. `b.id IS NULL` (the anti-join) and `COALESCE(b.x, 0) = 0`
//! keep the null-extended rows and are not reported, nor is a join that
//! never null-extends that side anyway (a foreign key's parent row, `ON
//! true` to a row always there).
//!
//! No fix is offered: moving the condition into `ON` keeps the unmatched
//! rows (with NULLs), writing `INNER JOIN` makes the columns non-nullable
//! — either changes the query, and which one was meant is for its author.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{BoolExprType, Node, Node as N, NullTestType};

use super::condition_span;
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        let Some(qual) = select.where_clause.as_ref() else {
            continue;
        };
        let (conjuncts, whole): (Vec<&Node>, bool) = match qual {
            N::BoolExpr(and) if and.boolop == BoolExprType::AndExpr => {
                (and.args.iter().flatten().collect(), false)
            }
            n => (vec![n], true),
        };
        for conjunct in conjuncts {
            let Some(location) = conjunct.to_ref().location() else {
                continue;
            };
            let Some(joins) = ctx.view.nullified_joins(location) else {
                continue;
            };
            let Some(join) = joins.first() else {
                continue;
            };
            let Some(span) = condition_span(ctx, conjunct, whole) else {
                continue;
            };
            let entry = typedpg_core::quote_identifier(&join.entry);
            let article = |kind: &str| if kind == "INNER" { "an" } else { "a" };
            // The join as written, and what it already behaves as when a
            // foreign key (or a join above) keeps one side of it from
            // being NULL-extended: a `FULL JOIN` whose other side always
            // finds a match is a `LEFT` or `RIGHT` one.
            let side = if join.before == join.written {
                format!("{} JOIN", join.written)
            } else {
                format!(
                    "{} JOIN, which already behaves as {} {} JOIN",
                    join.written,
                    article(join.before),
                    join.before
                )
            };
            let message = format!(
                "this condition is never true for the NULL-extended rows of {entry}, on the \
                 nullable side of a {side}: the join behaves as {} {} JOIN",
                article(join.reduced),
                join.reduced,
            );
            let help = if is_not_null_test(conjunct) {
                format!(
                    "to keep only the rows with a match in {entry}, write an INNER JOIN; to keep \
                     the unmatched rows, drop the condition"
                )
            } else {
                format!(
                    "to keep the rows without a match in {entry}, move the condition into the \
                     join's ON clause; to drop them, write an INNER JOIN"
                )
            };
            out.push(RawFinding {
                span,
                message,
                help: Some(help),
                fix: Vec::new(),
            });
        }
    }
    out
}

/// Whether `node` is an `IS NOT NULL` test.
fn is_not_null_test(node: &Node) -> bool {
    matches!(node, N::NullTest(t) if t.nulltesttype == NullTestType::IsNotNull)
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::LeftJoinNullified;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (id bigint PRIMARY KEY, name text, active boolean);
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint REFERENCES users,
                    owner_id bigint NOT NULL REFERENCES users,
                    status text NOT NULL,
                    score int
                 );
                 CREATE TABLE tags (id bigint PRIMARY KEY, post_id bigint, label text);",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn a_where_rejecting_the_nullable_side_is_reported() {
        let db = catalog();
        for (sql, reported, written, reduced) in [
            (
                "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                 WHERE p.status = 'x'",
                "p.status = 'x'",
                "LEFT",
                "an INNER",
            ),
            (
                "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                 WHERE u.active AND p.score > 0",
                "p.score > 0",
                "LEFT",
                "an INNER",
            ),
            (
                "SELECT u.id FROM posts p RIGHT JOIN users u ON p.user_id = u.id \
                 WHERE lower(p.status) = 'x'",
                "lower(p.status) = 'x'",
                "RIGHT",
                "an INNER",
            ),
            (
                "SELECT u.id FROM users u FULL JOIN posts p ON p.user_id = u.id \
                 WHERE p.score = 1",
                "p.score = 1",
                "FULL",
                "a RIGHT",
            ),
            (
                "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                 WHERE (p.score = 1 OR p.score = 2)",
                "p.score = 1 OR p.score = 2",
                "LEFT",
                "an INNER",
            ),
            (
                "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                 LEFT JOIN tags t ON t.post_id = p.id WHERE t.label = 'a'",
                "t.label = 'a'",
                "LEFT",
                "an INNER",
            ),
            // The `FROM` of `IS NOT DISTINCT FROM` is the condition's.
            (
                "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                 WHERE p.status IS NOT DISTINCT FROM 'x' ORDER BY u.id",
                "p.status IS NOT DISTINCT FROM 'x'",
                "LEFT",
                "an INNER",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert!(
                findings[0].message.contains(&format!("{written} JOIN"))
                    && findings[0]
                        .message
                        .contains(&format!("behaves as {reduced} JOIN")),
                "{sql}: {}",
                findings[0].message
            );
            assert!(findings[0].help.as_ref().unwrap().contains("ON clause"));
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn a_full_join_a_foreign_key_already_reduces_is_named_as_written() {
        let db = catalog();
        // `owner_id` is a NOT NULL foreign key: every post finds its user,
        // so `u` is never NULL-extended and the FULL JOIN is a LEFT one
        // before the WHERE makes it an inner join.
        let sql = "SELECT u.id FROM users u FULL JOIN posts p ON p.owner_id = u.id \
                   WHERE p.score > 0";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "p.score > 0");
        assert_eq!(
            findings[0].message,
            "this condition is never true for the NULL-extended rows of p, on the nullable \
             side of a FULL JOIN, which already behaves as a LEFT JOIN: the join behaves as an \
             INNER JOIN"
        );
        // The same from the other side: a RIGHT JOIN already.
        let sql = "SELECT u.id FROM posts p FULL JOIN users u ON p.owner_id = u.id \
                   WHERE p.score > 0";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].message.contains(
                "nullable side of a FULL JOIN, which already behaves as a RIGHT JOIN: the join \
                 behaves as an INNER JOIN"
            ),
            "{}",
            findings[0].message
        );
    }

    #[test]
    fn is_not_null_on_the_nullable_side_says_inner_join() {
        let db = catalog();
        let sql = "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                   WHERE p.id IS NOT NULL";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "p.id IS NOT NULL");
        assert!(
            findings[0]
                .help
                .as_ref()
                .unwrap()
                .starts_with("to keep only the rows with a match")
        );
    }

    #[test]
    fn conditions_keeping_the_null_extended_rows_are_not_reported() {
        let db = catalog();
        for sql in [
            // The anti-join.
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE p.id IS NULL",
            // Not strict.
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             WHERE coalesce(p.score, 0) = 0",
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             WHERE p.score IS DISTINCT FROM 1",
            // An OR with an arm keeping them.
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             WHERE p.score = 1 OR p.id IS NULL",
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             WHERE p.score = 1 OR u.active",
            // On the preserved side.
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id WHERE u.name = 'a'",
            // In ON, where it belongs.
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id AND p.score > 0",
            // Inner joins.
            "SELECT u.id FROM users u JOIN posts p ON p.user_id = u.id WHERE p.score > 0",
            // The parent side of a foreign key never null-extends: nothing
            // is lost.
            "SELECT p.id FROM posts p LEFT JOIN users u ON u.id = p.owner_id \
             WHERE u.name = 'a'",
            // A parameter that may be NULL.
            "SELECT u.id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
             WHERE ($1::int IS NULL OR p.score = $1)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
