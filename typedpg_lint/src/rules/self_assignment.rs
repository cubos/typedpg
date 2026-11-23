//! The `self_assignment` rule: reports an assignment of a column to its
//! own value — `SET a = a`, or `SET a = t.a` naming the target — in an
//! `UPDATE`, an `INSERT … ON CONFLICT DO UPDATE` (where `t.a` is the
//! existing row's value; `excluded.a`, the value proposed for insertion,
//! is a change) and a `MERGE … WHEN MATCHED THEN UPDATE` (where `t.a` is
//! the target row's own value). The analyzer says what the value names (see
//! [`typedpg_analyzer::QueryView::column_ref_binding`]), so `SET a = s.a`
//! of a FROM item or MERGE source `s`, or `SET a = excluded.a`, is not one.
//! A multiple-column `SET (a, b) = (…)` is not read.
//!
//! What the assignment does decides what is said, or whether anything is:
//! - An `UPDATE OF` trigger of the table (or of a partition) listing the
//!   column fires because the SET list names it, even to assign the value
//!   it has: the assignment has an effect, and is not reported.
//! - Beside another assignment it changes nothing (the row is rewritten
//!   anyway): reported, with a fix removing it.
//! - When the SET list holds only self-assignments, it rewrites the row
//!   with the same values: a new row version, row locks, the row counted as
//!   updated, and — unlike `ON CONFLICT DO NOTHING` — the existing row
//!   returned by `RETURNING`. With a `RETURNING` clause that is the point
//!   (`ON CONFLICT (k) DO UPDATE SET k = t.k RETURNING id` gets the
//!   existing row's id), and with an `UPDATE` trigger that sees the rows (a
//!   row-level one, or one with a transition table) the trigger runs on
//!   it: neither is reported. Otherwise it is reported as rewriting the
//!   row with the same value, with no fix (a SET list can't be empty).
//!
//! The fix removes the assignment, only when the target is a plain or
//! partitioned table without rewrite rules.

use typedpg_analyzer::RelKind;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{CmdType, Node as N, RangeVar, ResTarget};

use super::{location, scan_until, starts_clause};
use crate::{Ctx, RawFinding, SqlEdit};

/// The statement a SET list belongs to, for the help.
#[derive(Clone, Copy)]
enum Statement {
    Update,
    OnConflict,
    Merge,
}

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::UpdateStmt(stmt) => {
                assignments(
                    ctx,
                    Statement::Update,
                    stmt.relation.as_deref(),
                    &stmt.target_list,
                    stmt.returning_clause.is_some(),
                    &mut out,
                );
            }
            NodeRef::InsertStmt(stmt) => {
                if let Some(clause) = stmt.on_conflict_clause.as_deref() {
                    assignments(
                        ctx,
                        Statement::OnConflict,
                        stmt.relation.as_deref(),
                        &clause.target_list,
                        stmt.returning_clause.is_some(),
                        &mut out,
                    );
                }
            }
            NodeRef::MergeStmt(stmt) => {
                for when in &stmt.merge_when_clauses {
                    if let Some(N::MergeWhenClause(when)) = when
                        && when.command_type == CmdType::CmdUpdate
                    {
                        assignments(
                            ctx,
                            Statement::Merge,
                            stmt.relation.as_deref(),
                            &when.target_list,
                            stmt.returning_clause.is_some(),
                            &mut out,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The self-assignments of one `SET` list writing `relation`.
fn assignments(
    ctx: &Ctx<'_>,
    statement: Statement,
    relation: Option<&RangeVar>,
    target_list: &[Option<N>],
    returning: bool,
    out: &mut Vec<RawFinding>,
) {
    let Some(relation) = relation else {
        return;
    };
    let target = relation
        .alias
        .as_ref()
        .map_or(relation.relname.as_str(), |alias| alias.aliasname.as_str());
    // The table as the analyzer resolved the statement's target.
    let class = match ctx.view.relation_of(relation) {
        Some(typedpg_analyzer::RelationRef::Relation(oid)) => ctx.catalog.class_row(oid),
        _ => None,
    };
    let removable_table = class.is_some_and(|class| {
        matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
            && !ctx.catalog.lint_relation_has_rules(class.oid)
    });
    let items: Vec<&ResTarget> = target_list
        .iter()
        .filter_map(|item| match item {
            Some(N::ResTarget(target)) => Some(&**target),
            _ => None,
        })
        .collect();
    let selves: Vec<usize> = (0..items.len())
        .filter(|&index| assigns_itself(ctx, items[index], target))
        .collect();
    if selves.is_empty() {
        return;
    }
    // Only self-assignments: the statement rewrites the row as it is.
    let rewrite_only = selves.len() == items.len();
    if rewrite_only {
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        // Its RETURNING returns the row; a trigger that sees the rows
        // runs on it. On a relation that isn't a table without rules, what
        // the rewrite does is unknown.
        if returning
            || !removable_table
            || class
                .is_none_or(|class| ctx.catalog.lint_update_trigger_sees_rows(class.oid, &names))
        {
            return;
        }
    }
    for index in selves {
        let item = items[index];
        // Naming the column fires its UPDATE OF trigger.
        if class.is_some_and(|class| {
            ctx.catalog
                .lint_update_of_trigger_lists(class.oid, &item.name)
        }) {
            continue;
        }
        let Some(start) = location(item.location) else {
            continue;
        };
        let (end, _) = scan_until(ctx, start, |text| text == "," || starts_clause(text));
        let column = typedpg_core::quote_identifier(&item.name);
        let (message, help, fix) = if rewrite_only {
            let help = match statement {
                Statement::Update => "assign the value meant",
                Statement::OnConflict => {
                    "use DO NOTHING to leave the existing row as it is (it then counts and \
                     returns no row), or assign the value meant"
                }
                Statement::Merge => {
                    "use WHEN MATCHED THEN DO NOTHING to leave the row as it is, or assign the \
                     value meant"
                }
            };
            (
                format!("assigning {column} its own value rewrites the row with the same value"),
                help,
                None,
            )
        } else {
            (
                format!("assigning {column} its own value changes nothing"),
                "remove the assignment, or assign the value meant",
                removable_table
                    .then(|| removal(ctx, &items, index, start, end))
                    .flatten(),
            )
        };
        out.push(RawFinding {
            span: start..end.max(start + 1),
            message,
            help: Some(help.to_owned()),
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

        // An UPDATE OF trigger listing the column fires on it: the
        // assignment has an effect.
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
        assert!(findings.is_empty(), "{findings:?}");
        // One listing another column doesn't.
        let sql = "UPDATE users SET code = code, email = 'x' WHERE id = 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(!findings[0].fix.is_empty());
    }

    #[test]
    fn on_conflict_and_merge_self_assignments_are_reported() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE a (id int PRIMARY KEY, x int, y int);
             CREATE TABLE b (id int PRIMARY KEY, x int);",
        )
        .unwrap();
        for (sql, span, expected) in [
            (
                "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET x = a.x, y = excluded.y",
                "x = a.x",
                Some(
                    "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET y = excluded.y",
                ),
            ),
            (
                "INSERT INTO a AS t VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE \
                 SET y = excluded.y, x = t.x WHERE t.y > 0",
                "x = t.x",
                Some(
                    "INSERT INTO a AS t VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE \
                     SET y = excluded.y WHERE t.y > 0",
                ),
            ),
            // The only assignment.
            (
                "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET x = a.x",
                "x = a.x",
                None,
            ),
            (
                "MERGE INTO a t USING b s ON t.id = s.id \
                 WHEN MATCHED THEN UPDATE SET y = t.y, x = s.x \
                 WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.x, 0)",
                "y = t.y",
                Some(
                    "MERGE INTO a t USING b s ON t.id = s.id \
                     WHEN MATCHED THEN UPDATE SET x = s.x \
                     WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.x, 0)",
                ),
            ),
            (
                "MERGE INTO a USING b ON a.id = b.id WHEN MATCHED THEN UPDATE SET y = y",
                "y = y",
                None,
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], span, "{sql}");
            match expected {
                Some(expected) => assert_eq!(fixed(sql, &findings), expected, "{sql}"),
                None => assert!(findings[0].fix.is_empty(), "{sql}: {findings:?}"),
            }
        }
        for sql in [
            "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET x = excluded.x",
            "INSERT INTO a AS t VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET x = excluded.x",
            "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET x = a.y",
            "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT DO NOTHING",
            "MERGE INTO a t USING b s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = s.x",
            "MERGE INTO a t USING b s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = t.y",
            "MERGE INTO a t USING b s ON t.id = s.id WHEN MATCHED THEN DELETE",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }

        // An UPDATE OF trigger listing the column fires on the assignment.
        db.apply_sql(
            "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RETURN NEW; END $$;
             CREATE TRIGGER t BEFORE UPDATE OF x ON a \
             FOR EACH ROW EXECUTE FUNCTION touch();",
        )
        .unwrap();
        for sql in [
            "INSERT INTO a VALUES (1, 2, 3) ON CONFLICT (id) DO UPDATE SET x = a.x, y = excluded.y",
            "MERGE INTO a t USING b s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = t.x, y = 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    /// A SET list of self-assignments alone rewrites the row: reported as
    /// such, unless RETURNING returns the row (the idiom getting the
    /// existing row back from an upsert) or an UPDATE trigger sees it.
    #[test]
    fn a_rewrite_of_the_row_as_it_is() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE v (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, k text UNIQUE, n int);
             CREATE TABLE b (id int PRIMARY KEY, k text);",
        )
        .unwrap();
        for (sql, help) in [
            (
                "INSERT INTO v (k) VALUES ($1) ON CONFLICT (k) DO UPDATE SET k = v.k",
                "use DO NOTHING",
            ),
            ("UPDATE v SET k = k WHERE id = 1", "assign the value meant"),
            (
                "MERGE INTO v USING b ON v.id = b.id WHEN MATCHED THEN UPDATE SET k = v.k",
                "WHEN MATCHED THEN DO NOTHING",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(
                findings[0].message,
                "assigning k its own value rewrites the row with the same value"
            );
            assert!(findings[0].help.as_deref().unwrap().contains(help), "{sql}");
            assert!(findings[0].fix.is_empty(), "{sql}");
        }
        for sql in [
            "INSERT INTO v (k) VALUES ($1) ON CONFLICT (k) DO UPDATE SET k = v.k RETURNING id",
            "INSERT INTO v AS t (k) VALUES ($1) ON CONFLICT (k) DO UPDATE SET k = t.k, n = t.n \
             RETURNING id",
            "UPDATE v SET k = k WHERE id = 1 RETURNING n",
            "MERGE INTO v USING b ON v.id = b.id WHEN MATCHED THEN UPDATE SET k = v.k \
             RETURNING v.id",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // Beside a real assignment it changes nothing, RETURNING or not.
        let sql = "INSERT INTO v (k) VALUES ($1) ON CONFLICT (k) DO UPDATE \
                   SET k = v.k, n = excluded.n RETURNING id";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].message,
            "assigning k its own value changes nothing"
        );
        assert!(!findings[0].fix.is_empty());

        // A row trigger, or a statement trigger with a transition table,
        // runs on the rewritten row.
        for trigger in [
            "CREATE TRIGGER t AFTER UPDATE ON v FOR EACH ROW EXECUTE FUNCTION touch()",
            "CREATE TRIGGER t AFTER UPDATE ON v REFERENCING NEW TABLE AS changed \
             FOR EACH STATEMENT EXECUTE FUNCTION touch()",
            "CREATE TRIGGER t BEFORE UPDATE OF k ON v FOR EACH ROW EXECUTE FUNCTION touch()",
        ] {
            let mut db = db.clone();
            db.apply_sql(&format!(
                "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RETURN NULL; END $$; {trigger};"
            ))
            .unwrap();
            for sql in [
                "INSERT INTO v (k) VALUES ($1) ON CONFLICT (k) DO UPDATE SET k = v.k",
                "UPDATE v SET k = k WHERE id = 1",
            ] {
                let findings = lint_rule(&db, sql, RULE);
                assert!(findings.is_empty(), "{trigger}: {sql}: {findings:?}");
            }
        }
        // A statement trigger without a transition table, or an UPDATE OF
        // trigger on another column, doesn't see it.
        for trigger in [
            "CREATE TRIGGER t AFTER UPDATE ON v FOR EACH STATEMENT EXECUTE FUNCTION touch()",
            "CREATE TRIGGER t BEFORE UPDATE OF n ON v FOR EACH ROW EXECUTE FUNCTION touch()",
        ] {
            let mut db = db.clone();
            db.apply_sql(&format!(
                "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RETURN NULL; END $$; {trigger};"
            ))
            .unwrap();
            let findings = lint_rule(&db, "UPDATE v SET k = k WHERE id = 1", RULE);
            assert_eq!(findings.len(), 1, "{trigger}: {findings:?}");
        }
        // `UPDATE v SET k = k, n = 1` with a `BEFORE UPDATE OF k` trigger
        // fires it: not reported.
        let mut with_of = db.clone();
        with_of
            .apply_sql(
                "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RETURN NEW; END $$;
                 CREATE TRIGGER t BEFORE UPDATE OF k ON v FOR EACH ROW EXECUTE FUNCTION touch();",
            )
            .unwrap();
        let findings = lint_rule(&with_of, "UPDATE v SET k = k, n = 1 WHERE id = 1", RULE);
        assert!(findings.is_empty(), "{findings:?}");
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
