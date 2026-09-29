//! `ALTER ... RENAME TO` and `ALTER ... SET SCHEMA` handlers.
//!
//! Cover the subset of `ALTER FUNCTION`, `ALTER AGGREGATE`, `ALTER PROCEDURE`,
//! `ALTER TABLE`, `ALTER TYPE`, and `ALTER DOMAIN` that changes the *identity*
//! of an object — every other attribute (STRICT, VOLATILE, owner, tablespace,
//! …) is irrelevant for static type analysis and remains a no-op.

use typedpg_pg_query::protobuf::{AlterObjectSchemaStmt, ObjectType, RenameStmt, node};

use super::DdlError;
use super::util::{node_string, resolve_type_name};
use super::views;
use crate::oid::{PgClassOid, PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, PgProc};
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
        ObjectType::ObjectFunction
        | ObjectType::ObjectProcedure
        | ObjectType::ObjectRoutine
        | ObjectType::ObjectAggregate => rename_function_like(interp, stmt, rename_type),
        ObjectType::ObjectType | ObjectType::ObjectDomain => rename_type_obj(interp, stmt),
        ObjectType::ObjectSchema => rename_schema(interp, stmt),
        // RENAME ATTRIBUTE of a composite type is a column rename of its
        // relation.
        ObjectType::ObjectColumn | ObjectType::ObjectAttribute => rename_column(interp, stmt),
        ObjectType::ObjectTabconstraint => rename_constraint(interp, stmt),
        ObjectType::ObjectDomconstraint => rename_domain_constraint(interp, stmt),
        ObjectType::ObjectTrigger | ObjectType::ObjectPolicy | ObjectType::ObjectRule => {
            let relid = stmt
                .relation
                .as_ref()
                .and_then(|rv| crate::ddl::util::lookup_relation(interp, rv).ok())
                .map(|(_, oid)| oid);
            match rename_type {
                ObjectType::ObjectTrigger => crate::ddl::triggers::rename_trigger(interp, stmt)?,
                ObjectType::ObjectPolicy => crate::ddl::policies::rename_policy(interp, stmt)?,
                _ => crate::ddl::rules::rename_rule(interp, stmt)?,
            }
            if let Some(relid) = relid {
                use crate::ddl::coldeps::Dependent;
                let name = stmt.subname.clone();
                let old = match rename_type {
                    ObjectType::ObjectTrigger => Dependent::Trigger { relid, name },
                    ObjectType::ObjectPolicy => Dependent::Policy { relid, name },
                    _ => Dependent::Rule { relid, name },
                };
                crate::ddl::coldeps::rename(interp, &old, &stmt.newname);
            }
            Ok(())
        }
        ObjectType::ObjectStatisticExt => crate::ddl::statistics::rename_statistics(interp, stmt),
        ObjectType::ObjectFdw => crate::ddl::fdw::rename_foreign_object(interp, true, stmt),
        ObjectType::ObjectPublication => crate::ddl::publications::rename_publication(interp, stmt),
        ObjectType::ObjectLanguage => crate::ddl::languages::rename_language(interp, stmt),
        ObjectType::ObjectConversion => crate::ddl::conversions::rename_conversion(interp, stmt),
        ObjectType::ObjectEventTrigger => {
            crate::ddl::event_triggers::rename_event_trigger(interp, stmt)
        }
        ObjectType::ObjectForeignServer => {
            crate::ddl::fdw::rename_foreign_object(interp, false, stmt)
        }
        ObjectType::ObjectTsconfiguration
        | ObjectType::ObjectTsdictionary
        | ObjectType::ObjectTsparser
        | ObjectType::ObjectTstemplate => {
            crate::ddl::text_search::rename(interp, rename_type, stmt)
        }
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
    let is_index_backed = matches!(
        interp.pg_constraint.get(&target_oid).map(|c| c.contype),
        Some(
            crate::pg_catalog::ConType::PrimaryKey
                | crate::pg_catalog::ConType::Unique
                | crate::pg_catalog::ConType::Exclusion
        )
    );
    // An index-backed constraint renames its index first
    // (RenameRelationInternal), then RenameConstraintById.
    if is_index_backed {
        crate::ddl::util::check_relation_name_unused(interp, nsoid, &stmt.newname)?;
    }
    check_constraint_name_free(interp, class_oid, &stmt.newname, &relname)?;
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
            Some(crate::pg_catalog::RelKind::Index | crate::pg_catalog::RelKind::PartitionedIndex)
        )
    {
        interp.rename_pg_class(idx_oid, stmt.newname.clone(), nsoid);
    }
    Ok(())
}

/// RenameConstraintById: the new name must be free among the relation's
/// constraints.
pub(crate) fn check_constraint_name_free(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
    name: &str,
    relname: &str,
) -> Result<(), DdlError> {
    if interp
        .pg_constraint
        .values()
        .any(|c| c.conrelid == relid && c.conname == name)
    {
        return Err(DdlError::DuplicateObject(format!(
            "constraint \"{name}\" for relation \"{relname}\" already exists"
        )));
    }
    Ok(())
}

/// `ALTER DOMAIN d RENAME CONSTRAINT old TO new` (get_domain_constraint_oid,
/// RenameConstraintById).
fn rename_domain_constraint(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    let Some(node::Node::List(l)) = stmt.object.as_deref().and_then(|o| o.node.as_ref()) else {
        return Ok(());
    };
    let tn = typedpg_pg_query::protobuf::TypeName {
        names: l.items.clone(),
        ..Default::default()
    };
    let type_oid = crate::ddl::util::lookup_type_name(&tn, interp)?;
    let domain = crate::ddl::util::format_type_for_message(interp, type_oid);
    let constraints = interp.domain_constraints.entry(type_oid).or_default();
    let Some(pos) = constraints.iter().position(|c| c.name == stmt.subname) else {
        return Err(DdlError::TypeNotFound(format!(
            "constraint \"{}\" for domain {domain} does not exist",
            stmt.subname
        )));
    };
    if constraints.iter().any(|c| c.name == stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "constraint \"{}\" for domain {domain} already exists",
            stmt.newname
        )));
    }
    constraints[pos].name = stmt.newname.clone();
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
    // RangeVarCallbackForAlterRelation.
    if interp.is_system_class(class_oid) {
        return Err(DdlError::Parse(format!(
            "permission denied: \"{}\" is a system catalog",
            rv.relname
        )));
    }
    check_alter_relation_kind(
        interp,
        class_oid,
        ObjectType::try_from(stmt.rename_type).unwrap_or(ObjectType::Undefined),
        false,
    )?;

    let old_name = rv.relname.clone();
    let new_name = stmt.newname.clone();
    // RenameRelationInternal: the new name must be free for the relation
    // and for its row type.
    crate::ddl::util::check_relation_name_unused(interp, nsoid, &new_name)?;

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
    if matches!(
        class.as_ref().map(|c| c.relkind),
        Some(crate::pg_catalog::RelKind::Index | crate::pg_catalog::RelKind::PartitionedIndex)
    ) && let Some(indrelid) = interp.pg_index.get(&class_oid).map(|i| i.indrelid)
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
    let cascade = stmt.behavior == typedpg_pg_query::protobuf::DropBehavior::DropCascade as i32;
    let typed = crate::ddl::tables::typed::typed_table_dependents(interp, relid, cascade)?;
    rename_column_in(interp, relid, &stmt.subname, &stmt.newname, rv.inh, 0)?;
    for table in typed {
        rename_column_in(interp, table, &stmt.subname, &stmt.newname, true, 0)?;
    }
    Ok(())
}

/// `renameatt_internal` (tablecmds.c): with recursion, every descendant is
/// renamed first — refused where the column also comes from a parent
/// outside the tree (`expected_parents`); without it, no child may have
/// the column. Then the column must exist, not be a system column, not be
/// inherited from elsewhere, and the new name must be free.
fn rename_column_in(
    interp: &mut PgCatalog,
    relid: crate::oid::PgClassOid,
    old: &str,
    new: &str,
    recurse: bool,
    expected_parents: i16,
) -> Result<(), DdlError> {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    if recurse {
        let tree = crate::ddl::tables::inherit::all_inheritors(interp, relid);
        for &child in &tree[1..] {
            let numparents = interp
                .pg_inherits
                .iter()
                .filter(|h| h.inhrelid == child && tree.contains(&h.inhparent))
                .count() as i16;
            rename_column_in(interp, child, old, new, false, numparents)?;
        }
    } else if expected_parents == 0
        && !crate::ddl::tables::inherit::children_of(interp, relid).is_empty()
    {
        return Err(DdlError::Parse(format!(
            "inherited column \"{old}\" must be renamed in child tables too"
        )));
    }
    let Some(attr) = interp.attribute_by_name(relid, old).cloned() else {
        if crate::pg_catalog::SYSTEM_COLUMNS
            .iter()
            .any(|(n, ..)| *n == old)
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot rename system column \"{old}\""
            )));
        }
        return Err(DdlError::Parse(format!("column \"{old}\" does not exist")));
    };
    if attr.attinhcount > expected_parents {
        return Err(DdlError::Parse(format!(
            "cannot rename inherited column \"{old}\""
        )));
    }
    // check_for_column_name_collision.
    if interp.attribute_by_name(relid, new).is_some() {
        return Err(DdlError::DuplicateObject(format!(
            "column \"{new}\" of relation \"{relname}\" already exists"
        )));
    }
    if crate::pg_catalog::SYSTEM_COLUMNS
        .iter()
        .any(|(n, ..)| *n == new)
    {
        return Err(DdlError::DuplicateObject(format!(
            "column name \"{new}\" conflicts with a system column name"
        )));
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
    crate::ddl::functions::rename_routine(interp, stmt, expected)
}

fn rename_type_obj(interp: &mut PgCatalog, stmt: &RenameStmt) -> Result<(), DdlError> {
    crate::ddl::types::rename_type(interp, stmt)
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
    // RenameSchema.
    if interp.namespace_oid(new).is_some() {
        return Err(DdlError::DuplicateObject(format!(
            "schema \"{new}\" already exists"
        )));
    }
    crate::ddl::schema_stmt::check_schema_name(new)?;

    interp.rename_pg_namespace(nsoid, new.clone());
    views::rewrite_views_on_schema_rename(interp, old, new);
    Ok(())
}

// ─── ALTER ... SET SCHEMA ───────────────────────────────────────────────────

pub fn set_schema(interp: &mut PgCatalog, stmt: &AlterObjectSchemaStmt) -> Result<(), DdlError> {
    let object_type = ObjectType::try_from(stmt.object_type).unwrap_or(ObjectType::Undefined);
    let new_schema = stmt.newschema.clone();
    // AlterTableNamespace looks the relation up before the new schema.
    if matches!(
        object_type,
        ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectForeignTable
            | ObjectType::ObjectSequence
    ) {
        return set_relation_schema(interp, stmt, object_type, &new_schema);
    }
    let new_nsoid = crate::ddl::util::existing_namespace(interp, &new_schema)?;

    match object_type {
        ObjectType::ObjectFunction
        | ObjectType::ObjectProcedure
        | ObjectType::ObjectRoutine
        | ObjectType::ObjectAggregate => {
            set_function_like_schema(interp, stmt, new_nsoid, object_type)
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            set_type_schema(interp, stmt, new_nsoid)
        }
        ObjectType::ObjectExtension => match stmt.object.as_deref().and_then(node_string) {
            Some(name) => {
                let name = name.to_owned();
                crate::ddl::extensions::set_extension_schema(interp, &name, new_nsoid)
            }
            None => Ok(()),
        },
        ObjectType::ObjectTsconfiguration
        | ObjectType::ObjectTsdictionary
        | ObjectType::ObjectTsparser
        | ObjectType::ObjectTstemplate => {
            crate::ddl::text_search::set_schema(interp, object_type, stmt, new_nsoid)
        }
        _ => Ok(()),
    }
}

/// RangeVarCallbackForAlterRelation (tablecmds.c): the relation kinds an
/// `ALTER {TABLE | VIEW | ...}` may name, and those SET SCHEMA can't move
/// on their own.
fn check_alter_relation_kind(
    interp: &PgCatalog,
    relid: PgClassOid,
    object_type: ObjectType,
    set_schema: bool,
) -> Result<(), DdlError> {
    use crate::pg_catalog::RelKind;
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let name = &class.relname;
    let wrong = |what: &str| Err(DdlError::Parse(format!("\"{name}\" is not {what}")));
    match (object_type, class.relkind) {
        (ObjectType::ObjectSequence, k) if k != RelKind::Sequence => return wrong("a sequence"),
        (ObjectType::ObjectView, k) if k != RelKind::View => return wrong("a view"),
        (ObjectType::ObjectMatview, k) if k != RelKind::MaterializedView => {
            return wrong("a materialized view");
        }
        (ObjectType::ObjectForeignTable, k) if k != RelKind::ForeignTable => {
            return wrong("a foreign table");
        }
        (_, RelKind::CompositeType) => {
            return Err(DdlError::Parse(format!(
                "\"{name}\" is a composite type (Use ALTER TYPE instead.)"
            )));
        }
        (_, RelKind::Index | RelKind::PartitionedIndex) if set_schema => {
            return Err(DdlError::Parse(format!(
                "cannot change schema of index \"{name}\" (Change the schema of the table \
                 instead.)"
            )));
        }
        _ => {}
    }
    Ok(())
}

/// sequenceIsOwned: the table whose column owns sequence `seq` (an auto or
/// internal dependency on the column: serial, identity, OWNED BY).
fn sequence_owner(interp: &PgCatalog, seq: PgClassOid) -> Option<PgClassOid> {
    use crate::pg_catalog::{DepType, PG_CLASS_RELID};
    let seq_obj = crate::oid::PgGenericOid::from_nonzero(seq.into_nonzero());
    interp
        .iter_pg_depend()
        .find(|d| {
            d.classid == PG_CLASS_RELID
                && d.objid == seq_obj
                && d.objsubid == 0
                && d.refclassid == PG_CLASS_RELID
                && d.refobjsubid > 0
                && matches!(d.deptype, DepType::Auto | DepType::Internal)
        })
        .and_then(|d| PgClassOid::new(d.refobjid.get()))
}

/// `ALTER TABLE ... SET SCHEMA` (AlterTableNamespace): the relation, its
/// row type, its indexes and the sequences it owns move to the schema,
/// whose names they may not clash with there. An owned sequence doesn't
/// move by itself, and nothing moves in to or out of the temporary or
/// TOAST schema.
fn set_relation_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    object_type: ObjectType,
    new_schema: &str,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (old_nsoid, class_oid) = match crate::ddl::util::lookup_relation(interp, rv) {
        Ok(found) => found,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    check_alter_relation_kind(interp, class_oid, object_type, true)?;
    let Some(class) = interp.pg_class.get(&class_oid).cloned() else {
        return Ok(());
    };
    if class.relkind == crate::pg_catalog::RelKind::Sequence
        && let Some(table) = sequence_owner(interp, class_oid)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot move an owned sequence into another schema (Sequence \"{}\" is linked to \
             table \"{}\".)",
            class.relname,
            interp
                .pg_class
                .get(&table)
                .map(|c| c.relname.as_str())
                .unwrap_or("?")
        )));
    }
    // RangeVarGetAndCheckCreationNamespace, then CheckSetNamespace.
    let temp_target = new_schema == "pg_temp";
    let new_nsoid = if temp_target {
        None
    } else {
        Some(crate::ddl::util::existing_namespace(interp, new_schema)?)
    };
    if temp_target
        || interp.temp_namespace == Some(old_nsoid)
        || (new_nsoid.is_some() && new_nsoid == interp.temp_namespace)
    {
        return Err(DdlError::Parse(
            "cannot move objects into or out of temporary schemas".into(),
        ));
    }
    let Some(new_nsoid) = new_nsoid else {
        return Ok(());
    };
    let old_schema = interp
        .namespace_name(old_nsoid)
        .unwrap_or_default()
        .to_owned();
    if new_schema == "pg_toast" || old_schema == "pg_toast" {
        return Err(DdlError::Parse(
            "cannot move objects into or out of TOAST schema".into(),
        ));
    }
    if new_nsoid == old_nsoid {
        return Ok(());
    }

    // AlterTableNamespaceInternal: the relation, its row type (and its
    // array), its indexes, its sequences — each name must be free there.
    let mut indexes: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == class_oid)
        .map(|i| i.indexrelid)
        .collect();
    indexes.sort();
    let sequences = crate::ddl::sequences::owned_sequences(interp, class_oid, None);
    let relation_taken = |interp: &PgCatalog, relid: PgClassOid| -> Result<(), DdlError> {
        let Some(c) = interp.pg_class.get(&relid) else {
            return Ok(());
        };
        if interp
            .class_by_qname
            .contains_key(&(new_nsoid, c.relname.clone()))
        {
            return Err(DdlError::DuplicateObject(format!(
                "relation \"{}\" already exists in schema \"{new_schema}\"",
                c.relname
            )));
        }
        Ok(())
    };
    relation_taken(interp, class_oid)?;
    let row_types: Vec<crate::oid::PgTypeOid> = class
        .reltype
        .into_iter()
        .flat_map(|t| std::iter::once(t).chain(interp.array_type_of(t)))
        .collect();
    for &t in &row_types {
        if let Some(typ) = interp.pg_type.get(&t)
            && interp
                .type_by_qname
                .contains_key(&(new_nsoid, typ.typname.clone()))
        {
            return Err(DdlError::DuplicateObject(format!(
                "type \"{}\" already exists in schema \"{new_schema}\"",
                typ.typname
            )));
        }
    }
    for &rel in indexes.iter().chain(sequences.iter()) {
        relation_taken(interp, rel)?;
    }

    let name = class.relname.clone();
    interp.rename_pg_class(class_oid, name.clone(), new_nsoid);
    for t in row_types {
        if let Some(typname) = interp.pg_type.get(&t).map(|t| t.typname.clone()) {
            interp.rename_pg_type(t, typname, new_nsoid);
        }
    }
    for rel in indexes.into_iter().chain(sequences) {
        if let Some(relname) = interp.pg_class.get(&rel).map(|c| c.relname.clone()) {
            interp.rename_pg_class(rel, relname, new_nsoid);
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
    crate::ddl::functions::set_routine_schema(interp, stmt, new_nsoid, expected)
}

fn set_type_schema(
    interp: &mut PgCatalog,
    stmt: &AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
) -> Result<(), DdlError> {
    crate::ddl::types::set_type_schema(interp, stmt, new_nsoid)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Extract `(schema_opt, name, arg_oids)` from an `ObjectWithArgs` target.
pub(crate) fn extract_func_target(
    object: &Option<Box<typedpg_pg_query::protobuf::Node>>,
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
