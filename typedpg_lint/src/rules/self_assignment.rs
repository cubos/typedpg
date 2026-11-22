//! The `self_assignment` rule: reports an `UPDATE` assignment of a column
//! to its own value — `SET a = a`, or `SET a = t.a` naming the target —
//! which changes nothing, and usually stands where another value was
//! meant. The analyzer says what the value names (see
//! [`typedpg_analyzer::QueryView::column_ref_binding`]), so `SET a = s.a`
//! of a FROM item `s` is not one. A multiple-column `SET (a, b) = (…)` is
//! not read.
//!
//! The fix removes the assignment, only when:
//! - another assignment remains (a SET list can't be empty);
//! - the target is a plain or partitioned table without rewrite rules;
//! - no `UPDATE OF` trigger of the table (or of a partition) lists the
//!   column: such a trigger fires when the SET list names the column, even
//!   to assign the value it has, so removing it would stop the trigger.

use typedpg_analyzer::RelKind;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{Node as N, ResTarget, UpdateStmt};

use super::{location, scan_until, starts_clause};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::UpdateStmt(stmt) = node {
            update(ctx, stmt, &mut out);
        }
    }
    out
}

fn update(ctx: &Ctx<'_>, stmt: &UpdateStmt, out: &mut Vec<RawFinding>) {
    let Some(relation) = stmt.relation.as_deref() else {
        return;
    };
    let target = relation
        .alias
        .as_ref()
        .map_or(relation.relname.as_str(), |alias| alias.aliasname.as_str());
    let class = relation
        .catalogname
        .is_empty()
        .then(|| {
            let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
            ctx.catalog.resolve_table(schema, &relation.relname)
        })
        .flatten();
    let removable_table = class.is_some_and(|class| {
        matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
            && !ctx.catalog.lint_relation_has_rules(class.oid)
    });
    let items: Vec<&ResTarget> = stmt
        .target_list
        .iter()
        .filter_map(|item| match item {
            Some(N::ResTarget(target)) => Some(&**target),
            _ => None,
        })
        .collect();
    for (index, item) in items.iter().enumerate() {
        if !assigns_itself(ctx, item, target) {
            continue;
        }
        let Some(start) = location(item.location) else {
            continue;
        };
        let (end, _) = scan_until(ctx, start, |text| text == "," || starts_clause(text));
        let removable = items.len() > 1
            && removable_table
            && class.is_some_and(|class| {
                !ctx.catalog
                    .lint_update_of_trigger_lists(class.oid, &item.name)
            });
        let fix = if removable {
            removal(ctx, &items, index, start, end)
        } else {
            None
        };
        out.push(RawFinding {
            span: start..end.max(start + 1),
            message: format!(
                "assigning {} its own value changes nothing",
                typedpg_core::quote_identifier(&item.name)
            ),
            help: Some("remove the assignment, or assign the value meant".to_owned()),
            fix: fix.into_iter().collect(),
        });
    }
}

/// Whether `item` is `col = col` (or `col = target.col`), the value naming
/// the target's own column.
fn assigns_itself(ctx: &Ctx<'_>, item: &ResTarget, target: &str) -> bool {
    if !item.indirection.is_empty() {
        return false;
    }
    let Some(N::ColumnRef(column)) = item.val.as_ref() else {
        return false;
    };
    if !matches!(column.fields.last(), Some(Some(N::String(name))) if name.sval == item.name) {
        return false;
    }
    ctx.view
        .column_ref_binding(column.location)
        .is_some_and(|binding| !binding.outer && binding.source.as_deref() == Some(target))
}

/// The edit removing item `index` of `items` (bytes `start..end`) with the
/// comma that separates it.
fn removal(
    ctx: &Ctx<'_>,
    items: &[&ResTarget],
    index: usize,
    start: usize,
    end: usize,
) -> Option<SqlEdit> {
    if let Some(next) = items.get(index + 1) {
        return Some(SqlEdit {
            range: start..location(next.location)?,
            text: String::new(),
        });
    }
    // The last one: from the end of the item before its comma.
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

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::SelfAssignment;

    #[test]
    fn self_assignments_are_removed() {
        let db = catalog();
        for (sql, expected) in [
            (
                "UPDATE users SET name = name, email = 'x' WHERE id = 1",
                "UPDATE users SET email = 'x' WHERE id = 1",
            ),
            (
                "UPDATE users SET email = 'x', name = users.name WHERE id = 1",
                "UPDATE users SET email = 'x' WHERE id = 1",
            ),
            (
                "UPDATE users u SET code = 'abc', name = u.name, email = 'x' WHERE id = 1",
                "UPDATE users u SET code = 'abc', email = 'x' WHERE id = 1",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn what_has_no_fix() {
        let db = catalog();
        // The only assignment.
        let sql = "UPDATE users SET name = name WHERE id = 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "name = name");
        assert!(findings[0].fix.is_empty());

        // An UPDATE OF trigger listing the column fires on it.
        let mut db = catalog();
        db.apply_sql(
            "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RETURN NEW; END $$;
             CREATE TRIGGER t BEFORE UPDATE OF name ON users \
             FOR EACH ROW EXECUTE FUNCTION touch();",
        )
        .unwrap();
        let sql = "UPDATE users SET name = name, email = 'x' WHERE id = 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty());
        // One listing another column doesn't.
        let sql = "UPDATE users SET code = code, email = 'x' WHERE id = 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(!findings[0].fix.is_empty());
    }

    #[test]
    fn assignments_of_other_values_are_not_reported() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE a (id int PRIMARY KEY, x int, y int);
             CREATE TABLE b (id int PRIMARY KEY, x int);",
        )
        .unwrap();
        for sql in [
            "UPDATE a SET x = y WHERE id = 1",
            "UPDATE a SET x = b.x FROM b WHERE a.id = b.id",
            "UPDATE a SET x = x + 1 WHERE id = 1",
            "UPDATE a SET (x, y) = (1, 2) WHERE id = 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
