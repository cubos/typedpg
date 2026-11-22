//! The `insert_without_columns` rule: reports an `INSERT INTO t VALUES …`
//! or `INSERT INTO t SELECT …` without a column list. Its values go to the
//! table's columns by position, so the statement depends on the column
//! order: a column added, or a table recreated in another order, silently
//! moves the values (or fails only when the types disagree).
//!
//! The fix writes the column list the statement targets now. Without one,
//! PostgreSQL (`checkInsertTargets`) targets every column in `attnum` order,
//! dropped ones excluded, and the N values of each row go to the first N of
//! them — generated and identity columns included, which then only accept
//! `DEFAULT`, as they do when listed. So the list is the table's first N
//! columns. It is withheld when the number of values isn't plain from the
//! text (a `*` or a `(row).*` in the select list) or the table isn't
//! resolved. `INSERT … DEFAULT VALUES` names no value and is not reported.

use typedpg_core::quote_identifier;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{InsertStmt, Node as N, SelectStmt, SetOperation};

use super::{location, relation_end, table_name};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::InsertStmt(stmt) = node
            && stmt.cols.is_empty()
            && let Some(N::SelectStmt(select)) = stmt.select_stmt.as_ref()
        {
            insert(ctx, stmt, select, &mut out);
        }
    }
    out
}

fn insert(ctx: &Ctx<'_>, stmt: &InsertStmt, select: &SelectStmt, out: &mut Vec<RawFinding>) {
    let Some(relation) = stmt.relation.as_deref() else {
        return;
    };
    let Some(relation_start) = location(relation.location) else {
        return;
    };
    let start = ctx
        .tokens
        .iter()
        .filter(|token| token.end <= relation_start)
        .rfind(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case("INSERT"))
        .map_or(relation_start, |token| token.start);
    let Some(relation_end) = relation_end(ctx, relation) else {
        return;
    };
    // The column list goes after the table and its alias.
    let mut insert_at = relation_end;
    if relation.alias.is_some() {
        let mut after = ctx
            .tokens
            .iter()
            .filter(|token| token.start >= relation_end);
        let mut alias = after.next();
        if alias.is_some_and(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case("AS")) {
            alias = after.next();
        }
        match alias {
            Some(token) => insert_at = token.end,
            None => return,
        }
    }
    let fix = column_list(ctx, relation, select)
        .map(|columns| {
            vec![SqlEdit {
                range: insert_at..insert_at,
                text: format!(" ({})", columns.join(", ")),
            }]
        })
        .unwrap_or_default();
    out.push(RawFinding {
        span: start..relation_end,
        message: format!(
            "INSERT into {} without a column list depends on the table's column order",
            table_name(relation)
        ),
        help: Some("list the columns the values go to".to_owned()),
        fix,
    });
}

/// The columns the statement's values go to, quoted: the table's first N,
/// for N values per row.
fn column_list(
    ctx: &Ctx<'_>,
    relation: &typedpg_pg_query::ast::RangeVar,
    select: &SelectStmt,
) -> Option<Vec<String>> {
    let width = width(select)?;
    // The table as the analyzer resolved the statement's target.
    let Some(typedpg_analyzer::RelationRef::Relation(oid)) = ctx.view.relation_of(relation) else {
        return None;
    };
    let columns: Vec<String> = ctx
        .catalog
        .attributes_of(oid)
        .iter()
        .filter(|attribute| attribute.attnum > 0)
        .take(width)
        .map(|attribute| quote_identifier(&attribute.attname))
        .collect();
    (columns.len() == width && width > 0).then_some(columns)
}

/// The number of values each row of `select` has, when the text says it:
/// a `VALUES` row's length, or a select list without `*`.
pub(crate) fn width(select: &SelectStmt) -> Option<usize> {
    if select.op != SetOperation::SetopNone {
        return width(select.larg.as_deref()?);
    }
    if let Some(Some(N::List(row))) = select.values_lists.first() {
        return Some(row.items.len());
    }
    let mut count = 0;
    for target in &select.target_list {
        let Some(N::ResTarget(target)) = target else {
            return None;
        };
        let expands = match target.val.as_ref()? {
            N::ColumnRef(column) => matches!(column.fields.last(), Some(Some(N::AStar(_)))),
            N::AIndirection(indirection) => {
                matches!(indirection.indirection.last(), Some(Some(N::AStar(_))))
            }
            _ => false,
        };
        if expands {
            return None;
        }
        count += 1;
    }
    Some(count)
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::InsertWithoutColumns;

    #[test]
    fn the_targeted_columns_are_listed() {
        let db = catalog();
        for (sql, expected) in [
            (
                "INSERT INTO users VALUES (1, 'a', 'a@x', 'abc')",
                "INSERT INTO users (id, name, email, code) VALUES (1, 'a', 'a@x', 'abc')",
            ),
            (
                // The first N columns: the rest take their defaults.
                "INSERT INTO users VALUES (1, 'a', 'a@x')",
                "INSERT INTO users (id, name, email) VALUES (1, 'a', 'a@x')",
            ),
            (
                "INSERT INTO public.posts AS p SELECT 1, 1, 'x' RETURNING p.id",
                "INSERT INTO public.posts AS p (id, user_id, title) SELECT 1, 1, 'x' RETURNING p.id",
            ),
            (
                "INSERT INTO posts SELECT id, id, email FROM users UNION ALL SELECT 2, 1, 'y'",
                "INSERT INTO posts (id, user_id, title) SELECT id, id, email FROM users UNION ALL \
                 SELECT 2, 1, 'y'",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn generated_and_quoted_columns() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE t (
                id int GENERATED ALWAYS AS IDENTITY,
                \"Name\" text,
                doubled int GENERATED ALWAYS AS (id * 2) STORED,
                note text
             )",
        )
        .unwrap();
        let sql = "INSERT INTO t VALUES (DEFAULT, 'a', DEFAULT, 'n')";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            fixed(sql, &findings),
            "INSERT INTO t (id, \"Name\", doubled, note) VALUES (DEFAULT, 'a', DEFAULT, 'n')"
        );
    }

    #[test]
    fn what_has_no_fix() {
        let db = catalog();
        let sql = "INSERT INTO posts SELECT * FROM posts";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "INSERT INTO posts");
        assert!(findings[0].fix.is_empty());
    }

    #[test]
    fn inserts_with_a_column_list_are_not_reported() {
        let db = catalog();
        for sql in [
            "INSERT INTO users (id, email) VALUES (1, 'a@x')",
            "INSERT INTO users (id, email) SELECT 1, 'a@x'",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql("CREATE TABLE d (id int GENERATED ALWAYS AS IDENTITY, x text)")
            .unwrap();
        let findings = lint_rule(&db, "INSERT INTO d DEFAULT VALUES", RULE);
        assert!(findings.is_empty(), "{findings:?}");
    }
}
