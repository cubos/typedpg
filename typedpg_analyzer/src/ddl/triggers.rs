//! CREATE / DROP TRIGGER. Triggers don't change query types, but PG
//! validates them when they are created (`CreateTriggerFiringOn`) and a
//! trigger depends on its function, so the catalog keeps a minimal record.

use typedpg_pg_query::protobuf::{CreateTrigStmt, node};

use super::DdlError;
use crate::oid::{PgClassOid, PgProcOid};
use crate::pg_catalog::{PgCatalog, RelKind};

/// A trigger (`pg_trigger`): its name, function and firing kind.
#[derive(Clone, Debug)]
pub(crate) struct Trigger {
    pub(crate) name: String,
    pub(crate) function: PgProcOid,
    /// The `TRIGGER_TYPE_INSERT | _DELETE | _UPDATE | _TRUNCATE` bits of
    /// an `INSTEAD OF ... FOR EACH ROW` trigger, else 0 — what the
    /// relcache's `trig_*_instead_row` flags are made of.
    pub(crate) instead_row_events: i32,
}

impl Trigger {
    /// The relation has an INSTEAD OF row trigger for this event
    /// (`TRIGGER_TYPE_INSERT` / `_UPDATE` / `_DELETE` bits).
    pub(crate) fn is_instead_row_for(&self, event_bit: i32) -> bool {
        self.instead_row_events & event_bit != 0
    }
}

/// `TRIGGER_TYPE_UPDATE` (trigger.h).
pub(crate) const TRIGGER_TYPE_UPDATE: i32 = 1 << 4;

/// `TRIGGER_TYPE_*` bits (trigger.h).
const TRIGGER_TYPE_BEFORE: i32 = 1 << 1;
pub(crate) const TRIGGER_TYPE_INSERT: i32 = 1 << 2;
pub(crate) const TRIGGER_TYPE_DELETE: i32 = 1 << 3;
const TRIGGER_TYPE_INSTEAD: i32 = 1 << 6;

/// CreateTriggerFiringOn: the WHEN condition is transformed as a boolean
/// WHERE clause (EXPR_KIND_TRIGGER_WHEN) over the relation as `old` and
/// `new`, and may only reference the row values the trigger has — none
/// for a statement trigger, no OLD for INSERT, no NEW for DELETE, and for a
/// BEFORE trigger no NEW system column nor generated column (their values
/// don't exist yet).
fn check_when_clause(
    interp: &PgCatalog,
    relid: PgClassOid,
    stmt: &CreateTrigStmt,
    when: &typedpg_pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::qualified_name::QualifiedName;
    use crate::scope::Scope;

    let unsupported = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(e.to_string());
    super::expr_kind::check_expr_kind(interp, when, super::expr_kind::ExprKind::TriggerWhen)?;
    crate::resolve::check_no_srf_in_clause(when, interp, "trigger WHEN conditions")
        .map_err(unsupported)?;
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = Scope::default();
    for alias in ["old", "new"] {
        scope.add_dml_target(
            interp,
            alias,
            QualifiedName::new(nspname.clone(), class.relname.clone()),
            &attrs,
        );
    }
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let result = infer_expr(
        when,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(unsupported)?;
    // transformWhereClause: coerce_to_boolean.
    if result.type_oid != crate::pg_catalog::oid::BOOL
        && result.type_oid != crate::pg_catalog::oid::UNKNOWN
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of WHEN must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }

    let before = stmt.timing & TRIGGER_TYPE_BEFORE != 0;
    let has_generated = attrs.iter().any(|a| a.attgenerated.is_some());
    let Some(inner) = when.node.as_ref() else {
        return Ok(());
    };
    for (n, ..) in inner.nodes() {
        let typedpg_pg_query::NodeRef::ColumnRef(cr) = n else {
            continue;
        };
        let fields: Vec<Option<&str>> = cr.fields.iter().map(super::util::node_string).collect();
        let (varno, column) = match fields.as_slice() {
            // `old` / `new` as a whole row, `old.*`, `old.col`.
            [Some(rel)]
                if (*rel == "old" || *rel == "new") && !attrs.iter().any(|a| a.attname == *rel) =>
            {
                (*rel, None)
            }
            [Some(rel), rest] if *rel == "old" || *rel == "new" => (*rel, *rest),
            // An unqualified column is ambiguous between OLD and NEW; the
            // analysis above reported it.
            _ => continue,
        };
        if !stmt.row {
            return Err(DdlError::Parse(
                "statement trigger's WHEN condition cannot reference column values".into(),
            ));
        }
        if varno == "old" {
            if stmt.events & TRIGGER_TYPE_INSERT != 0 {
                return Err(DdlError::Parse(
                    "INSERT trigger's WHEN condition cannot reference OLD values".into(),
                ));
            }
            continue;
        }
        if stmt.events & TRIGGER_TYPE_DELETE != 0 {
            return Err(DdlError::Parse(
                "DELETE trigger's WHEN condition cannot reference NEW values".into(),
            ));
        }
        if !before {
            continue;
        }
        match column {
            None => {
                if has_generated {
                    return Err(DdlError::Parse(
                        "BEFORE trigger's WHEN condition cannot reference NEW generated columns \
                         (A whole-row reference is used and the table contains generated \
                         columns.)"
                            .into(),
                    ));
                }
            }
            Some(col) => match attrs.iter().find(|a| a.attname == col) {
                Some(attr) if attr.attgenerated.is_some() => {
                    return Err(DdlError::Parse(format!(
                        "BEFORE trigger's WHEN condition cannot reference NEW generated columns \
                         (Column \"{col}\" is a generated column.)"
                    )));
                }
                Some(_) => {}
                None if crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, ..)| *n == col) =>
                {
                    return Err(DdlError::UnsupportedDdl(
                        "BEFORE trigger's WHEN condition cannot reference NEW system columns"
                            .into(),
                    ));
                }
                None => {}
            },
        }
    }
    Ok(())
}

pub fn create_trigger(interp: &mut PgCatalog, stmt: &CreateTrigStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let Some(class) = interp.pg_class.get(&relid).cloned() else {
        return Ok(());
    };
    let instead = stmt.timing & TRIGGER_TYPE_INSTEAD != 0;
    let before_or_after = !instead;
    match class.relkind {
        RelKind::View if stmt.row && before_or_after => {
            return Err(DdlError::Parse(format!(
                "\"{}\" is a view (Views cannot have row-level BEFORE or AFTER triggers.)",
                class.relname
            )));
        }
        RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable if instead => {
            return Err(DdlError::Parse(format!(
                "\"{}\" is a table (Tables cannot have INSTEAD OF triggers.)",
                class.relname
            )));
        }
        RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable | RelKind::View => {}
        _ => {
            return Err(DdlError::Parse(format!(
                "relation \"{}\" cannot have triggers",
                class.relname
            )));
        }
    }

    if let Some(when) = stmt.when_clause.as_deref() {
        check_when_clause(interp, relid, stmt, when)?;
    }

    // The function takes no declared arguments (the trigger's arguments go
    // through TG_ARGV) and must return `trigger`.
    let parts: Vec<&str> = stmt
        .funcname
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return Ok(()),
    };
    let Some((_, function)) =
        super::alter::find_proc(interp, schema, name, &|p| p.proargtypes.is_empty())
    else {
        return Err(DdlError::TypeNotFound(format!(
            "function {}() does not exist",
            parts.join(".")
        )));
    };
    let returns_trigger = interp
        .pg_proc
        .get(&function)
        .and_then(|p| interp.pg_type.get(&p.prorettype))
        .is_some_and(|t| t.typname == "trigger");
    if !returns_trigger {
        return Err(DdlError::Parse(format!(
            "function {name} must return type trigger"
        )));
    }

    let instead_row_events = if instead && stmt.row { stmt.events } else { 0 };
    let triggers = interp.triggers.entry(relid).or_default();
    if let Some(existing) = triggers.iter_mut().find(|t| t.name == stmt.trigname) {
        if !stmt.replace {
            return Err(DdlError::DuplicateObject(format!(
                "trigger \"{}\" for relation \"{}\" already exists",
                stmt.trigname, class.relname
            )));
        }
        existing.function = function;
        existing.instead_row_events = instead_row_events;
        return Ok(());
    }
    triggers.push(Trigger {
        name: stmt.trigname.clone(),
        function,
        instead_row_events,
    });
    Ok(())
}

/// `DROP TRIGGER [IF EXISTS] name ON table`.
pub(crate) fn drop_trigger(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::List(list)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let parts: Vec<String> = list
        .items
        .iter()
        .filter_map(super::util::node_string)
        .map(str::to_owned)
        .collect();
    let Some((trigname, rel)) = parts.split_last() else {
        return Ok(());
    };
    let rv = typedpg_pg_query::protobuf::RangeVar {
        schemaname: if rel.len() == 2 {
            rel[0].clone()
        } else {
            String::new()
        },
        relname: rel.last().cloned().unwrap_or_default(),
        inh: true,
        ..Default::default()
    };
    let relid: PgClassOid = super::util::lookup_relation(interp, &rv)?.1;
    let relname = rv.relname.clone();
    let triggers = interp.triggers.entry(relid).or_default();
    let before = triggers.len();
    triggers.retain(|t| &t.name != trigname);
    if triggers.len() == before && !missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "trigger \"{trigname}\" for table \"{relname}\" does not exist"
        )));
    }
    Ok(())
}

/// The triggers (as `(relation, name)`) that run function `proc`.
pub(crate) fn triggers_using_function(
    interp: &PgCatalog,
    proc: PgProcOid,
) -> Vec<(PgClassOid, String)> {
    interp
        .triggers
        .iter()
        .flat_map(|(&relid, ts)| {
            ts.iter()
                .filter(move |t| t.function == proc)
                .map(move |t| (relid, t.name.clone()))
        })
        .collect()
}

/// `ALTER TRIGGER name ON table RENAME TO new` (renametrig).
pub(crate) fn rename_trigger(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let relid = match super::util::lookup_relation(interp, rv) {
        Ok((_, oid)) => oid,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    let triggers = interp.triggers.entry(relid).or_default();
    if triggers.iter().any(|t| t.name == stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "trigger \"{}\" for relation \"{}\" already exists",
            stmt.newname, rv.relname
        )));
    }
    let Some(trigger) = triggers.iter_mut().find(|t| t.name == stmt.subname) else {
        return Err(DdlError::TypeNotFound(format!(
            "trigger \"{}\" for table \"{}\" does not exist",
            stmt.subname, rv.relname
        )));
    };
    trigger.name = stmt.newname.clone();
    Ok(())
}
