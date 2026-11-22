//! The `count_not_null` rule: reports `count(expr)` over a value the
//! analyzer proves is never NULL — a NOT NULL column of a table no outer
//! join null-extends, a literal (`count(1)`), a column the call's own
//! `FILTER` proves non-NULL — which counts every row, as
//! `count(*)` does more plainly (and without evaluating `expr`).
//!
//! Only `pg_catalog.count("any")` is judged, without `DISTINCT` (which
//! counts distinct values) and not over a row or composite value; a
//! parameter in the argument may be NULL. Every inference of the call
//! must prove the argument non-NULL; one that doesn't see a `WHERE`'s
//! facts makes the rule miss `count(x) … WHERE x IS NOT NULL`. The fix
//! rewrites the argument to `*` — `FILTER` and `OVER` stay — when it is a
//! plain column or a constant, whose evaluation can't raise or have an
//! effect.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{FuncCall, Node};

use super::location;
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::FuncCall(call) = node else {
            continue;
        };
        if call.agg_star || call.agg_distinct || call.args.len() != 1 || !call.agg_order.is_empty()
        {
            continue;
        }
        if ctx.view.count_arg_nullable(call.location) != Some(false)
            || !own_level_aggregate(ctx, call.args[0].as_ref())
        {
            continue;
        }
        let Some((open, close)) = argument_parens(ctx, call) else {
            continue;
        };
        let start = location(call.location).unwrap_or(open);
        let argument = ctx.sql[open + 1..close].trim();
        let fix = if plain_argument(call.args[0].as_ref()) {
            vec![SqlEdit {
                range: open + 1..close,
                text: "*".to_owned(),
            }]
        } else {
            Vec::new()
        };
        out.push(RawFinding {
            span: start..close + 1,
            message: format!("count({argument}) counts every row: its argument is never NULL here"),
            help: Some("use count(*)".to_owned()),
            fix,
        });
    }
    out
}

/// Whether an aggregate over `argument` belongs to the query level it is
/// written in, as `count(*)` there does: PostgreSQL evaluates an aggregate
/// whose argument reads only outer columns in that outer query
/// (`agglevelsup`), so `count(outer.id)` in a subquery counts the outer
/// rows. An argument without columns is the call's own level's; a column
/// this can't place gives up.
fn own_level_aggregate(ctx: &Ctx<'_>, argument: Option<&Node>) -> bool {
    let Some(argument) = argument else {
        return false;
    };
    let mut columns = false;
    let mut own = false;
    for (node, _) in argument.nodes() {
        if let NodeRef::ColumnRef(column) = node {
            columns = true;
            match ctx.view.column_ref_binding(column.location) {
                Some(binding) => own |= !binding.outer,
                None => return false,
            }
        }
    }
    !columns || own
}

/// The byte offsets of the `(` and `)` around the call's arguments.
fn argument_parens(ctx: &Ctx<'_>, call: &FuncCall) -> Option<(usize, usize)> {
    let start = location(call.location)?;
    let first = ctx.tokens.iter().position(|token| token.start >= start)?;
    let open = (first..ctx.tokens.len())
        .find(|&i| &ctx.sql[ctx.tokens[i].start..ctx.tokens[i].end] == "(")?;
    let mut depth = 0usize;
    for token in &ctx.tokens[open..] {
        match &ctx.sql[token.start..token.end] {
            "(" | "[" => depth += 1,
            ")" | "]" => {
                depth -= 1;
                if depth == 0 {
                    return Some((ctx.tokens[open].start, token.start));
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether evaluating `node` can't raise or have an effect: a plain column
/// or a constant, possibly cast to its own written type.
fn plain_argument(node: Option<&Node>) -> bool {
    match node {
        Some(Node::ColumnRef(column)) => column
            .fields
            .iter()
            .all(|field| matches!(field, Some(Node::String(_)))),
        Some(Node::AConst(value)) => !value.isnull,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{fixed, lint_rule};

    const RULE: Rule = Rule::CountNotNull;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (id bigint PRIMARY KEY, name text, email text NOT NULL);
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    title text
                 );
                 CREATE TYPE pair AS (a int, b int);
                 CREATE TABLE pairs (id int PRIMARY KEY, p pair NOT NULL);",
            )
            .unwrap();
        catalog
    }

    #[track_caller]
    fn check(db: &PgCatalog, sql: &str, expected: &str) {
        let findings = lint_rule(db, sql, RULE);
        assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        assert_eq!(fixed(sql, &findings), expected, "{sql}");
    }

    #[test]
    fn a_count_of_a_never_null_value_is_reported() {
        let db = catalog();
        check(
            &db,
            "SELECT count(id) FROM users",
            "SELECT count(*) FROM users",
        );
        check(
            &db,
            "SELECT count(1) FROM users",
            "SELECT count(*) FROM users",
        );
        check(
            &db,
            "SELECT COUNT( email ) FROM users",
            "SELECT COUNT(*) FROM users",
        );
        check(
            &db,
            "SELECT pg_catalog.count(u.id) FILTER (WHERE u.name > 'a') FROM users u",
            "SELECT pg_catalog.count(*) FILTER (WHERE u.name > 'a') FROM users u",
        );
        check(
            &db,
            "SELECT count(name) FILTER (WHERE name IS NOT NULL) FROM users",
            "SELECT count(*) FILTER (WHERE name IS NOT NULL) FROM users",
        );
        check(
            &db,
            "SELECT user_id, count(p.id) OVER (PARTITION BY user_id) FROM posts p",
            "SELECT user_id, count(*) OVER (PARTITION BY user_id) FROM posts p",
        );
        // The preserved side of an outer join.
        check(
            &db,
            "SELECT count(u.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id",
            "SELECT count(*) FROM users u LEFT JOIN posts p ON p.user_id = u.id",
        );
    }

    #[test]
    fn an_argument_that_may_raise_keeps_its_call() {
        let db = catalog();
        let sql = "SELECT count(id / 2) FROM users";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty());
    }

    #[test]
    fn a_count_that_may_skip_rows_is_kept() {
        let db = catalog();
        for sql in [
            "SELECT count(name) FROM users",
            "SELECT count(*) FROM users",
            "SELECT count(DISTINCT id) FROM users",
            // Null-extended by the outer join.
            "SELECT count(p.id) FROM users u LEFT JOIN posts p ON p.user_id = u.id",
            "SELECT count($1::int) FROM users",
            "SELECT count(NULL::int) FROM users",
            "SELECT count(p) FROM pairs",
            "SELECT count(title) FROM posts",
            // An outer aggregate: it counts the users, not the posts.
            "SELECT u.id, (SELECT count(u.id) FROM posts p WHERE p.id > 0) FROM users u \
             GROUP BY u.id",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
