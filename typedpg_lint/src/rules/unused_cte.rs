//! The `unused_cte` rule: reports a `WITH` query nothing references. A
//! `SELECT` (or `VALUES`) one is never run — the planner skips a `WITH`
//! query no one reads — so it is dead text, or the query meant to read it
//! and doesn't. A data-modifying one (`INSERT`, `UPDATE`, `DELETE`,
//! `MERGE`) runs whether or not it is read, which is how it is used: it is
//! not reported.
//!
//! Any table reference by its name anywhere in the statement counts as a
//! use, even one a nested `WITH` of the same name may answer, and a
//! recursive query's reference to itself too: only a name that appears
//! nowhere is reported.
//!
//! The fix removes the item, and the `WITH` keyword with it when it was the
//! only one. Removing it can't change what runs; it is withheld anyway when
//! the body binds a parameter (the statement's parameters would change),
//! calls a function that may be volatile, or has a `SEARCH` / `CYCLE`
//! clause.

use typedpg_analyzer::ProVolatile;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{CommonTableExpr, Node as N, WithClause};

use super::location;
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::WithClause(with) = node {
            with_clause(ctx, with, &mut out);
        }
    }
    out
}

fn with_clause(ctx: &Ctx<'_>, with: &WithClause, out: &mut Vec<RawFinding>) {
    let ctes: Vec<&CommonTableExpr> = with
        .ctes
        .iter()
        .filter_map(|item| match item {
            Some(N::CommonTableExpr(cte)) => Some(&**cte),
            _ => None,
        })
        .collect();
    for (index, cte) in ctes.iter().enumerate() {
        if !matches!(cte.ctequery.as_ref(), Some(N::SelectStmt(_))) || referenced(ctx, &cte.ctename)
        {
            continue;
        }
        let Some(start) = location(cte.location) else {
            continue;
        };
        let Some(name) = ctx.tokens.iter().find(|token| token.start == start) else {
            continue;
        };
        let fix = if cte.search_clause.is_none() && cte.cycle_clause.is_none() && pure(ctx, cte) {
            removal(ctx, with, &ctes, index)
        } else {
            None
        };
        out.push(RawFinding {
            span: name.start..name.end,
            message: format!(
                "WITH query {} is never referenced",
                &ctx.sql[name.start..name.end]
            ),
            help: Some("remove it, or read it where it was meant to be".to_owned()),
            fix: fix.into_iter().collect(),
        });
    }
}

/// Whether a table reference anywhere in the statement names `name`.
fn referenced(ctx: &Ctx<'_>, name: &str) -> bool {
    ctx.tree.nodes().any(|(node, _)| {
        matches!(node, NodeRef::RangeVar(range)
            if range.schemaname.is_empty() && range.relname == name)
    })
}

/// Whether `cte`'s body binds no parameter and calls only functions whose
/// every overload of that name is not volatile.
fn pure(ctx: &Ctx<'_>, cte: &CommonTableExpr) -> bool {
    let Some(body) = cte.ctequery.as_ref() else {
        return false;
    };
    body.nodes().all(|(node, _)| match node {
        NodeRef::ParamRef(_) => false,
        NodeRef::FuncCall(call) => {
            let names: Vec<&str> = call
                .funcname
                .iter()
                .filter_map(|n| match n {
                    Some(N::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                })
                .collect();
            let (schema, name) = match names.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => return false,
            };
            let overloads = ctx.catalog.find_functions(schema, name);
            !overloads.is_empty()
                && overloads
                    .iter()
                    .all(|proc| proc.provolatile != ProVolatile::Volatile)
        }
        _ => true,
    })
}

/// The edit removing item `index` of `ctes`, with its comma, or the whole
/// `WITH` when it is the only item.
fn removal(
    ctx: &Ctx<'_>,
    with: &WithClause,
    ctes: &[&CommonTableExpr],
    index: usize,
) -> Option<SqlEdit> {
    let start = location(ctes[index].location)?;
    let end = item_end(ctx, start)?;
    if ctes.len() == 1 {
        // `WITH [RECURSIVE] x AS (…) SELECT …`: up to the statement.
        let with_start = location(with.location)?;
        let next = ctx.tokens.iter().find(|token| token.start >= end)?;
        return Some(SqlEdit {
            range: with_start..next.start,
            text: String::new(),
        });
    }
    if let Some(next) = ctes.get(index + 1) {
        return Some(SqlEdit {
            range: start..location(next.location)?,
            text: String::new(),
        });
    }
    let comma = ctx.tokens.iter().rposition(|token| token.end <= start)?;
    if &ctx.sql[ctx.tokens[comma].start..ctx.tokens[comma].end] != "," {
        return None;
    }
    let before = ctx.tokens.get(comma.checked_sub(1)?)?;
    Some(SqlEdit {
        range: before.end..end,
        text: String::new(),
    })
}

/// The end of the `WITH` item starting at byte `start`: the `)` closing
/// the body after its `AS [[NOT] MATERIALIZED]`.
fn item_end(ctx: &Ctx<'_>, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut after_as = false;
    let mut body = false;
    for token in ctx.tokens.iter().filter(|token| token.start >= start) {
        let text = &ctx.sql[token.start..token.end];
        if body {
            match text {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(token.end);
                    }
                }
                _ => {}
            }
            continue;
        }
        match text {
            "(" if after_as => {
                body = true;
                depth = 1;
            }
            "(" => depth += 1,
            ")" => depth = depth.checked_sub(1)?,
            _ if depth == 0 && text.eq_ignore_ascii_case("AS") => after_as = true,
            _ if after_as
                && (text.eq_ignore_ascii_case("NOT")
                    || text.eq_ignore_ascii_case("MATERIALIZED")) => {}
            _ if after_as => return None,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::UnusedCte;

    #[test]
    fn unused_with_queries_are_removed() {
        let db = catalog();
        for (sql, expected) in [
            (
                "WITH x AS (SELECT id FROM users) SELECT id FROM posts",
                "SELECT id FROM posts",
            ),
            (
                "WITH x AS (SELECT id FROM users), y AS (SELECT 1) SELECT * FROM y",
                "WITH y AS (SELECT 1) SELECT * FROM y",
            ),
            (
                "WITH y AS (SELECT 1), x (a) AS MATERIALIZED (SELECT count(*) FROM users) \
                 SELECT * FROM y",
                "WITH y AS (SELECT 1) SELECT * FROM y",
            ),
            (
                "WITH RECURSIVE x AS (SELECT 1) SELECT id FROM users",
                "SELECT id FROM users",
            ),
            (
                "SELECT (WITH x AS (SELECT 1) SELECT 2) AS n",
                "SELECT (SELECT 2) AS n",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], "x", "{sql}");
            assert_eq!(findings[0].message, "WITH query x is never referenced");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn what_has_no_fix() {
        let db = catalog();
        for sql in [
            // A parameter, a volatile function.
            "WITH x AS (SELECT $1::int) SELECT id FROM users",
            "WITH x AS (SELECT random()) SELECT id FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn referenced_or_data_modifying_queries_are_not_reported() {
        let db = catalog();
        for sql in [
            "WITH x AS (SELECT id FROM users) SELECT id FROM x",
            "WITH x AS (SELECT id FROM users), y AS (SELECT id FROM x) SELECT id FROM y",
            "WITH x AS (SELECT id FROM users) SELECT id FROM posts WHERE user_id IN \
             (SELECT id FROM x)",
            "WITH d AS (DELETE FROM posts WHERE id = 1 RETURNING id) SELECT 1",
            "WITH RECURSIVE r (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) \
             SELECT n FROM r",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
