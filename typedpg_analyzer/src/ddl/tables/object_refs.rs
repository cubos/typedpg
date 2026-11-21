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

/// check_index_is_clusterable, on the index of `relid` named `name`.
/// Returns the index.
pub(crate) fn check_clusterable_index(
    interp: &PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<PgClassOid, DdlError> {
    let index = table_index(interp, relid, name)?.indexrelid;
    check_clusterable_index_oid(interp, relid, index)?;
    Ok(index)
}

/// check_index_is_clusterable: `index` must be a complete, valid index of
/// `relid` whose access method can cluster.
pub(crate) fn check_clusterable_index_oid(
    interp: &PgCatalog,
    relid: PgClassOid,
    index: PgClassOid,
) -> Result<(), DdlError> {
    let name = relname_of(interp, index);
    let Some(row) = interp.pg_index.get(&index).filter(|i| i.indrelid == relid) else {
        return Err(DdlError::Parse(format!(
            "\"{name}\" is not an index for table \"{}\"",
            relname_of(interp, relid)
        )));
    };
    crate::ddl::indexes::check_am_clusterable(interp, index, &name)?;
    if row.indpred.is_some() {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot cluster on partial index \"{name}\""
        )));
    }
    if interp.invalid_indexes.contains(&index) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot cluster on invalid index \"{name}\""
        )));
    }
    Ok(())
}

/// `ALTER TABLE ... CLUSTER ON index` (ATExecClusterOn): marks the index
/// `indisclustered`.
pub(super) fn cluster_on(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let index = check_clusterable_index(interp, relid, &cmd.name)?;
    check_markable_clustered(interp, relid)?;
    interp.clustered_indexes.insert(relid, index);
    Ok(())
}

/// mark_index_clustered: a partitioned table's indexes are never marked
/// clustered (nor unmarked).
pub(super) fn check_markable_clustered(
    interp: &PgCatalog,
    relid: PgClassOid,
) -> Result<(), DdlError> {
    if interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned) {
        return Err(DdlError::Parse(
            "cannot mark index clustered in partitioned table".into(),
        ));
    }
    Ok(())
}

/// `ALTER TABLE ... REPLICA IDENTITY USING INDEX index`
/// (ATExecReplicaIdentity).
pub(super) fn replica_identity(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(ast::Node::ReplicaIdentityStmt(ri)) = cmd.def.as_ref() else {
        return Ok(());
    };
    // REPLICA_IDENTITY_INDEX.
    if ri.identity_type != "i" {
        interp.replica_identity_indexes.remove(&relid);
        match ri.identity_type.as_str() {
            kind @ ("n" | "f") => {
                interp
                    .replica_identity_kinds
                    .insert(relid, kind.chars().next().unwrap_or('d'));
            }
            _ => {
                interp.replica_identity_kinds.remove(&relid);
            }
        }
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
    let relkind = interp.pg_class.get(&relid).map(|c| c.relkind);
    for &attnum in index.indkey.iter().take(key_columns) {
        if let Some(attr) = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            // A materialized view's columns are never declared NOT NULL
            // (what the analyzer infers of them aside).
            && (!attr.attnotnull || relkind == Some(RelKind::MaterializedView))
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
    let Some(ast::Node::AtAlterConstraint(alter)) = cmd.def.as_ref() else {
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
    if alter.alter_enforceability && !matches!(con.contype, ConType::ForeignKey | ConType::Check) {
        return Err(DdlError::Parse(format!(
            "cannot alter enforceability of constraint \"{}\" of relation \"{relname}\" (Only \
             foreign key and check constraints can change enforceability.)",
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
    // A partition's clone of a partitioned table's foreign key, or a row
    // derived for a referenced partition, is altered through the topmost
    // constraint it derives from.
    if let Some(parent) = foreign_keys::fk_root(interp, con.oid) {
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
    // PostgreSQL 19: a CHECK constraint's enforceability changes too, on
    // every descendant.
    if alter.alter_enforceability && con.contype == ConType::Check {
        alter_check_enforceability(interp, &con, alter.is_enforced, rec.recurse, false, &[])?;
        return Ok(());
    }
    // ATExecAlterConstraintInternal: a change of enforceability, or else
    // one of deferrability, rewrites the row (AlterConstrUpdateConstraintEntry)
    // with every attribute the command names — a constraint made ENFORCED
    // is validated, a NOT ENFORCED one is not valid — and so each clone's.
    // Naming what the constraint already is changes nothing: ENFORCED on an
    // enforced NOT VALID key leaves it NOT VALID.
    let details = interp.fk_details.get(&con.oid);
    let enforceability_changes = alter.alter_enforceability && alter.is_enforced != con.conenforced;
    let deferrability_changes = alter.alter_deferrability
        && details.is_none_or(|d| {
            (d.deferrable(), d.initdeferred()) != (alter.deferrable, alter.initdeferred)
        });
    if enforceability_changes || deferrability_changes {
        let clones = std::cell::RefCell::new(Vec::new());
        foreign_keys::for_each_fk_clone(interp, con.oid, &|c| clones.borrow_mut().push(c.oid));
        for oid in std::iter::once(con.oid).chain(clones.into_inner()) {
            if alter.alter_enforceability
                && let Some(c) = interp.pg_constraint.get_mut(&oid)
            {
                // NOT ENFORCED → ENFORCED checks the rows (phase 3); a
                // deferrability change only rewrites the row, marking an
                // enforced NOT VALID key validated unchecked.
                let unchecked = !enforceability_changes && !c.convalidated && alter.is_enforced;
                c.conenforced = alter.is_enforced;
                c.convalidated = alter.is_enforced;
                if unchecked {
                    interp.fk_validity_unproven.insert(oid);
                } else if enforceability_changes {
                    interp.fk_validity_unproven.remove(&oid);
                }
            }
            if alter.alter_deferrability
                && let Some(d) = interp.fk_details.get_mut(&oid)
            {
                d.set_deferrability(alter.deferrable, alter.initdeferred);
            }
        }
    }
    if alter.alter_inheritability {
        inherit::alter_not_null_inheritability(interp, &con, alter.noinherit)?;
    }
    Ok(())
}

/// ATExecAlterCheckConstrEnforceability (PostgreSQL 19): `con` becomes
/// [NOT] ENFORCED (and valid only when enforced), then — unless NO INHERIT —
/// so does the same-named constraint of every descendant. A constraint
/// still inherited from an enforced parent stays enforced; ALTERed directly
/// to NOT ENFORCED it is an error. `changing` holds the constraints a NOT
/// ENFORCED change visits. Returns whether `con` changed.
fn alter_check_enforceability(
    interp: &mut PgCatalog,
    con: &PgConstraint,
    is_enforced: bool,
    recurse: bool,
    recursing: bool,
    changing: &[PgConstraintOid],
) -> Result<bool, DdlError> {
    let rel = con.conrelid;
    let relname = relname_of(interp, rel);
    let partition = super::inherit_cmd::is_partition(interp, rel);
    let mut target = is_enforced;
    if !is_enforced
        && (!recursing || !partition)
        && let Some(parent) = enforced_check_parent(interp, con, changing)
    {
        if !recursing {
            return Err(DdlError::Parse(format!(
                "cannot mark inherited constraint \"{}\" as NOT ENFORCED (The matching \
                 constraint on parent table \"{}\" is ENFORCED.)",
                con.conname,
                relname_of(interp, parent)
            )));
        }
        target = true;
    }
    // The constraint is about to be marked validated: an enforced but not
    // valid descendant conflicts with it.
    if recursing && is_enforced && con.conenforced && !con.convalidated {
        return Err(DdlError::Parse(format!(
            "constraint \"{}\" conflicts with NOT VALID constraint on child table \"{relname}\" \
             (You may need to use ALTER TABLE ... VALIDATE CONSTRAINT on table \"{relname}\" \
             first.)",
            con.conname
        )));
    }
    let changed = con.conenforced != target;
    if changed && let Some(c) = interp.pg_constraint.get_mut(&con.oid) {
        c.conenforced = target;
        c.convalidated = target;
    }
    if !is_enforced || changed {
        let mut children: Vec<PgClassOid> = Vec::new();
        let mut changing = changing.to_vec();
        if !recursing && !con.connoinherit {
            children = inherit::all_inheritors(interp, rel)
                .into_iter()
                .filter(|&c| c != rel)
                .collect();
            if !is_enforced {
                changing = vec![con.oid];
            }
            for &child in &children {
                if !recurse {
                    return Err(DdlError::Parse(
                        "constraint must be altered on child tables too (Do not specify the \
                         ONLY keyword.)"
                            .into(),
                    ));
                }
                if !is_enforced {
                    let child_con =
                        table_constraint(interp, child, &con.conname).map_err(|_| {
                            DdlError::TypeNotFound(format!(
                                "constraint \"{}\" for table \"{}\" does not exist",
                                con.conname,
                                relname_of(interp, child)
                            ))
                        })?;
                    if !changing.contains(&child_con.oid) {
                        changing.push(child_con.oid);
                    }
                }
            }
        }
        for child in children {
            let child_con = table_constraint(interp, child, &con.conname)?.clone();
            alter_check_enforceability(interp, &child_con, is_enforced, false, true, &changing)?;
        }
    }
    Ok(changed)
}

/// ATCheckCheckConstrHasEnforcedParent: a parent of `con`'s table whose
/// same-named CHECK constraint is inheritable and enforced — through a
/// parent `changing` is about to change only when that one in turn has
/// such a parent (regular inheritance only: a partition has one parent).
fn enforced_check_parent(
    interp: &PgCatalog,
    con: &PgConstraint,
    changing: &[PgConstraintOid],
) -> Option<PgClassOid> {
    if con.coninhcount <= 0 {
        return None;
    }
    let partition = super::inherit_cmd::is_partition(interp, con.conrelid);
    let mut parents: Vec<_> = interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhrelid == con.conrelid)
        .collect();
    parents.sort_by_key(|i| i.inhseqno);
    for inh in parents {
        let Some(parent_con) = interp.pg_constraint.values().find(|c| {
            c.conrelid == inh.inhparent
                && c.conname == con.conname
                && c.contype == ConType::Check
                && !c.connoinherit
                && c.conenforced
        }) else {
            continue;
        };
        if !partition
            && changing.contains(&parent_con.oid)
            && enforced_check_parent(interp, parent_con, changing).is_none()
        {
            continue;
        }
        return Some(inh.inhparent);
    }
    None
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
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    use crate::ddl::triggers::TriggerState;
    let state = match cmd.subtype {
        AlterTableType::AtEnableTrig => TriggerState::Origin,
        AlterTableType::AtEnableAlwaysTrig => TriggerState::Always,
        AlterTableType::AtEnableReplicaTrig => TriggerState::Replica,
        _ => TriggerState::Disabled,
    };
    crate::ddl::triggers::set_trigger_state(interp, relid, cmd, state)
}
