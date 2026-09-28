//! `ALTER ... RENAME TO` and `ALTER ... SET SCHEMA` handlers.
//!
//! Cover the subset of `ALTER FUNCTION`, `ALTER AGGREGATE`, `ALTER PROCEDURE`,
//! `ALTER TABLE`, `ALTER TYPE`, and `ALTER DOMAIN` that changes the *identity*
//! of an object — every other attribute (STRICT, VOLATILE, owner, tablespace,
//! …) is irrelevant for static type analysis and remains a no-op.

use pg_query::protobuf::{AlterObjectSchemaStmt, ObjectType, RenameStmt, node};

use super::DdlError;
use super::util::{ensure_namespace, node_string, resolve_type_name};
use super::views;
use crate::oid::{PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, PgProc, ProKind};
use crate::qualified_name::QualifiedName;

// ─── ALTER ... RENAME TO ────────────────────────────────────────────────────

pub fn rename(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let rename_type = ObjectType::try_from(stmt.rename_type).unwrap_or(ObjectType::Undefined);

    match rename_type {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectSequence
        | ObjectType::ObjectForeignTable
        | ObjectType::ObjectIndex => rename_relation(interp, stmt),
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectAggregate => {
            rename_function_like(interp, stmt, rename_type)
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => rename_type_obj(interp, stmt),
        ObjectType::ObjectSchema => rename_schema(interp, stmt),
        // RENAME ATTRIBUTE of a composite type is a column rename of its
        // relation.
        ObjectType::ObjectColumn | ObjectType::ObjectAttribute => rename_column(interp, stmt),
        ObjectType::ObjectTabconstraint | ObjectType::ObjectDomconstraint => {
            rename_constraint(interp, stmt)
        }
        ObjectType::ObjectTrigger => crate::ddl::triggers::rename_trigger(interp, stmt),
        ObjectType::ObjectPolicy => crate::ddl::policies::rename_policy(interp, stmt),
        _ => Ok(()),
    }
}

fn rename_constraint(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (schema_name, relname) = crate::ddl::util::range_var_names(rv, interp);
    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(&schema_name, &relname).to_string(),
        ));
    };
    let Some(class_oid) = interp
        .class_by_qname
        .get(&(nsoid, relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(&schema_name, &relname).to_string(),
        ));
    };

    // PG (RENAME CONSTRAINT) emits `constraint "x" for table "t" does not
    // exist` (note "for table", not "of relation" — DROP CONSTRAINT uses the
    // latter wording, RENAME uses the former).
    let target_oid = interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == class_oid && c.conname == stmt.subname)
        .map(|c| c.oid);
    let Some(target_oid) = target_oid else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "constraint \"{}\" for table \"{relname}\" does not exist",
            stmt.subname,
        )));
    };

    let old_name = stmt.subname.clone();
    if interp.pg_constraint.get(&target_oid).map(|c| c.contype)
        == Some(crate::pg_catalog::ConType::Check)
    {
        return crate::ddl::tables::check_inherit::rename_check(
            interp,
            class_oid,
            &old_name,
            &stmt.newname,
            rv.inh,
        );
    }
    let is_pkey_or_unique = matches!(
        interp.pg_constraint.get(&target_oid).map(|c| c.contype),
        Some(crate::pg_catalog::ConType::PrimaryKey | crate::pg_catalog::ConType::Unique)
    );
    if let Some(row) = interp.pg_constraint.get_mut(&target_oid) {
        row.conname = stmt.newname.clone();
    }
    // The backing index for PK/UNIQUE shares its name with the constraint
    // (PG conflates them). Rename the pg_class entry so subsequent DROP
    // INDEX / SQL references find the index under its new name too.
    if is_pkey_or_unique
        && let Some(idx_oid) = interp.class_by_qname.get(&(nsoid, old_name)).copied()
        && matches!(
            interp.pg_class.get(&idx_oid).map(|c| c.relkind),
            Some(crate::pg_catalog::RelKind::Index)
        )
    {
        interp.rename_pg_class(idx_oid, stmt.newname.clone(), nsoid);
    }
    Ok(())
}

fn rename_relation(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (schema_name, _) = crate::ddl::util::range_var_names(rv, interp);
    let found = crate::ddl::util::lookup_relation(interp, rv);
    let (nsoid, class_oid) = match found {
        Ok(found) => found,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };

    let old_name = rv.relname.clone();
    let new_name = stmt.newname.clone();
    // RenameRelationInternal: the new name must be free for the relation
    // and for its row type.
    crate::ddl::util::check_relation_name_free(interp, nsoid, &new_name)?;

    let class = interp.pg_class.get(&class_oid).cloned();
    interp.rename_pg_class(class_oid, new_name.clone(), nsoid);

    // The relation's row type (and its array) follow the rename.
    if let Some(type_oid) = class.as_ref().and_then(|c| c.reltype) {
        interp.rename_pg_type(type_oid, new_name.clone(), nsoid);
        if let Some(arr_oid) = interp.array_type_of(type_oid) {
            interp.rename_pg_type(arr_oid, format!("_{new_name}"), nsoid);
        }
    }
    // Renaming a constraint's index renames the constraint too
    // (RenameRelationInternal → RenameConstraintById).
    if class.as_ref().map(|c| c.relkind) == Some(crate::pg_catalog::RelKind::Index)
        && let Some(indrelid) = interp.pg_index.get(&class_oid).map(|i| i.indrelid)
    {
        for c in interp.pg_constraint.values_mut() {
            if c.conrelid == indrelid && c.conname == old_name {
                c.conname = new_name.clone();
            }
        }
    }

    views::rewrite_views_on_table_rename(interp, &schema_name, &old_name, &schema_name, &new_name);

    Ok(())
}

fn rename_column(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let schema_name = crate::ddl::util::range_var_names(rv, interp).0;
    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(&schema_name, &rv.relname).to_string(),
        ));
    };
    let Some(relid) = interp
        .class_by_qname
        .get(&(nsoid, rv.relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(&schema_name, &rv.relname).to_string(),
        ));
    };

    // renameatt_check / find_typed_table_dependencies.
    if interp.typed_tables.contains_key(&relid) {
        return Err(DdlError::Parse(
            "cannot rename column of typed table".into(),
        ));
    }
    let cascade = stmt.behavior == pg_query::protobuf::DropBehavior::DropCascade as i32;
    let typed = crate::ddl::tables::typed::typed_table_dependents(interp, relid, cascade)?;
    rename_column_in(interp, relid, &stmt.subname, &stmt.newname, rv.inh, false)?;
    for table in typed {
        rename_column_in(interp, table, &stmt.subname, &stmt.newname, true, true)?;
    }
    Ok(())
}

/// `renameatt_internal` (tablecmds.c): the column must exist and the new
/// name be free; an inherited column is renamed only through its parent,
/// whose rename reaches every child (`ONLY` is refused while a child has
/// the column).
fn rename_column_in(
    interp: &mut PgCatalog,
    relid: crate::oid::PgClassOid,
    old: &str,
    new: &str,
    recurse: bool,
    recursing: bool,
) -> Result<(), DdlError> {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let Some(attr) = interp.attribute_by_name(relid, old).cloned() else {
        return Err(DdlError::Parse(format!("column \"{old}\" does not exist")));
    };
    if !recursing && attr.attinhcount > 0 {
        return Err(DdlError::Parse(format!(
            "cannot rename inherited column \"{old}\""
        )));
    }
    if interp.attribute_by_name(relid, new).is_some() {
        return Err(DdlError::DuplicateObject(format!(
            "column \"{new}\" of relation \"{relname}\" already exists"
        )));
    }
    let children: Vec<crate::oid::PgClassOid> =
        crate::ddl::tables::inherit::children_of(interp, relid)
            .into_iter()
            .filter(|&c| interp.attribute_by_name(c, old).is_some())
            .collect();
    if !recurse && !children.is_empty() {
        return Err(DdlError::Parse(format!(
            "inherited column \"{old}\" must be renamed in child tables too"
        )));
    }
    for child in children {
        rename_column_in(interp, child, old, new, true, true)?;
    }
    if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(col) = attrs.iter_mut().find(|c| c.attname == old)
    {
        col.attname = new.to_owned();
    }
    views::rewrite_views_on_column_rename(interp, relid, old, new);
    Ok(())
}

fn rename_function_like(
    interp: &mut PgCatalog,
    stmt: &RenameStmt,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema_opt, old_name, arg_oids)) = extract_func_target(&stmt.object, interp) else {
        return Ok(());
    };

    let want_kind = match expected {
        ObjectType::ObjectAggregate => ProKind::Aggregate,
        ObjectType::ObjectProcedure => ProKind::Procedure,
        _ => ProKind::Function,
    };
    let matches_kind = move |k: ProKind| {
        matches!(
            (want_kind, k),
            (ProKind::Function, ProKind::Function)
                | (ProKind::Function, ProKind::Window)
                | (ProKind::Procedure, ProKind::Procedure)
                | (ProKind::Aggregate, ProKind::Aggregate)
        )
    };
    let matches = move |p: &PgProc| matches_kind(p.prokind) && p.proargtypes == arg_oids;

    let Some((nsoid, oid)) = find_proc(interp, schema_opt.as_deref(), &old_name, &matches) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "{} {old_name} does not exist for the requested argument types",
            match expected {
                ObjectType::ObjectAggregate => "aggregate",
                ObjectType::ObjectProcedure => "procedure",
                _ => "function",
            }
        )));
    };

    interp.rename_pg_proc(oid, stmt.newname.clone(), nsoid);
    Ok(())
}

fn rename_type_obj(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_deref() else {
        return Ok(());
    };
    let parts: Vec<&str> = match object.node.as_ref() {
        Some(node::Node::TypeName(tn)) => tn.names.iter().filter_map(node_string).collect(),
        Some(node::Node::List(list)) => list.items.iter().filter_map(node_string).collect(),
        _ => return Ok(()),
    };

    let (schema_name, old_name) = match parts.as_slice() {
        [s, n] => ((*s).to_owned(), (*n).to_owned()),
        [n] => (
            crate::ddl::util::type_lookup_schema(interp, n),
            (*n).to_owned(),
        ),
        _ => return Ok(()),
    };

    let Some(nsoid) = interp.namespace_oid(&schema_name) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(
            QualifiedName::new(&schema_name, &old_name).to_string(),
        ));
    };
    let Some(&type_oid) = interp.type_by_qname.get(&(nsoid, old_name.clone())) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(
            QualifiedName::new(&schema_name, &old_name).to_string(),
        ));
    };

    let new_name = stmt.newname.clone();
    interp.rename_pg_type(type_oid, new_name.clone(), nsoid);

    let arr_old = format!("_{old_name}");
    if let Some(&arr_oid) = interp.type_by_qname.get(&(nsoid, arr_old)) {
        interp.rename_pg_type(arr_oid, format!("_{new_name}"), nsoid);
    }
    Ok(())
}

fn rename_schema(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let old = &stmt.subname;
    let new = &stmt.newname;

    let Some(nsoid) = interp.namespace_oid(old) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "schema \"{old}\" does not exist"
        )));
    };

    interp.rename_pg_namespace(nsoid, new.clone());
    views::rewrite_views_on_schema_rename(interp, old, new);
    Ok(())
}

// ─── ALTER ... SET SCHEMA ───────────────────────────────────────────────────

pub fn set_schema(interp: &mut PgCatalog, stmt: &AlterObjectSchemaStmt) -> Result<(), DdlError> {
    let object_type = ObjectType::try_from(stmt.object_type).unwrap_or(ObjectType::Undefined);
    let new_schema = stmt.newschema.clone();
    let new_nsoid = ensure_namespace(interp, &new_schema)?;

    match object_type {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectForeignTable
        | ObjectType::ObjectSequence => set_relation_schema(interp, stmt, new_nsoid, &new_schema),
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectAggregate => {
            set_function_like_schema(interp, stmt, new_nsoid, object_type)
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            set_type_schema(interp, stmt, new_nsoid)
        }
        _ => Ok(()),
    }
}

fn set_relation_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
    new_schema: &str,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let old_schema = crate::ddl::util::range_var_names(rv, interp).0;
    let Some(old_nsoid) = interp.namespace_oid(&old_schema) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(&old_schema, &rv.relname).to_string(),
        ));
    };
    let Some(class_oid) = interp
        .class_by_qname
        .get(&(old_nsoid, rv.relname.clone()))
        .copied()
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(&old_schema, &rv.relname).to_string(),
        ));
    };

    let name = rv.relname.clone();
    interp.rename_pg_class(class_oid, name.clone(), new_nsoid);

    if let Some(&type_oid) = interp.type_by_qname.get(&(old_nsoid, name.clone())) {
        interp.rename_pg_type(type_oid, name.clone(), new_nsoid);
        let arr_key = format!("_{name}");
        if let Some(&arr_oid) = interp.type_by_qname.get(&(old_nsoid, arr_key.clone())) {
            interp.rename_pg_type(arr_oid, arr_key, new_nsoid);
        }
    }

    views::rewrite_views_on_table_rename(interp, &old_schema, &name, new_schema, &name);
    Ok(())
}

fn set_function_like_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
    expected: ObjectType,
) -> Result<(), DdlError> {
    let Some((schema_opt, name, arg_oids)) = extract_func_target(&stmt.object, interp) else {
        return Ok(());
    };
    let want_kind = match expected {
        ObjectType::ObjectAggregate => ProKind::Aggregate,
        ObjectType::ObjectProcedure => ProKind::Procedure,
        _ => ProKind::Function,
    };
    let matches_kind = move |k: ProKind| {
        matches!(
            (want_kind, k),
            (ProKind::Function, ProKind::Function)
                | (ProKind::Function, ProKind::Window)
                | (ProKind::Procedure, ProKind::Procedure)
                | (ProKind::Aggregate, ProKind::Aggregate)
        )
    };
    let matches = move |p: &PgProc| matches_kind(p.prokind) && p.proargtypes == arg_oids;

    let Some((_, oid)) = find_proc(interp, schema_opt.as_deref(), &name, &matches) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "{} {name} does not exist for the requested argument types",
            match expected {
                ObjectType::ObjectAggregate => "aggregate",
                ObjectType::ObjectProcedure => "procedure",
                _ => "function",
            }
        )));
    };

    interp.rename_pg_proc(oid, name, new_nsoid);
    Ok(())
}

fn set_type_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_deref() else {
        return Ok(());
    };
    let parts: Vec<&str> = match object.node.as_ref() {
        Some(node::Node::TypeName(tn)) => tn.names.iter().filter_map(node_string).collect(),
        Some(node::Node::List(list)) => list.items.iter().filter_map(node_string).collect(),
        _ => return Ok(()),
    };
    let (old_schema, name) = match parts.as_slice() {
        [s, n] => ((*s).to_owned(), (*n).to_owned()),
        [n] => (
            crate::ddl::util::type_lookup_schema(interp, n),
            (*n).to_owned(),
        ),
        _ => return Ok(()),
    };

    let Some(old_nsoid) = interp.namespace_oid(&old_schema) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(
            QualifiedName::new(&old_schema, &name).to_string(),
        ));
    };
    let Some(&type_oid) = interp.type_by_qname.get(&(old_nsoid, name.clone())) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(
            QualifiedName::new(&old_schema, &name).to_string(),
        ));
    };

    interp.rename_pg_type(type_oid, name.clone(), new_nsoid);

    let arr_key = format!("_{name}");
    if let Some(&arr_oid) = interp.type_by_qname.get(&(old_nsoid, arr_key.clone())) {
        interp.rename_pg_type(arr_oid, arr_key, new_nsoid);
    }

    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Extract `(schema_opt, name, arg_oids)` from an `ObjectWithArgs` target.
pub(crate) fn extract_func_target(
    object: &Option<Box<pg_query::protobuf::Node>>,
    interp: &PgCatalog,
) -> Option<(Option<String>, String, Vec<PgTypeOid>)> {
    let node = object.as_deref()?;
    let owa = match node.node.as_ref()? {
        node::Node::ObjectWithArgs(owa) => owa,
        _ => return None,
    };

    let parts: Vec<&str> = owa.objname.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [s, n] => (Some((*s).to_owned()), (*n).to_owned()),
        [n] => (None, (*n).to_owned()),
        _ => return None,
    };

    let arg_oids: Vec<PgTypeOid> = owa
        .objargs
        .iter()
        .filter_map(|n| {
            if let Some(node::Node::TypeName(tn)) = n.node.as_ref() {
                resolve_type_name(tn, interp)
            } else {
                None
            }
        })
        .collect();

    Some((schema, name, arg_oids))
}

/// Resolve `(nspoid, oid)` of a `pg_proc` row matching `predicate`, walking
/// the search path when `schema` is `None`.
pub(crate) fn find_proc(
    snapshot: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    matches: &dyn Fn(&PgProc) -> bool,
) -> Option<(PgNamespaceOid, PgProcOid)> {
    let candidate_schemas: Vec<PgNamespaceOid> = if let Some(s) = schema {
        snapshot.namespace_oid(s).into_iter().collect()
    } else {
        let mut v = Vec::new();
        if let Some(pg_oid) = snapshot.namespace_oid("pg_catalog")
            && !snapshot.search_path.contains(&pg_oid)
        {
            v.push(pg_oid);
        }
        v.extend(snapshot.search_path.iter().copied());
        v
    };
    for nsoid in candidate_schemas {
        if let Some(oids) = snapshot.proc_by_qname.get(&(nsoid, name.to_owned())) {
            for &oid in oids {
                if let Some(p) = snapshot.pg_proc.get(&oid)
                    && matches(p)
                {
                    return Some((nsoid, oid));
                }
            }
        }
    }
    None
}
