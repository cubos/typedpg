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
        .map(|name| match interp.attribute_by_name(relid, name) {
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
    let mut indexes: Vec<&PgIndex> = interp
        .pg_index
        .values()
        .filter(|i| i.indrelid == target)
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
fn fk_types_compatible(
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
) -> Result<(), DdlError> {
    let relname = relname_of(interp, relid);
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
        conname,
        ConType::ForeignKey,
        fk_attnums,
        Some(target),
        pk.iter().map(|(an, _)| *an).collect(),
        false,
        Vec::new(),
        with_period,
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
    Ok(())
}
