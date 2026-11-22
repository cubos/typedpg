//! The `check_violation` rule: reports a constant an `INSERT … VALUES`, an
//! `UPDATE … SET` or an `ON CONFLICT DO UPDATE SET` stores that a `CHECK`
//! constraint of the table — or of the column's domain — rejects
//! whatever the row's other columns: the statement fails for every row it
//! writes (`UPDATE … SET status = 'actve'` on a `CHECK (status IN
//! ('active', 'inactive'))` column). `NOT VALID` constraints count: they
//! bind the rows written after them.
//!
//! The analyzer already rejects an `INSERT` whose row it proves fails; this
//! rule adds the writes it can't reject, because they may write no row (an
//! `UPDATE`, an `ON CONFLICT DO UPDATE`), and the columns with a type
//! modifier (`varchar(20)`, whose value is stored as written when it
//! fits). Not reported when a BEFORE ROW trigger or a rule may change the
//! row, or the target is a view (see
//! [`typedpg_analyzer::PgCatalog::assignment_violation`]).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{Node, Node as N, OnConflictAction, RangeVar};

use super::{expression_end, operand_start};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::InsertStmt(stmt) => {
                let Some(relid) = relation_oid(ctx, stmt.relation.as_deref()) else {
                    continue;
                };
                let columns: Vec<String> = if stmt.cols.is_empty() {
                    ctx.catalog
                        .attributes_of(relid)
                        .iter()
                        .filter(|a| a.attnum > 0)
                        .map(|a| a.attname.clone())
                        .collect()
                } else {
                    let names: Option<Vec<String>> = stmt
                        .cols
                        .iter()
                        .map(|c| match c {
                            Some(N::ResTarget(t)) if t.indirection.is_empty() => {
                                Some(t.name.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    let Some(names) = names else {
                        continue;
                    };
                    names
                };
                if let Some(N::SelectStmt(select)) = stmt.select_stmt.as_ref() {
                    for row in select.values_lists.iter().flatten() {
                        let N::List(row) = row else {
                            continue;
                        };
                        for (value, column) in row.items.iter().zip(&columns) {
                            if let Some(value) = value {
                                assignment(ctx, relid, column, value, false, &mut out);
                            }
                        }
                    }
                }
                if let Some(conflict) = stmt.on_conflict_clause.as_deref()
                    && conflict.action == OnConflictAction::OnconflictUpdate
                {
                    set_list(ctx, relid, &conflict.target_list, &mut out);
                }
            }
            NodeRef::UpdateStmt(stmt) => {
                if let Some(relid) = relation_oid(ctx, stmt.relation.as_deref()) {
                    set_list(ctx, relid, &stmt.target_list, &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

/// The relation a statement writes.
fn relation_oid(
    ctx: &Ctx<'_>,
    relation: Option<&RangeVar>,
) -> Option<typedpg_analyzer::PgClassOid> {
    let relation = relation?;
    if !relation.catalogname.is_empty() {
        return None;
    }
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    Some(ctx.catalog.resolve_table(schema, &relation.relname)?.oid)
}

/// The `SET col = value` items of an UPDATE or ON CONFLICT DO UPDATE.
fn set_list(
    ctx: &Ctx<'_>,
    relid: typedpg_analyzer::PgClassOid,
    targets: &[Option<Node>],
    out: &mut Vec<RawFinding>,
) {
    for target in targets.iter().flatten() {
        let N::ResTarget(t) = target else {
            continue;
        };
        if !t.indirection.is_empty() {
            continue;
        }
        if let Some(value) = t.val.as_ref() {
            assignment(ctx, relid, &t.name, value, true, out);
        }
    }
}

fn assignment(
    ctx: &Ctx<'_>,
    relid: typedpg_analyzer::PgClassOid,
    column: &str,
    value: &Node,
    update: bool,
    out: &mut Vec<RawFinding>,
) {
    // A multi-column `SET (a, b) = (…)` source is no constant.
    if matches!(value, N::MultiAssignRef(_)) {
        return;
    }
    let Some(violation) = ctx
        .catalog
        .assignment_violation(relid, column, value, update)
    else {
        return;
    };
    let Some(start) = operand_start(value) else {
        return;
    };
    let end = expression_end(ctx, start).max(start + 1);
    let fails = if update {
        "the statement fails as soon as it updates a row"
    } else {
        "the statement fails for every row it inserts"
    };
    let column = typedpg_core::quote_identifier(column);
    let message = match &violation.constraint {
        Some(name) => {
            format!("storing this value in {column} violates CHECK constraint \"{name}\": {fails}")
        }
        None => format!(
            "storing this value in {column} fails: {}",
            violation.message
        ),
    };
    let help = match &violation.allowed {
        Some(allowed) => format!("the constraint allows {allowed}"),
        None => "store a value the constraint allows".to_owned(),
    };
    out.push(RawFinding {
        span: start..end,
        message,
        help: Some(help),
        fix: Vec::new(),
    });
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::CheckViolation;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE DOMAIN pct AS int CHECK (VALUE BETWEEN 0 AND 100);
                 CREATE TABLE orders (
                    id bigint PRIMARY KEY,
                    status text NOT NULL CHECK (status IN ('open', 'paid', 'void')),
                    code varchar(10) CHECK (code IN ('a', 'b')),
                    qty int CHECK (qty > 0),
                    discount pct,
                    note text,
                    kind text,
                    extra text,
                    CHECK (kind <> 'gift' OR note IS NOT NULL)
                 );
                 ALTER TABLE orders ADD CONSTRAINT extra_short
                    CHECK (extra IN ('x', 'y')) NOT VALID;
                 CREATE TABLE audited (id int PRIMARY KEY, state text CHECK (state IN ('a', 'b')));
                 CREATE FUNCTION fix_state() RETURNS trigger LANGUAGE plpgsql
                    AS $$ BEGIN NEW.state := 'a'; RETURN NEW; END $$;
                 CREATE TRIGGER fix BEFORE INSERT OR UPDATE ON audited
                    FOR EACH ROW EXECUTE FUNCTION fix_state();",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn a_constant_a_check_rejects_is_reported() {
        let db = catalog();
        for (sql, reported, constraint) in [
            (
                "UPDATE orders SET status = 'opne' WHERE id = 1",
                "'opne'",
                Some("orders_status_check"),
            ),
            (
                "UPDATE orders SET qty = 0 WHERE id = $1",
                "0",
                Some("orders_qty_check"),
            ),
            (
                "UPDATE orders SET qty = -5 WHERE id = $1",
                "-5",
                Some("orders_qty_check"),
            ),
            // A varchar(n) column: the analyzer leaves it to the lint.
            (
                "INSERT INTO orders (id, status, code) VALUES (1, 'open', 'c')",
                "'c'",
                Some("orders_code_check"),
            ),
            (
                "INSERT INTO orders (id, status) VALUES (1, 'open') \
                 ON CONFLICT (id) DO UPDATE SET status = 'closed'",
                "'closed'",
                Some("orders_status_check"),
            ),
            // NOT VALID constraints bind new rows.
            (
                "UPDATE orders SET extra = 'z' WHERE id = 1",
                "'z'",
                Some("extra_short"),
            ),
            // A domain's CHECK.
            ("UPDATE orders SET discount = 150 WHERE id = 1", "150", None),
            // In a data-modifying CTE.
            (
                "WITH u AS (UPDATE orders SET status = 'x' WHERE id = 1 RETURNING id) \
                 SELECT id FROM u",
                "'x'",
                Some("orders_status_check"),
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            if let Some(constraint) = constraint {
                assert!(
                    findings[0].message.contains(constraint),
                    "{sql}: {}",
                    findings[0].message
                );
            }
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn the_help_lists_what_the_constraint_allows() {
        let db = catalog();
        let findings = lint_rule(&db, "UPDATE orders SET status = 'opne' WHERE id = 1", RULE);
        assert_eq!(
            findings[0].help.as_deref(),
            Some("the constraint allows one of 'open', 'paid', 'void'")
        );
    }

    #[test]
    fn values_the_checks_allow_or_may_allow_are_not_reported() {
        let db = catalog();
        for sql in [
            "UPDATE orders SET status = 'paid' WHERE id = 1",
            "UPDATE orders SET qty = 3 WHERE id = 1",
            "UPDATE orders SET qty = $1 WHERE id = 1",
            "UPDATE orders SET qty = NULL WHERE id = 1",
            // Depends on another column of the row.
            "UPDATE orders SET kind = 'gift' WHERE id = 1",
            "UPDATE orders SET note = 'x' WHERE id = 1",
            "UPDATE orders SET status = status WHERE id = 1",
            // A trigger may change the row.
            "UPDATE audited SET state = 'z' WHERE id = 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
