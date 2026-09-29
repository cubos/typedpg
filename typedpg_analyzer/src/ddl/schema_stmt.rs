//! CREATE SCHEMA handler.

use typedpg_pg_query::protobuf::{CreateSchemaStmt, node};

use super::DdlError;
use super::util::ensure_namespace;
use crate::pg_catalog::PgCatalog;

/// CreateSchemaCommand / RenameSchema: the `pg_` prefix is reserved for
/// system schemas (IsReservedName), even for a superuser.
pub(crate) fn check_schema_name(name: &str) -> Result<(), DdlError> {
    if name.starts_with("pg_") {
        return Err(DdlError::Parse(format!(
            "unacceptable schema name \"{name}\" (The prefix \"pg_\" is reserved for system \
             schemas.)"
        )));
    }
    Ok(())
}

/// `CREATE SCHEMA [IF NOT EXISTS] name [AUTHORIZATION role] [elements]`
/// (`CreateSchemaCommand`, schemacmds.c).
pub fn create_schema(interp: &mut PgCatalog, stmt: &CreateSchemaStmt) -> Result<(), DdlError> {
    // get_rolespec_oid: the owner role must exist — `public` never does.
    // CURRENT_USER / CURRENT_ROLE / SESSION_USER are known once the
    // migrations name them (SET ROLE / SET SESSION AUTHORIZATION).
    use typedpg_pg_query::protobuf::RoleSpecType;
    let role =
        stmt.authrole
            .as_ref()
            .and_then(|r| match RoleSpecType::try_from(r.roletype).ok()? {
                RoleSpecType::RolespecCstring => Some(Ok(r.rolename.clone())),
                RoleSpecType::RolespecPublic => Some(Err("public".to_owned())),
                RoleSpecType::RolespecCurrentRole | RoleSpecType::RolespecCurrentUser => interp
                    .session_identity
                    .current_user()
                    .map(|u| Ok(u.to_owned())),
                RoleSpecType::RolespecSessionUser => interp
                    .session_identity
                    .session_user()
                    .map(|u| Ok(u.to_owned())),
                RoleSpecType::Undefined => None,
            });
    let role = match role {
        Some(Ok(name)) if super::session::role_may_exist(&name) => Some(name),
        Some(Ok(name)) | Some(Err(name)) => {
            return Err(DdlError::TypeNotFound(format!(
                "role \"{name}\" does not exist"
            )));
        }
        None => None,
    };
    // `CREATE SCHEMA AUTHORIZATION role` names the schema after the role.
    let name = if stmt.schemaname.is_empty() {
        match role {
            Some(role) => role,
            None => return Ok(()),
        }
    } else {
        stmt.schemaname.clone()
    };
    check_schema_name(&name)?;
    if interp.namespace_oid(&name).is_some() {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "schema \"{name}\" already exists"
        )));
    }
    ensure_namespace(interp, &name)?;

    // The elements are created in the new schema: an explicit different
    // schema is an error (setSchemaName, parse_utilcmd.c), and they run with
    // the new schema pushed in front of the search path, so their unqualified
    // references see it first (PushOverrideSearchPath).
    for elt in &stmt.schema_elts {
        let target_schema = match elt.node.as_ref() {
            Some(node::Node::CreateStmt(s)) => s.relation.as_ref().map(|r| &r.schemaname),
            Some(node::Node::ViewStmt(s)) => s.view.as_ref().map(|r| &r.schemaname),
            Some(node::Node::CreateSeqStmt(s)) => s.sequence.as_ref().map(|r| &r.schemaname),
            Some(node::Node::IndexStmt(s)) => s.relation.as_ref().map(|r| &r.schemaname),
            Some(node::Node::CreateTrigStmt(s)) => s.relation.as_ref().map(|r| &r.schemaname),
            _ => None,
        };
        if let Some(schema) = target_schema
            && !schema.is_empty()
            && *schema != name
        {
            return Err(DdlError::Parse(format!(
                "CREATE specifies a schema ({schema}) different from the one being created ({name})"
            )));
        }
    }
    // transformCreateSchemaStmtElements runs them grouped by kind so that
    // no element refers to one created later: sequences, tables, views,
    // indexes, triggers, then grants — each group in the order given.
    let rank = |node: &node::Node| match node {
        node::Node::CreateSeqStmt(_) => 0,
        node::Node::CreateStmt(_) => 1,
        node::Node::ViewStmt(_) => 2,
        node::Node::IndexStmt(_) => 3,
        node::Node::CreateTrigStmt(_) => 4,
        _ => 5,
    };
    let mut elements: Vec<&node::Node> = stmt
        .schema_elts
        .iter()
        .filter_map(|elt| elt.node.as_ref())
        .collect();
    elements.sort_by_key(|node| rank(node));
    let saved = interp.push_search_path_front(&name);
    let mut result = Ok(());
    for node in elements {
        result = super::apply_statement(interp, node);
        if result.is_err() {
            break;
        }
    }
    interp.restore_search_path(saved);
    result
}
