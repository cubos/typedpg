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

/// check_index_is_clusterable: `name` must be a non-partial index of
/// `relid`. Returns the index.
pub(crate) fn check_clusterable_index(
    interp: &PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<PgClassOid, DdlError> {
    let index = table_index(interp, relid, name)?;
    if index.indpred.is_some() {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot cluster on partial index \"{name}\""
        )));
    }
    Ok(index.indexrelid)
}

/// `ALTER TABLE ... CLUSTER ON index` (ATExecClusterOn): marks the index
/// `indisclustered`.
pub(super) fn cluster_on(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let index = check_clusterable_index(interp, relid, &cmd.name)?;
    interp.clustered_indexes.insert(relid, index);
    Ok(())
}

/// `ALTER TABLE ... REPLICA IDENTITY USING INDEX index`
/// (ATExecReplicaIdentity).
pub(super) fn replica_identity(
    interp: &mut PgCatalog,
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
        interp.replica_identity_indexes.remove(&relid);
        return Ok(());
    }
    let name = &ri.name;
    let index = table_index(interp, relid, name)?;
    if !index.indisunique {
        return Err(DdlError::Parse(format!(
            "cannot use non-unique index \"{name}\" as replica identity"
        )));
    }
    // ATExecReplicaIdentity: deferred uniqueness checks aren't usable.
    if interp.nonimmediate_indexes.contains(&index.indexrelid) {
        return Err(DdlError::Parse(format!(
            "cannot use non-immediate index \"{name}\" as replica identity"
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
    let index_oid = index.indexrelid;
    interp.replica_identity_indexes.insert(relid, index_oid);
    Ok(())
}

/// The key columns of the table's replica identity index
/// (`INDEX_ATTR_BITMAP_IDENTITY_KEY`).
pub(crate) fn replica_identity_columns(interp: &PgCatalog, relid: PgClassOid) -> Vec<i16> {
    interp
        .replica_identity_indexes
        .get(&relid)
        .and_then(|index| interp.pg_index.get(index))
        .map(|index| key_columns(index).to_vec())
        .unwrap_or_default()
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

/// `ALTER TABLE ... ALTER CONSTRAINT name [NOT] DEFERRABLE ... | [NOT]
/// ENFORCED | [NO] INHERIT` (ATExecAlterConstraint): deferrability and
/// enforceability change only on a foreign key, inheritability only on a
/// not-null constraint — not to NO INHERIT on a partitioned table or for an
/// inherited constraint.
pub(super) fn alter_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(node::Node::AtalterConstraint(alter)) =
        cmd.def.as_deref().and_then(|d| d.node.as_ref())
    else {
        return Ok(());
    };
    let relname = relname_of(interp, relid);
    let partitioned = interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned);
    if partitioned && !rec.recurse {
        return Err(DdlError::Parse(
            "constraint must be altered in child tables too (Do not specify the ONLY keyword.)"
                .into(),
        ));
    }
    let con = table_constraint(interp, relid, &alter.conname)?.clone();
    if alter.alter_deferrability && con.contype != ConType::ForeignKey {
        return Err(DdlError::Parse(format!(
            "constraint \"{}\" of relation \"{relname}\" is not a foreign key constraint",
            alter.conname
        )));
    }
    if alter.alter_enforceability && con.contype != ConType::ForeignKey {
        return Err(DdlError::Parse(format!(
            "cannot alter enforceability of constraint \"{}\" of relation \"{relname}\"",
            alter.conname
        )));
    }
    if alter.alter_inheritability && con.contype != ConType::NotNull {
        return Err(DdlError::Parse(format!(
            "constraint \"{}\" of relation \"{relname}\" is not a not-null constraint",
            alter.conname
        )));
    }
    if alter.alter_inheritability && alter.noinherit && partitioned {
        return Err(DdlError::UnsupportedDdl(format!(
            "not-null constraint \"{}\" on partitioned table \"{relname}\" cannot be NO INHERIT",
            alter.conname
        )));
    }
    // A partition's clone of a partitioned table's foreign key is altered
    // through the parent.
    if let Some(parent) = foreign_keys::fk_parent(interp, con.oid) {
        return Err(DdlError::Parse(format!(
            "cannot alter constraint \"{}\" on relation \"{relname}\" (Constraint \"{}\" is \
             derived from constraint \"{}\" of relation \"{}\". You may alter the constraint it \
             derives from instead.)",
            alter.conname,
            alter.conname,
            parent.conname,
            relname_of(interp, parent.conrelid)
        )));
    }
    if alter.alter_inheritability && alter.noinherit && con.coninhcount > 0 {
        return Err(DdlError::Parse(format!(
            "cannot alter inherited constraint \"{}\" on relation \"{relname}\"",
            alter.conname
        )));
    }
    // AlterConstrUpdateConstraintEntry: a constraint made ENFORCED is
    // validated; a NOT ENFORCED one is not valid.
    if alter.alter_enforceability {
        let enforced = alter.is_enforced;
        if let Some(c) = interp.pg_constraint.get_mut(&con.oid) {
            c.conenforced = enforced;
            c.convalidated = enforced;
        }
        foreign_keys::for_each_fk_clone(interp, con.oid, &|c| {
            c.conenforced = enforced;
            c.convalidated = enforced;
        });
    }
    if alter.alter_inheritability {
        inherit::alter_not_null_inheritability(interp, &con, alter.noinherit)?;
    }
    Ok(())
}

/// `ALTER TABLE ... VALIDATE CONSTRAINT name` (ATExecValidateConstraint):
/// foreign key, CHECK and not-null constraints can be validated, not a NOT
/// ENFORCED one. A CHECK or not-null constraint is validated on every
/// descendant too (not under ONLY while there are any) unless NO INHERIT.
pub(super) fn validate_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let con = table_constraint(interp, relid, &cmd.name)?.clone();
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
    if !con.conenforced {
        return Err(DdlError::Parse(
            "cannot validate NOT ENFORCED constraint".into(),
        ));
    }
    if con.convalidated {
        return Ok(());
    }
    match con.contype {
        ConType::NotNull => inherit::validate_not_null(interp, &con, rec),
        ConType::Check => {
            // QueueCheckConstraintValidation: the descendants' copies first.
            if !rec.recursing && !con.connoinherit {
                for child in inherit::all_inheritors(interp, relid) {
                    if child == relid {
                        continue;
                    }
                    if !rec.recurse {
                        return Err(DdlError::Parse(
                            "constraint must be validated on child tables too".into(),
                        ));
                    }
                    let child_con = interp
                        .pg_constraint
                        .values()
                        .find(|c| {
                            c.conrelid == child
                                && c.contype == ConType::Check
                                && c.conname == con.conname
                        })
                        .map(|c| c.oid);
                    if let Some(c) = child_con.and_then(|oid| interp.pg_constraint.get_mut(&oid)) {
                        c.convalidated = true;
                    }
                }
            }
            if let Some(c) = interp.pg_constraint.get_mut(&con.oid) {
                c.convalidated = true;
            }
            Ok(())
        }
        _ => {
            // QueueFKConstraintValidation: the partitions' clones too.
            if let Some(c) = interp.pg_constraint.get_mut(&con.oid) {
                c.convalidated = true;
            }
            foreign_keys::for_each_fk_clone(interp, con.oid, &|c| c.convalidated = true);
            Ok(())
        }
    }
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
