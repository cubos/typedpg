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
use typedpg_pg_query::ast::{self, AConstValue, LimitOption, Token};

use super::limit_without_order::implicit_fetch;
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    // The statement's own SELECT, and whether it can raise nothing: a
    // subquery's LIMIT keeps the planner from pulling it up into the
    // query around it, where a constant it outputs is folded into that
    // query's expressions at plan time (`SELECT x::int2 FROM (SELECT 40000
    // AS x LIMIT 1) s WHERE false` succeeds, and fails without the LIMIT).
    let root = match ctx.tree.stmts.first().and_then(|raw| raw.stmt.as_ref()) {
        Some(ast::Node::SelectStmt(select)) => Some(&**select),
        _ => None,
    };
    // Computed once, and only for a merged subquery's LIMIT.
    let root_error_free = std::cell::OnceCell::new();
    let root_error_free = || {
        *root_error_free.get_or_init(|| {
            root.is_some_and(|select| ctx.view.select_is_error_free(ctx.catalog, select))
        })
    };
    // The selects the planner may merge into the query around them: a FROM
    // subquery, a CTE, an `EXISTS` / `IN` subquery (a scalar subquery is
    // never merged).
    let mut mergeable: Vec<*const ast::SelectStmt> = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let inner = match node {
            NodeRef::RangeSubselect(sub) => sub.subquery.as_ref(),
            NodeRef::CommonTableExpr(cte) => cte.ctequery.as_ref(),
            NodeRef::SubLink(link)
                if matches!(
                    link.sub_link_type,
                    ast::SubLinkType::ExistsSublink | ast::SubLinkType::AnySublink
                ) =>
            {
                link.subselect.as_ref()
            }
            _ => None,
        };
        if let Some(ast::Node::SelectStmt(select)) = inner {
            mergeable.push(&**select);
        }
    }
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if select.limit_offset.is_some() || select.limit_option == LimitOption::WithTies {
            continue;
        }
        let Some(count) = select.limit_count.as_ref() else {
            continue;
        };
        if !is_positive_integer(count) {
            continue;
        }
        if !select_at_most_one_row(ctx.catalog, ctx.view, select) {
            continue;
        }
        let Some(clause) = limit_clause(ctx, select, count) else {
            continue;
        };
        // Removing LIMIT lets PG evaluate the rest of the query on rows the
        // scan previously stopped before, so withhold the fix when an
        // expression of the select could raise on one of them.
        let merged = mergeable.contains(&std::ptr::from_ref(select));
        let error_free =
            ctx.view.select_is_error_free(ctx.catalog, select) && (!merged || root_error_free());
        let message = match (clause.fetch, clause.count.clone()) {
            (None, Some(count)) => format!(
                "LIMIT {} has no effect: the query returns at most one row",
                &ctx.sql[count]
            ),
            (Some(first), Some(count)) => format!(
                "FETCH {first} {} has no effect: the query returns at most one row",
                &ctx.sql[count]
            ),
            // `FETCH FIRST ROW ONLY`: quoted as written.
            (_, None) => format!(
                "{} has no effect: the query returns at most one row",
                ctx.tokens
                    .iter()
                    .filter(|token| token.start >= clause.keyword && token.end <= clause.end)
                    .map(|token| &ctx.sql[token.start..token.end])
                    .collect::<Vec<_>>()
                    .join(" ")
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
fn is_positive_integer(node: &ast::Node) -> bool {
    matches!(
        node,
        ast::Node::AConst(value)
            if !value.isnull
                && matches!(value.val, Some(AConstValue::Integer(ref count)) if count.ival >= 1)
    )
}

/// One written `LIMIT` / `FETCH FIRST` clause.
struct LimitClause {
    /// The keyword's first byte (`LIMIT` or `FETCH`).
    keyword: usize,
    /// The count literal's bytes (`None` for `FETCH FIRST ROW ONLY`).
    count: Option<std::ops::Range<usize>>,
    /// The clause's last byte: the count, or `ONLY` for a `FETCH`.
    end: usize,
    /// `Some("FIRST" | "NEXT")` when written as a `FETCH`.
    fetch: Option<&'static str>,
}

/// The clause `count` (`select`'s) belongs to, read from the tokens
/// around it.
fn limit_clause(ctx: &Ctx<'_>, select: &ast::SelectStmt, count: &ast::Node) -> Option<LimitClause> {
    let Some(at) = first_location(NodeRef::AConst(match count {
        ast::Node::AConst(value) => value,
        _ => return None,
    })) else {
        // The implicit 1 of `FETCH FIRST ROW ONLY` has no location.
        let fetch = implicit_fetch(ctx, select)?;
        let first = ctx.tokens.iter().find(|token| token.start > fetch.start)?;
        return Some(LimitClause {
            keyword: fetch.start,
            count: None,
            end: fetch.end,
            fetch: Some(if is_keyword(ctx, first, "FIRST") {
                "FIRST"
            } else {
                "NEXT"
            }),
        });
    };
    let index = ctx.tokens.iter().position(|token| token.start == at)?;
    let current = ctx.tokens[index];
    let previous = ctx.tokens.get(index.checked_sub(1)?)?;
    if is_keyword(ctx, previous, "LIMIT") {
        return Some(LimitClause {
            keyword: previous.start,
            count: Some(current.start..current.end),
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
            count: Some(current.start..current.end),
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
            count: Some(current.start..current.end),
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
            token.token == Token::SqlComment
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
    fn a_fetch_without_a_count_is_removed_whole() {
        // PG stores the implicit 1 with no location: the clause is read
        // from the tokens.
        let db = catalog();
        for (sql, message, fixed_sql) in [
            (
                "SELECT email FROM users WHERE email = $email FETCH FIRST ROW ONLY",
                "FETCH FIRST ROW ONLY has no effect: the query returns at most one row",
                "SELECT email FROM users WHERE email = $email",
            ),
            (
                "SELECT count(*) FROM users fetch next rows only",
                "fetch next rows only has no effect: the query returns at most one row",
                "SELECT count(*) FROM users",
            ),
            (
                "SELECT (SELECT name FROM users WHERE id = $id FETCH FIRST ROW ONLY) AS name",
                "FETCH FIRST ROW ONLY has no effect: the query returns at most one row",
                "SELECT (SELECT name FROM users WHERE id = $id) AS name",
            ),
        ] {
            let findings = limit_findings(&db, sql);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].message, message);
            assert_eq!(fixed(sql, &findings), fixed_sql);
        }
        // Rows it can still cut stay.
        assert!(limit_findings(&db, "SELECT id FROM users FETCH FIRST ROW ONLY").is_empty());
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
    fn a_nested_limit_under_a_query_that_may_raise_keeps_its_fix_withheld() {
        let db = catalog();
        // Without the LIMIT the subquery is pulled up and `40000::int2`
        // folded at plan time: an error, even with no row to return.
        let sql = "SELECT x::int2 AS y FROM (SELECT 40000 AS x LIMIT 1) s WHERE false";
        let findings: Vec<_> = lint(&db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::RedundantLimit)
            .collect();
        assert_eq!(findings.len(), 1, "{findings:?}");
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
    fn a_non_lateral_from_subquery_binds_the_outer_level() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE outer_rows (id int PRIMARY KEY, lookup bigint);
             CREATE TABLE inner_rows (id int PRIMARY KEY, lookup double precision);
             CREATE TABLE keys (id bigint PRIMARY KEY);
             CREATE TABLE exact_rows (id int PRIMARY KEY, lookup bigint);",
        )
        .unwrap();
        // The subquery's `x` is the outer `inner_rows`: its float8 `lookup`
        // can equal two bigint keys, so the LIMIT still matters.
        let shadowed = "SELECT ARRAY( \
             SELECT s.id FROM outer_rows x \
             CROSS JOIN (SELECT k.id FROM keys k WHERE k.id = x.lookup LIMIT 1) s \
             WHERE x.id = 1 \
         ) AS arr FROM inner_rows x WHERE x.id = 1";
        assert!(
            limit_findings(&db, shadowed).is_empty(),
            "{:?}",
            limit_findings(&db, shadowed)
        );
        // Against the outer `exact_rows`' bigint `lookup`, the key lookup is
        // one row and the LIMIT is redundant.
        let exact = "SELECT ARRAY( \
             SELECT s.id FROM exact_rows x \
             CROSS JOIN (SELECT k.id FROM keys k WHERE k.id = x.lookup LIMIT 1) s \
             WHERE x.id = 1 \
         ) AS arr FROM exact_rows x WHERE x.id = 1";
        let findings = limit_findings(&db, exact);
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(redundant_limit)\nSELECT 1 LIMIT 1";
        assert!(lint(&db, sql).is_empty());
    }
}
