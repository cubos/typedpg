//! The `redundant_limit` rule: reports a `LIMIT` / `FETCH FIRST` on a query
//! the analyzer proves returns at most one row, such as one over a primary
//! key or a `SELECT` without `FROM`. The fix removes the clause, but only
//! when the select is error-free: without the limit the scan may read rows
//! it previously stopped before, so a WHERE or target-list expression that
//! can raise would change the errors the query raises.
//!
//! `LIMIT 0` (and `LIMIT ALL` / `LIMIT NULL`) are not reported: the first
//! needs no fix and the others limit nothing.

use typedpg_analyzer::select_at_most_one_row;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{self, LimitOption, Token, a_const::Val, node};

use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.limit_offset.is_some() || select.limit_option() == LimitOption::WithTies {
            continue;
        }
        let Some(count) = select.limit_count.as_deref() else {
            continue;
        };
        if !is_positive_integer(count) {
            continue;
        }
        if !select_at_most_one_row(ctx.catalog, ctx.view, select) {
            continue;
        }
        let Some(clause) = limit_clause(ctx, count) else {
            continue;
        };
        // Removing LIMIT lets PG evaluate the rest of the query on rows the
        // scan previously stopped before, so withhold the fix when an
        // expression of the select could raise on one of them.
        let error_free = ctx.view.select_is_error_free(ctx.catalog, select);
        let count_text = &ctx.sql[clause.count.clone()];
        let message = match clause.fetch {
            None => format!("LIMIT {count_text} has no effect: the query returns at most one row"),
            Some(first) => format!(
                "FETCH {first} {count_text} has no effect: the query returns at most one row"
            ),
        };
        let start = delete_start(ctx, clause.keyword);
        out.push(RawFinding {
            span: clause.keyword..clause.end,
            message,
            help: Some("remove it".to_owned()),
            fix: if error_free {
                vec![SqlEdit {
                    range: start..clause.end,
                    text: String::new(),
                }]
            } else {
                Vec::new()
            },
        });
    }
    out
}

/// Whether `node` is an integer constant at least 1 (not `ALL`, `NULL`, a
/// parameter or an expression).
fn is_positive_integer(node: &protobuf::Node) -> bool {
    matches!(
        node.node,
        Some(node::Node::AConst(ref value))
            if !value.isnull
                && matches!(value.val, Some(Val::Ival(ref count)) if count.ival >= 1)
    )
}

/// One written `LIMIT` / `FETCH FIRST` clause.
struct LimitClause {
    /// The keyword's first byte (`LIMIT` or `FETCH`).
    keyword: usize,
    /// The count literal's bytes.
    count: std::ops::Range<usize>,
    /// The clause's last byte: the count, or `ONLY` for a `FETCH`.
    end: usize,
    /// `Some("FIRST" | "NEXT")` when written as a `FETCH`.
    fetch: Option<&'static str>,
}

/// The clause `count` belongs to, read from the tokens around it.
fn limit_clause(ctx: &Ctx<'_>, count: &protobuf::Node) -> Option<LimitClause> {
    let at = first_location(NodeRef::AConst(match count.node.as_ref()? {
        node::Node::AConst(value) => value,
        _ => return None,
    }))?;
    let index = ctx.tokens.iter().position(|token| token.start == at)?;
    let current = ctx.tokens[index];
    let previous = ctx.tokens.get(index.checked_sub(1)?)?;
    if is_keyword(ctx, previous, "LIMIT") {
        return Some(LimitClause {
            keyword: previous.start,
            count: current.start..current.end,
            end: current.end,
            fetch: None,
        });
    }
    if is_keyword(ctx, previous, "FIRST") || is_keyword(ctx, previous, "NEXT") {
        let before = ctx.tokens.get(index.checked_sub(2)?)?;
        if !is_keyword(ctx, before, "FETCH") {
            return None;
        }
        let only = ctx.tokens[index + 1..]
            .iter()
            .find(|token| is_keyword(ctx, token, "ONLY"))?;
        return Some(LimitClause {
            keyword: before.start,
            count: current.start..current.end,
            end: only.end,
            fetch: Some(if is_keyword(ctx, previous, "FIRST") {
                "FIRST"
            } else {
                "NEXT"
            }),
        });
    }
    if is_keyword(ctx, previous, "FETCH") {
        // `FETCH 5 ROWS ONLY`: no FIRST / NEXT keyword.
        let only = ctx.tokens[index + 1..]
            .iter()
            .find(|token| is_keyword(ctx, token, "ONLY"))?;
        return Some(LimitClause {
            keyword: previous.start,
            count: current.start..current.end,
            end: only.end,
            fetch: Some("FIRST"),
        });
    }
    None
}

/// Whether the token reads `keyword`, case-insensitively.
fn is_keyword(ctx: &Ctx<'_>, token: &crate::Tok, keyword: &str) -> bool {
    ctx.sql[token.start..token.end].eq_ignore_ascii_case(keyword)
}

/// The first byte a node covers: the minimum of its location and its
/// descendants' (a cast's location is its `::`, an expression's its
/// operator).
fn first_location(node: NodeRef<'_>) -> Option<usize> {
    let mut first = node
        .location()
        .and_then(|location| usize::try_from(location).ok());
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let Some(location) = node
            .location()
            .and_then(|location| usize::try_from(location).ok())
        {
            first = Some(first.map_or(location, |first| first.min(location)));
        }
        stack.extend(node.children());
    }
    first
}

/// Where deleting the clause should start: the whitespace before its
/// keyword. A newline that ends a `--` comment is kept, so the text after
/// the clause doesn't become part of the comment.
fn delete_start(ctx: &Ctx<'_>, keyword: usize) -> usize {
    let bytes = ctx.sql.as_bytes();
    let mut start = keyword;
    while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
        start -= 1;
    }
    let mut before = start;
    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before < start && ends_in_line_comment(&ctx.sql[..before]) {
        start
    } else {
        before
    }
}

/// Whether `sql` ends exactly inside a `--` comment.
fn ends_in_line_comment(sql: &str) -> bool {
    match typedpg_pg_query::scan(sql) {
        Ok(scanned) => scanned.tokens.last().is_some_and(|token| {
            token.token() == Token::SqlComment
                && usize::try_from(token.end).is_ok_and(|end| end == sql.len())
        }),
        Err(_) => true,
    }
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

    /// The `redundant_limit` findings of `sql` (another rule may fire too).
    fn limit_findings(catalog: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint(catalog, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::RedundantLimit)
            .collect()
    }

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
    fn a_limit_on_a_primary_key_lookup_is_redundant() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE id = $id LIMIT 1";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::RedundantLimit);
        assert_eq!(findings[0].severity, Severity::Info);
        assert_eq!(
            findings[0].message,
            "LIMIT 1 has no effect: the query returns at most one row"
        );
        assert_eq!(findings[0].help.as_deref(), Some("remove it"));
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 1");
        assert_eq!(
            fixed(sql, &findings),
            "SELECT id, name FROM users WHERE id = $id"
        );
    }

    #[test]
    fn aggregates_and_from_less_selects_are_one_row() {
        let db = catalog();
        let sql = "SELECT count(*) FROM users LIMIT 10";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "LIMIT 10 has no effect: the query returns at most one row"
        );
        assert_eq!(fixed(sql, &findings), "SELECT count(*) FROM users");

        let sql = "SELECT 1 LIMIT 1";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(fixed(sql, &findings), "SELECT 1");
    }

    #[test]
    fn fetch_first_is_removed_whole() {
        let db = catalog();
        let sql = "SELECT email FROM users WHERE email = $email FETCH FIRST 3 ROWS ONLY";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "FETCH FIRST 3 has no effect: the query returns at most one row"
        );
        assert_eq!(
            fixed(sql, &findings),
            "SELECT email FROM users WHERE email = $email"
        );
    }

    #[test]
    fn a_limit_inside_a_subquery_is_found() {
        let db = catalog();
        let sql = "SELECT (SELECT name FROM users WHERE id = $id LIMIT 1) AS name";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            fixed(sql, &findings),
            "SELECT (SELECT name FROM users WHERE id = $id) AS name"
        );
    }

    #[test]
    fn comments_are_kept() {
        let db = catalog();
        let sql = "SELECT id FROM users WHERE id = $id /* keep */ LIMIT 1 -- tail";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            fixed(sql, &findings),
            "SELECT id FROM users WHERE id = $id /* keep */ -- tail"
        );

        // A line comment right before the clause keeps its newline.
        let sql = "SELECT id FROM users WHERE id = $id -- why\nLIMIT 1\n";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            fixed(sql, &findings),
            "SELECT id FROM users WHERE id = $id -- why\n\n"
        );
    }

    #[test]
    fn a_fix_that_could_change_the_errors_raised_is_withheld() {
        let db = catalog();
        // Without LIMIT PG may evaluate the division on a row the key
        // lookup never had to read.
        let sql = "SELECT id FROM users WHERE id = $id AND 1/(id-2) > 0 LIMIT 1";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::RedundantLimit);
        assert!(findings[0].fix.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_limit_that_can_change_the_rows_is_kept() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users LIMIT 5",
            "SELECT id FROM users WHERE id = $id LIMIT 1 OFFSET 0",
            "SELECT id FROM users WHERE id = $id LIMIT 0",
            "SELECT id FROM users WHERE id = $id LIMIT $n",
            "SELECT id FROM users WHERE id = $id LIMIT ALL",
            "SELECT id FROM users WHERE id = $id LIMIT NULL",
            "SELECT id FROM users ORDER BY id FETCH FIRST 1 ROWS WITH TIES",
        ] {
            assert!(
                limit_findings(&db, sql).is_empty(),
                "{sql}: {:?}",
                limit_findings(&db, sql)
            );
        }
    }

    #[test]
    fn a_limit_inside_a_set_operation_is_kept() {
        let db = catalog();
        assert!(
            limit_findings(
                &db,
                "SELECT id FROM users UNION SELECT user_id FROM posts LIMIT 1"
            )
            .is_empty()
        );
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(redundant_limit)\nSELECT 1 LIMIT 1";
        assert!(lint(&db, sql).is_empty());
    }
}
