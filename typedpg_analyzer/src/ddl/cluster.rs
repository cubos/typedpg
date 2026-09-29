//! Objects outside the schema that migrations can still name: tablespaces,
//! subscriptions and large objects. None affects typing, but COMMENT,
//! GRANT and DROP look them up (get_object_address), so the catalog keeps
//! the ones the migrations create next to the built-in tablespaces.

use typedpg_pg_query::protobuf::{
    CreateSubscriptionStmt, CreateTableSpaceStmt, DropSubscriptionStmt, DropTableSpaceStmt,
    FuncCall, a_const, node,
};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

#[derive(Clone, Debug, Default)]
pub(crate) struct ClusterObjects {
    /// Tablespaces created by migrations (`pg_default` and `pg_global`
    /// always exist).
    tablespaces: Vec<String>,
    subscriptions: Vec<String>,
    /// Large objects created with a constant OID (`lo_create(1234)`).
    large_objects: Vec<u32>,
}

/// get_tablespace_oid.
pub(crate) fn tablespace_exists(interp: &PgCatalog, name: &str) -> bool {
    matches!(name, "pg_default" | "pg_global")
        || interp.cluster_objects.tablespaces.iter().any(|t| t == name)
}

/// get_subscription_oid.
pub(crate) fn subscription_exists(interp: &PgCatalog, name: &str) -> bool {
    interp
        .cluster_objects
        .subscriptions
        .iter()
        .any(|s| s == name)
}

/// LargeObjectExists.
pub(crate) fn large_object_exists(interp: &PgCatalog, oid: u32) -> bool {
    interp.cluster_objects.large_objects.contains(&oid)
}

pub(crate) fn create_tablespace(
    interp: &mut PgCatalog,
    stmt: &CreateTableSpaceStmt,
) -> Result<(), DdlError> {
    if tablespace_exists(interp, &stmt.tablespacename) {
        return Err(DdlError::DuplicateObject(format!(
            "tablespace \"{}\" already exists",
            stmt.tablespacename
        )));
    }
    interp
        .cluster_objects
        .tablespaces
        .push(stmt.tablespacename.clone());
    Ok(())
}

pub(crate) fn drop_tablespace(
    interp: &mut PgCatalog,
    stmt: &DropTableSpaceStmt,
) -> Result<(), DdlError> {
    let objects = &mut interp.cluster_objects.tablespaces;
    let before = objects.len();
    objects.retain(|t| *t != stmt.tablespacename);
    if objects.len() == before && !stmt.missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "tablespace \"{}\" does not exist",
            stmt.tablespacename
        )));
    }
    Ok(())
}

pub(crate) fn create_subscription(
    interp: &mut PgCatalog,
    stmt: &CreateSubscriptionStmt,
) -> Result<(), DdlError> {
    if subscription_exists(interp, &stmt.subname) {
        return Err(DdlError::DuplicateObject(format!(
            "subscription \"{}\" already exists",
            stmt.subname
        )));
    }
    interp
        .cluster_objects
        .subscriptions
        .push(stmt.subname.clone());
    Ok(())
}

pub(crate) fn alter_subscription(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if subscription_exists(interp, name) {
        Ok(())
    } else {
        Err(DdlError::TypeNotFound(format!(
            "subscription \"{name}\" does not exist"
        )))
    }
}

pub(crate) fn drop_subscription(
    interp: &mut PgCatalog,
    stmt: &DropSubscriptionStmt,
) -> Result<(), DdlError> {
    let objects = &mut interp.cluster_objects.subscriptions;
    let before = objects.len();
    objects.retain(|s| *s != stmt.subname);
    if objects.len() == before && !stmt.missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "subscription \"{}\" does not exist",
            stmt.subname
        )));
    }
    Ok(())
}

/// The large-object calls of a top-level `SELECT` with a constant OID:
/// `lo_create(oid)`, `lo_from_bytea(oid, data)` and `lo_import(file, oid)`
/// create the object, `lo_unlink(oid)` removes it. (An OID of 0, or one
/// computed at run time, names an object the analyzer can't know.)
pub(crate) fn large_object_calls(interp: &mut PgCatalog, fc: &FuncCall) {
    let name: Vec<&str> = fc
        .funcname
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    let name = match name.as_slice() {
        [name] | ["pg_catalog", name] => *name,
        _ => return,
    };
    let constant = |n: Option<&typedpg_pg_query::protobuf::Node>| match n?.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            a_const::Val::Ival(i) => u32::try_from(i.ival).ok().filter(|&v| v != 0),
            a_const::Val::Fval(f) => f.fval.parse::<u32>().ok().filter(|&v| v != 0),
            _ => None,
        },
        _ => None,
    };
    let objects = &mut interp.cluster_objects.large_objects;
    match name {
        "lo_create" | "lo_from_bytea" => {
            if let Some(oid) = constant(fc.args.first()) {
                objects.push(oid);
            }
        }
        "lo_import" => {
            if let Some(oid) = constant(fc.args.get(1)) {
                objects.push(oid);
            }
        }
        "lo_unlink" => {
            if let Some(oid) = constant(fc.args.first()) {
                objects.retain(|&o| o != oid);
            }
        }
        _ => {}
    }
}
