//! `ALTER TABLE ... INHERIT parent` / `NO INHERIT parent`
//! (ATExecAddInherit / CreateInheritance, ATExecDropInherit /
//! RemoveInheritance): an existing table joins or leaves a parent. Its
//! columns and CHECK / not-null constraints must already match the
//! parent's; they are then counted as inherited.

use super::*;

/// `relispartition`: the relation is a partition of a partitioned table.
pub(super) fn is_partition(interp: &PgCatalog, relid: PgClassOid) -> bool {
    interp.pg_inherits.iter().any(|i| {
        i.inhrelid == relid
            && interp
                .pg_class
                .get(&i.inhparent)
                .is_some_and(|c| c.relkind == RelKind::Partitioned)
    })
}

fn parent_rangevar(cmd: &AlterTableCmd) -> Option<&typedpg_pg_query::protobuf::RangeVar> {
    match cmd.def.as_deref().and_then(|d| d.node.as_ref()) {
        Some(node::Node::RangeVar(rv)) => Some(rv),
        _ => None,
    }
}

use super::inherit::all_inheritors;

pub(super) fn add_inherit(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(rv) = parent_rangevar(cmd) else {
        return Ok(());
    };
    // ATPrepAddInherit.
    if is_partition(interp, relid) {
        return Err(DdlError::Parse(
            "cannot change inheritance of a partition".into(),
        ));
    }
    if interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned) {
        return Err(DdlError::Parse(
            "cannot change inheritance of partitioned table".into(),
        ));
    }
    let parent = super::super::util::lookup_relation(interp, rv)?.1;
    let parent_class = interp.pg_class.get(&parent).cloned();
    match parent_class.as_ref().map(|c| c.relkind) {
        Some(RelKind::Table | RelKind::ForeignTable) => {}
        Some(RelKind::Partitioned) => {
            return Err(DdlError::Parse(format!(
                "cannot inherit from partitioned table \"{}\"",
                rv.relname
            )));
        }
        Some(kind) => {
            let kinds = kind.plural();
            return Err(DdlError::Parse(format!(
                "ALTER action INHERIT cannot be performed on relation \"{}\" (This operation \
                 is not supported for {kinds}.)",
                rv.relname
            )));
        }
        None => return Ok(()),
    }
    if is_partition(interp, parent) {
        return Err(DdlError::Parse("cannot inherit from a partition".into()));
    }
    // ATExecAddInherit: permanent tables can't inherit from temporary ones.
    if super::constraints::persistence(interp, parent) == 't'
        && super::constraints::persistence(interp, relid) != 't'
    {
        return Err(DdlError::Parse(format!(
            "cannot inherit from temporary relation \"{}\"",
            rv.relname
        )));
    }
    let child_name = relname_of(interp, relid);
    let parent_name = relname_of(interp, parent);
    if all_inheritors(interp, relid).contains(&parent) {
        return Err(DdlError::DuplicateObject(format!(
            "circular inheritance not allowed (\"{parent_name}\" is already a child of \
             \"{child_name}\".)"
        )));
    }
    // CreateInheritance.
    if interp
        .pg_inherits
        .iter()
        .any(|i| i.inhrelid == relid && i.inhparent == parent)
    {
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{parent_name}\" would be inherited from more than once"
        )));
    }
    create_inheritance(interp, relid, parent, &child_name, false)
}

/// CreateInheritance: merge the child's columns and constraints into the
/// parent's and record the `pg_inherits` row.
fn create_inheritance(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    parent: PgClassOid,
    child_name: &str,
    partition: bool,
) -> Result<(), DdlError> {
    merge_attributes_into_existing(interp, relid, parent, child_name, partition)?;
    merge_constraints_into_existing(interp, relid, parent, child_name, partition)?;
    let inhseqno = interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhrelid == relid)
        .map(|i| i.inhseqno)
        .max()
        .unwrap_or(0)
        + 1;
    interp.pg_inherits.push(PgInherits {
        inhrelid: relid,
        inhparent: parent,
        inhseqno,
    });
    Ok(())
}

/// MergeAttributesIntoExisting: every parent column must exist in the child
/// with the same type, collation, NOT NULL-ness and generation.
fn merge_attributes_into_existing(
    interp: &mut PgCatalog,
    child: PgClassOid,
    parent: PgClassOid,
    child_name: &str,
    partition: bool,
) -> Result<(), DdlError> {
    let parent_attrs = interp.attributes_of(parent).to_vec();
    for pa in &parent_attrs {
        let Some(ca) = interp.attribute_by_name(child, &pa.attname).cloned() else {
            return Err(DdlError::Parse(format!(
                "child table is missing column \"{}\"",
                pa.attname
            )));
        };
        if ca.atttypid != pa.atttypid || ca.atttypmod != pa.atttypmod {
            return Err(DdlError::Parse(format!(
                "child table \"{child_name}\" has different type for column \"{}\"",
                pa.attname
            )));
        }
        if ca.attcollation != pa.attcollation {
            return Err(DdlError::Parse(format!(
                "child table \"{child_name}\" has different collation for column \"{}\"",
                pa.attname
            )));
        }
        // A parent's inheritable not-null constraint needs one here.
        if pa.attnotnull
            && !ca.attnotnull
            && inherit::not_null_constraint(interp, parent, pa.attnum)
                .is_none_or(|c| !c.connoinherit)
        {
            return Err(DdlError::Parse(format!(
                "column \"{}\" in child table \"{child_name}\" must be marked NOT NULL",
                pa.attname
            )));
        }
        // The child column is generated if and only if the parent column
        // is, and of the same kind.
        match (pa.attgenerated, ca.attgenerated) {
            (Some(_), None) => {
                return Err(DdlError::Parse(format!(
                    "column \"{}\" in child table must be a generated column",
                    pa.attname
                )));
            }
            (None, Some(_)) => {
                return Err(DdlError::Parse(format!(
                    "column \"{}\" in child table must not be a generated column",
                    pa.attname
                )));
            }
            (Some(parent_kind), Some(child_kind)) if parent_kind != child_kind => {
                let kind_name = |g: AttGenerated| match g {
                    AttGenerated::Stored => "STORED",
                    AttGenerated::Virtual => "VIRTUAL",
                };
                return Err(DdlError::Parse(format!(
                    "column \"{}\" inherits from generated column of different kind (Parent \
                     column is {}, child column is {}.)",
                    pa.attname,
                    kind_name(parent_kind),
                    kind_name(child_kind)
                )));
            }
            _ => {}
        }
    }
    if let Some(attrs) = interp.pg_attribute.get_mut(&child) {
        for a in attrs.iter_mut() {
            if parent_attrs.iter().any(|pa| pa.attname == a.attname) {
                a.attinhcount += 1;
                // A partition's columns are never local.
                if partition {
                    a.attislocal = false;
                }
            }
        }
    }
    Ok(())
}

/// MergeConstraintsIntoExisting: every inheritable CHECK of the parent
/// must exist in the child with the same definition; the child's matching
/// CHECK and not-null constraints become inherited.
fn merge_constraints_into_existing(
    interp: &mut PgCatalog,
    child: PgClassOid,
    parent: PgClassOid,
    child_name: &str,
    partition: bool,
) -> Result<(), DdlError> {
    let mut parent_cons: Vec<PgConstraint> = interp
        .pg_constraint
        .values()
        .filter(|c| c.conrelid == parent && matches!(c.contype, ConType::Check | ConType::NotNull))
        .cloned()
        .collect();
    parent_cons.sort_by(|a, b| a.conname.cmp(&b.conname));
    let mut merged = Vec::new();
    for pc in &parent_cons {
        // A NO INHERIT constraint is not inherited.
        if pc.connoinherit {
            continue;
        }
        let parent_def = interp.check_defs.get(&pc.oid).cloned();
        let child_con = if pc.contype == ConType::NotNull {
            // Not-null constraints match by column.
            let Some(colname) = pc.conkey.first().and_then(|&an| {
                interp
                    .attributes_of(parent)
                    .iter()
                    .find(|a| a.attnum == an)
                    .map(|a| a.attname.clone())
            }) else {
                continue;
            };
            let found = interp
                .attribute_by_name(child, &colname)
                .and_then(|a| inherit::not_null_constraint(interp, child, a.attnum))
                .cloned();
            let Some(con) = found else {
                return Err(DdlError::Parse(format!(
                    "column \"{colname}\" in child table \"{child_name}\" must be marked NOT NULL"
                )));
            };
            con
        } else {
            let Some(con) = interp
                .pg_constraint
                .values()
                .find(|c| {
                    c.conrelid == child && c.contype == ConType::Check && c.conname == pc.conname
                })
                .cloned()
            else {
                return Err(DdlError::Parse(format!(
                    "child table is missing constraint \"{}\"",
                    pc.conname
                )));
            };
            let child_def = interp.check_defs.get(&con.oid);
            if let (Some(p), Some(c)) = (parent_def.as_ref(), child_def)
                && p.expr != c.expr
            {
                return Err(DdlError::Parse(format!(
                    "child table \"{child_name}\" has different definition for check \
                     constraint \"{}\"",
                    pc.conname
                )));
            }
            con
        };
        if child_con.connoinherit {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{}\" conflicts with non-inherited constraint on child table \
                 \"{child_name}\"",
                child_con.conname
            )));
        }
        if pc.convalidated && child_con.conenforced && !child_con.convalidated {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{}\" conflicts with NOT VALID constraint on child table \
                 \"{child_name}\"",
                child_con.conname
            )));
        }
        if pc.conenforced && !child_con.conenforced {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{}\" conflicts with NOT ENFORCED constraint on child table \
                 \"{child_name}\"",
                child_con.conname
            )));
        }
        merged.push(child_con.oid);
    }
    for oid in merged {
        if let Some(row) = interp.pg_constraint.get_mut(&oid) {
            row.coninhcount += 1;
            if partition {
                row.conislocal = false;
            }
        }
    }
    Ok(())
}

pub(super) fn drop_inherit(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(rv) = parent_rangevar(cmd) else {
        return Ok(());
    };
    if is_partition(interp, relid) {
        return Err(DdlError::Parse(
            "cannot change inheritance of a partition".into(),
        ));
    }
    let parent = super::super::util::lookup_relation(interp, rv)?.1;
    remove_inheritance(interp, relid, parent, "parent")
}

/// `ALTER TABLE parent ATTACH PARTITION name FOR VALUES ...`
/// (ATExecAttachPartition).
pub(super) fn attach_partition(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::PartitionCmd(pc)) = cmd.def.as_deref().and_then(|d| d.node.as_ref())
    else {
        return Ok(());
    };
    let Some(rv) = pc.name.as_ref() else {
        return Ok(());
    };
    let attach = super::super::util::lookup_relation(interp, rv)?.1;
    let attach_kind = interp.pg_class.get(&attach).map(|c| c.relkind);
    if !matches!(
        attach_kind,
        Some(RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable)
    ) {
        let kinds = attach_kind.map_or("this relation", RelKind::plural);
        return Err(DdlError::Parse(format!(
            "ALTER action ATTACH PARTITION cannot be performed on relation \"{}\" (This \
             operation is not supported for {kinds}.)",
            rv.relname
        )));
    }
    let attach_name = relname_of(interp, attach);
    let parent_name = relname_of(interp, parent);
    if is_partition(interp, attach) {
        return Err(DdlError::Parse(format!(
            "\"{attach_name}\" is already a partition"
        )));
    }
    if interp.typed_tables.contains_key(&attach) {
        return Err(DdlError::Parse(
            "cannot attach a typed table as partition".into(),
        ));
    }
    if interp.pg_inherits.iter().any(|i| i.inhrelid == attach) {
        return Err(DdlError::Parse(
            "cannot attach inheritance child as partition".into(),
        ));
    }
    if interp.pg_inherits.iter().any(|i| i.inhparent == attach)
        && attach_kind != Some(RelKind::Partitioned)
    {
        return Err(DdlError::Parse(
            "cannot attach inheritance parent as partition".into(),
        ));
    }
    if all_inheritors(interp, attach).contains(&parent) {
        return Err(DdlError::DuplicateObject(format!(
            "circular inheritance not allowed (\"{parent_name}\" is already a child of \
             \"{attach_name}\".)"
        )));
    }
    // ATExecAttachPartition: a permanent table's partitions are permanent,
    // a temporary one's temporary.
    let parent_temp = super::super::util::is_temp_relation(interp, parent);
    let attach_temp = super::super::util::is_temp_relation(interp, attach);
    if !parent_temp && attach_temp {
        return Err(DdlError::Parse(format!(
            "cannot attach a temporary relation as partition of permanent relation \
             \"{parent_name}\""
        )));
    }
    if parent_temp && !attach_temp {
        return Err(DdlError::Parse(format!(
            "cannot attach a permanent relation as partition of temporary relation \
             \"{parent_name}\""
        )));
    }
    // The partition may have neither an identity column nor columns the
    // parent lacks.
    for a in interp.attributes_of(attach) {
        if a.attidentity.is_some() {
            return Err(DdlError::Parse(format!(
                "table \"{attach_name}\" being attached contains an identity column \"{}\" \
                 (The new partition may not contain an identity column.)",
                a.attname
            )));
        }
        if interp.attribute_by_name(parent, &a.attname).is_none() {
            return Err(DdlError::Parse(format!(
                "table \"{attach_name}\" contains column \"{}\" not found in parent \
                 \"{parent_name}\" (The new partition may contain only the columns present \
                 in parent.)",
                a.attname
            )));
        }
    }
    if let Some(bound) = pc.bound.as_ref() {
        super::partbound::add_partition_bound(interp, parent, attach, bound)?;
    }
    create_inheritance(interp, attach, parent, &attach_name, true)?;
    // MergeAttributesIntoExisting: a partition's column shares its
    // parent's identity.
    let identities: Vec<(String, AttIdentity)> = interp
        .attributes_of(parent)
        .iter()
        .filter_map(|a| a.attidentity.map(|i| (a.attname.clone(), i)))
        .collect();
    if let Some(attrs) = interp.pg_attribute.get_mut(&attach) {
        for (name, identity) in identities {
            if let Some(a) = attrs.iter_mut().find(|a| a.attname == name) {
                a.attidentity = Some(identity);
                a.atthasdef = true;
            }
        }
    }
    super::foreign_keys::clone_parent_fks(interp, parent, attach)?;
    // AttachPartitionEnsureIndexes.
    super::partidx::clone_parent_indexes(interp, parent, attach)?;
    crate::ddl::triggers::clone_row_triggers_to_partition(interp, parent, attach)
}

/// `ALTER TABLE parent DETACH PARTITION name` (ATExecDetachPartition).
pub(super) fn detach_partition(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::PartitionCmd(pc)) = cmd.def.as_deref().and_then(|d| d.node.as_ref())
    else {
        return Ok(());
    };
    let Some(rv) = pc.name.as_ref() else {
        return Ok(());
    };
    // ATExecDetachPartition: the default partition's constraint would
    // change under a concurrent detach.
    if pc.concurrent
        && inherit::children_of(interp, parent).iter().any(|c| {
            matches!(
                interp.partition_bounds.get(c),
                Some(super::partbound::Bound::Default)
            )
        })
    {
        return Err(DdlError::Parse(
            "cannot detach partitions concurrently when a default partition exists".into(),
        ));
    }
    let part = super::super::util::lookup_relation(interp, rv)?.1;
    // DetachAddConstraintIfNeeded: a partition detached concurrently keeps
    // its partition constraint as a CHECK constraint.
    let partition_check = if pc.concurrent
        && interp
            .pg_inherits
            .iter()
            .any(|i| i.inhrelid == part && i.inhparent == parent)
    {
        super::partbound::partition_constraint_check(interp, parent, part)
    } else {
        None
    };
    super::foreign_keys::detach_fks(interp, parent, part);
    remove_inheritance(interp, part, parent, "partition")?;
    super::partidx::detach_partition_indexes(interp, part);
    crate::ddl::triggers::drop_cloned_triggers(interp, part);
    interp.partition_bounds.remove(&part);
    // DetachPartitionFinalize: the identity was the parent's.
    if let Some(attrs) = interp.pg_attribute.get_mut(&part) {
        for a in attrs.iter_mut().filter(|a| a.attidentity.is_some()) {
            a.attidentity = None;
            a.atthasdef = false;
        }
    }
    if let Some((expr, vars)) = partition_check {
        // The constraint is parsed from SQL built for it.
        let _barrier = crate::error::DiagContextGuard::barrier();
        add_detached_partition_check(interp, part, expr, &vars)?;
    }
    Ok(())
}

/// DetachAddConstraintIfNeeded: the partition constraint becomes a CHECK
/// constraint (named as AddRelationNewConstraints names one: after its one
/// column, if it reads one), unless the partition already has it.
fn add_detached_partition_check(
    interp: &mut PgCatalog,
    part: PgClassOid,
    expr: typedpg_pg_query::protobuf::Node,
    vars: &[String],
) -> Result<(), DdlError> {
    let stored = super::check_inherit::StoredExpr::written(&expr);
    if interp.pg_constraint.values().any(|c| {
        c.conrelid == part
            && c.contype == ConType::Check
            && interp
                .check_defs
                .get(&c.oid)
                .is_some_and(|d| d.expr == stored)
    }) {
        return Ok(());
    }
    let addition = match vars {
        [one] => one.clone(),
        _ => String::new(),
    };
    let name = inherit::choose_constraint_name(interp, part, &addition, "check");
    let mut conkey: Vec<i16> = vars
        .iter()
        .filter_map(|v| interp.attribute_by_name(part, v).map(|a| a.attnum))
        .collect();
    conkey.sort_unstable();
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: name,
        conrelid: part,
        contype: ConType::Check,
        conkey,
        confrelid: None,
        confkey: Vec::new(),
        conislocal: true,
        coninhcount: 0,
        conenforced: true,
        convalidated: true,
        connoinherit: false,
        conperiod: false,
    });
    let mut def = super::check_inherit::CheckDef::new(stored, false);
    def.cook(interp, part);
    interp.check_defs.insert(oid, def);
    Ok(())
}

/// `ALTER TABLE parent DETACH PARTITION part FINALIZE`
/// (ATExecDetachPartitionFinalize): completes a concurrent detach that was
/// interrupted. A migration's concurrent detach always completes, so none
/// is ever pending: DeleteInheritsTuple reports a partition as having no
/// pending detach, RemoveInheritance any other relation as not a
/// partition.
pub(super) fn detach_partition_finalize(
    interp: &PgCatalog,
    parent: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::PartitionCmd(pc)) = cmd.def.as_deref().and_then(|d| d.node.as_ref())
    else {
        return Ok(());
    };
    let Some(rv) = pc.name.as_ref() else {
        return Ok(());
    };
    let part = super::super::util::lookup_relation(interp, rv)?.1;
    let part_name = relname_of(interp, part);
    if interp
        .pg_inherits
        .iter()
        .any(|i| i.inhrelid == part && i.inhparent == parent)
    {
        return Err(DdlError::Parse(format!(
            "cannot complete detaching partition \"{part_name}\" (There's no pending \
             concurrent detach.)"
        )));
    }
    Err(DdlError::TableNotFound(format!(
        "relation \"{part_name}\" is not a partition of relation \"{}\"",
        relname_of(interp, parent)
    )))
}

/// RemoveInheritance: drop the `pg_inherits` row and give back the
/// inherited columns and constraints to the child.
fn remove_inheritance(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    parent: PgClassOid,
    role: &str,
) -> Result<(), DdlError> {
    let Some(pos) = interp
        .pg_inherits
        .iter()
        .position(|i| i.inhrelid == relid && i.inhparent == parent)
    else {
        let (a, b) = if role == "partition" {
            (relname_of(interp, relid), relname_of(interp, parent))
        } else {
            (relname_of(interp, parent), relname_of(interp, relid))
        };
        return Err(DdlError::TableNotFound(format!(
            "relation \"{a}\" is not a {role} of relation \"{b}\""
        )));
    };
    interp.pg_inherits.remove(pos);

    // The columns the parent contributed.
    let parent_cols: Vec<String> = interp
        .attributes_of(parent)
        .iter()
        .map(|a| a.attname.clone())
        .collect();
    if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
        for a in attrs.iter_mut() {
            if a.attinhcount > 0 && parent_cols.contains(&a.attname) {
                a.attinhcount -= 1;
                if a.attinhcount == 0 {
                    a.attislocal = true;
                }
            }
        }
    }

    // Its CHECK constraints (by name) and not-null constraints (by column).
    let parent_cons: Vec<PgConstraint> = interp
        .pg_constraint
        .values()
        .filter(|c| c.conrelid == parent && matches!(c.contype, ConType::Check | ConType::NotNull))
        .filter(|c| !interp.check_defs.get(&c.oid).is_some_and(|d| d.no_inherit))
        .cloned()
        .collect();
    let mut targets = Vec::new();
    for pc in &parent_cons {
        let child_con = if pc.contype == ConType::NotNull {
            pc.conkey
                .first()
                .and_then(|&an| {
                    let name = &interp
                        .attributes_of(parent)
                        .iter()
                        .find(|a| a.attnum == an)?
                        .attname;
                    interp.attribute_by_name(relid, name).map(|a| a.attnum)
                })
                .and_then(|attnum| inherit::not_null_constraint(interp, relid, attnum))
                .map(|c| c.oid)
        } else {
            interp
                .pg_constraint
                .values()
                .find(|c| {
                    c.conrelid == relid && c.contype == ConType::Check && c.conname == pc.conname
                })
                .map(|c| c.oid)
        };
        targets.extend(child_con);
    }
    for oid in targets {
        if let Some(row) = interp.pg_constraint.get_mut(&oid)
            && row.coninhcount > 0
        {
            row.coninhcount -= 1;
            if row.coninhcount == 0 {
                row.conislocal = true;
            }
        }
    }
    Ok(())
}
