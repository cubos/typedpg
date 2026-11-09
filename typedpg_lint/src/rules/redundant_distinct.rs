//! The `redundant_distinct` rule: reports a plain `DISTINCT` on rows the
//! schema proves are already distinct, such as a `SELECT` whose output
//! contains a unique key of every table it reads. The fix removes the
//! keyword, but only when the select is error-free: a hash aggregate
//! evaluates the target list on every row, while the scan without
//! `DISTINCT` may stop earlier, so an expression that can raise would
//! change the errors the query raises.
//!
//! The proof is the `distinct` simplification's (see
//! [`typedpg_analyzer::redundant_distinct_spans`] and the analyzer's
//! `simplify::dedup` module): this rule only reports the spans that
//! simplification would delete.

use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    ctx.view
        .redundant_distinct_spans_with_fix(ctx.catalog)
        .into_iter()
        .map(|(span, error_free)| RawFinding {
            span: span.clone(),
            message: "DISTINCT has no effect: the rows are already distinct".to_owned(),
            help: Some("remove DISTINCT (a unique key of every table is in the output)".to_owned()),
            // Removing DISTINCT lets PG evaluate the target list on more
            // rows (a hash aggregate reads every row first): the fix is
            // withheld when an expression could raise on one of them.
            fix: if error_free {
                vec![SqlEdit {
                    range: span,
                    text: String::new(),
                }]
            } else {
                Vec::new()
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Finding, LintOptions, Rule, Severity, lint_query};
    use typedpg_analyzer::PgCatalog;

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
                    user_id bigint NOT NULL REFERENCES users
                 );",
            )
            .unwrap();
        catalog
    }

    fn lint(catalog: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint_query(catalog, sql, &LintOptions::default()).unwrap()
    }

    /// `sql` with every finding's fix applied, in the original text.
    fn fixed(sql: &str, findings: &[Finding]) -> String {
        let mut edits: Vec<SqlEdit> = findings
            .iter()
            .flat_map(|finding| finding.fix.clone())
            .collect();
        edits.sort_by_key(|edit| edit.range.start);
        let mut out = String::with_capacity(sql.len());
        let mut last = 0;
        for edit in edits {
            out.push_str(&sql[last..edit.range.start]);
            out.push_str(&edit.text);
            last = edit.range.end;
        }
        out.push_str(&sql[last..]);
        out
    }

    #[test]
    fn a_primary_key_in_the_output_makes_distinct_redundant() {
        let db = catalog();
        let sql = "SELECT DISTINCT id FROM users";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::RedundantDistinct);
        assert_eq!(findings[0].severity, Severity::Info);
        assert_eq!(
            findings[0].message,
            "DISTINCT has no effect: the rows are already distinct"
        );
        assert_eq!(
            findings[0].help.as_deref(),
            Some("remove DISTINCT (a unique key of every table is in the output)")
        );
        assert_eq!(&sql[findings[0].span.clone()], "DISTINCT ");
        assert_eq!(fixed(sql, &findings), "SELECT id FROM users");
    }

    #[test]
    fn a_unique_column_or_join_key_makes_distinct_redundant() {
        let db = catalog();
        for sql in [
            "SELECT DISTINCT email FROM users",
            "SELECT DISTINCT p.id, u.name FROM posts p JOIN users u ON u.id = p.user_id",
            "SELECT * FROM (SELECT DISTINCT id FROM users) s",
        ] {
            let findings = lint(&db, sql);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].rule, Rule::RedundantDistinct);
        }
    }

    #[test]
    fn named_parameters_keep_their_text_in_the_fix() {
        let db = catalog();
        let sql = "SELECT DISTINCT id, name FROM users WHERE id = $id";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            fixed(sql, &findings),
            "SELECT id, name FROM users WHERE id = $id"
        );
    }

    #[test]
    fn comments_are_kept() {
        let db = catalog();
        let sql = "SELECT DISTINCT /* keep */ id FROM users";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        let fixed = fixed(sql, &findings);
        assert!(fixed.contains("/* keep */"), "{fixed}");
        assert!(!fixed.contains("DISTINCT"), "{fixed}");
    }

    #[test]
    fn a_fix_that_could_change_the_errors_raised_is_withheld() {
        let db = catalog();
        // A hash aggregate (DISTINCT) reads both rows and evaluates the
        // division on each; without DISTINCT the scan may stop at the first.
        let sql = "SELECT DISTINCT id, 1/(id-2) FROM users";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::RedundantDistinct);
        assert!(findings[0].fix.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_distinct_that_can_remove_rows_is_kept() {
        let db = catalog();
        for sql in [
            "SELECT DISTINCT name FROM users",
            "SELECT DISTINCT ON (name) id FROM users ORDER BY name",
            "SELECT DISTINCT user_id FROM posts",
        ] {
            assert!(lint(&db, sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(redundant_distinct)\nSELECT DISTINCT id FROM users";
        assert!(lint(&db, sql).is_empty());
    }
}
