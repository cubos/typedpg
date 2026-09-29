//! Partitioned indexes. An index on a partitioned table has a matching
//! index on every partition — an equivalent existing one is attached, or a
//! clone is created (DefineIndex's recursion, AttachPartitionEnsureIndexes)
//! — and one backing a PRIMARY KEY / UNIQUE / EXCLUDE constraint gives the
//! partition an inherited constraint too. The partitions' indexes then
//! follow their parent: they can't be dropped on their own, and they
//! detach with the partition.

use super::*;

/// The constraint an index backs, if any (the constraint shares its name).
fn backing_constraint(interp: &PgCatalog, index: PgClassOid) -> Option<PgConstraint> {
    let class = interp.pg_class.get(&index)?;
    let table = interp.pg_index.get(&index)?.indrelid;
    interp
        .pg_constraint
        .values()
        .find(|c| {
            c.conrelid == table
                && c.conname == class.relname
                && matches!(
                    c.contype,
                    ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
                )
        })
        .cloned()
}

fn map_attnums(interp: &PgCatalog, from: PgClassOid, to: PgClassOid, keys: &[i16]) -> Vec<i16> {
    keys.iter()
        .map(|&an| {
            if an == 0 {
                return 0;
            }
            interp
                .attributes_of(from)
                .iter()
                .find(|a| a.attnum == an)
                .and_then(|a| interp.attribute_by_name(to, &a.attname))
                .map_or(0, |a| a.attnum)
        })
        .collect()
}

/// Give partition `part` the counterpart of `parent_index`, recursing into
/// sub-partitions.
pub(crate) fn ensure_partition_index(
    interp: &mut PgCatalog,
    parent_index: PgClassOid,
    part: PgClassOid,
) -> Result<(), DdlError> {
    let Some(pi) = interp.pg_index.get(&parent_index).cloned() else {
        return Ok(());
    };
    let parent_con = backing_constraint(interp, parent_index);
    let indkey = map_attnums(interp, pi.indrelid, part, &pi.indkey);
    let am = interp.index_access_methods.get(&parent_index).cloned();

    // An equivalent, unattached index of the partition (CompareIndexInfo).
    let existing = interp
        .pg_index
        .values()
        .filter(|i| {
            i.indrelid == part
                && i.indisunique == pi.indisunique
                && i.indkey == indkey
                && i.indexprs == pi.indexprs
                && i.indpred == pi.indpred
                && interp.index_access_methods.get(&i.indexrelid) == am.as_ref()
                && !interp.index_parents.contains_key(&i.indexrelid)
                && !interp.invalid_indexes.contains(&i.indexrelid)
        })
        .map(|i| i.indexrelid)
        .filter(|&i| backing_constraint(interp, i).is_some() == parent_con.is_some())
        .min();
    let child_index = match existing {
        Some(index) => {
            // Its constraint becomes inherited.
            if let Some(con) = backing_constraint(interp, index)
                && let Some(row) = interp.pg_constraint.get_mut(&con.oid)
            {
                row.coninhcount += 1;
                row.conislocal = false;
            }
            index
        }
        None => {
            let clone = create_clone(interp, &pi, parent_con.as_ref(), part, indkey, am)?;
            if interp.nonimmediate_indexes.contains(&parent_index) {
                interp.nonimmediate_indexes.insert(clone);
            }
            clone
        }
    };
    interp.index_parents.insert(child_index, parent_index);

    if interp.pg_class.get(&part).map(|c| c.relkind) == Some(RelKind::Partitioned) {
        for sub in inherit::children_of(interp, part) {
            ensure_partition_index(interp, child_index, sub)?;
        }
    }
    Ok(())
}

fn create_clone(
    interp: &mut PgCatalog,
    pi: &PgIndex,
    parent_con: Option<&PgConstraint>,
    part: PgClassOid,
    indkey: Vec<i16>,
    am: Option<String>,
) -> Result<PgClassOid, DdlError> {
    let Some(class) = interp.pg_class.get(&part).cloned() else {
        return Err(DdlError::Internal(format!("partition {part} missing")));
    };
    // ChooseIndexName over the partition.
    let colnames: Vec<String> = indkey
        .iter()
        .map(|&an| {
            interp
                .attributes_of(part)
                .iter()
                .find(|a| a.attnum == an)
                .map_or_else(|| "expr".to_owned(), |a| a.attname.clone())
        })
        .collect();
    let (addition, label) = match parent_con.map(|c| c.contype) {
        Some(ConType::PrimaryKey) => (String::new(), "pkey"),
        Some(ConType::Unique) => (super::super::util::index_name_addition(&colnames), "key"),
        Some(ConType::Exclusion) => (super::super::util::index_name_addition(&colnames), "excl"),
        _ => (super::super::util::index_name_addition(&colnames), "idx"),
    };
    let name = super::super::util::choose_relation_name(
        interp,
        class.relnamespace,
        &class.relname,
        &addition,
        label,
    );
    let index = PgClassOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_class(PgClass {
        oid: index,
        relname: name.clone(),
        relnamespace: class.relnamespace,
        relkind: RelKind::Index,
        reltype: None,
    });
    interp.insert_pg_index(PgIndex {
        indexrelid: index,
        indrelid: part,
        indnatts: pi.indnatts,
        indnkeyatts: pi.indnkeyatts,
        indisunique: pi.indisunique,
        indisprimary: pi.indisprimary,
        indisexclusion: pi.indisexclusion,
        indkey: indkey.clone(),
        indexprs: pi.indexprs.clone(),
        indpred: pi.indpred.clone(),
    });
    if let Some(am) = am {
        interp.index_access_methods.insert(index, am);
    }
    if let Some(con) = parent_con {
        let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
        interp.insert_pg_constraint(PgConstraint {
            oid,
            conname: name,
            conrelid: part,
            contype: con.contype,
            conkey: map_attnums(interp, con.conrelid, part, &con.conkey),
            confrelid: None,
            confkey: Vec::new(),
            conislocal: false,
            coninhcount: 1,
            conenforced: con.conenforced,
            convalidated: con.convalidated,
            connoinherit: con.connoinherit,
            conperiod: con.conperiod,
        });
    }
    Ok(index)
}

/// CREATE TABLE ... PARTITION OF / ATTACH PARTITION: the partition gets a
/// counterpart of every index of the partitioned table.
pub(crate) fn clone_parent_indexes(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
) -> Result<(), DdlError> {
    if interp.pg_class.get(&parent).map(|c| c.relkind) != Some(RelKind::Partitioned) {
        return Ok(());
    }
    let mut indexes: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == parent)
        .map(|i| i.indexrelid)
        .collect();
    indexes.sort();
    for index in indexes {
        ensure_partition_index(interp, index, part)?;
    }
    Ok(())
}

/// A new index (or constraint index) of a partitioned table reaches every
/// partition.
pub(crate) fn propagate_new_index(
    interp: &mut PgCatalog,
    table: PgClassOid,
    index: PgClassOid,
) -> Result<(), DdlError> {
    if interp.pg_class.get(&table).map(|c| c.relkind) != Some(RelKind::Partitioned) {
        return Ok(());
    }
    for part in inherit::children_of(interp, table) {
        ensure_partition_index(interp, index, part)?;
    }
    Ok(())
}

/// DETACH PARTITION: the partition's indexes (and their constraints)
/// become its own again.
pub(crate) fn detach_partition_indexes(interp: &mut PgCatalog, part: PgClassOid) {
    let attached: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == part && interp.index_parents.contains_key(&i.indexrelid))
        .map(|i| i.indexrelid)
        .collect();
    for index in attached {
        interp.index_parents.remove(&index);
        if let Some(con) = backing_constraint(interp, index)
            && let Some(row) = interp.pg_constraint.get_mut(&con.oid)
        {
            row.coninhcount = (row.coninhcount - 1).max(0);
            row.conislocal = true;
        }
    }
}

/// The index `index` hangs from, when it is a partition's copy.
pub(crate) fn parent_index_of(interp: &PgCatalog, index: PgClassOid) -> Option<PgClassOid> {
    interp.index_parents.get(&index).copied()
}

/// Every partition index below `index`, deepest first.
pub(crate) fn child_indexes(interp: &PgCatalog, index: PgClassOid) -> Vec<PgClassOid> {
    let mut out = Vec::new();
    let direct: Vec<PgClassOid> = interp
        .index_parents
        .iter()
        .filter(|(_, p)| **p == index)
        .map(|(c, _)| *c)
        .collect();
    for child in direct {
        out.extend(child_indexes(interp, child));
        out.push(child);
    }
    out
}

/// CompareIndexInfo: `child` (an index of a partition) has the definition
/// of `parent` (an index of the partitioned table) — uniqueness, access
/// method, key and INCLUDE columns (matched by name), expressions and
/// predicate.
fn index_definitions_match(interp: &PgCatalog, parent: &PgIndex, child: &PgIndex) -> bool {
    child.indisunique == parent.indisunique
        && child.indnatts == parent.indnatts
        && child.indnkeyatts == parent.indnkeyatts
        && child.indkey == map_attnums(interp, parent.indrelid, child.indrelid, &parent.indkey)
        && child.indexprs == parent.indexprs
        && child.indpred == parent.indpred
        && access_method(interp, child.indexrelid) == access_method(interp, parent.indexrelid)
}

/// An index's access method (`relam`; btree unless recorded otherwise).
fn access_method(interp: &PgCatalog, index: PgClassOid) -> &str {
    interp
        .index_access_methods
        .get(&index)
        .map_or("btree", String::as_str)
}

/// An index of a partitioned table is a partitioned index (relkind `I`).
pub(crate) fn is_partitioned_index(interp: &PgCatalog, index: PgClassOid) -> bool {
    interp
        .pg_index
        .get(&index)
        .and_then(|i| interp.pg_class.get(&i.indrelid))
        .is_some_and(|t| t.relkind == RelKind::Partitioned)
}

/// `ALTER INDEX parent ATTACH PARTITION child` (ATExecAttachPartitionIdx).
pub(crate) fn attach_partition_index(
    interp: &mut PgCatalog,
    parent_index: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(node::Node::PartitionCmd(pc)) = cmd.def.as_deref().and_then(|d| d.node.as_ref())
    else {
        return Ok(());
    };
    let Some(rv) = pc.name.as_ref() else {
        return Ok(());
    };
    // RangeVarCallbackForAttachIndex.
    let child_index = super::super::util::lookup_relation(interp, rv)?.1;
    if !matches!(
        interp.pg_class.get(&child_index).map(|c| c.relkind),
        Some(RelKind::Index | RelKind::PartitionedIndex)
    ) {
        return Err(DdlError::Parse(format!(
            "\"{}\" is not an index",
            rv.relname
        )));
    }
    let (Some(parent), Some(child)) = (
        interp.pg_index.get(&parent_index).cloned(),
        interp.pg_index.get(&child_index).cloned(),
    ) else {
        return Ok(());
    };
    let parent_name = relname_of(interp, parent_index);
    let child_name = relname_of(interp, child_index);
    let parent_table = parent.indrelid;
    let part_table = child.indrelid;
    let current = interp.index_parents.get(&child_index).copied();
    if current == Some(parent_index) {
        // Already attached: one more round of validation.
        validate_partitioned_index(interp, parent_index);
        return Ok(());
    }
    let cannot = |detail: String| {
        DdlError::Parse(format!(
            "cannot attach index \"{child_name}\" as a partition of index \"{parent_name}\" \
             ({detail})"
        ))
    };
    // refuseDupeIndexAttach.
    if interp.index_parents.iter().any(|(c, p)| {
        *p == parent_index && interp.pg_index.get(c).map(|i| i.indrelid) == Some(part_table)
    }) {
        return Err(cannot(format!(
            "Another index is already attached for partition \"{}\".",
            relname_of(interp, part_table)
        )));
    }
    if current.is_some() {
        return Err(cannot(format!(
            "Index \"{child_name}\" is already attached to another index."
        )));
    }
    if !inherit::children_of(interp, parent_table).contains(&part_table) {
        return Err(cannot(format!(
            "Index \"{child_name}\" is not an index on any partition of table \"{}\".",
            relname_of(interp, parent_table)
        )));
    }
    if !index_definitions_match(interp, &parent, &child) {
        return Err(cannot("The index definitions do not match.".into()));
    }
    let parent_con = backing_constraint(interp, parent_index);
    let child_con = backing_constraint(interp, child_index);
    if parent_con.is_some() && child_con.is_none() {
        return Err(cannot(format!(
            "The index \"{parent_name}\" belongs to a constraint in table \"{}\" but no \
             constraint exists for index \"{child_name}\".",
            relname_of(interp, parent_table)
        )));
    }
    // verifyPartitionIndexNotNull.
    if parent.indisprimary {
        let attrs = interp.attributes_of(part_table);
        let nkey = usize::try_from(child.indnkeyatts).unwrap_or(0);
        for an in child.indkey.iter().take(nkey) {
            if let Some(att) = attrs.iter().find(|a| a.attnum == *an)
                && !att.attnotnull
            {
                return Err(DdlError::Parse(format!(
                    "invalid primary key definition (Column \"{}\" of relation \"{}\" is not \
                     marked NOT NULL.)",
                    att.attname,
                    relname_of(interp, part_table)
                )));
            }
        }
    }
    // IndexSetParentIndex / ConstraintSetParentConstraint.
    interp.index_parents.insert(child_index, parent_index);
    if parent_con.is_some()
        && let Some(con) = child_con
        && let Some(row) = interp.pg_constraint.get_mut(&con.oid)
    {
        row.conislocal = false;
        row.coninhcount += 1;
    }
    validate_partitioned_index(interp, parent_index);
    Ok(())
}

/// validatePartitionedIndex: a partitioned index is valid once every
/// partition has a valid index attached to it — which may validate the
/// index it is itself attached to.
fn validate_partitioned_index(interp: &mut PgCatalog, index: PgClassOid) {
    if !interp.invalid_indexes.contains(&index) {
        return;
    }
    let Some(table) = interp.pg_index.get(&index).map(|i| i.indrelid) else {
        return;
    };
    let valid_children = interp
        .index_parents
        .iter()
        .filter(|(c, p)| **p == index && !interp.invalid_indexes.contains(*c))
        .count();
    if valid_children == inherit::children_of(interp, table).len() {
        interp.invalid_indexes.remove(&index);
        if let Some(parent) = interp.index_parents.get(&index).copied() {
            validate_partitioned_index(interp, parent);
        }
    }
}
