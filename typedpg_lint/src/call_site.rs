//! The call-site rules: `fetch_may_return_many`, `fetch_all_single_row`
//! and `unused_returning`.
//!
//! They run over the query's [`QueryView`](typedpg_analyzer::QueryView) like
//! every other query rule: the method called on the `sql!` result decides
//! what to report, and the analyzer's cardinality proof decides whether the
//! statement can return more than one row. A finding's span and fix are in
//! the analyzed SQL text, which `lint_query_called` maps back to the text
//! the user wrote.

use typedpg_analyzer::statement_at_most_one_row;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::RawStmt;
use typedpg_pg_query::protobuf::node::Node;

use crate::{CallMethod, Ctx, RawFinding, Rule, SqlEdit};

/// The findings of `rule` at this call site, if `rule` is a call-site rule
/// that fires for `call`.
pub(crate) fn findings(rule: Rule, ctx: &Ctx<'_>, call: Option<CallMethod>) -> Vec<RawFinding> {
    let Some(call) = call else {
        return Vec::new();
    };
    match rule {
        Rule::FetchMayReturnMany => match call {
            CallMethod::FetchOne | CallMethod::FetchOptional
                if !statement_at_most_one_row(ctx.catalog, ctx.view) =>
            {
                vec![fetch_may_return_many(call, ctx)]
            }
            _ => Vec::new(),
        },
        Rule::FetchAllSingleRow => match call {
            CallMethod::FetchAll if statement_at_most_one_row(ctx.catalog, ctx.view) => {
                vec![fetch_all_single_row(ctx)]
            }
            _ => Vec::new(),
        },
        Rule::UnusedReturning => match call {
            CallMethod::Execute => unused_returning(ctx),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The finding for `fetch_one` / `fetch_optional` on a statement the proof
/// does not bound to one row. The span is the statement's first token.
fn fetch_may_return_many(call: CallMethod, ctx: &Ctx<'_>) -> RawFinding {
    let method = match call {
        CallMethod::FetchOptional => "fetch_optional",
        _ => "fetch_one",
    };
    RawFinding {
        span: statement_start(ctx),
        message: format!(
            "`.{method}()` fails with TooManyRows if this query returns a second row, and nothing \
             in the schema limits it to one"
        ),
        help: Some(
            "filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetch_all()".to_owned(),
        ),
        fix: Vec::new(),
    }
}

/// The finding for `fetch_all` on a statement the proof bounds to one row.
fn fetch_all_single_row(ctx: &Ctx<'_>) -> RawFinding {
    RawFinding {
        span: statement_start(ctx),
        message: "`.fetch_all()` on a query that returns at most one row".to_owned(),
        help: Some("use .fetch_optional() (or .fetch_one())".to_owned()),
        fix: Vec::new(),
    }
}

/// The finding for `execute()` on a top-level `INSERT` / `UPDATE` /
/// `DELETE` / `MERGE` with a `RETURNING` list, whose rows `execute()`
/// discards.
///
/// The fix deletes the clause, unless the returning list calls a function or
/// contains a sublink: removing the call could change what the statement
/// observes or does, so that one is reported only.
fn unused_returning(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let Some(raw) = ctx.view.tree.stmts.first() else {
        return Vec::new();
    };
    let Some(statement) = raw.stmt.as_deref().and_then(|node| node.node.as_ref()) else {
        return Vec::new();
    };
    let returning = match statement {
        Node::InsertStmt(stmt) => stmt.returning_clause.as_ref(),
        Node::UpdateStmt(stmt) => stmt.returning_clause.as_ref(),
        Node::DeleteStmt(stmt) => stmt.returning_clause.as_ref(),
        Node::MergeStmt(stmt) => stmt.returning_clause.as_ref(),
        _ => None,
    };
    let Some(returning) = returning else {
        return Vec::new();
    };
    let Some((keyword, end)) = returning_span(ctx, raw) else {
        return Vec::new();
    };
    let fix = if returning.exprs.iter().any(contains_call_or_subquery) {
        Vec::new()
    } else {
        vec![SqlEdit {
            range: whitespace_start(ctx.sql, keyword)..end,
            text: String::new(),
        }]
    };
    vec![RawFinding {
        span: keyword..end,
        message: "`.execute()` discards the RETURNING rows".to_owned(),
        help: Some("remove RETURNING, or fetch the rows".to_owned()),
        fix,
    }]
}

/// The first non-comment token of the query: `SELECT`, `INSERT`, …
fn statement_start(ctx: &Ctx<'_>) -> std::ops::Range<usize> {
    ctx.tokens
        .first()
        .map_or(0..0, |token| token.start..token.end)
}

/// The position of the top-level `RETURNING` keyword and the end of its
/// list, in `ctx.sql`.
///
/// The keyword is the last `RETURNING` outside parentheses of the statement
/// (a CTE's own `RETURNING` is nested), and the list ends at the statement's
/// last token, keeping any trailing `;`.
fn returning_span(ctx: &Ctx<'_>, raw: &RawStmt) -> Option<(usize, usize)> {
    let start = usize::try_from(raw.stmt_location).unwrap_or(0);
    let end = match usize::try_from(raw.stmt_len) {
        Ok(0) => ctx.sql.len(),
        Ok(len) => start.saturating_add(len).min(ctx.sql.len()),
        Err(_) => ctx.sql.len(),
    };
    let tokens: Vec<&crate::Tok> = ctx
        .tokens
        .iter()
        .filter(|token| token.start >= start && token.end <= end)
        .collect();
    let mut depth = 0usize;
    let mut keyword = None;
    for (index, token) in tokens.iter().enumerate() {
        let text = &ctx.sql[token.start..token.end];
        if text == "(" {
            depth += 1;
        } else if text == ")" {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && text.eq_ignore_ascii_case("RETURNING") {
            keyword = Some(index);
        }
    }
    let keyword_index = keyword?;
    let keyword = tokens[keyword_index].start;
    let last = tokens[keyword_index..]
        .iter()
        .rev()
        .find(|token| &ctx.sql[token.start..token.end] != ";")?
        .end;
    Some((keyword, last))
}

/// The start of the whitespace run that ends at `at`, so deleting from here
/// takes the space before `RETURNING` with the clause.
fn whitespace_start(sql: &str, at: usize) -> usize {
    let mut start = at;
    while let Some(c) = sql[..start].chars().next_back() {
        if !c.is_whitespace() {
            break;
        }
        start -= c.len_utf8();
    }
    start
}

/// Whether the expression calls a function or contains a sublink anywhere.
fn contains_call_or_subquery(node: &typedpg_pg_query::protobuf::Node) -> bool {
    let Some(inner) = node.node.as_ref() else {
        return false;
    };
    let mut stack = vec![inner.to_ref()];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::FuncCall(_) | NodeRef::SubLink(_) => return true,
            _ => stack.extend(node.children()),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use typedpg_analyzer::PgCatalog;

    use crate::{CallMethod, Finding, LintOptions, Severity, lint_query_called};

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (id integer PRIMARY KEY, org_id integer, name text);
                 CREATE TABLE orgs (id integer PRIMARY KEY, name text);",
            )
            .unwrap();
        catalog
    }

    fn lint(catalog: &PgCatalog, sql: &str, call: Option<CallMethod>) -> Vec<Finding> {
        lint_query_called(catalog, sql, call, &LintOptions::default()).unwrap()
    }

    fn only(findings: Vec<Finding>, rule: Rule) -> Vec<Finding> {
        findings
            .into_iter()
            .filter(|finding| finding.rule == rule)
            .collect()
    }

    /// Apply a finding's fix to `sql`.
    fn fixed(sql: &str, finding: &Finding) -> String {
        let mut fixed = sql.to_owned();
        for edit in finding.fix.iter().rev() {
            fixed.replace_range(edit.range.clone(), &edit.text);
        }
        fixed
    }

    #[test]
    fn fetch_one_on_a_unique_key_is_fine() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE id = $1";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::FetchOne)),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::FetchOptional)),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_one_on_a_non_unique_filter_may_return_many() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        let findings = only(
            lint(&db, sql, Some(CallMethod::FetchOne)),
            Rule::FetchMayReturnMany,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = &findings[0];
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(
            finding.message,
            "`.fetch_one()` fails with TooManyRows if this query returns a second row, and \
             nothing in the schema limits it to one"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some("filter by a unique key, add LIMIT 1 with an ORDER BY, or use .fetch_all()")
        );
        assert_eq!(&sql[finding.span.clone()], "SELECT");
    }

    #[test]
    fn fetch_optional_names_its_own_method() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        let findings = only(
            lint(&db, sql, Some(CallMethod::FetchOptional)),
            Rule::FetchMayReturnMany,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0]
                .message
                .starts_with("`.fetch_optional()` fails with TooManyRows"),
            "{}",
            findings[0].message
        );
    }

    #[test]
    fn a_limit_of_one_is_proof_enough() {
        let db = catalog();
        let sql = "SELECT id, name FROM users LIMIT 1";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::FetchOne)),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_optional_on_an_aggregate_is_fine() {
        let db = catalog();
        let sql = "SELECT count(*) FROM users";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::FetchOptional)),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
    }

    #[test]
    fn fetch_all_on_a_single_row_query_is_reported() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE id = $1";
        let findings = only(
            lint(&db, sql, Some(CallMethod::FetchAll)),
            Rule::FetchAllSingleRow,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = &findings[0];
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(
            finding.message,
            "`.fetch_all()` on a query that returns at most one row"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some("use .fetch_optional() (or .fetch_one())")
        );
        assert_eq!(&sql[finding.span.clone()], "SELECT");
    }

    #[test]
    fn fetch_all_on_a_non_unique_query_is_fine() {
        let db = catalog();
        let sql = "SELECT id, name FROM users WHERE org_id = $1";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::FetchAll)),
                Rule::FetchAllSingleRow
            )
            .is_empty()
        );
    }

    #[test]
    fn execute_on_a_returning_insert_reports_and_fixes() {
        let db = catalog();
        let sql = "INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id";
        let findings = only(
            lint(&db, sql, Some(CallMethod::Execute)),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = &findings[0];
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(finding.message, "`.execute()` discards the RETURNING rows");
        assert_eq!(
            finding.help.as_deref(),
            Some("remove RETURNING, or fetch the rows")
        );
        assert_eq!(&sql[finding.span.clone()], "RETURNING id");
        assert_eq!(
            fixed(sql, finding),
            "INSERT INTO users (id, name) VALUES (1, 'x')"
        );
    }

    #[test]
    fn execute_on_a_returning_update_with_a_call_reports_without_a_fix() {
        let db = catalog();
        let sql = "UPDATE users SET name = 'x' WHERE id = $1 RETURNING now()";
        let findings = only(
            lint(&db, sql, Some(CallMethod::Execute)),
            Rule::UnusedReturning,
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty(), "{:?}", findings[0].fix);
        assert_eq!(&sql[findings[0].span.clone()], "RETURNING now()");
    }

    #[test]
    fn execute_without_returning_is_fine() {
        let db = catalog();
        let sql = "UPDATE users SET name = 'x' WHERE id = $1";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::Execute)),
                Rule::UnusedReturning
            )
            .is_empty()
        );
    }

    #[test]
    fn no_call_site_reports_none_of_them() {
        let db = catalog();
        for sql in [
            "SELECT id, name FROM users WHERE org_id = $1",
            "SELECT id, name FROM users WHERE id = $1",
            "INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id",
        ] {
            let findings = lint(&db, sql, None);
            for rule in [
                Rule::FetchMayReturnMany,
                Rule::FetchAllSingleRow,
                Rule::UnusedReturning,
            ] {
                assert!(only(findings.clone(), rule).is_empty(), "{sql}: {rule}");
            }
        }
    }

    #[test]
    fn an_allow_comment_suppresses_the_call_site_rules() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(fetch_may_return_many)\n\
                   SELECT id, name FROM users WHERE org_id = $1";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::FetchOne)),
                Rule::FetchMayReturnMany
            )
            .is_empty()
        );
        let sql = "-- typedpg-lint: allow(unused_returning)\n\
                   INSERT INTO users (id, name) VALUES (1, 'x') RETURNING id";
        assert!(
            only(
                lint(&db, sql, Some(CallMethod::Execute)),
                Rule::UnusedReturning
            )
            .is_empty()
        );
    }
}
