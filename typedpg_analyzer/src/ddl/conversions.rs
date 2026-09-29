//! Encoding conversions (conversioncmds.c): CREATE CONVERSION names a
//! conversion function with the fixed signature, conversion names are
//! unique per schema. The encoding names themselves aren't checked (PG
//! accepts many spellings of each).

use pg_query::protobuf::CreateConversionStmt;

use super::DdlError;
use super::util::node_string;
use crate::oid::{PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::PgCatalog;

/// `(int4, int4, cstring, internal, int4, bool)`.
const SIGNATURE: [u32; 6] = [23, 23, 2275, 2281, 23, 16];

pub fn create_conversion(
    interp: &mut PgCatalog,
    stmt: &CreateConversionStmt,
) -> Result<(), DdlError> {
    let (nsoid, name) = super::util::ensure_qualified_name(interp, &stmt.conversion_name)?;
    let parts: Vec<&str> = stmt.func_name.iter().filter_map(node_string).collect();
    let (schema, func) = match parts.as_slice() {
        [schema, func] => (Some(*schema), *func),
        [func] => (None, *func),
        _ => return Ok(()),
    };
    let wanted: Vec<PgTypeOid> = SIGNATURE.iter().map(|&o| PgTypeOid::from_raw(o)).collect();
    // FindDefaultConversionProc / LookupFuncName with the fixed argument
    // types.
    let Some(proc) = interp
        .find_functions(schema, func)
        .into_iter()
        .find(|p| p.proargtypes == wanted)
    else {
        return Err(DdlError::TypeNotFound(format!(
            "function {func}(integer, integer, cstring, internal, integer, boolean) does not \
             exist"
        )));
    };
    if proc.prorettype != crate::pg_catalog::oid::INT4 {
        return Err(DdlError::Parse(format!(
            "encoding conversion function {func} must return type integer"
        )));
    }
    if interp
        .conversions
        .iter()
        .any(|(n, ns)| *n == name && *ns == nsoid)
    {
        return Err(DdlError::DuplicateObject(format!(
            "conversion \"{name}\" already exists"
        )));
    }
    interp.conversions.push((name, nsoid));
    Ok(())
}

fn find(interp: &PgCatalog, names: &[&str]) -> Option<(String, PgNamespaceOid)> {
    let (schema, name) = match names {
        [schema, name] => (Some(*schema), *name),
        [name] => (None, *name),
        _ => return None,
    };
    interp
        .schemas_for_lookup(schema)
        .into_iter()
        .find_map(|ns| {
            interp
                .conversions
                .iter()
                .find(|(n, s)| n == name && *s == ns)
                .cloned()
        })
}

/// DROP CONVERSION [IF EXISTS] name.
pub(crate) fn drop_conversion(
    interp: &mut PgCatalog,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(pg_query::protobuf::node::Node::List(l)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let names: Vec<&str> = l.items.iter().filter_map(node_string).collect();
    match find(interp, &names) {
        Some(found) => {
            interp.conversions.retain(|c| *c != found);
            Ok(())
        }
        None if missing_ok => Ok(()),
        None => Err(DdlError::TypeNotFound(format!(
            "conversion \"{}\" does not exist",
            names.join(".")
        ))),
    }
}

/// ALTER CONVERSION name RENAME TO new.
pub(crate) fn rename_conversion(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(pg_query::protobuf::node::Node::List(l)) =
        stmt.object.as_deref().and_then(|o| o.node.as_ref())
    else {
        return Ok(());
    };
    let names: Vec<&str> = l.items.iter().filter_map(node_string).collect();
    let Some((old, ns)) = find(interp, &names) else {
        return Err(DdlError::TypeNotFound(format!(
            "conversion \"{}\" does not exist",
            names.join(".")
        )));
    };
    if interp
        .conversions
        .iter()
        .any(|(n, s)| *n == stmt.newname && *s == ns)
    {
        let schema = interp.namespace_name(ns).unwrap_or("?").to_owned();
        return Err(DdlError::DuplicateObject(format!(
            "conversion \"{}\" already exists in schema \"{schema}\"",
            stmt.newname
        )));
    }
    for c in interp
        .conversions
        .iter_mut()
        .filter(|(n, s)| *n == old && *s == ns)
    {
        c.0.clone_from(&stmt.newname);
    }
    Ok(())
}
