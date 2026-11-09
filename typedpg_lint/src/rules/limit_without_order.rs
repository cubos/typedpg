//! The `limit_without_order` rule: reports a `LIMIT`, an `OFFSET` or a
//! `DISTINCT ON` without an `ORDER BY`, which picks arbitrary rows (and, for
//! `DISTINCT ON`, an arbitrary row per group).
//!
//! Not reported when the choice doesn't matter: a query proven to return at
//! most one row ([`typedpg_analyzer::select_at_most_one_row`]), a subquery
//! of `EXISTS` (existence doesn't care which rows), an `IN` / `= ANY` /
//! `ALL` in the `WHERE` of an `UPDATE` or `DELETE` (the batching idiom), a
//! `LIMIT 0`, or a locking clause with `SKIP LOCKED` (the queue pattern
//! wants any unlocked row).

use std::collections::HashSet;

use typedpg_analyzer::{ProVolatile, select_at_most_one_row};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, LockWaitPolicy, SelectStmt, SubLinkType, a_const::Val, node::Node,
};

use crate::{Ctx, RawFinding};

const HELP: &str = "add an ORDER BY that makes the choice deterministic, or allow it with \
                    -- typedpg-lint: allow(limit_without_order)";

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let batched = batched_subqueries(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if !select.sort_clause.is_empty() {
            continue;
        }
        let pointer: *const SelectStmt = select;
        if batched.contains(&pointer) {
            continue;
        }
        if has_skip_locked(select) || limit_is_zero(select) {
            continue;
        }
        if constant_target(ctx, select) {
            continue;
        }
        if select_at_most_one_row(ctx.catalog, ctx.view, select) {
            continue;
        }
        if let Some(span) = distinct_on_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "DISTINCT ON without ORDER BY keeps an arbitrary row of each group"
                    .to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
        if let Some(span) = limit_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "LIMIT without ORDER BY returns arbitrary rows".to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
        if let Some(span) = offset_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "OFFSET without ORDER BY skips arbitrary rows".to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
    }
    out
}

/// The selects whose arbitrary row choice doesn't matter: the subquery of
/// an `EXISTS` (existence doesn't care which rows), or an `IN` / `= ANY` /
/// `ALL` in the `WHERE` of an `UPDATE` or `DELETE` — the batching idiom
/// (`DELETE … WHERE id IN (SELECT … LIMIT 1000)`) deliberately takes any
/// batch. An `IN` elsewhere (a `SELECT`'s WHERE, a projection) does care:
/// the limit changes which values the outer query sees.
fn batched_subqueries(ctx: &Ctx<'_>) -> HashSet<*const SelectStmt> {
    let mut batched = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SubLink(sublink) = node else {
            continue;
        };
        match sublink.sub_link_type() {
            SubLinkType::ExistsSublink => {
                insert_sublink_select(&mut batched, sublink);
            }
            SubLinkType::AnySublink | SubLinkType::AllSublink if in_dml_where(ctx, sublink) => {
                insert_sublink_select(&mut batched, sublink);
            }
            _ => {}
        }
    }
    batched
}

fn insert_sublink_select(batched: &mut HashSet<*const SelectStmt>, sublink: &protobuf::SubLink) {
    if let Some(Node::SelectStmt(select)) = sublink
        .subselect
        .as_deref()
        .and_then(|node| node.node.as_ref())
    {
        let pointer: *const SelectStmt = &**select;
        batched.insert(pointer);
    }
}

/// Whether this `IN` / `ANY` / `ALL` sublink sits in the `WHERE` of an
/// `UPDATE` or `DELETE` at its own query level. Nested `SELECT`s inside the
/// WHERE are other levels, whose sublinks aren't exempt.
fn in_dml_where(ctx: &Ctx<'_>, target: &protobuf::SubLink) -> bool {
    let target: *const protobuf::SubLink = target;
    for (node, _) in ctx.tree.nodes() {
        let where_clause = match node {
            NodeRef::UpdateStmt(update) => update.where_clause.as_deref(),
            NodeRef::DeleteStmt(delete) => delete.where_clause.as_deref(),
            _ => continue,
        };
        let Some(where_clause) = where_clause.and_then(|node| node.node.as_ref()) else {
            continue;
        };
        if where_contains(where_clause.to_ref(), target) {
            return true;
        }
    }
    false
}

/// Whether `target` is a sublink of `node` at this query level (not inside
/// a nested `SELECT`, which is another level).
fn where_contains(node: NodeRef<'_>, target: *const protobuf::SubLink) -> bool {
    match node {
        NodeRef::SelectStmt(_) => false,
        NodeRef::SubLink(sublink) => {
            if std::ptr::eq(sublink, target) {
                return true;
            }
            false
        }
        _ => node
            .children()
            .into_iter()
            .any(|child| where_contains(child, target)),
    }
}

fn has_skip_locked(select: &SelectStmt) -> bool {
    select.locking_clause.iter().any(|node| {
        matches!(&node.node, Some(Node::LockingClause(locking))
            if locking.wait_policy() == LockWaitPolicy::LockWaitSkip)
    })
}

/// Whether the clause is a literal `LIMIT 0` / `FETCH FIRST 0 ROWS`.
fn limit_is_zero(select: &SelectStmt) -> bool {
    matches!(
        select.limit_count.as_deref().and_then(|node| node.node.as_ref()),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(Val::Ival(ref count)) if count.ival == 0)
    )
}

/// Whether every output row carries the same values: the target list reads
/// no column (anywhere, sublinks included) and calls nothing that may vary
/// per row. Whichever rows the plan picks, the output is then the same.
fn constant_target(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    // A set operation's rows come from its arms; there is no target list.
    if select.op() != typedpg_pg_query::protobuf::SetOperation::SetopNone
        || select.target_list.is_empty()
    {
        return false;
    }
    let mut stack: Vec<NodeRef<'_>> = select
        .target_list
        .iter()
        .filter_map(|node| node.node.as_ref().map(|inner| inner.to_ref()))
        .collect();
    while let Some(node) = stack.pop() {
        match node {
            // A star is a column reference too, and any column makes the
            // output depend on the row.
            NodeRef::ColumnRef(_) => return false,
            // An operator may be a volatile user-defined function: give up.
            NodeRef::AExpr(_) => return false,
            NodeRef::FuncCall(call) => {
                let names: Vec<&str> = call
                    .funcname
                    .iter()
                    .filter_map(|node| match &node.node {
                        Some(Node::String(string)) => Some(string.sval.as_str()),
                        _ => None,
                    })
                    .collect();
                let (schema, name) = match names.as_slice() {
                    [name] => (None, *name),
                    [schema, name] => (Some(*schema), *name),
                    _ => return false,
                };
                let procs = ctx.catalog.find_functions(schema, name);
                // Every overload counts, not just the one PG picks: a
                // volatile or set-returning candidate may make each row
                // different even when no column is read.
                if procs.is_empty()
                    || procs
                        .iter()
                        .any(|proc| proc.proretset || proc.provolatile == ProVolatile::Volatile)
                {
                    return false;
                }
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    true
}

/// The `DISTINCT ON (…)` part when it has expressions.
fn distinct_on_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    // Plain `DISTINCT` is a one-NIL list; `DISTINCT ON (…)` parses as the
    // expressions themselves.
    if select.distinct_clause.is_empty() || select.distinct_clause[0].node.is_none() {
        return None;
    }
    let first = select
        .distinct_clause
        .iter()
        .filter_map(|item| {
            item.node
                .as_ref()
                .map(|inner| inner.to_ref())
                .and_then(first_location)
        })
        .min()?;
    let distinct = ctx
        .tokens
        .iter()
        .rposition(|token| token.end <= first && is_keyword(ctx, token, "DISTINCT"))?;
    let on = ctx.tokens.get(distinct + 1)?;
    if !is_keyword(ctx, on, "ON") {
        return None;
    }
    let open = ctx.tokens.get(distinct + 2)?;
    if &ctx.sql[open.start..open.end] != "(" {
        return None;
    }
    let mut depth = 0usize;
    for token in &ctx.tokens[distinct + 2..] {
        match &ctx.sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(ctx.tokens[distinct].start..token.end);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `LIMIT …` / `FETCH …` clause, when there is one (`LIMIT ALL` and
/// `LIMIT NULL` don't pick rows).
fn limit_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let count = select.limit_count.as_deref()?;
    if is_null_literal(count) {
        return None;
    }
    let at = node_start(count)?;
    let index = ctx.tokens.iter().position(|token| token.start == at)?;
    let previous = ctx.tokens.get(index.checked_sub(1)?)?;
    if is_keyword(ctx, previous, "LIMIT") {
        return Some(previous.start..clause_value_end(ctx, at));
    }
    if is_keyword(ctx, previous, "FIRST") || is_keyword(ctx, previous, "NEXT") {
        let fetch = ctx.tokens.get(index.checked_sub(2)?)?;
        if !is_keyword(ctx, fetch, "FETCH") {
            return None;
        }
        return Some(fetch.start..fetch_end(ctx, index, at));
    }
    if is_keyword(ctx, previous, "FETCH") {
        return Some(previous.start..fetch_end(ctx, index, at));
    }
    None
}

/// The end of a `FETCH` clause: its `ONLY`, or the count when it has none.
fn fetch_end(ctx: &Ctx<'_>, index: usize, count_start: usize) -> usize {
    ctx.tokens[index + 1..]
        .iter()
        .find(|token| is_keyword(ctx, token, "ONLY"))
        .map_or_else(|| clause_value_end(ctx, count_start), |token| token.end)
}

/// The `OFFSET …` clause, when there is one that skips rows.
fn offset_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let offset = select.limit_offset.as_deref()?;
    // `OFFSET 0` is the optimization-fence idiom: it skips nothing.
    if matches!(
        offset.node.as_ref(),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(Val::Ival(ref count)) if count.ival == 0)
    ) {
        return None;
    }
    let at = node_start(offset)?;
    let index = ctx.tokens.iter().position(|token| token.start == at)?;
    let previous = ctx.tokens.get(index.checked_sub(1)?)?;
    if !is_keyword(ctx, previous, "OFFSET") {
        return None;
    }
    Some(previous.start..clause_value_end(ctx, at))
}

/// Whether the token reads `keyword`, case-insensitively.
fn is_keyword(ctx: &Ctx<'_>, token: &crate::Tok, keyword: &str) -> bool {
    ctx.sql[token.start..token.end].eq_ignore_ascii_case(keyword)
}

/// The first byte a protobuf node covers.
fn node_start(node: &protobuf::Node) -> Option<usize> {
    first_location(node.node.as_ref()?.to_ref())
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

fn is_null_literal(node: &protobuf::Node) -> bool {
    matches!(&node.node, Some(Node::AConst(value)) if value.isnull)
}

/// The end of the expression starting at `start`: the last token before the
/// next clause keyword at this parenthesis level.
fn clause_value_end(ctx: &Ctx<'_>, start: usize) -> usize {
    let mut depth = 0usize;
    let mut seen = false;
    let mut end = start;
    for token in ctx.tokens.iter().filter(|token| token.start >= start) {
        let text = &ctx.sql[token.start..token.end];
        if text == ")" {
            if depth == 0 {
                break;
            }
            depth -= 1;
        } else if text == "(" {
            depth += 1;
        } else if seen && depth == 0 && is_clause_boundary(text) {
            break;
        }
        seen = true;
        end = token.end;
    }
    end
}

fn is_clause_boundary(text: &str) -> bool {
    matches!(text, ";" | ",")
        || [
            "LIMIT",
            "OFFSET",
            "FETCH",
            "FOR",
            "UNION",
            "INTERSECT",
            "EXCEPT",
        ]
        .iter()
        .any(|keyword| text.eq_ignore_ascii_case(keyword))
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn a_limit_without_order_is_reported() {
        let db = catalog();
        let sql = "SELECT id FROM users LIMIT 10";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::LimitWithoutOrder);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert_eq!(
            findings[0].message,
            "LIMIT without ORDER BY returns arbitrary rows"
        );
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add an ORDER BY that makes the choice deterministic, or allow it with \
                 -- typedpg-lint: allow(limit_without_order)"
            )
        );
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 10");
        assert!(findings[0].fix.is_empty());
    }

    #[test]
    fn offset_and_distinct_on_without_order_are_reported() {
        let db = catalog();
        let sql = "SELECT id FROM users OFFSET 20";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "OFFSET without ORDER BY skips arbitrary rows"
        );
        assert_eq!(&sql[findings[0].span.clone()], "OFFSET 20");

        let sql = "SELECT DISTINCT ON (name) id FROM users";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "DISTINCT ON without ORDER BY keeps an arbitrary row of each group"
        );
        assert_eq!(&sql[findings[0].span.clone()], "DISTINCT ON (name)");
    }

    #[test]
    fn a_set_operations_own_limit_is_reported() {
        let db = catalog();
        let sql = "SELECT id FROM users UNION SELECT user_id FROM posts LIMIT 5";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::LimitWithoutOrder);
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 5");
    }

    #[test]
    fn a_nested_select_is_reported() {
        let db = catalog();
        let sql = "SELECT (SELECT id FROM users LIMIT 1) AS id";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::LimitWithoutOrder);
    }

    #[test]
    fn an_order_by_settles_the_choice() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY id LIMIT 10",
            "SELECT id FROM users ORDER BY id OFFSET 10",
            "SELECT DISTINCT ON (name) id FROM users ORDER BY name, id",
        ] {
            assert!(lint(&db, sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn a_query_with_one_row_needs_no_order() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id = $id LIMIT 10",
            "SELECT count(*) FROM users LIMIT 10",
            "SELECT 1 LIMIT 10",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn exists_and_dml_batching_subqueries_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM posts LIMIT 5)",
            "UPDATE users SET name = 'x' WHERE id IN (SELECT user_id FROM posts LIMIT 1000)",
            "DELETE FROM users WHERE id IN (SELECT user_id FROM posts LIMIT 1000)",
            "DELETE FROM users WHERE id = ANY (SELECT user_id FROM posts LIMIT 1000)",
            "DELETE FROM users WHERE id > ALL (SELECT user_id FROM posts LIMIT 1000)",
            "SELECT id FROM users LIMIT 5 FOR UPDATE SKIP LOCKED",
            "SELECT id FROM users LIMIT 0",
            "SELECT id FROM users LIMIT ALL",
            "SELECT id FROM users LIMIT NULL",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn an_in_sublink_outside_a_dml_where_is_reported() {
        let db = catalog();
        for sql in [
            "SELECT 1 WHERE 1 IN (SELECT id FROM users LIMIT 1)",
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts LIMIT 1000)",
            "SELECT id FROM users WHERE id = ANY (SELECT user_id FROM posts LIMIT 1000)",
            "SELECT id FROM users WHERE id > ALL (SELECT user_id FROM posts LIMIT 1000)",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_constant_target_makes_the_choice_irrelevant() {
        let db = catalog();
        for sql in [
            "SELECT 1 AS one FROM users LIMIT 1",
            "SELECT true FROM users LIMIT 1",
            "SELECT 1 FROM users OFFSET 10",
            // A call that is stable within the statement keeps the output
            // the same too.
            "SELECT now() FROM users LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for sql in [
            "SELECT id FROM users LIMIT 1",
            "SELECT name FROM users OFFSET 10",
            // A set-returning call returns a different value per row.
            "SELECT generate_series(1, 3) FROM users LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}");
        }
    }

    #[test]
    fn an_offset_zero_is_not_reported() {
        let db = catalog();
        assert!(
            lint(&db, "SELECT id FROM users OFFSET 0")
                .into_iter()
                .all(|finding| finding.rule != Rule::LimitWithoutOrder)
        );
        // Next to a LIMIT, the LIMIT is still reported.
        let sql = "SELECT id FROM users LIMIT 1 OFFSET 0";
        let findings: Vec<Finding> = lint(&db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
            .collect();
        assert_eq!(findings.len(), 1);
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 1");
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(limit_without_order)\nSELECT id FROM users LIMIT 10";
        assert!(lint(&db, sql).is_empty());
    }
}
