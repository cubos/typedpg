//! The `scalar_subquery_may_return_many` rule: reports a subquery used as
//! a value — `(SELECT x FROM t WHERE …)` in a select list, a comparison,
//! a `SET`, a function argument, `(a, b) = (SELECT …)` — that may return
//! more than one row. PostgreSQL then fails the statement at run time with
//! `more than one row returned by a subquery used as an expression`.
//!
//! The subquery is fine when [`typedpg_analyzer::select_at_most_one_row`]
//! proves it returns at most one row (an aggregate without `GROUP BY`, a
//! unique key pinned by its `WHERE`, outer references included, …), or when
//! its own `LIMIT` is `0` or `1`. `IN`, `EXISTS`, `ANY` / `ALL` and
//! `ARRAY(…)` subqueries take any number of rows, and are not reported.
//!
//! The proof gives up on what it can't see through, and a finding is only
//! reported where that can't hide a single row: every `FROM` item is a
//! plain or partitioned table (not a view, a CTE, a function or a
//! subquery) whose unique indexes are all plain columns without a
//! predicate, the target list has no set-returning call, and the subquery
//! is no set operation. A `VALUES` list of two rows or more always returns
//! them all.

use std::collections::HashSet;

use typedpg_analyzer::{RelKind, select_at_most_one_row};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{
    AConstValue, LimitOption, Node as N, SelectStmt, SetOperation, SubLinkType,
};

use super::node_span;
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    // A spread's sample row is not the query the user runs.
    if !ctx.view.analyzed.spreads.is_empty() {
        return Vec::new();
    }
    let cte_names: HashSet<&str> = ctx
        .tree
        .nodes()
        .filter_map(|(node, _)| match node {
            NodeRef::CommonTableExpr(cte) => Some(cte.ctename.as_str()),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    // `SET (a, b) = (SELECT …)` repeats the sublink under each column.
    let mut seen = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SubLink(sub) = node else {
            continue;
        };
        if !seen.insert(sub.location) {
            continue;
        }
        if !matches!(
            sub.sub_link_type,
            SubLinkType::ExprSublink
                | SubLinkType::MultiexprSublink
                | SubLinkType::RowcompareSublink
        ) {
            continue;
        }
        let Some(N::SelectStmt(select)) = sub.subselect.as_ref() else {
            continue;
        };
        if !may_return_many(ctx, select, &cte_names) {
            continue;
        }
        let Some(span) = node_span(ctx, node) else {
            continue;
        };
        out.push(RawFinding {
            span,
            message: "this subquery is used as a value but may return more than one row: \
                      PostgreSQL then fails with \"more than one row returned by a subquery \
                      used as an expression\""
                .to_owned(),
            help: Some(
                "make it return at most one row: filter on a unique key, aggregate, or add \
                 ORDER BY … LIMIT 1"
                    .to_owned(),
            ),
            fix: Vec::new(),
        });
    }
    out
}

/// Whether `select` may return more than one row, as far as the proof can
/// tell and only where its giving up can't hide a single row.
fn may_return_many(ctx: &Ctx<'_>, select: &SelectStmt, cte_names: &HashSet<&str>) -> bool {
    if select.op != SetOperation::SetopNone {
        return false;
    }
    if !select.values_lists.is_empty() {
        return select.values_lists.len() >= 2 && select.limit_count.is_none();
    }
    if select.limit_option == LimitOption::WithTies {
        return false;
    }
    if let Some(count) = select.limit_count.as_ref() {
        match count {
            // `LIMIT ALL` / `LIMIT NULL` limit nothing.
            N::AConst(value) if value.isnull => {}
            N::AConst(value) => match value.val {
                Some(AConstValue::Integer(ref count)) if count.ival >= 2 => {}
                _ => return false,
            },
            _ => return false,
        }
    }
    if select.from_clause.is_empty() {
        return false;
    }
    if !select
        .from_clause
        .iter()
        .all(|item| plain_tables(ctx, item.as_ref(), cte_names))
    {
        return false;
    }
    if has_set_returning_call(ctx, select) {
        return false;
    }
    if select
        .group_clause
        .iter()
        .any(|key| matches!(key, Some(N::GroupingSet(_))))
    {
        return false;
    }
    // `SELECT DISTINCT outer.x FROM t`: one value per evaluation, which the
    // proof doesn't read as fixed.
    if !select.distinct_clause.is_empty() && !reads_own_columns(ctx, select) {
        return false;
    }
    if let Some(qual) = select.where_clause.as_ref()
        && ctx.catalog.condition_constant(qual) == Some(false)
    {
        return false;
    }
    !select_at_most_one_row(ctx.catalog, ctx.view, select)
}

/// Whether FROM item `item` is a join tree of plain or partitioned tables
/// whose unique indexes are all on plain columns, without a predicate (a
/// proof over an expression or partial index would be one the analyzer
/// can't make).
fn plain_tables(ctx: &Ctx<'_>, item: Option<&N>, cte_names: &HashSet<&str>) -> bool {
    match item {
        Some(N::RangeVar(range)) => {
            if range.schemaname.is_empty() && cte_names.contains(range.relname.as_str()) {
                return false;
            }
            let schema = (!range.schemaname.is_empty()).then_some(range.schemaname.as_str());
            let Some(class) = ctx.catalog.resolve_table(schema, &range.relname) else {
                return false;
            };
            if !matches!(class.relkind, RelKind::Table | RelKind::Partitioned) {
                return false;
            }
            ctx.catalog.iter_indexes().all(|index| {
                index.indrelid != class.oid
                    || !(index.indisunique || index.indisexclusion)
                    || (index.indexprs.is_empty()
                        && index.indpred.is_none()
                        && !index.indisexclusion)
            })
        }
        Some(N::JoinExpr(join)) => {
            plain_tables(ctx, join.larg.as_ref(), cte_names)
                && plain_tables(ctx, join.rarg.as_ref(), cte_names)
        }
        _ => false,
    }
}

/// Whether the target list (or `VALUES`) calls a set-returning function,
/// outside nested subqueries.
fn has_set_returning_call(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = select.target_list.iter().flatten().map(N::to_ref).collect();
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::SubLink(_) => continue,
            NodeRef::FuncCall(call) => {
                let names: Vec<&str> = call
                    .funcname
                    .iter()
                    .filter_map(|part| match part {
                        Some(N::String(s)) => Some(s.sval.as_str()),
                        _ => None,
                    })
                    .collect();
                let (schema, name) = match names.as_slice() {
                    [name] => (None, *name),
                    [schema, name] => (Some(*schema), *name),
                    _ => return true,
                };
                if ctx
                    .catalog
                    .find_functions(schema, name)
                    .iter()
                    .any(|proc| proc.proretset)
                {
                    return true;
                }
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    false
}

/// Whether a target of `select` references a column of its own level (not
/// only outer ones).
fn reads_own_columns(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = select.target_list.iter().flatten().map(N::to_ref).collect();
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::SubLink(_) => continue,
            NodeRef::ColumnRef(column) => {
                match ctx.view.column_ref_binding(column.location) {
                    Some(binding) if binding.outer => {}
                    // Unresolved: assume it is the level's own.
                    _ => return true,
                }
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, lint_rule};

    const RULE: Rule = Rule::ScalarSubqueryMayReturnMany;

    fn db() -> typedpg_analyzer::PgCatalog {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE orders (
                id bigint PRIMARY KEY,
                user_id bigint NOT NULL REFERENCES users,
                total numeric NOT NULL,
                code text NOT NULL,
                region text NOT NULL,
                UNIQUE (region, code)
             );
             CREATE TABLE settings (key text NOT NULL, value text);
             CREATE UNIQUE INDEX settings_key ON settings (lower(key));
             CREATE VIEW user_names AS SELECT id, name FROM users;",
        )
        .unwrap();
        db
    }

    fn assert_reported(sql: &str, subquery: &str) {
        let db = db();
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], subquery, "{sql}");
        assert_eq!(findings[0].severity, crate::Severity::Warning);
    }

    fn assert_clean(sql: &str) {
        let db = db();
        let findings = lint_rule(&db, sql, RULE);
        assert!(findings.is_empty(), "{sql}: {findings:?}");
    }

    #[test]
    fn a_subquery_over_a_non_key_is_reported() {
        assert_reported(
            "SELECT u.id, (SELECT o.total FROM orders o WHERE o.user_id = u.id) AS total FROM users u",
            "(SELECT o.total FROM orders o WHERE o.user_id = u.id)",
        );
        assert_reported(
            "SELECT id FROM users WHERE id = (SELECT user_id FROM orders WHERE total > $min)",
            "(SELECT user_id FROM orders WHERE total > $min)",
        );
        assert_reported(
            "UPDATE users SET name = (SELECT title FROM posts WHERE posts.user_id = users.id) WHERE id = $id",
            "(SELECT title FROM posts WHERE posts.user_id = users.id)",
        );
        assert_reported(
            "SELECT upper((SELECT title FROM posts WHERE user_id = $u)) AS t",
            "(SELECT title FROM posts WHERE user_id = $u)",
        );
        // One column of a two-column key.
        assert_reported(
            "SELECT (SELECT id FROM orders WHERE code = $code) AS id",
            "(SELECT id FROM orders WHERE code = $code)",
        );
        // A LIMIT of two rows or more, and VALUES of several rows.
        assert_reported(
            "SELECT (SELECT id FROM orders WHERE user_id = $u LIMIT 5) AS id",
            "(SELECT id FROM orders WHERE user_id = $u LIMIT 5)",
        );
        assert_reported("SELECT (VALUES (1), (2)) AS v", "(VALUES (1), (2))");
        // An aggregate of the outer level does not collapse the subquery.
        assert_reported(
            "SELECT (SELECT max(u.id) FROM posts) AS m FROM users u",
            "(SELECT max(u.id) FROM posts)",
        );
        // GROUP BY a column that is not pinned: one row per group.
        assert_reported(
            "SELECT (SELECT count(*) FROM orders WHERE user_id = $u GROUP BY region) AS n",
            "(SELECT count(*) FROM orders WHERE user_id = $u GROUP BY region)",
        );
    }

    #[test]
    fn a_row_comparison_and_multi_assignment_are_reported() {
        assert_reported(
            "SELECT id FROM users WHERE (id, name) = (SELECT user_id, title FROM posts WHERE title = $t)",
            "(SELECT user_id, title FROM posts WHERE title = $t)",
        );
        assert_reported(
            "UPDATE users SET (name, code) = (SELECT title, 'abc' FROM posts WHERE posts.user_id = users.id) WHERE id = $id",
            "(SELECT title, 'abc' FROM posts WHERE posts.user_id = users.id)",
        );
    }

    #[test]
    fn a_subquery_proven_one_row_is_fine() {
        for sql in [
            // A key, a correlated key, a composite key.
            "SELECT (SELECT name FROM users WHERE id = $id) AS name",
            "SELECT p.id, (SELECT u.name FROM users u WHERE u.id = p.user_id) AS name FROM posts p",
            "SELECT (SELECT id FROM orders WHERE region = $r AND code = $c) AS id",
            "SELECT (SELECT name FROM users WHERE email = $e) AS name",
            // Aggregates without GROUP BY, GROUP BY a pinned value.
            "SELECT u.id, (SELECT count(*) FROM posts p WHERE p.user_id = u.id) AS n FROM users u",
            "SELECT (SELECT sum(total) FROM orders) AS s",
            "SELECT (SELECT count(*) FROM orders WHERE region = $r GROUP BY region) AS n",
            // LIMIT 1, FETCH FIRST 1 ROW ONLY, LIMIT 0.
            "SELECT (SELECT title FROM posts WHERE user_id = $u ORDER BY id DESC LIMIT 1) AS t",
            "SELECT (SELECT title FROM posts ORDER BY id FETCH FIRST 1 ROW ONLY) AS t",
            "SELECT (SELECT title FROM posts LIMIT 0) AS t",
            // No FROM.
            "SELECT (SELECT 1) AS one",
            "SELECT (VALUES (1)) AS one",
            // DISTINCT over a value fixed per evaluation.
            "SELECT (SELECT DISTINCT u.name FROM posts p WHERE p.user_id = u.id) AS n FROM users u",
        ] {
            assert_clean(sql);
        }
    }

    #[test]
    fn subqueries_taking_any_number_of_rows_are_fine() {
        for sql in [
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts)",
            "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)",
            "SELECT id FROM users WHERE id = ANY (SELECT user_id FROM posts)",
            "SELECT id FROM users WHERE id > ALL (SELECT user_id FROM posts)",
            "SELECT ARRAY(SELECT title FROM posts WHERE user_id = u.id) AS titles FROM users u",
            "SELECT u.id, t.title FROM users u CROSS JOIN LATERAL (SELECT title FROM posts p WHERE p.user_id = u.id) t",
        ] {
            assert_clean(sql);
        }
    }

    #[test]
    fn what_the_proof_cannot_see_through_is_not_reported() {
        for sql in [
            // A view, a CTE, a function, a FROM subquery.
            "SELECT (SELECT name FROM user_names WHERE id = $id) AS name",
            "WITH one AS (SELECT id FROM users WHERE id = $id) SELECT (SELECT id FROM one) AS id",
            "SELECT (SELECT x FROM generate_series(1, 1) x) AS x",
            "SELECT (SELECT s.id FROM (SELECT id FROM users) s WHERE s.id = $id) AS id",
            // A table with an expression unique index.
            "SELECT (SELECT value FROM settings WHERE lower(key) = $k) AS v",
            // A set-returning target, a set operation, a parameter LIMIT.
            "SELECT (SELECT unnest(ARRAY[id]) FROM users WHERE id = $id) AS x",
            "SELECT (SELECT id FROM users WHERE id = 1 UNION SELECT id FROM users WHERE id = 2) AS x",
            "SELECT (SELECT id FROM posts LIMIT $n) AS x",
            // A WHERE that is never true.
            "SELECT (SELECT id FROM posts WHERE false) AS x",
        ] {
            assert_clean(sql);
        }
    }

    #[test]
    fn a_partitioned_table_key_is_one_row() {
        let mut db = db();
        db.apply_sql(
            "CREATE TABLE measures (id bigint, day date, v int, PRIMARY KEY (id, day))
                 PARTITION BY RANGE (day);
             CREATE TABLE measures_2024 PARTITION OF measures
                 FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');",
        )
        .unwrap();
        let clean = "SELECT (SELECT v FROM measures WHERE id = $id AND day = $day) AS v";
        assert!(lint_rule(&db, clean, RULE).is_empty());
        let many = "SELECT (SELECT v FROM measures WHERE id = $id) AS v";
        assert_eq!(lint_rule(&db, many, RULE).len(), 1);
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        assert_clean(
            "-- typedpg-lint: allow(scalar_subquery_may_return_many)\n\
             SELECT (SELECT id FROM posts WHERE title = $t) AS id",
        );
    }
}
