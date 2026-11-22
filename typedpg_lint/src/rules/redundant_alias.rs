//! The `redundant_alias` rule: reports an output column alias — of a
//! select list or a `RETURNING` list — that is the name PostgreSQL gives
//! the column without it (`FigureColname`): `SELECT name AS name`,
//! `SELECT u.id AS id`, `SELECT count(*) AS count`. The fix removes the
//! alias. Only names the analyzer derives exactly as PostgreSQL does are
//! compared, and the alias's spelling must read back as the name (`AS "Id"`
//! is not `id`).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AExprKind, MinMaxOp, Node, Node as N, ResTarget, SubLinkType};

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
    if figure_colname(value).map(|(name, _)| name) != Some(target.name.as_str()) {
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

/// The last string of a name list (`pg_catalog.count` is `count`).
fn last_string(nodes: &[Option<Node>]) -> Option<&str> {
    nodes.iter().rev().find_map(|n| match n {
        Some(N::String(s)) => Some(s.sval.as_str()),
        _ => None,
    })
}

/// FigureColnameInternal (parse_target.c): the name PostgreSQL gives an
/// output column of expression `node` without an alias, and the strength
/// of that choice (2 for a column or function name, 1 for a fallback such
/// as `case`). `None` where it would be `?column?`, or where the analyzer
/// does not model the choice.
fn figure_colname(node: &Node) -> Option<(&str, u8)> {
    match node {
        N::ColumnRef(column) => match column.fields.last()? {
            Some(N::String(s)) => Some((s.sval.as_str(), 2)),
            _ => None,
        },
        N::FuncCall(call) => Some((last_string(&call.funcname)?, 2)),
        N::AExpr(expr) if expr.kind == AExprKind::AexprNullif => Some(("nullif", 2)),
        N::CoalesceExpr(_) => Some(("coalesce", 2)),
        N::MinMaxExpr(expr) => Some((
            match expr.op {
                MinMaxOp::IsGreatest => "greatest",
                MinMaxOp::IsLeast => "least",
            },
            2,
        )),
        N::AArrayExpr(_) => Some(("array", 2)),
        N::RowExpr(_) => Some(("row", 2)),
        N::SubLink(link) => match link.sub_link_type {
            SubLinkType::ExistsSublink => Some(("exists", 2)),
            SubLinkType::ArraySublink => Some(("array", 2)),
            _ => None,
        },
        N::CaseExpr(_) => Some(("case", 1)),
        N::TypeCast(cast) => match cast.arg.as_ref().and_then(figure_colname) {
            Some((name, 2)) => Some((name, 2)),
            _ => Some((last_string(&cast.type_name.as_ref()?.names)?, 1)),
        },
        _ => None,
    }
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
            // trim() is btrim(): its column is `btrim`.
            "SELECT trim(name) AS trim FROM users",
            "UPDATE users SET name = 'x' WHERE id = 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
