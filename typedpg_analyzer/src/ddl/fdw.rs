//! Foreign-data wrappers, foreign servers and user mappings
//! (foreigncmds.c). They don't affect typing, but foreign tables and
//! IMPORT FOREIGN SCHEMA name a server that must exist, and PG keeps the
//! names unique and the dependencies between them.

use pg_query::protobuf::{
    CreateFdwStmt, CreateForeignServerStmt, CreateUserMappingStmt, DropUserMappingStmt,
    ImportForeignSchemaStmt, RoleSpec, RoleSpecType, node,
};

use super::DdlError;
use super::util::node_string;
use crate::oid::PgClassOid;
use crate::pg_catalog::PgCatalog;

/// The foreign-data catalogs (`pg_foreign_data_wrapper`,
/// `pg_foreign_server`, `pg_user_mapping`, `pg_foreign_table.ftserver`).
#[derive(Clone, Debug, Default)]
pub(crate) struct ForeignData {
    wrappers: Vec<String>,
    /// `(server, wrapper)`.
    servers: Vec<(String, String)>,
    /// `(user, server)`.
    user_mappings: Vec<(String, String)>,
    pub(crate) table_servers: std::collections::HashMap<PgClassOid, String>,
}

fn fdw_missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("foreign-data wrapper \"{name}\" does not exist"))
}

fn server_missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("server \"{name}\" does not exist"))
}

/// get_foreign_server_oid.
pub(crate) fn check_server(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if interp.foreign_data.servers.iter().any(|(s, _)| s == name) {
        Ok(())
    } else {
        Err(server_missing(name))
    }
}

fn check_fdw(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if interp.foreign_data.wrappers.iter().any(|w| w == name) {
        Ok(())
    } else {
        Err(fdw_missing(name))
    }
}

/// The HANDLER / VALIDATOR functions (lookup_fdw_handler_func /
/// lookup_fdw_validator_func).
fn check_functions(
    interp: &PgCatalog,
    options: &[pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    for opt in options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let Some(node::Node::List(l)) = de.arg.as_deref().and_then(|a| a.node.as_ref()) else {
            continue;
        };
        let parts: Vec<&str> = l.items.iter().filter_map(node_string).collect();
        let (schema, name) = match parts.as_slice() {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            _ => continue,
        };
        let candidates = interp.find_functions(schema, name);
        match de.defname.as_str() {
            "handler" => {
                let Some(p) = candidates.iter().find(|p| p.proargtypes.is_empty()) else {
                    return Err(DdlError::TypeNotFound(format!(
                        "function {name}() does not exist"
                    )));
                };
                if interp
                    .pg_type
                    .get(&p.prorettype)
                    .map(|t| t.typname.as_str())
                    != Some("fdw_handler")
                {
                    return Err(DdlError::Parse(format!(
                        "function {name} must return type fdw_handler"
                    )));
                }
            }
            "validator" => {
                // (text[], oid)
                let wanted = [
                    crate::oid::PgTypeOid::from_raw(1009),
                    crate::pg_catalog::oid::OID,
                ];
                if !candidates.iter().any(|p| p.proargtypes == wanted) {
                    return Err(DdlError::TypeNotFound(format!(
                        "function {name}(text[], oid) does not exist"
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn create_fdw(interp: &mut PgCatalog, stmt: &CreateFdwStmt) -> Result<(), DdlError> {
    if interp.foreign_data.wrappers.contains(&stmt.fdwname) {
        return Err(DdlError::DuplicateObject(format!(
            "foreign-data wrapper \"{}\" already exists",
            stmt.fdwname
        )));
    }
    check_functions(interp, &stmt.func_options)?;
    interp.foreign_data.wrappers.push(stmt.fdwname.clone());
    Ok(())
}

pub fn alter_fdw(
    interp: &PgCatalog,
    stmt: &pg_query::protobuf::AlterFdwStmt,
) -> Result<(), DdlError> {
    check_fdw(interp, &stmt.fdwname)?;
    check_functions(interp, &stmt.func_options)
}

pub fn create_server(
    interp: &mut PgCatalog,
    stmt: &CreateForeignServerStmt,
) -> Result<(), DdlError> {
    if interp
        .foreign_data
        .servers
        .iter()
        .any(|(s, _)| *s == stmt.servername)
    {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "server \"{}\" already exists",
            stmt.servername
        )));
    }
    check_fdw(interp, &stmt.fdwname)?;
    interp
        .foreign_data
        .servers
        .push((stmt.servername.clone(), stmt.fdwname.clone()));
    Ok(())
}

pub fn alter_server(
    interp: &PgCatalog,
    stmt: &pg_query::protobuf::AlterForeignServerStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.servername)
}

/// The role a user mapping is for, as PG names it in messages.
fn role_name(role: Option<&RoleSpec>) -> String {
    match role.map(|r| RoleSpecType::try_from(r.roletype)) {
        Some(Ok(RoleSpecType::RolespecPublic)) | None => "public".to_owned(),
        Some(Ok(RoleSpecType::RolespecCstring)) => {
            role.map(|r| r.rolename.clone()).unwrap_or_default()
        }
        Some(_) => "current_user".to_owned(),
    }
}

pub fn create_user_mapping(
    interp: &mut PgCatalog,
    stmt: &CreateUserMappingStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.servername)?;
    let user = role_name(stmt.user.as_ref());
    let key = (user.clone(), stmt.servername.clone());
    if interp.foreign_data.user_mappings.contains(&key) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "user mapping for \"{user}\" already exists for server \"{}\"",
            stmt.servername
        )));
    }
    interp.foreign_data.user_mappings.push(key);
    Ok(())
}

pub fn alter_user_mapping(
    interp: &PgCatalog,
    stmt: &pg_query::protobuf::AlterUserMappingStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.servername)?;
    let user = role_name(stmt.user.as_ref());
    if !interp
        .foreign_data
        .user_mappings
        .contains(&(user.clone(), stmt.servername.clone()))
    {
        return Err(DdlError::TypeNotFound(format!(
            "user mapping for \"{user}\" does not exist for server \"{}\"",
            stmt.servername
        )));
    }
    Ok(())
}

pub fn drop_user_mapping(
    interp: &mut PgCatalog,
    stmt: &DropUserMappingStmt,
) -> Result<(), DdlError> {
    if check_server(interp, &stmt.servername).is_err() {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(server_missing(&stmt.servername));
    }
    let user = role_name(stmt.user.as_ref());
    let key = (user.clone(), stmt.servername.clone());
    let before = interp.foreign_data.user_mappings.len();
    interp.foreign_data.user_mappings.retain(|m| *m != key);
    if interp.foreign_data.user_mappings.len() == before && !stmt.missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "user mapping for \"{user}\" does not exist for server \"{}\"",
            stmt.servername
        )));
    }
    Ok(())
}

/// IMPORT FOREIGN SCHEMA: the server and the local schema must exist. The
/// imported tables come from the remote server, which the analyzer can't
/// see.
pub fn import_foreign_schema(
    interp: &PgCatalog,
    stmt: &ImportForeignSchemaStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.server_name)?;
    if interp.namespace_oid(&stmt.local_schema).is_none() {
        return Err(DdlError::TableNotFound(format!(
            "schema \"{}\" does not exist",
            stmt.local_schema
        )));
    }
    Ok(())
}

/// DROP FOREIGN DATA WRAPPER / DROP SERVER, with their dependents.
pub(crate) fn drop_foreign_object(
    interp: &mut PgCatalog,
    wrapper: bool,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let Some(name) = node_string(obj_node).map(str::to_owned) else {
        return Ok(());
    };
    let servers: Vec<String> = if wrapper {
        if check_fdw(interp, &name).is_err() {
            return if missing_ok {
                Ok(())
            } else {
                Err(fdw_missing(&name))
            };
        }
        interp
            .foreign_data
            .servers
            .iter()
            .filter(|(_, w)| *w == name)
            .map(|(s, _)| s.clone())
            .collect()
    } else {
        if check_server(interp, &name).is_err() {
            return if missing_ok {
                Ok(())
            } else {
                Err(server_missing(&name))
            };
        }
        vec![name.clone()]
    };
    let tables: Vec<PgClassOid> = interp
        .foreign_data
        .table_servers
        .iter()
        .filter(|(_, s)| servers.contains(s))
        .map(|(t, _)| *t)
        .collect();
    let has_mappings = interp
        .foreign_data
        .user_mappings
        .iter()
        .any(|(_, s)| servers.contains(s));
    let has_dependents = !tables.is_empty() || has_mappings || (wrapper && !servers.is_empty());
    if has_dependents && !cascade {
        let what = if wrapper {
            "foreign-data wrapper"
        } else {
            "server"
        };
        return Err(DdlError::DependencyError(format!(
            "cannot drop {what} {name} because other objects depend on it"
        )));
    }
    for table in tables {
        super::drop::drop_relation_by_oid(interp, table);
    }
    let fd = &mut interp.foreign_data;
    fd.user_mappings.retain(|(_, s)| !servers.contains(s));
    fd.servers.retain(|(s, _)| !servers.contains(s));
    if wrapper {
        fd.wrappers.retain(|w| *w != name);
    }
    Ok(())
}

/// ALTER FOREIGN DATA WRAPPER / SERVER ... RENAME TO.
pub(crate) fn rename_foreign_object(
    interp: &mut PgCatalog,
    wrapper: bool,
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
    let new = stmt.newname.clone();
    let fd = &mut interp.foreign_data;
    if wrapper {
        if !fd.wrappers.contains(&old) {
            return Err(fdw_missing(&old));
        }
        if fd.wrappers.contains(&new) {
            return Err(DdlError::DuplicateObject(format!(
                "foreign-data wrapper \"{new}\" already exists"
            )));
        }
        for w in fd.wrappers.iter_mut().filter(|w| **w == old) {
            w.clone_from(&new);
        }
        for (_, w) in fd.servers.iter_mut().filter(|(_, w)| *w == old) {
            w.clone_from(&new);
        }
    } else {
        if !fd.servers.iter().any(|(s, _)| *s == old) {
            return Err(server_missing(&old));
        }
        if fd.servers.iter().any(|(s, _)| *s == new) {
            return Err(DdlError::DuplicateObject(format!(
                "server \"{new}\" already exists"
            )));
        }
        for (s, _) in fd.servers.iter_mut().filter(|(s, _)| *s == old) {
            s.clone_from(&new);
        }
        for (_, s) in fd.user_mappings.iter_mut().filter(|(_, s)| *s == old) {
            s.clone_from(&new);
        }
        for s in fd.table_servers.values_mut().filter(|s| **s == old) {
            s.clone_from(&new);
        }
    }
    Ok(())
}
