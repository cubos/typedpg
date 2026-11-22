//! CREATE SCHEMA handler.

use std::borrow::Cow;

use typedpg_pg_query::NodeBox;
use typedpg_pg_query::ast::{self, AlterTableType, ConstrType, CreateSchemaStmt, ObjectType};

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
    use typedpg_pg_query::ast::RoleSpecType;
    let role = stmt.authrole.as_ref().and_then(|r| match r.roletype {
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
        match &role {
            Some(role) => role.clone(),
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
    // NamespaceCreate: owned by `AUTHORIZATION`'s role, else the current
    // role, with that role's schema default privileges.
    let owner = match &role {
        Some(role) => interp.roles.id_or_intern(role),
        None => super::roles::current_role(interp),
    };
    if let Some(ns) = interp.namespace_oid(&name) {
        super::acl::schema_created(interp, ns, owner, false);
    }

    // The elements are created in the new schema: an explicit different
    // schema is an error (setSchemaName, parse_utilcmd.c), and they run with
    // the new schema pushed in front of the search path, so their unqualified
    // references see it first (PushOverrideSearchPath).
    for elt in &stmt.schema_elts {
        let target_schema = match elt.as_ref() {
            Some(ast::Node::CreateStmt(s)) => s.relation.as_ref().map(|r| &r.schemaname),
            Some(ast::Node::ViewStmt(s)) => s.view.as_ref().map(|r| &r.schemaname),
            Some(ast::Node::CreateSeqStmt(s)) => s.sequence.as_ref().map(|r| &r.schemaname),
            Some(ast::Node::IndexStmt(s)) => s.relation.as_ref().map(|r| &r.schemaname),
            Some(ast::Node::CreateTrigStmt(s)) => s.relation.as_ref().map(|r| &r.schemaname),
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
    let rank = |node: &ast::Node| match node {
        ast::Node::CreateSeqStmt(_) => 0,
        ast::Node::CreateStmt(_) => 1,
        ast::Node::ViewStmt(_) => 2,
        ast::Node::IndexStmt(_) => 3,
        ast::Node::CreateTrigStmt(_) => 4,
        _ => 5,
    };
    let mut elements: Vec<Cow<'_, ast::Node>> = Vec::new();
    let mut foreign_keys: Vec<Cow<'_, ast::Node>> = Vec::new();
    for node in stmt.schema_elts.iter().filter_map(|elt| elt.as_ref()) {
        match node {
            ast::Node::CreateStmt(create) => {
                let (create, fks) = split_foreign_keys(node, create)?;
                elements.push(create);
                foreign_keys.extend(fks.into_iter().map(Cow::Owned));
            }
            _ => elements.push(Cow::Borrowed(node)),
        }
    }
    elements.sort_by_key(|node| rank(node));
    // PostgreSQL 19 runs the tables' foreign keys last, as ALTER TABLE ...
    // ADD CONSTRAINT, so a table may reference one created after it.
    elements.extend(foreign_keys);
    let saved = interp.push_search_path_front(&name);
    // If the requested authorization is different from the current user,
    // temporarily set the current user so that the elements are created with
    // the correct ownership (`CreateSchemaCommand`'s
    // `SetUserIdAndSecContext(owner_uid)`).
    let saved_identity = interp.session_identity.clone();
    let owner_name = interp.roles.name(owner).to_owned();
    let current_name = interp
        .session_identity
        .current_user()
        .map(str::to_owned)
        .unwrap_or_else(|| PgCatalog::MIGRATIONS_USER.to_owned());
    if owner_name != current_name {
        interp
            .session_identity
            .set_session_role(Some(owner_name), false);
        interp.refresh_search_path();
    }
    let mut result = Ok(());
    for node in elements {
        result = super::apply_statement(interp, &node);
        if result.is_err() {
            break;
        }
    }
    interp.session_identity = saved_identity;
    interp.refresh_search_path();
    interp.restore_search_path(saved);
    result
}

/// transformCreateSchemaCreateTable (parse_utilcmd.c): a CREATE TABLE
/// inside CREATE SCHEMA loses its foreign keys — table-level ones, and a
/// column's `REFERENCES` with the attribute clauses right after it — which
/// become `ALTER TABLE ... ADD CONSTRAINT` statements run after every other
/// element. The table is cloned only when it has one.
fn split_foreign_keys<'a>(
    node: &'a ast::Node,
    create: &NodeBox<ast::CreateStmt>,
) -> Result<(Cow<'a, ast::Node>, Vec<ast::Node>), DdlError> {
    let is_fk = |n: &Option<ast::Node>| matches!(n, Some(ast::Node::Constraint(c)) if c.contype == ConstrType::ConstrForeign);
    let has_fk = create.table_elts.iter().any(|elt| match elt {
        Some(ast::Node::ColumnDef(cd)) => cd.constraints.iter().any(is_fk),
        other => is_fk(other),
    });
    if !has_fk {
        return Ok((Cow::Borrowed(node), Vec::new()));
    }
    let add_constraint = |constr: ast::Constraint| {
        ast::Node::AlterTableStmt(NodeBox::new(ast::AlterTableStmt {
            relation: create.relation.clone(),
            cmds: vec![Some(ast::Node::AlterTableCmd(NodeBox::new(
                ast::AlterTableCmd {
                    subtype: AlterTableType::AtAddConstraint,
                    def: Some(ast::Node::Constraint(NodeBox::new(constr))),
                    ..Default::default()
                },
            )))],
            objtype: ObjectType::ObjectTable,
            missing_ok: false,
        }))
    };
    let mut fks = Vec::new();
    let mut new_stmt = (**create).clone();
    let mut new_elts = Vec::with_capacity(create.table_elts.len());
    for elt in &create.table_elts {
        match elt {
            Some(ast::Node::Constraint(c)) if c.contype == ConstrType::ConstrForeign => {
                fks.push(add_constraint((**c).clone()));
            }
            Some(ast::Node::ColumnDef(cd)) if cd.constraints.iter().any(is_fk) => {
                // The attribute clauses fold into the constraint before them
                // (transformConstraintAttrs), so the k-th non-attribute node
                // is `folded[k]`.
                let mut folded = super::tables::fold_constraint_attrs(&cd.constraints)?.into_iter();
                let mut kept = Vec::with_capacity(cd.constraints.len());
                let mut after_fk = false;
                for n in &cd.constraints {
                    let Some(ast::Node::Constraint(c)) = n else {
                        kept.push(n.clone());
                        continue;
                    };
                    match c.contype {
                        ConstrType::ConstrAttrDeferrable
                        | ConstrType::ConstrAttrNotDeferrable
                        | ConstrType::ConstrAttrDeferred
                        | ConstrType::ConstrAttrImmediate
                        | ConstrType::ConstrAttrEnforced
                        | ConstrType::ConstrAttrNotEnforced => {
                            if !after_fk {
                                kept.push(n.clone());
                            }
                        }
                        ConstrType::ConstrForeign => {
                            let mut fk = folded.next().expect("one folded node per constraint");
                            fk.fk_attrs =
                                vec![Some(ast::Node::String(NodeBox::new(ast::String {
                                    sval: cd.colname.clone(),
                                })))];
                            fks.push(add_constraint(fk));
                            after_fk = true;
                        }
                        _ => {
                            folded.next();
                            kept.push(n.clone());
                            after_fk = false;
                        }
                    }
                }
                let mut new_cd = (**cd).clone();
                new_cd.constraints = kept;
                new_elts.push(Some(ast::Node::ColumnDef(NodeBox::new(new_cd))));
            }
            other => new_elts.push(other.clone()),
        }
    }
    new_stmt.table_elts = new_elts;
    Ok((
        Cow::Owned(ast::Node::CreateStmt(NodeBox::new(new_stmt))),
        fks,
    ))
}
