//! Event triggers (event_trigger.c): the event must be known, the
//! function must exist and return `event_trigger`, and names are unique.

use pg_query::protobuf::{AlterEventTrigStmt, CreateEventTrigStmt};

use super::DdlError;
use super::util::node_string;
use crate::pg_catalog::PgCatalog;

fn missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("event trigger \"{name}\" does not exist"))
}

pub fn create_event_trigger(
    interp: &mut PgCatalog,
    stmt: &CreateEventTrigStmt,
) -> Result<(), DdlError> {
    // CreateEventTrigger: the event name is validated first.
    if !matches!(
        stmt.eventname.as_str(),
        "ddl_command_start" | "ddl_command_end" | "sql_drop" | "table_rewrite" | "login"
    ) {
        return Err(DdlError::Parse(format!(
            "unrecognized event name \"{}\"",
            stmt.eventname
        )));
    }
    if interp
        .event_triggers
        .iter()
        .any(|(t, _)| *t == stmt.trigname)
    {
        return Err(DdlError::DuplicateObject(format!(
            "event trigger \"{}\" already exists",
            stmt.trigname
        )));
    }
    let parts: Vec<&str> = stmt.funcname.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return Ok(()),
    };
    let Some(proc) = interp
        .find_functions(schema, name)
        .into_iter()
        .find(|p| p.proargtypes.is_empty())
    else {
        return Err(DdlError::TypeNotFound(format!(
            "function {name}() does not exist"
        )));
    };
    if interp
        .pg_type
        .get(&proc.prorettype)
        .map(|t| t.typname.as_str())
        != Some("event_trigger")
    {
        return Err(DdlError::Parse(format!(
            "function {name} must return type event_trigger"
        )));
    }
    let function = proc.oid;
    interp
        .event_triggers
        .push((stmt.trigname.clone(), function));
    Ok(())
}

pub fn alter_event_trigger(interp: &PgCatalog, stmt: &AlterEventTrigStmt) -> Result<(), DdlError> {
    if interp
        .event_triggers
        .iter()
        .any(|(t, _)| *t == stmt.trigname)
    {
        Ok(())
    } else {
        Err(missing(&stmt.trigname))
    }
}

/// DROP EVENT TRIGGER [IF EXISTS] name.
pub(crate) fn drop_event_trigger(
    interp: &mut PgCatalog,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(name) = node_string(obj_node).map(str::to_owned) else {
        return Ok(());
    };
    let before = interp.event_triggers.len();
    interp.event_triggers.retain(|(t, _)| *t != name);
    if interp.event_triggers.len() == before && !missing_ok {
        return Err(missing(&name));
    }
    Ok(())
}

/// ALTER EVENT TRIGGER name RENAME TO new.
pub(crate) fn rename_event_trigger(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(old) = stmt
        .object
        .as_deref()
        .and_then(node_string)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    if interp
        .event_triggers
        .iter()
        .any(|(t, _)| *t == stmt.newname)
    {
        return Err(DdlError::DuplicateObject(format!(
            "event trigger \"{}\" already exists",
            stmt.newname
        )));
    }
    let Some((t, _)) = interp.event_triggers.iter_mut().find(|(t, _)| *t == old) else {
        return Err(missing(&old));
    };
    t.clone_from(&stmt.newname);
    Ok(())
}

/// The event triggers executing function `function`.
pub(crate) fn event_triggers_using(
    interp: &PgCatalog,
    function: crate::oid::PgProcOid,
) -> Vec<String> {
    interp
        .event_triggers
        .iter()
        .filter(|(_, f)| *f == function)
        .map(|(t, _)| t.clone())
        .collect()
}
