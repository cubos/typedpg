//! CREATE / DROP TRIGGER. Triggers don't change query types, but PG
//! validates them when they are created (`CreateTriggerFiringOn`) and a
//! trigger depends on its function, so the catalog keeps a minimal record.

use pg_query::protobuf::{CreateTrigStmt, node};

use super::DdlError;
use crate::oid::{PgClassOid, PgProcOid};
use crate::pg_catalog::{PgCatalog, RelKind};

/// A trigger (`pg_trigger`): its name and function.
#[derive(Clone, Debug)]
pub(crate) struct Trigger {
    pub(crate) name: String,
    pub(crate) function: PgProcOid,
}

/// `TRIGGER_TYPE_INSTEAD` (trigger.h).
const TRIGGER_TYPE_INSTEAD: i32 = 1 << 6;

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

    let triggers = interp.triggers.entry(relid).or_default();
    if let Some(existing) = triggers.iter_mut().find(|t| t.name == stmt.trigname) {
        if !stmt.replace {
            return Err(DdlError::DuplicateObject(format!(
                "trigger \"{}\" for relation \"{}\" already exists",
                stmt.trigname, class.relname
            )));
        }
        existing.function = function;
        return Ok(());
    }
    triggers.push(Trigger {
        name: stmt.trigname.clone(),
        function,
    });
    Ok(())
}

/// `DROP TRIGGER [IF EXISTS] name ON table`.
pub(crate) fn drop_trigger(
    interp: &mut PgCatalog,
    obj_node: &pg_query::protobuf::Node,
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
    let rv = pg_query::protobuf::RangeVar {
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
