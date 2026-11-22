//! The `redundant_join` rule: reports a join that can't change the rows —
//! it neither drops nor repeats one — and whose table nothing else in the
//! statement reads, so the join is dead text (or the query meant to read
//! the table and doesn't). Two joins are proven so:
//!
//! - `LEFT JOIN t ON t.k1 = a.c1 AND …` whose conjuncts are all equalities
//!   of a column of `t` with a column of the same type of another table,
//!   under deterministic collations, equating every column of a full
//!   unique key of `t` (non-partial, immediate, on plain columns) through
//!   its index's own btree equality: each row matches at most one row of
//!   `t`, and a LEFT JOIN keeps the rows that match none;
//! - `[INNER] JOIN parent ON parent.pk = child.fk` over exactly a foreign
//!   key whose columns are NOT NULL, enforced, validated and NOT
//!   DEFERRABLE, into a parent without row security: each child row
//!   matches exactly one parent row.
//!
//! The proofs are the analyzer's (see
//! [`typedpg_analyzer::QueryView::redundant_joins`]; the foreign key one is
//! the `fk_join` simplification's): only the FROM of a plain `SELECT`
//! (no `WITH`, set operation, locking clause, volatile function, view, or
//! `USING` / `NATURAL` join) is read, the joined relation must be a plain
//! or partitioned table on the join's right-hand side, and any reference
//! that may read it — qualified by its name, a whole-row reference, an
//! unqualified `*` or an unqualified name one of its columns has, anywhere
//! in the statement — keeps the join.
//!
//! The fix deletes the join, from its keyword through the end of its `ON`
//! condition. It is withheld when a comment is in the way.

use typedpg_analyzer::RedundantJoinProof;

use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    ctx.view
        .redundant_joins(ctx.catalog)
        .into_iter()
        .map(|join| {
            let name = typedpg_core::quote_identifier(&join.relation);
            let message = match join.proof {
                RedundantJoinProof::UniqueKey => format!(
                    "the LEFT JOIN of {name} can't change the rows (each row matches at most \
                     one row of it, on a unique key) and none of its columns are used"
                ),
                RedundantJoinProof::ForeignKey => format!(
                    "the JOIN of {name} can't change the rows (a foreign key finds exactly one \
                     row of it for each row) and none of its columns are used"
                ),
            };
            RawFinding {
                span: join.span,
                message,
                help: Some(
                    "remove the join, or read the columns it was meant to provide".to_owned(),
                ),
                fix: join
                    .fix
                    .into_iter()
                    .map(|range| SqlEdit {
                        range,
                        text: String::new(),
                    })
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::rules::test_support::{fixed, lint_rule};
    use crate::{Rule, Severity};

    const RULE: Rule = Rule::RedundantJoin;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    name text,
                    email text NOT NULL UNIQUE
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    editor_id bigint REFERENCES users,
                    editor_email text,
                    author_code int,
                    title text NOT NULL
                 );
                 CREATE TABLE tags (id bigint, name text, live boolean);
                 CREATE UNIQUE INDEX tags_name ON tags (name) WHERE live;
                 CREATE TABLE pairs (a bigint, b bigint, label text, UNIQUE (a, b));
                 CREATE TABLE secret (id bigint PRIMARY KEY);
                 ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
                 CREATE TABLE notes (
                    id bigint PRIMARY KEY,
                    secret_id bigint NOT NULL REFERENCES secret,
                    deferred_id bigint NOT NULL REFERENCES users DEFERRABLE,
                    unchecked_id bigint NOT NULL
                 );
                 ALTER TABLE notes ADD CONSTRAINT notes_unchecked
                    FOREIGN KEY (unchecked_id) REFERENCES users NOT VALID;",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn a_left_join_on_a_unique_key_is_reported_and_removed() {
        let db = catalog();
        for (sql, span, fixed_sql) in [
            (
                "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id",
                "LEFT JOIN users u ON u.id = p.editor_id",
                "SELECT p.title FROM posts p",
            ),
            (
                "SELECT p.title FROM posts p LEFT OUTER JOIN users u ON p.editor_email = u.email \
                 WHERE p.id = 1",
                "LEFT OUTER JOIN users u ON p.editor_email = u.email",
                "SELECT p.title FROM posts p WHERE p.id = 1",
            ),
            (
                // Every column of a composite key.
                "SELECT p.title FROM posts p LEFT JOIN pairs x ON x.a = p.id AND x.b = p.user_id",
                "LEFT JOIN pairs x ON x.a = p.id AND x.b = p.user_id",
                "SELECT p.title FROM posts p",
            ),
            (
                // Row security only hides rows: still at most one.
                "SELECT n.id FROM notes n LEFT JOIN secret s ON s.id = n.secret_id",
                "LEFT JOIN secret s ON s.id = n.secret_id",
                "SELECT n.id FROM notes n",
            ),
            (
                // In the middle of a join chain.
                "SELECT p.title, w.name FROM posts p LEFT JOIN users u ON u.id = p.editor_id \
                 JOIN users w ON w.id = p.user_id",
                "LEFT JOIN users u ON u.id = p.editor_id",
                "SELECT p.title, w.name FROM posts p JOIN users w ON w.id = p.user_id",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], span, "{sql}");
            assert_eq!(findings[0].severity, Severity::Info);
            assert_eq!(fixed(sql, &findings), fixed_sql, "{sql}");
        }
        let findings = lint_rule(
            &db,
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id",
            RULE,
        );
        assert_eq!(
            findings[0].message,
            "the LEFT JOIN of u can't change the rows (each row matches at most one row of it, \
             on a unique key) and none of its columns are used"
        );
        assert_eq!(
            findings[0].help.as_deref(),
            Some("remove the join, or read the columns it was meant to provide")
        );
    }

    #[test]
    fn an_inner_join_through_a_foreign_key_is_reported_and_removed() {
        let db = catalog();
        for (sql, span, fixed_sql) in [
            (
                "SELECT p.title FROM posts p JOIN users u ON u.id = p.user_id",
                "JOIN users u ON u.id = p.user_id",
                "SELECT p.title FROM posts p",
            ),
            (
                "SELECT p.title FROM posts p INNER JOIN users u ON p.user_id = u.id ORDER BY p.id",
                "INNER JOIN users u ON p.user_id = u.id",
                "SELECT p.title FROM posts p ORDER BY p.id",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], span, "{sql}");
            assert_eq!(fixed(sql, &findings), fixed_sql, "{sql}");
            assert_eq!(
                findings[0].message,
                "the JOIN of u can't change the rows (a foreign key finds exactly one row of it \
                 for each row) and none of its columns are used"
            );
        }
    }

    #[test]
    fn a_join_that_refines_a_column_is_kept() {
        // The parent's CHECK refines the referencing column through the
        // join: without it the generated type would widen.
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE kinds (id int PRIMARY KEY CHECK (id IN (1, 2)));
             CREATE TABLE items (id int PRIMARY KEY, kind_id int NOT NULL REFERENCES kinds);",
        )
        .unwrap();
        for sql in [
            "SELECT i.kind_id FROM items i JOIN kinds k ON k.id = i.kind_id",
            "SELECT i.kind_id FROM items i LEFT JOIN kinds k ON k.id = i.kind_id",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // Not reading the refined column, the join is redundant again.
        let sql = "SELECT i.id FROM items i JOIN kinds k ON k.id = i.kind_id";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(fixed(sql, &findings), "SELECT i.id FROM items i");
    }

    #[test]
    fn a_comment_in_the_join_withholds_the_fix() {
        let db = catalog();
        let sql = "SELECT p.title FROM posts p LEFT JOIN users u /* why */ ON u.id = p.editor_id";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty());
    }

    #[test]
    fn joins_that_may_change_the_rows_or_are_read_are_not_reported() {
        let db = catalog();
        for sql in [
            // A column of the joined table is used.
            "SELECT p.title, u.name FROM posts p LEFT JOIN users u ON u.id = p.editor_id",
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id \
             WHERE u.name IS NULL",
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id ORDER BY u.name",
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id ORDER BY name",
            "SELECT p.title FROM posts p JOIN users u ON u.id = p.user_id GROUP BY p.title, u.id",
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id \
             WHERE EXISTS (SELECT 1 FROM tags t WHERE t.name = u.name)",
            "SELECT u FROM posts p LEFT JOIN users u ON u.id = p.editor_id",
            "SELECT u.* FROM posts p LEFT JOIN users u ON u.id = p.editor_id",
            "SELECT w.name FROM posts p LEFT JOIN users u ON u.id = p.editor_id \
             LEFT JOIN users w ON w.email = u.email",
            // Every column, through `*`.
            "SELECT * FROM posts p LEFT JOIN users u ON u.id = p.editor_id",
            // A partial unique index.
            "SELECT p.title FROM posts p LEFT JOIN tags t ON t.name = p.title",
            // No unique key: the join may repeat rows.
            "SELECT p.title FROM posts p LEFT JOIN tags t ON t.id = p.id",
            // Only part of a composite key.
            "SELECT p.title FROM posts p LEFT JOIN pairs x ON x.a = p.id",
            // An ON condition that is not a key equality.
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id \
             AND u.name = 'x'",
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id OR u.id = p.id",
            // A key compared with another type.
            "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.author_code",
            // A nullable foreign key: an inner join drops its NULL rows.
            "SELECT p.title FROM posts p JOIN users u ON u.id = p.editor_id",
            // A deferrable foreign key.
            "SELECT n.id FROM notes n JOIN users u ON u.id = n.deferred_id",
            // A NOT VALID foreign key.
            "SELECT n.id FROM notes n JOIN users u ON u.id = n.unchecked_id",
            // Row security may hide the parent row.
            "SELECT n.id FROM notes n JOIN secret s ON s.id = n.secret_id",
            // An inner join on a unique key that no foreign key backs.
            "SELECT p.title FROM posts p JOIN users u ON u.email = p.editor_email",
            // LATERAL, a subquery, a function.
            "SELECT p.title FROM posts p LEFT JOIN LATERAL \
             (SELECT u.id FROM users u WHERE u.id = p.editor_id) s ON true",
            "SELECT p.title FROM posts p LEFT JOIN (SELECT id FROM users) s ON s.id = p.editor_id",
            // USING.
            "SELECT p.title FROM posts p LEFT JOIN users u USING (id)",
            // The referencing side: a user has many posts.
            "SELECT u.name FROM users u LEFT JOIN posts p ON p.user_id = u.id",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(redundant_join)\n\
                   SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.editor_id";
        assert!(lint_rule(&db, sql, RULE).is_empty());
    }
}
