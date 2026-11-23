//! The `redundant_alias` rule: reports an output column alias — of a
//! select list or a `RETURNING` list — that is the name PostgreSQL gives
//! the column without it (`FigureColname`): `SELECT name AS name`,
//! `SELECT u.id AS id`, `SELECT count(*) AS count`. The fix removes the
//! alias. Only names the analyzer derives exactly as PostgreSQL does are
//! compared, and the alias's spelling must read back as the name (`AS "Id"`
//! is not `id`).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{Node as N, ResTarget};

use super::{leftmost_start, scan_until, starts_clause};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let targets = match node {
            NodeRef::SelectStmt(sel) => &sel.target_list,
            NodeRef::InsertStmt(stmt) => match stmt.returning_clause.as_ref() {
                Some(returning) => &returning.exprs,
                None => continue,
            },
            NodeRef::UpdateStmt(stmt) => match stmt.returning_clause.as_ref() {
                Some(returning) => &returning.exprs,
                None => continue,
            },
            NodeRef::DeleteStmt(stmt) => match stmt.returning_clause.as_ref() {
                Some(returning) => &returning.exprs,
                None => continue,
            },
            NodeRef::MergeStmt(stmt) => match stmt.returning_clause.as_ref() {
                Some(returning) => &returning.exprs,
                None => continue,
            },
            _ => continue,
        };
        for target in targets {
            if let Some(N::ResTarget(target)) = target {
                alias(ctx, target, &mut out);
            }
        }
    }
    out
}

fn alias(ctx: &Ctx<'_>, target: &ResTarget, out: &mut Vec<RawFinding>) {
    if target.name.is_empty() {
        return;
    }
    let Some(value) = target.val.as_ref() else {
        return;
    };
    // `?column?` is no name the query chose: an alias spelling it stays.
    if target.name == "?column?"
        || typedpg_analyzer::certain_colname(Some(value)).as_deref() != Some(target.name.as_str())
    {
        return;
    }
    let Some(start) = leftmost_start(value) else {
        return;
    };
    // The alias is the last token of the item; `AS` may precede it.
    let (_, stop) = scan_until(ctx, start, |text| text == "," || starts_clause(text));
    let last = match stop {
        Some(index) => index.checked_sub(1),
        None => ctx.tokens.len().checked_sub(1),
    };
    let Some(alias_index) = last.filter(|&index| ctx.tokens[index].start > start) else {
        return;
    };
    let alias_token = ctx.tokens[alias_index];
    if identifier_value(&ctx.sql[alias_token.start..alias_token.end]).as_deref()
        != Some(target.name.as_str())
    {
        return;
    }
    let as_index = alias_index.checked_sub(1).filter(|&index| {
        ctx.sql[ctx.tokens[index].start..ctx.tokens[index].end].eq_ignore_ascii_case("AS")
    });
    let first = as_index.unwrap_or(alias_index);
    let Some(before) = first.checked_sub(1).map(|index| ctx.tokens[index]) else {
        return;
    };
    // The expression keeps a token of its own.
    if before.start < start {
        return;
    }
    let span = ctx.tokens[first].start..alias_token.end;
    out.push(RawFinding {
        span,
        message: format!(
            "alias {} is the name the column already has",
            &ctx.sql[alias_token.start..alias_token.end]
        ),
        help: Some("remove the alias".to_owned()),
        fix: vec![SqlEdit {
            range: before.end..alias_token.end,
            text: String::new(),
        }],
    });
}

/// The name an identifier token reads as: a quoted one unescaped, a bare
/// one folded to lower case. `None` for anything else.
fn identifier_value(token: &str) -> Option<String> {
    if let Some(inner) = token.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        return Some(inner.replace("\"\"", "\""));
    }
    token
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        .then(|| token.to_lowercase())
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::RedundantAlias;

    #[test]
    fn aliases_naming_the_column_its_own_name_are_removed() {
        let db = catalog();
        for (sql, expected) in [
            ("SELECT name AS name FROM users", "SELECT name FROM users"),
            (
                "SELECT u.id AS id, name FROM users u",
                "SELECT u.id, name FROM users u",
            ),
            ("SELECT name name FROM users", "SELECT name FROM users"),
            (
                "SELECT name AS \"name\" FROM users",
                "SELECT name FROM users",
            ),
            (
                "SELECT count(*) AS count FROM users",
                "SELECT count(*) FROM users",
            ),
            (
                "SELECT id, coalesce(name, '') AS coalesce FROM users",
                "SELECT id, coalesce(name, '') FROM users",
            ),
            (
                "SELECT CASE WHEN id > 1 THEN 1 END AS \"case\" FROM users",
                "SELECT CASE WHEN id > 1 THEN 1 END FROM users",
            ),
            (
                "SELECT id::int AS id FROM users",
                "SELECT id::int FROM users",
            ),
            // A cast is named after a column it reads, a `CASE` after an
            // `ELSE` that has a name of its own.
            (
                "SELECT name::text AS name FROM users",
                "SELECT name::text FROM users",
            ),
            (
                "SELECT CASE WHEN id > 1 THEN 'x' ELSE name END AS name FROM users",
                "SELECT CASE WHEN id > 1 THEN 'x' ELSE name END FROM users",
            ),
            ("SELECT 1::int AS int4", "SELECT 1::int"),
            ("SELECT current_date AS current_date", "SELECT current_date"),
            (
                "SELECT x.id FROM (SELECT id AS id FROM users) x",
                "SELECT x.id FROM (SELECT id FROM users) x",
            ),
            (
                "UPDATE users SET name = 'x' WHERE id = 1 RETURNING id AS id",
                "UPDATE users SET name = 'x' WHERE id = 1 RETURNING id",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn aliases_that_rename_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT name AS full_name FROM users",
            "SELECT name AS \"Name\" FROM users",
            "SELECT 1 AS \"?column?\"",
            "SELECT id + 1 AS id FROM users",
            // PG names it `id`, after its ELSE.
            "SELECT CASE WHEN id > 1 THEN 1 ELSE id END AS \"case\" FROM users",
            // PG names it after the subquery's own target, `?column?`.
            "SELECT (SELECT 1)::int AS int4",
            // trim() is btrim(): its column is `btrim`.
            "SELECT trim(name) AS trim FROM users",
            "UPDATE users SET name = 'x' WHERE id = 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
