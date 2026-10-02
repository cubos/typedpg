//! FOREIGN KEY constraints: `ATAddForeignKeyConstraint` (tablecmds.c),
//! which both CREATE TABLE (through transformFKConstraints) and ALTER TABLE
//! ADD CONSTRAINT reach — including PG 18's temporal foreign keys (`PERIOD`
//! on the last column of each side, referencing a WITHOUT OVERLAPS key).

use super::*;

/// The built-in btree operator families with cross-type equality: a
/// foreign key may pair any two of a family's types (`get_opfamily_member`
/// finds an `=` taking the referenced column's opclass type and the
/// referencing column's type).
const BTREE_CROSS_TYPE_FAMILIES: &[&[PgTypeOid]] = {
    use crate::pg_catalog::oid;
    &[
        &[oid::INT2, oid::INT4, oid::INT8],
        &[oid::FLOAT4, oid::FLOAT8],
        &[oid::DATE, oid::TIMESTAMP, oid::TIMESTAMPTZ],
        &[oid::TEXT, oid::NAME],
    ]
};

/// Resolve a list of column names of `relid` (transformColumnNameList).
fn column_attnums(
    interp: &PgCatalog,
    relid: PgClassOid,
    names: &[String],
) -> Result<Vec<(i16, PgTypeOid)>, DdlError> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| match interp.attribute_by_name(relid, name) {
            // transformColumnNameList: at most INDEX_MAX_KEYS columns.
            Some(_) if i >= crate::ddl::indexes::INDEX_MAX_KEYS => {
                Err(DdlError::UnsupportedDdl(format!(
                    "cannot have more than {} keys in a foreign key",
                    crate::ddl::indexes::INDEX_MAX_KEYS
                )))
            }
            Some(a) => Ok((a.attnum, a.atttypid)),
            None if crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .any(|(n, ..)| *n == name) =>
            {
                Err(DdlError::Parse(
                    "system columns cannot be used in foreign keys".into(),
                ))
            }
            None => Err(DdlError::Parse(format!(
                "column \"{name}\" referenced in foreign key constraint does not exist"
            ))),
        })
        .collect()
}

fn node_names(nodes: &[typedpg_pg_query::protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(crate::ddl::util::node_string)
        .map(str::to_owned)
        .collect()
}

/// The unique index the referenced columns name
/// (transformFkeyCheckAttrs): as many key columns, the same set — the
/// PERIOD column last in a temporal one — unique (or, for a temporal
/// foreign key, an exclusion index), not partial, no expressions, not
/// deferrable. Returns whether it is a WITHOUT OVERLAPS index.
fn check_referenced_attrs(
    interp: &PgCatalog,
    target: PgClassOid,
    target_name: &str,
    attnums: &[i16],
    with_period: bool,
) -> Result<bool, DdlError> {
    for (i, a) in attnums.iter().enumerate() {
        if attnums[i + 1..].contains(a) {
            return Err(DdlError::Parse(
                "foreign key referenced-columns list must not contain duplicates".into(),
            ));
        }
    }
    let wanted: std::collections::BTreeSet<i16> = attnums.iter().copied().collect();
    // transformFkeyCheckAttrs: invalid indexes are out.
    let mut indexes: Vec<&PgIndex> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == target && !interp.invalid_indexes.contains(&i.indexrelid))
        .collect();
    indexes.sort_by_key(|i| i.indexrelid);
    let mut found_deferrable = false;
    for index in indexes {
        let key = key_columns(index);
        if key.len() != attnums.len()
            || !(if with_period {
                index.indisexclusion
            } else {
                index.indisunique
            })
            || index.indpred.is_some()
            || !index.indexprs.is_empty()
            || key
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                != wanted
        {
            continue;
        }
        // The last attribute of the index must be the PERIOD part.
        if with_period && key.last() != attnums.last() {
            continue;
        }
        if interp.nonimmediate_indexes.contains(&index.indexrelid) {
            found_deferrable = true;
            continue;
        }
        return Ok(index.indisexclusion);
    }
    // Constraint-backed keys, for catalogs whose constraints carry no
    // pg_index row.
    if !with_period
        && !found_deferrable
        && interp.pg_constraint.values().any(|c| {
            c.conrelid == target
                && matches!(c.contype, ConType::PrimaryKey | ConType::Unique)
                && !c.conperiod
                && c.conkey
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == wanted
        })
    {
        return Ok(false);
    }
    Err(if found_deferrable {
        DdlError::Parse(format!(
            "cannot use a deferrable unique constraint for referenced table \"{target_name}\""
        ))
    } else {
        DdlError::DependencyError(format!(
            "there is no unique constraint matching given keys for referenced table \
             \"{target_name}\""
        ))
    })
}

/// Whether a referencing column of type `fktype` can reference one of type
/// `pktype` through the referenced index's operator class — for a temporal
/// key's GiST opclass or a polymorphic btree one, only the same type (both
/// must resolve the polymorphic input alike); for a concrete btree opclass,
/// a cross-type member of its family or implicit casts of both types to
/// the opclass type.
pub(super) fn fk_types_compatible(
    interp: &PgCatalog,
    pktype: PgTypeOid,
    fktype: PgTypeOid,
    gist: bool,
) -> bool {
    let pkbase = interp.unwrap_domain(pktype);
    let fkbase = interp.unwrap_domain(fktype);
    if pkbase == fkbase {
        return true;
    }
    if gist {
        return false;
    }
    let Some(opcintype) = crate::ddl::opclass::default_opclass_intype(interp, pkbase, "btree")
    else {
        return false;
    };
    if interp
        .pg_type
        .get(&opcintype)
        .is_some_and(|t| t.typtype == TypType::Pseudo)
    {
        return false;
    }
    if BTREE_CROSS_TYPE_FAMILIES
        .iter()
        .any(|family| family.contains(&opcintype) && family.contains(&fkbase))
    {
        return true;
    }
    use crate::coerce::{CoercionContext, can_coerce};
    can_coerce(pktype, opcintype, CoercionContext::Implicit, interp)
        && can_coerce(fktype, opcintype, CoercionContext::Implicit, interp)
}

/// ATAddForeignKeyConstraint: validate FOREIGN KEY `c` of `relid` over
/// the local columns `fk_names` and record it. `created` is CREATE TABLE's
/// case, where the constraint is valid unless NOT ENFORCED.
pub(super) fn add_foreign_key(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
    fk_names: &[String],
    default_name: ConName,
    created: bool,
    recurse: bool,
) -> Result<(), DdlError> {
    let relname = relname_of(interp, relid);
    // ATExecAddConstraint: an explicit name must be free on the table —
    // CREATE TABLE's foreign keys are added after its other constraints.
    if !c.conname.is_empty()
        && interp
            .pg_constraint
            .values()
            .any(|x| x.conrelid == relid && x.conname == c.conname)
    {
        return Err(DdlError::DuplicateObject(format!(
            "constraint \"{}\" for relation \"{relname}\" already exists",
            c.conname
        )));
    }
    let conname = ConName::from_explicit(&c.conname, default_name).resolve(interp, relid);

    // The referenced table.
    let pkrv = c
        .pktable
        .as_ref()
        .ok_or_else(|| DdlError::Parse(format!("FOREIGN KEY on {relname} without REFERENCES")))?;
    let (target_schema, target_name) = range_var_names(pkrv, interp);
    let target = interp
        .namespace_oid(&target_schema)
        .and_then(|ns| {
            interp
                .class_by_qname
                .get(&(ns, target_name.clone()))
                .copied()
        })
        .ok_or_else(|| {
            let shown = if pkrv.schemaname.is_empty() {
                target_name.clone()
            } else {
                QualifiedName::new(&target_schema, &target_name).to_string()
            };
            DdlError::TableNotFound(format!(
                "relation \"{shown}\" does not exist (referenced by foreign key constraint \
                 \"{conname}\")"
            ))
        })?;
    if !recurse && interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned) {
        return Err(DdlError::Parse(format!(
            "cannot use ONLY for foreign key on partitioned table \"{relname}\" referencing \
             relation \"{target_name}\""
        )));
    }
    if !matches!(
        interp.pg_class.get(&target).map(|c| c.relkind),
        Some(RelKind::Table | RelKind::Partitioned)
    ) {
        return Err(DdlError::Parse(format!(
            "referenced relation \"{target_name}\" is not a table"
        )));
    }
    check_fk_persistence(interp, relid, c)?;

    // The referencing columns.
    let fk = column_attnums(interp, relid, fk_names)?;
    let with_period = c.fk_with_period || c.pk_with_period;
    if with_period && !c.fk_with_period {
        return Err(DdlError::Parse(
            "foreign key uses PERIOD on the referenced table but not the referencing table".into(),
        ));
    }
    // validateFkOnDeleteSetColumns.
    for col in node_names(&c.fk_del_set_cols) {
        column_attnums(interp, relid, std::slice::from_ref(&col))?;
        if !fk_names.contains(&col) {
            return Err(DdlError::Parse(format!(
                "column \"{col}\" referenced in ON DELETE SET action must be part of foreign key"
            )));
        }
    }

    // The referenced columns: the primary key's, or the named ones.
    let pk_names_given = node_names(&c.pk_attrs);
    let (pk, pk_names, pk_has_without_overlaps) = if pk_names_given.is_empty() {
        // transformFkeyGetPrimaryKey.
        let mut primaries: Vec<&PgIndex> = interp
            .pg_index
            .values()
            .filter(|i| i.indrelid == target && i.indisprimary)
            .collect();
        primaries.sort_by_key(|i| i.indexrelid);
        let (attnums, without_overlaps) = match primaries.first() {
            Some(index) => {
                if interp.nonimmediate_indexes.contains(&index.indexrelid) {
                    return Err(DdlError::Parse(format!(
                        "cannot use a deferrable primary key for referenced table \
                         \"{target_name}\""
                    )));
                }
                (key_columns(index).to_vec(), index.indisexclusion)
            }
            None => match interp
                .pg_constraint
                .values()
                .find(|x| x.conrelid == target && x.contype == ConType::PrimaryKey)
            {
                Some(pkc) => (pkc.conkey.clone(), pkc.conperiod),
                None => {
                    return Err(DdlError::DependencyError(format!(
                        "there is no primary key for referenced table \"{target_name}\""
                    )));
                }
            },
        };
        if without_overlaps && !c.fk_with_period {
            return Err(DdlError::Parse(
                "foreign key uses PERIOD on the referenced table but not the referencing table"
                    .into(),
            ));
        }
        let attrs = interp.attributes_of(target);
        let typed: Vec<(i16, PgTypeOid)> = attnums
            .iter()
            .map(|&an| {
                let t = attrs
                    .iter()
                    .find(|a| a.attnum == an)
                    .map_or(crate::pg_catalog::oid::UNKNOWN, |a| a.atttypid);
                (an, t)
            })
            .collect();
        let names: Vec<String> = attnums
            .iter()
            .map(|&an| {
                attrs
                    .iter()
                    .find(|a| a.attnum == an)
                    .map(|a| a.attname.clone())
                    .unwrap_or_default()
            })
            .collect();
        (typed, names, without_overlaps)
    } else {
        let typed = column_attnums(interp, target, &pk_names_given)?;
        if with_period && !c.pk_with_period {
            return Err(DdlError::Parse(
                "foreign key uses PERIOD on the referencing table but not the referenced table"
                    .into(),
            ));
        }
        let attnums: Vec<i16> = typed.iter().map(|(an, _)| *an).collect();
        let without_overlaps =
            check_referenced_attrs(interp, target, &target_name, &attnums, with_period)?;
        (typed, pk_names_given, without_overlaps)
    };
    if pk_has_without_overlaps && !with_period {
        return Err(DdlError::Parse(
            "foreign key must use PERIOD when referencing a primary key using WITHOUT OVERLAPS"
                .into(),
        ));
    }

    let fk_attnums: Vec<i16> = fk.iter().map(|(an, _)| *an).collect();
    check_fk_generated_columns(interp, relid, c, &fk_attnums)?;

    // Some actions are unsupported for foreign keys using PERIOD.
    if c.fk_with_period {
        if matches!(c.fk_upd_action.as_str(), "r" | "c" | "n" | "d") {
            return Err(DdlError::UnsupportedDdl(
                "unsupported ON UPDATE action for foreign key constraint using PERIOD".into(),
            ));
        }
        if matches!(c.fk_del_action.as_str(), "r" | "c" | "n" | "d") {
            return Err(DdlError::UnsupportedDdl(
                "unsupported ON DELETE action for foreign key constraint using PERIOD".into(),
            ));
        }
    }

    if fk.len() != pk.len() {
        return Err(DdlError::Parse(
            "number of referencing and referenced columns for foreign key disagree".into(),
        ));
    }
    // The equality (or, for the PERIOD column, overlaps) operators: a
    // temporal key's index is a GiST one.
    for (i, ((_, fktype), (_, pktype))) in fk.iter().zip(&pk).enumerate() {
        if !fk_types_compatible(interp, *pktype, *fktype, pk_has_without_overlaps) {
            return Err(DdlError::DependencyError(format!(
                "foreign key constraint \"{conname}\" cannot be implemented (Key columns \
                 \"{}\" of the referencing table and \"{}\" of the referenced table are of \
                 incompatible types: {} and {}.)",
                fk_names[i],
                pk_names[i],
                format_type_for_message(interp, *fktype),
                format_type_for_message(interp, *pktype)
            )));
        }
    }

    let oid = emit_constraint_with_backing_index(
        interp,
        relid,
        conname.clone(),
        ConType::ForeignKey,
        fk_attnums,
        Some(target),
        pk.iter().map(|(an, _)| *an).collect(),
        false,
        Vec::new(),
        with_period,
        false,
        None,
    )?;
    if let Some(row) = interp.pg_constraint.get_mut(&oid) {
        row.conenforced = c.is_enforced;
        // transformFKConstraints: CREATE TABLE's foreign keys are valid
        // unless NOT ENFORCED.
        row.convalidated = if created {
            c.is_enforced
        } else {
            c.initially_valid
        };
    }
    interp.fk_details.insert(oid, FkDetails::of(c));
    // ATAddForeignKeyConstraint: the referenced side first, then the
    // referencing one.
    recurse_referenced(interp, oid, &conname)?;
    recurse_referencing(interp, relid, oid)
}

/// addFkRecurseReferenced: a foreign key referencing a partitioned table
/// gets one more `pg_constraint` row per referenced partition (and theirs,
/// recursively) — on the referencing relation, pointing at the partition,
/// derived from the row pointing at its parent. `base` is the name the
/// rows are named after.
fn recurse_referenced(
    interp: &mut PgCatalog,
    con: PgConstraintOid,
    base: &str,
) -> Result<(), DdlError> {
    let Some(pkrel) = interp.pg_constraint.get(&con).and_then(|c| c.confrelid) else {
        return Ok(());
    };
    if interp.pg_class.get(&pkrel).map(|c| c.relkind) != Some(RelKind::Partitioned) {
        return Ok(());
    }
    for part in super::partbound::partition_desc_order(interp, pkrel) {
        add_referenced_partition_row(interp, con, part, base)?;
    }
    Ok(())
}

/// addFkConstraint (referenced side) for partition `part` of the table
/// `parent_con` references, then its partitions.
fn add_referenced_partition_row(
    interp: &mut PgCatalog,
    parent_con: PgConstraintOid,
    part: PgClassOid,
    base: &str,
) -> Result<(), DdlError> {
    let Some(parent) = interp.pg_constraint.get(&parent_con).cloned() else {
        return Ok(());
    };
    let Some(pkrel) = parent.confrelid else {
        return Ok(());
    };
    // The referenced columns, by name in the partition.
    let pk_attrs = interp.attributes_of(pkrel).to_vec();
    let confkey: Vec<i16> = parent
        .confkey
        .iter()
        .filter_map(|an| pk_attrs.iter().find(|a| a.attnum == *an))
        .filter_map(|a| interp.attribute_by_name(part, &a.attname).map(|p| p.attnum))
        .collect();
    let name = derived_fk_name(interp, parent.conrelid, base);
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: name,
        conrelid: parent.conrelid,
        contype: ConType::ForeignKey,
        conkey: parent.conkey.clone(),
        confrelid: Some(part),
        confkey,
        conislocal: false,
        coninhcount: 1,
        conenforced: parent.conenforced,
        convalidated: parent.convalidated,
        connoinherit: false,
        conperiod: parent.conperiod,
    });
    if let Some(mut details) = interp.fk_details.get(&parent_con).cloned() {
        details.parent = Some(parent_con);
        interp.fk_details.insert(oid, details);
    }
    recurse_referenced(interp, oid, base)
}

/// addFkConstraint's name for a derived row: `base` unless the relation
/// already has a constraint so named, else ChooseConstraintName(base, NULL,
/// "") — `base_1`, `base_2`, … the first no constraint in the namespace
/// has.
fn derived_fk_name(interp: &PgCatalog, relid: PgClassOid, base: &str) -> String {
    let on_rel = interp
        .pg_constraint
        .values()
        .any(|c| c.conrelid == relid && c.conname == base);
    if !on_rel {
        return base.to_owned();
    }
    let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
    let taken = |name: &str| {
        interp.pg_constraint.values().any(|c| {
            c.conname == name && interp.pg_class.get(&c.conrelid).map(|r| r.relnamespace) == nsoid
        })
    };
    (1..)
        .map(|pass| crate::ddl::util::make_object_name(base, "", &pass.to_string()))
        .find(|name| !taken(name))
        .unwrap_or_else(|| base.to_owned())
}

/// CloneFkReferenced: a new partition of a referenced partitioned table
/// gets a derived row for every foreign key referencing that table (not
/// for one whose parent row does too — it arrives through the parent).
fn clone_referenced_fks(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
) -> Result<(), DdlError> {
    let mut fks: Vec<PgConstraint> = interp
        .pg_constraint
        .values()
        .filter(|c| c.contype == ConType::ForeignKey && c.confrelid == Some(parent))
        .cloned()
        .collect();
    fks.sort_by_key(|c| c.oid);
    let oids: Vec<PgConstraintOid> = fks.iter().map(|c| c.oid).collect();
    for fk in fks {
        let parent_cloned = interp
            .fk_details
            .get(&fk.oid)
            .and_then(|d| d.parent)
            .is_some_and(|p| oids.contains(&p));
        if !parent_cloned {
            add_referenced_partition_row(interp, fk.oid, part, &fk.conname)?;
        }
    }
    Ok(())
}

/// What `pg_constraint` keeps of a foreign key beyond [`PgConstraint`]:
/// its actions, match type and deferrability (`confupdtype`,
/// `confdeltype`, `confmatchtype`, `condeferrable`, `condeferred`) and the
/// constraint of the partitioned table it was cloned from (`conparentid`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FkDetails {
    upd_action: String,
    del_action: String,
    match_type: String,
    deferrable: bool,
    initdeferred: bool,
    pub(crate) parent: Option<PgConstraintOid>,
}

impl FkDetails {
    /// `condeferrable`.
    pub(crate) fn deferrable(&self) -> bool {
        self.deferrable
    }

    fn of(c: &typedpg_pg_query::protobuf::Constraint) -> Self {
        let or = |s: &str, default: &str| {
            if s.is_empty() {
                default.to_owned()
            } else {
                s.to_owned()
            }
        };
        FkDetails {
            upd_action: or(&c.fk_upd_action, "a"),
            del_action: or(&c.fk_del_action, "a"),
            match_type: or(&c.fk_matchtype, "s"),
            deferrable: c.deferrable,
            initdeferred: c.initdeferred,
            parent: None,
        }
    }

    /// The same definition, the parent link aside.
    fn same_as(&self, other: &FkDetails) -> bool {
        self.upd_action == other.upd_action
            && self.del_action == other.del_action
            && self.match_type == other.match_type
            && self.deferrable == other.deferrable
            && self.initdeferred == other.initdeferred
    }
}

/// The foreign keys cloned from `con` into partitions (`conparentid`).
pub(crate) fn fk_clones(interp: &PgCatalog, con: PgConstraintOid) -> Vec<PgConstraintOid> {
    let mut clones: Vec<PgConstraintOid> = interp
        .fk_details
        .iter()
        .filter(|(_, d)| d.parent == Some(con))
        .map(|(oid, _)| *oid)
        .filter(|oid| interp.pg_constraint.contains_key(oid))
        .collect();
    clones.sort();
    clones
}

/// The constraint `con` was cloned from, if any.
pub(crate) fn fk_parent(interp: &PgCatalog, con: PgConstraintOid) -> Option<PgConstraint> {
    let parent = interp.fk_details.get(&con)?.parent?;
    interp.pg_constraint.get(&parent).cloned()
}

/// The topmost constraint `con` derives from (following `conparentid`),
/// if it derives from any.
pub(crate) fn fk_root(interp: &PgCatalog, con: PgConstraintOid) -> Option<PgConstraint> {
    let mut root = fk_parent(interp, con)?;
    while let Some(parent) = fk_parent(interp, root.oid) {
        root = parent;
    }
    Some(root)
}

/// addFkRecurseReferencing: a foreign key of a partitioned table reaches
/// every partition — attached to an equivalent foreign key the partition
/// has, or cloned.
fn recurse_referencing(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    con: PgConstraintOid,
) -> Result<(), DdlError> {
    if interp.pg_class.get(&relid).map(|c| c.relkind) != Some(RelKind::Partitioned) {
        return Ok(());
    }
    for part in super::inherit::children_of(interp, relid) {
        attach_or_clone(interp, part, con)?;
    }
    Ok(())
}

/// tryAttachPartitionForeignKey / CloneFkReferencing for one partition.
fn attach_or_clone(
    interp: &mut PgCatalog,
    part: PgClassOid,
    parent_con: PgConstraintOid,
) -> Result<(), DdlError> {
    let Some(parent) = interp.pg_constraint.get(&parent_con).cloned() else {
        return Ok(());
    };
    let parent_details = interp.fk_details.get(&parent_con).cloned();
    // The key columns, by name in the partition.
    let parent_attrs = interp.attributes_of(parent.conrelid).to_vec();
    let names: Vec<String> = parent
        .conkey
        .iter()
        .filter_map(|an| {
            parent_attrs
                .iter()
                .find(|a| a.attnum == *an)
                .map(|a| a.attname.clone())
        })
        .collect();
    let mapped: Vec<i16> = names
        .iter()
        .filter_map(|n| interp.attribute_by_name(part, n).map(|a| a.attnum))
        .collect();
    let part_name = relname_of(interp, part);
    let mut candidates: Vec<PgConstraint> = interp
        .pg_constraint
        .values()
        .filter(|c| {
            c.conrelid == part
                && c.contype == ConType::ForeignKey
                && c.confrelid == parent.confrelid
                && c.conkey == mapped
                && c.confkey == parent.confkey
        })
        .cloned()
        .collect();
    candidates.sort_by_key(|c| c.oid);
    for candidate in candidates {
        if candidate.conenforced != parent.conenforced {
            return Err(DdlError::Parse(format!(
                "constraint \"{}\" enforceability conflicts with constraint \"{}\" on relation \
                 \"{part_name}\"",
                parent.conname, candidate.conname
            )));
        }
        let details = interp.fk_details.get(&candidate.oid).cloned();
        let attachable = details.as_ref().is_none_or(|d| d.parent.is_none())
            && match (&details, &parent_details) {
                (Some(d), Some(p)) => d.same_as(p),
                _ => true,
            };
        if !attachable {
            continue;
        }
        // AttachPartitionForeignKey.
        if let Some(row) = interp.pg_constraint.get_mut(&candidate.oid) {
            row.conislocal = false;
            row.coninhcount = 1;
        }
        let mut details = details.or(parent_details.clone()).unwrap_or(FkDetails {
            upd_action: "a".into(),
            del_action: "a".into(),
            match_type: "s".into(),
            deferrable: false,
            initdeferred: false,
            parent: None,
        });
        details.parent = Some(parent_con);
        interp.fk_details.insert(candidate.oid, details);
        return Ok(());
    }
    // No luck finding a good constraint to reuse; create our own, under the
    // parent's name unless the partition uses it.
    let name = if interp
        .pg_constraint
        .values()
        .any(|c| c.conrelid == part && c.conname == parent.conname)
    {
        super::inherit::choose_constraint_name(
            interp,
            part,
            &crate::ddl::util::index_name_addition(&names),
            "fkey",
        )
    } else {
        parent.conname.clone()
    };
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: name,
        conrelid: part,
        contype: ConType::ForeignKey,
        conkey: mapped,
        confrelid: parent.confrelid,
        confkey: parent.confkey.clone(),
        conislocal: false,
        coninhcount: 1,
        conenforced: parent.conenforced,
        convalidated: parent.convalidated,
        connoinherit: false,
        conperiod: parent.conperiod,
    });
    if let Some(mut details) = parent_details {
        details.parent = Some(parent_con);
        interp.fk_details.insert(oid, details);
    }
    recurse_referencing(interp, part, oid)
}

/// CloneForeignKeyConstraints (CREATE TABLE ... PARTITION OF, ATTACH
/// PARTITION): the new partition gets the partitioned table's foreign keys
/// — it can't be the table one of them references.
pub(super) fn clone_parent_fks(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
) -> Result<(), DdlError> {
    let mut fks: Vec<PgConstraint> = interp
        .pg_constraint
        .values()
        .filter(|c| c.conrelid == parent && c.contype == ConType::ForeignKey)
        .cloned()
        .collect();
    fks.sort_by_key(|c| c.oid);
    let oids: Vec<PgConstraintOid> = fks.iter().map(|c| c.oid).collect();
    for fk in &fks {
        if fk.confrelid == Some(part) {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot attach table \"{}\" as a partition because it is referenced by foreign \
                 key \"{}\"",
                relname_of(interp, part),
                fk.conname
            )));
        }
    }
    // CloneFkReferencing: not a constraint whose parent is cloned too (the
    // rows derived for referenced partitions).
    for fk in fks {
        let parent_cloned = interp
            .fk_details
            .get(&fk.oid)
            .and_then(|d| d.parent)
            .is_some_and(|p| oids.contains(&p));
        if !parent_cloned {
            attach_or_clone(interp, part, fk.oid)?;
        }
    }
    clone_referenced_fks(interp, parent, part)
}

/// DetachPartitionFinalize: the partition's inherited foreign keys become
/// its own, and the rows derived for it as a referenced partition go (with
/// those derived from them).
pub(super) fn detach_fks(interp: &mut PgCatalog, parent: PgClassOid, part: PgClassOid) {
    let referenced: Vec<PgConstraintOid> = interp
        .pg_constraint
        .values()
        .filter(|c| c.contype == ConType::ForeignKey && c.confrelid == Some(part))
        .filter(|c| fk_parent(interp, c.oid).is_some_and(|p| p.confrelid == Some(parent)))
        .map(|c| c.oid)
        .collect();
    for oid in referenced {
        drop_fk_clones(interp, oid);
        interp.pg_constraint.remove(&oid);
        interp.fk_details.remove(&oid);
    }
    let inherited: Vec<PgConstraintOid> = interp
        .pg_constraint
        .values()
        .filter(|c| c.conrelid == part && c.contype == ConType::ForeignKey)
        .filter(|c| fk_parent(interp, c.oid).is_some_and(|p| p.conrelid == parent))
        .map(|c| c.oid)
        .collect();
    for oid in inherited {
        if let Some(row) = interp.pg_constraint.get_mut(&oid) {
            row.conislocal = true;
            row.coninhcount = 0;
        }
        if let Some(d) = interp.fk_details.get_mut(&oid) {
            d.parent = None;
        }
    }
}

/// A partitioned table's foreign key goes with the clones in its
/// partitions (their DEPENDENCY_INTERNAL on it).
/// The foreign keys that go with unique index `index` of `relid` over
/// columns `key` (PG's FK depends on its referenced index): those
/// referencing exactly `key` — unless another unique index or constraint
/// of `relid` (non-partial, no expressions) covers the same columns.
pub(crate) fn fks_relying_on(
    interp: &PgCatalog,
    relid: PgClassOid,
    key: &std::collections::BTreeSet<i16>,
    index: PgClassOid,
) -> Vec<PgConstraintOid> {
    let covers = |i: &crate::pg_catalog::PgIndex| {
        i.indrelid == relid
            && i.indisunique
            && i.indpred.is_none()
            && i.indexprs.is_empty()
            && i.indkey[..(i.indnkeyatts.max(0) as usize).min(i.indkey.len())]
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                == *key
    };
    if interp
        .pg_index
        .values()
        .any(|i| i.indexrelid != index && covers(i))
    {
        return Vec::new();
    }
    let mut fks: Vec<PgConstraintOid> = interp
        .pg_constraint
        .values()
        .filter(|c| {
            c.contype == ConType::ForeignKey
                && c.confrelid == Some(relid)
                && c.confkey
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == *key
                && interp
                    .fk_details
                    .get(&c.oid)
                    .is_none_or(|d| d.parent.is_none())
        })
        .map(|c| c.oid)
        .collect();
    fks.sort();
    fks
}

/// Drop foreign key `con` (and its clones), as a CASCADE through what it
/// depends on does; views over its tables have their nullability derived
/// again.
pub(crate) fn drop_fk_cascaded(interp: &mut PgCatalog, con: PgConstraintOid) {
    let Some(row) = interp.pg_constraint.remove(&con) else {
        return;
    };
    interp.fk_details.remove(&con);
    drop_fk_clones(interp, con);
    crate::ddl::views::refresh_dependent_view_nullability(interp, row.conrelid, true);
}

pub(crate) fn drop_fk_clones(interp: &mut PgCatalog, con: PgConstraintOid) {
    for clone in fk_clones(interp, con) {
        drop_fk_clones(interp, clone);
        interp.pg_constraint.remove(&clone);
        interp.fk_details.remove(&clone);
    }
}

/// Apply `f` to the clones of `con`, recursively.
pub(super) fn for_each_fk_clone(
    interp: &mut PgCatalog,
    con: PgConstraintOid,
    f: &dyn Fn(&mut PgConstraint),
) {
    for clone in fk_clones(interp, con) {
        if let Some(row) = interp.pg_constraint.get_mut(&clone) {
            f(row);
        }
        for_each_fk_clone(interp, clone, f);
    }
}
