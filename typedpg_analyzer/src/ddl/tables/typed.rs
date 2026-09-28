//! Typed tables (`CREATE TABLE ... OF type`, `ALTER TABLE ... [NOT] OF`):
//! the table's columns follow a stand-alone composite type, so they can
//! only change through `ALTER TYPE ... CASCADE` (`ATTypedTableRecursion`),
//! and the table depends on the type (DROP TYPE needs CASCADE).

use super::*;

/// The typed tables whose `reloftype` is `type_oid`, in creation order.
pub(crate) fn typed_tables_of(interp: &PgCatalog, type_oid: PgTypeOid) -> Vec<PgClassOid> {
    let mut tables: Vec<PgClassOid> = interp
        .typed_tables
        .iter()
        .filter(|(_, t)| **t == type_oid)
        .map(|(r, _)| *r)
        .collect();
    tables.sort();
    tables
}

/// `check_of_type`: a typed table needs a stand-alone composite type, not
/// a relation's row type. Returns the type's `typrelid`.
pub(super) fn check_of_type(
    interp: &PgCatalog,
    type_oid: PgTypeOid,
) -> Result<PgClassOid, DdlError> {
    let typrelid = interp
        .pg_type
        .get(&type_oid)
        .filter(|t| t.typtype == TypType::Composite)
        .and_then(|t| t.typrelid);
    let typname = || format_type_for_message(interp, type_oid);
    let Some(typrelid) = typrelid else {
        return Err(DdlError::Parse(format!(
            "type {} is not a composite type",
            typname()
        )));
    };
    if interp.pg_class.get(&typrelid).map(|c| c.relkind) != Some(RelKind::CompositeType) {
        return Err(DdlError::Parse(format!(
            "type {} is the row type of another table (A typed table must use a stand-alone \
             composite type created with CREATE TYPE.)",
            typname()
        )));
    }
    Ok(typrelid)
}

/// Record `relid` as a typed table of `type_oid` (`reloftype`; the
/// table's dependency on the type is derived from it).
pub(super) fn set_of_type(interp: &mut PgCatalog, relid: PgClassOid, type_oid: PgTypeOid) {
    interp.typed_tables.insert(relid, type_oid);
}

/// The ALTER TABLE restrictions on a typed table (ATPrepAddColumn,
/// ATPrepDropColumn, ATPrepAlterColumnType, ATPrepAddInherit).
pub(super) fn check_typed_table_cmd(
    interp: &PgCatalog,
    relid: PgClassOid,
    subtype: AlterTableType,
) -> Result<(), DdlError> {
    if !interp.typed_tables.contains_key(&relid) {
        return Ok(());
    }
    let msg = match subtype {
        AlterTableType::AtAddColumn => "cannot add column to typed table",
        AlterTableType::AtDropColumn => "cannot drop column from typed table",
        AlterTableType::AtAlterColumnType => "cannot alter column type of typed table",
        AlterTableType::AtAddInherit => "cannot change inheritance of typed table",
        _ => return Ok(()),
    };
    Err(DdlError::Parse(msg.into()))
}

/// The typed tables an `ALTER TYPE` of composite `relid` must also change
/// (find_typed_table_dependencies): refused unless CASCADE.
pub(crate) fn typed_table_dependents(
    interp: &PgCatalog,
    relid: PgClassOid,
    cascade: bool,
) -> Result<Vec<PgClassOid>, DdlError> {
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(Vec::new());
    };
    if class.relkind != RelKind::CompositeType {
        return Ok(Vec::new());
    }
    let Some(reltype) = class.reltype else {
        return Ok(Vec::new());
    };
    let tables = typed_tables_of(interp, reltype);
    if !tables.is_empty() && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot alter type \"{}\" because it is the type of a typed table (Use ALTER ... \
             CASCADE to alter the typed tables too.)",
            class.relname
        )));
    }
    Ok(tables)
}

/// `ALTER TABLE ... OF type` (ATExecAddOf): the table's columns must be
/// exactly the type's, in order.
pub(super) fn add_of(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::TypeName(tn)) = cmd.def.as_deref().and_then(|d| d.node.as_ref()) else {
        return Ok(());
    };
    let type_oid = lookup_type_name(tn, interp)?;
    let typrelid = check_of_type(interp, type_oid)?;
    if interp.pg_inherits.iter().any(|i| i.inhrelid == relid) {
        return Err(DdlError::Parse("typed tables cannot inherit".into()));
    }
    let relname = relname_of(interp, relid);
    let type_attrs = interp.attributes_of(typrelid).to_vec();
    let table_attrs = interp.attributes_of(relid).to_vec();
    let mut table_iter = table_attrs.iter();
    for type_attr in &type_attrs {
        let Some(table_attr) = table_iter.next() else {
            return Err(DdlError::Parse(format!(
                "table is missing column \"{}\"",
                type_attr.attname
            )));
        };
        if table_attr.attname != type_attr.attname {
            return Err(DdlError::Parse(format!(
                "table has column \"{}\" where type requires \"{}\"",
                table_attr.attname, type_attr.attname
            )));
        }
        if table_attr.atttypid != type_attr.atttypid
            || table_attr.atttypmod != type_attr.atttypmod
            || table_attr.attcollation != type_attr.attcollation
        {
            return Err(DdlError::Parse(format!(
                "table \"{relname}\" has different type for column \"{}\"",
                table_attr.attname
            )));
        }
    }
    if let Some(extra) = table_iter.next() {
        return Err(DdlError::Parse(format!(
            "table has extra column \"{}\"",
            extra.attname
        )));
    }
    set_of_type(interp, relid, type_oid);
    Ok(())
}

/// `ALTER TABLE ... NOT OF` (ATExecDropOf).
pub(super) fn drop_of(interp: &mut PgCatalog, relid: PgClassOid) -> Result<(), DdlError> {
    if !interp.typed_tables.contains_key(&relid) {
        return Err(DdlError::Parse(format!(
            "\"{}\" is not a typed table",
            relname_of(interp, relid)
        )));
    }
    interp.typed_tables.remove(&relid);
    Ok(())
}
