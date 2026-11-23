//! The `unbounded_write` rule: reports an `UPDATE` or `DELETE` without a
//! `WHERE` clause, which writes every row of its table — including one
//! inside a CTE. Only the presence of `WHERE` is read, not what it says.
//!
//! A table that provably holds at most one row has no "every row" to write
//! by mistake: a settings table keyed by `id boolean PRIMARY KEY CHECK
//! (id)` is written without `WHERE` by design (see
//! [`typedpg_analyzer::relation_holds_at_most_one_row`]). Its writes are not
//! reported.

use typedpg_analyzer::RelationRef;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::RangeVar;

use super::{location, relation_end, table_name};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::UpdateStmt(stmt) if stmt.where_clause.is_none() => {
                write_finding(ctx, "UPDATE", "changes", stmt.relation.as_deref(), &mut out);
            }
            NodeRef::DeleteStmt(stmt) if stmt.where_clause.is_none() => {
                write_finding(ctx, "DELETE", "removes", stmt.relation.as_deref(), &mut out);
            }
            _ => {}
        }
    }
    out
}

fn write_finding(
    ctx: &Ctx<'_>,
    keyword: &str,
    verb: &str,
    relation: Option<&RangeVar>,
    out: &mut Vec<RawFinding>,
) {
    let Some(relation) = relation else {
        return;
    };
    // The table as the analyzer resolved the statement's target (a `WITH`
    // query of the same name is no table).
    if let Some(RelationRef::Relation(oid)) = ctx.view.relation_of(relation)
        && typedpg_analyzer::relation_holds_at_most_one_row(ctx.catalog, ctx.view, oid)
    {
        return;
    }
    let Some(relation_start) = location(relation.location) else {
        return;
    };
    let start = ctx
        .tokens
        .iter()
        .filter(|token| token.end <= relation_start)
        .rfind(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case(keyword))
        .map_or(relation_start, |token| token.start);
    let end = relation_end(ctx, relation).unwrap_or(relation_start + relation.relname.len());
    let table = table_name(relation);
    out.push(RawFinding {
        span: start..end.max(start + keyword.len()),
        message: format!("{keyword} without WHERE {verb} every row of {table}"),
        help: Some(
            "add a WHERE clause, or allow it with -- typedpg-lint: allow(unbounded_write)"
                .to_owned(),
        ),
        fix: Vec::new(),
    });
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::UnboundedWrite;

    /// A table its key and a CHECK fix to one row has no "every row" to
    /// write by mistake; one that may hold more is still reported.
    #[test]
    fn a_singleton_table_is_written_without_where() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE fleet_settings (id boolean PRIMARY KEY CHECK (id), max_nodes int);
             CREATE TABLE one (id int PRIMARY KEY CHECK (id = 1), v text);
             CREATE TABLE no_check (id boolean PRIMARY KEY, v text);
             CREATE TABLE nullable (id boolean UNIQUE CHECK (id), v text);
             CREATE TABLE parent (id boolean PRIMARY KEY CHECK (id), v text);
             CREATE TABLE child () INHERITS (parent);
             CREATE TABLE ruled (id boolean PRIMARY KEY CHECK (id), v text);
             CREATE TABLE log (v text);
             CREATE RULE r AS ON UPDATE TO ruled DO ALSO DELETE FROM log;",
        )
        .unwrap();
        for sql in [
            "UPDATE fleet_settings SET max_nodes = 3",
            "DELETE FROM fleet_settings",
            "UPDATE one SET v = 'x'",
            "WITH d AS (DELETE FROM one RETURNING v) SELECT v FROM d",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for (sql, message) in [
            (
                "UPDATE no_check SET v = 'x'",
                "UPDATE without WHERE changes every row of no_check",
            ),
            (
                // A NULL key is a second row.
                "UPDATE nullable SET v = 'x'",
                "UPDATE without WHERE changes every row of nullable",
            ),
            (
                // A child holds rows the parent's key doesn't cover.
                "DELETE FROM parent",
                "DELETE without WHERE removes every row of parent",
            ),
            (
                "UPDATE ruled SET v = 'x'",
                "UPDATE without WHERE changes every row of ruled",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].message, message, "{sql}");
        }
    }
}
