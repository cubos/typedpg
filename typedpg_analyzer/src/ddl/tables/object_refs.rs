//! ALTER TABLE subcommands that name another object of the table — an
//! index (`CLUSTER ON`, `REPLICA IDENTITY USING INDEX`), a constraint
//! (`ALTER` / `VALIDATE CONSTRAINT`) or a trigger (`ENABLE` / `DISABLE
//! TRIGGER`). They don't change types, but PG resolves the object and
//! checks it suits the operation.

use super::*;

/// The index `name` in the table's schema (`get_relname_relid`), which
/// must belong to `relid`.
fn table_index<'a>(
    interp: &'a PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<&'a PgIndex, DdlError> {
    let relname = relname_of(interp, relid);
    let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
    let found = nsoid.and_then(|ns| interp.class_by_qname.get(&(ns, name.to_owned())).copied());
    let Some(index_oid) = found else {
        return Err(DdlError::TypeNotFound(format!(
            "index \"{name}\" for table \"{relname}\" does not exist"
        )));
    };
    match interp.pg_index.get(&index_oid) {
        Some(index) if index.indrelid == relid => Ok(index),
        _ => Err(DdlError::Parse(format!(
            "\"{name}\" is not an index for table \"{relname}\""
        ))),
    }
}

/// `ALTER TABLE ... CLUSTER ON index` (check_index_is_clusterable).
pub(super) fn cluster_on(
    interp: &PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let index = table_index(interp, relid, &cmd.name)?;
    if index.indpred.is_some() {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot cluster on partial index \"{}\"",
            cmd.name
        )));
    }
    Ok(())
}

/// `ALTER TABLE ... REPLICA IDENTITY USING INDEX index`
/// (ATExecReplicaIdentity).
pub(super) fn replica_identity(
    interp: &PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::ReplicaIdentityStmt(ri)) =
        cmd.def.as_deref().and_then(|d| d.node.as_ref())
    else {
        return Ok(());
    };
    // REPLICA_IDENTITY_INDEX.
    if ri.identity_type != "i" {
        return Ok(());
    }
    let name = &ri.name;
    let index = table_index(interp, relid, name)?;
    if !index.indisunique {
        return Err(DdlError::Parse(format!(
            "cannot use non-unique index \"{name}\" as replica identity"
        )));
    }
    if index.indkey.contains(&0) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot use expression index \"{name}\" as replica identity"
        )));
    }
    if index.indpred.is_some() {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot use partial index \"{name}\" as replica identity"
        )));
    }
    let key_columns = usize::try_from(index.indnkeyatts).unwrap_or(0);
    for &attnum in index.indkey.iter().take(key_columns) {
        if let Some(attr) = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            && !attr.attnotnull
        {
            return Err(DdlError::Parse(format!(
                "index \"{name}\" cannot be used as replica identity because column \"{}\" \
                 is nullable",
                attr.attname
            )));
        }
    }
    Ok(())
}

fn table_constraint<'a>(
    interp: &'a PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<&'a PgConstraint, DdlError> {
    interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && c.conname == name)
        .ok_or_else(|| {
            DdlError::TypeNotFound(format!(
                "constraint \"{name}\" of relation \"{}\" does not exist",
                relname_of(interp, relid)
            ))
        })
}

/// `ALTER TABLE ... ALTER CONSTRAINT name [NOT] DEFERRABLE ...`
/// (ATExecAlterConstraint): only foreign keys change deferrability.
pub(super) fn alter_constraint(
    interp: &PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::Constraint(c)) = cmd.def.as_deref().and_then(|d| d.node.as_ref()) else {
        return Ok(());
    };
    let con = table_constraint(interp, relid, &c.conname)?;
    if con.contype != ConType::ForeignKey {
        return Err(DdlError::Parse(format!(
            "constraint \"{}\" of relation \"{}\" is not a foreign key constraint",
            c.conname,
            relname_of(interp, relid)
        )));
    }
    Ok(())
}

/// `ALTER TABLE ... VALIDATE CONSTRAINT name` (ATExecValidateConstraint):
/// foreign key, CHECK and not-null constraints can be validated.
pub(super) fn validate_constraint(
    interp: &PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let con = table_constraint(interp, relid, &cmd.name)?;
    if !matches!(
        con.contype,
        ConType::ForeignKey | ConType::Check | ConType::NotNull
    ) {
        return Err(DdlError::Parse(format!(
            "cannot validate constraint \"{}\" of relation \"{}\" (This operation is not \
             supported for this type of constraint.)",
            cmd.name,
            relname_of(interp, relid)
        )));
    }
    Ok(())
}

/// `ALTER TABLE ... ENABLE / DISABLE [ALWAYS | REPLICA] TRIGGER name`
/// (EnableDisableTrigger).
pub(super) fn enable_disable_trigger(
    interp: &PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let exists = interp
        .triggers
        .get(&relid)
        .is_some_and(|ts| ts.iter().any(|t| t.name == cmd.name));
    if !exists {
        return Err(DdlError::TypeNotFound(format!(
            "trigger \"{}\" for table \"{}\" does not exist",
            cmd.name,
            relname_of(interp, relid)
        )));
    }
    Ok(())
}
