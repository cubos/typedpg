use super::*;

/// Emit `pg_constraint` rows for every PRIMARY KEY / UNIQUE / CHECK /
/// FOREIGN KEY constraint declared on a freshly-built table. FK targets
/// are validated (existence, column existence, type compatibility, and
/// uniqueness coverage on the referenced columns) and recorded with
/// `confrelid`/`confkey` so the dependency graph is traversable.
///
/// DefineRelation's order: the CHECK constraints, then the not-null ones
/// (`record_not_nulls`), then the index-backed ones, then the foreign keys.
pub(crate) fn emit_constraints(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    relname: &str,
    stmt: &CreateStmt,
    record_not_nulls: &dyn Fn(&mut PgCatalog) -> Result<(), DdlError>,
) -> Result<(), DdlError> {
    let attinfo_by_name: std::collections::HashMap<String, (i16, PgTypeOid)> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| (a.attname.clone(), (a.attnum, a.atttypid)))
        .collect();
    let attnum_of = |name: &str| attinfo_by_name.get(name).map(|(an, _)| *an);

    let mut to_emit: Vec<PendingConstraint> = Vec::new();
    // FOREIGN KEYs are resolved only after this table's own PRIMARY KEY /
    // UNIQUE constraints exist, so a self-reference finds them — PG likewise
    // adds FKs after creating the table and its indexes
    // (transformFKConstraints queues them as ALTER TABLE ADD CONSTRAINT).
    // `(constraint, local columns, default name)`.
    type PendingFk<'a> = (
        &'a typedpg_pg_query::protobuf::Constraint,
        Vec<String>,
        ConName,
    );
    let mut pending_fks: Vec<PendingFk> = Vec::new();

    // Column-level constraints.
    let mut column_constraints: Vec<(&typedpg_pg_query::protobuf::ColumnDef, Vec<_>)> = Vec::new();
    for elt in &stmt.table_elts {
        if let Some(node::Node::ColumnDef(cd)) = elt.node.as_ref() {
            column_constraints.push((&**cd, fold_constraint_attrs(&cd.constraints)?));
        }
    }
    for (cd, constraints) in &column_constraints {
        let Some(an) = attnum_of(&cd.colname) else {
            continue;
        };
        for c in constraints {
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrPrimary) => {
                    to_emit.push((
                        ConName::from_explicit(
                            &c.conname,
                            ConName::Relation {
                                addition: String::new(),
                                label: "pkey",
                            },
                        ),
                        ConType::PrimaryKey,
                        vec![an],
                        None,
                        Vec::new(),
                        None,
                        c.deferrable,
                        include_attnums(interp, relid, c)?.0,
                        false,
                    ));
                }
                Ok(ConstrType::ConstrUnique) => {
                    to_emit.push((
                        ConName::from_explicit(
                            &c.conname,
                            ConName::Relation {
                                addition: cd.colname.clone(),
                                label: "key",
                            },
                        ),
                        ConType::Unique,
                        vec![an],
                        None,
                        Vec::new(),
                        None,
                        c.deferrable,
                        include_attnums(interp, relid, c)?.0,
                        false,
                    ));
                }
                Ok(ConstrType::ConstrCheck) => {
                    to_emit.push((
                        ConName::from_explicit(
                            &c.conname,
                            ConName::Constraint {
                                addition: check_name_addition(interp, relid, c.raw_expr.as_deref()),
                                label: "check",
                            },
                        ),
                        ConType::Check,
                        check_conkey(interp, relid, c.raw_expr.as_deref()),
                        None,
                        Vec::new(),
                        Some((
                            check_inherit::CheckDef {
                                expr: check_inherit::StoredExpr::written(
                                    &c.raw_expr.clone().map(|b| *b).unwrap_or_default(),
                                ),
                                no_inherit: c.is_no_inherit,
                            },
                            c.is_enforced,
                        )),
                        c.deferrable,
                        include_attnums(interp, relid, c)?.0,
                        false,
                    ));
                }
                Ok(ConstrType::ConstrForeign) => {
                    pending_fks.push((
                        c,
                        vec![cd.colname.clone()],
                        ConName::Constraint {
                            addition: cd.colname.clone(),
                            label: "fkey",
                        },
                    ));
                }
                _ => {}
            }
        }
    }

    // Table-level constraints.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        let Some(node::Node::Constraint(c)) = elt.node.as_ref() else {
            continue;
        };
        let column_names: Vec<String> = c
            .keys
            .iter()
            .filter_map(|k| match k.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();
        let columns: Vec<i16> = column_names.iter().filter_map(|n| attnum_of(n)).collect();
        match ConstrType::try_from(c.contype) {
            Ok(ConstrType::ConstrPrimary) if !columns.is_empty() => {
                check_key_column_list(interp, relid, c, &column_names)?;
                to_emit.push((
                    ConName::from_explicit(
                        &c.conname,
                        ConName::Relation {
                            addition: String::new(),
                            label: "pkey",
                        },
                    ),
                    ConType::PrimaryKey,
                    columns,
                    None,
                    Vec::new(),
                    None,
                    c.deferrable,
                    include_attnums(interp, relid, c)?.0,
                    c.without_overlaps,
                ));
            }
            Ok(ConstrType::ConstrUnique) if !columns.is_empty() => {
                check_key_column_list(interp, relid, c, &column_names)?;
                to_emit.push((
                    ConName::from_explicit(
                        &c.conname,
                        ConName::Relation {
                            addition: crate::ddl::util::index_name_addition(
                                &[
                                    column_names.as_slice(),
                                    &include_attnums(interp, relid, c)?.1,
                                ]
                                .concat(),
                            ),
                            label: "key",
                        },
                    ),
                    ConType::Unique,
                    columns,
                    None,
                    Vec::new(),
                    None,
                    c.deferrable,
                    include_attnums(interp, relid, c)?.0,
                    c.without_overlaps,
                ));
            }
            Ok(ConstrType::ConstrCheck) => {
                to_emit.push((
                    ConName::from_explicit(
                        &c.conname,
                        ConName::Constraint {
                            addition: check_name_addition(interp, relid, c.raw_expr.as_deref()),
                            label: "check",
                        },
                    ),
                    ConType::Check,
                    check_conkey(interp, relid, c.raw_expr.as_deref()),
                    None,
                    Vec::new(),
                    Some((
                        check_inherit::CheckDef {
                            expr: check_inherit::StoredExpr::written(
                                &c.raw_expr.clone().map(|b| *b).unwrap_or_default(),
                            ),
                            no_inherit: c.is_no_inherit,
                        },
                        c.is_enforced,
                    )),
                    c.deferrable,
                    include_attnums(interp, relid, c)?.0,
                    false,
                ));
            }
            Ok(ConstrType::ConstrExclusion) => {
                let (keys, names) = exclusion_keys(interp, relid, c)?;
                to_emit.push((
                    ConName::from_explicit(
                        &c.conname,
                        ConName::Relation {
                            addition: crate::ddl::util::index_name_addition(
                                &[names.as_slice(), &include_attnums(interp, relid, c)?.1].concat(),
                            ),
                            label: "excl",
                        },
                    ),
                    ConType::Exclusion,
                    keys,
                    None,
                    Vec::new(),
                    None,
                    c.deferrable,
                    include_attnums(interp, relid, c)?.0,
                    false,
                ));
            }
            Ok(ConstrType::ConstrForeign) => {
                // Table-level FK uses `fk_attrs` for the local columns;
                // `keys` only carries column lists on PK / UNIQUE.
                let fk_names: Vec<String> = c
                    .fk_attrs
                    .iter()
                    .filter_map(|k| match k.node.as_ref()? {
                        node::Node::String(s) => Some(s.sval.clone()),
                        _ => None,
                    })
                    .collect();
                let default_name = ConName::Constraint {
                    addition: crate::ddl::util::index_name_addition(&fk_names),
                    label: "fkey",
                };
                pending_fks.push((c, fk_names, default_name));
            }
            _ => {}
        }
    }

    // transformIndexConstraints drops a PRIMARY KEY / UNIQUE spec that
    // repeats an earlier one's key (an unnamed duplicate; a PRIMARY KEY wins
    // over a UNIQUE).
    let mut kept: Vec<PendingConstraint> = Vec::new();
    for pending in to_emit {
        let index_backed = |t: ConType| matches!(t, ConType::PrimaryKey | ConType::Unique);
        let dup = kept.iter().position(|k| {
            index_backed(k.1)
                && index_backed(pending.1)
                && k.2 == pending.2
                && k.7 == pending.7
                && k.8 == pending.8
        });
        match dup {
            Some(i) if !pending.0.is_explicit() || !kept[i].0.is_explicit() => {
                if pending.1 == ConType::PrimaryKey && kept[i].1 == ConType::Unique {
                    kept[i] = pending;
                }
            }
            _ => kept.push(pending),
        }
    }
    let (checks, indexed): (Vec<_>, Vec<_>) = kept.into_iter().partition(|k| k.1 == ConType::Check);
    let mut kept = checks;
    let n_checks = kept.len();
    kept.extend(indexed);
    let n_all = kept.len();
    let mut check_names: Vec<String> = Vec::new();
    for (i, (conname, contype, conkey, confrelid, confkey, check, deferrable, include, period)) in
        kept.into_iter().enumerate()
    {
        if i == n_checks {
            record_not_nulls(interp)?;
        }
        let conname = conname.resolve(interp, relid);
        // AddRelationNewConstraints: two CHECK constraints of one command
        // can't share a name.
        if contype == ConType::Check {
            if check_names.contains(&conname) {
                return Err(DdlError::DuplicateObject(format!(
                    "check constraint \"{conname}\" already exists"
                )));
            }
            check_names.push(conname.clone());
        }
        let oid = emit_constraint_with_backing_index(
            interp, relid, conname, contype, conkey, confrelid, confkey, deferrable, include,
            period, false,
        )?;
        if let Some((def, enforced)) = check {
            // CREATE TABLE's constraints are valid unless NOT ENFORCED.
            if let Some(row) = interp.pg_constraint.get_mut(&oid) {
                row.connoinherit = def.no_inherit;
                row.conenforced = enforced;
                row.convalidated = enforced;
            }
            interp.check_defs.insert(oid, def);
        }
    }
    if n_all == n_checks {
        record_not_nulls(interp)?;
    }
    for (c, local_names, default_name) in pending_fks {
        super::foreign_keys::add_foreign_key(
            interp,
            relid,
            c,
            &local_names,
            default_name,
            true,
            true,
        )?;
    }
    let _ = relname;
    Ok(())
}

/// transformIndexConstraint's checks of a PRIMARY KEY / UNIQUE column
/// list: no column twice, and for WITHOUT OVERLAPS at least two columns,
/// the last a range or multirange (or a domain over one). A column that
/// doesn't exist is left to DefineIndex.
fn check_key_column_list(
    interp: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
    names: &[String],
) -> Result<(), DdlError> {
    let what = if c.contype == ConstrType::ConstrPrimary as i32 {
        "primary key constraint"
    } else {
        "unique constraint"
    };
    for (i, name) in names.iter().enumerate() {
        if names[..i].contains(name) {
            return Err(DdlError::DuplicateObject(format!(
                "column \"{name}\" appears twice in {what}"
            )));
        }
        if c.without_overlaps
            && i == names.len() - 1
            && let Some(attr) = interp.attribute_by_name(relid, name)
        {
            let base = interp.unwrap_domain(attr.atttypid);
            let range_like = interp
                .pg_type
                .get(&base)
                .is_some_and(|t| matches!(t.typtype, TypType::Range | TypType::Multirange));
            if !range_like {
                return Err(DdlError::Parse(format!(
                    "column \"{name}\" in WITHOUT OVERLAPS is not a range or multirange type"
                )));
            }
        }
    }
    if c.without_overlaps && names.len() < 2 {
        return Err(DdlError::Parse(
            "constraint using WITHOUT OVERLAPS needs at least two columns".into(),
        ));
    }
    Ok(())
}

/// DefineIndex for a WITHOUT OVERLAPS key's GiST index: every key column
/// needs a default GiST operator class (a scalar one only has it through
/// btree_gist), and a partition key column can't be the WITHOUT OVERLAPS
/// one — its operator is `&&`, not the partitioning equality.
fn check_without_overlaps_index(
    interp: &PgCatalog,
    relid: PgClassOid,
    attnums: &[i16],
) -> Result<(), DdlError> {
    let attrs = interp.attributes_of(relid);
    for attnum in attnums {
        if let Some(attr) = attrs.iter().find(|a| a.attnum == *attnum) {
            crate::ddl::opclass::resolve_index_opclass(interp, &[], attr.atttypid, "gist")?;
        }
    }
    if let (Some(part_key), Some(period)) = (interp.partition_keys.get(&relid), attnums.last())
        && part_key.contains(period)
    {
        let name = attrs
            .iter()
            .find(|a| a.attnum == *period)
            .map(|a| a.attname.clone())
            .unwrap_or_default();
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot match partition key to index on column \"{name}\" using non-equal operator \
             \"&&\""
        )));
    }
    Ok(())
}

/// The INCLUDE columns of an index-backed constraint, as attnums and names
/// (transformIndexConstraint: each must exist).
fn include_attnums(
    interp: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
) -> Result<(Vec<i16>, Vec<String>), DdlError> {
    let mut attnums = Vec::new();
    let mut names = Vec::new();
    for name in c.including.iter().filter_map(crate::ddl::util::node_string) {
        let attnum = interp
            .attribute_by_name(relid, name)
            .map(|a| a.attnum)
            .ok_or_else(|| {
                DdlError::Parse(format!("column \"{name}\" named in key does not exist"))
            })?;
        attnums.push(attnum);
        names.push(name.to_owned());
    }
    Ok((attnums, names))
}

/// transformConstraintAttrs (parse_utilcmd.c): a column's `[NOT]
/// DEFERRABLE` / `INITIALLY {DEFERRED | IMMEDIATE}` / `[NOT] ENFORCED`
/// clauses arrive as separate attribute nodes that apply to the constraint
/// before them; a lone INITIALLY DEFERRED implies DEFERRABLE, and a NOT
/// ENFORCED constraint is also NOT VALID. Deferrability only suits a key or
/// foreign-key constraint, enforceability a CHECK or foreign key.
pub(super) fn fold_constraint_attrs(
    constraints: &[typedpg_pg_query::protobuf::Node],
) -> Result<Vec<typedpg_pg_query::protobuf::Constraint>, DdlError> {
    use ConstrType as C;
    let mut out: Vec<typedpg_pg_query::protobuf::Constraint> = Vec::new();
    let mut saw_deferrability = false;
    let mut saw_initially = false;
    let mut saw_enforced = false;
    let syntax = |msg: &str| DdlError::Parse(msg.to_owned());
    for n in constraints {
        let Some(node::Node::Constraint(c)) = n.node.as_ref() else {
            continue;
        };
        let attr = ConstrType::try_from(c.contype).unwrap_or(C::Undefined);
        let is_attr = matches!(
            attr,
            C::ConstrAttrDeferrable
                | C::ConstrAttrNotDeferrable
                | C::ConstrAttrDeferred
                | C::ConstrAttrImmediate
                | C::ConstrAttrEnforced
                | C::ConstrAttrNotEnforced
        );
        if !is_attr {
            saw_deferrability = false;
            saw_initially = false;
            saw_enforced = false;
            out.push((**c).clone());
            continue;
        }
        let last = out.last_mut();
        let last_type = last
            .as_ref()
            .and_then(|l| ConstrType::try_from(l.contype).ok());
        // SUPPORTS_ATTRS.
        let supports_deferrability = matches!(
            last_type,
            Some(C::ConstrPrimary | C::ConstrUnique | C::ConstrExclusion | C::ConstrForeign)
        );
        let supports_enforced = matches!(last_type, Some(C::ConstrCheck | C::ConstrForeign));
        match attr {
            C::ConstrAttrDeferrable | C::ConstrAttrNotDeferrable => {
                let deferrable = attr == C::ConstrAttrDeferrable;
                if !supports_deferrability {
                    return Err(syntax(if deferrable {
                        "misplaced DEFERRABLE clause"
                    } else {
                        "misplaced NOT DEFERRABLE clause"
                    }));
                }
                if saw_deferrability {
                    return Err(syntax(
                        "multiple DEFERRABLE/NOT DEFERRABLE clauses not allowed",
                    ));
                }
                saw_deferrability = true;
                let last = last.expect("supports_deferrability implies a constraint");
                last.deferrable = deferrable;
                if !deferrable && saw_initially && last.initdeferred {
                    return Err(syntax(
                        "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
                    ));
                }
            }
            C::ConstrAttrDeferred | C::ConstrAttrImmediate => {
                let deferred = attr == C::ConstrAttrDeferred;
                if !supports_deferrability {
                    return Err(syntax(if deferred {
                        "misplaced INITIALLY DEFERRED clause"
                    } else {
                        "misplaced INITIALLY IMMEDIATE clause"
                    }));
                }
                if saw_initially {
                    return Err(syntax(
                        "multiple INITIALLY IMMEDIATE/DEFERRED clauses not allowed",
                    ));
                }
                saw_initially = true;
                let last = last.expect("supports_deferrability implies a constraint");
                last.initdeferred = deferred;
                if deferred {
                    if !saw_deferrability {
                        last.deferrable = true;
                    } else if !last.deferrable {
                        return Err(syntax(
                            "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
                        ));
                    }
                }
            }
            _ => {
                let enforced = attr == C::ConstrAttrEnforced;
                if !supports_enforced {
                    return Err(syntax(if enforced {
                        "misplaced ENFORCED clause"
                    } else {
                        "misplaced NOT ENFORCED clause"
                    }));
                }
                if saw_enforced {
                    return Err(syntax("multiple ENFORCED/NOT ENFORCED clauses not allowed"));
                }
                saw_enforced = true;
                let last = last.expect("supports_enforced implies a constraint");
                last.is_enforced = enforced;
                if !enforced {
                    // A NOT ENFORCED constraint must be marked as invalid.
                    last.skip_validation = true;
                    last.initially_valid = false;
                }
            }
        }
    }
    Ok(out)
}

/// Insert a `pg_constraint` row and, for PK/UNIQUE, the backing
/// `pg_class` (relkind = 'i') + `pg_index` rows that PG auto-creates.
///
/// PG conflates the constraint and its backing index — `<table>_pkey` is
/// both a constraint and an index, sharing one name. Mirror that so DROP
/// COLUMN / DROP TABLE cascade through `pg_index` and `ON CONFLICT ON
/// CONSTRAINT name` finds the index by its conname.
#[allow(clippy::too_many_arguments)] // one field per pg_constraint column
pub(super) fn emit_constraint_with_backing_index(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    conname: String,
    contype: ConType,
    conkey: Vec<i16>,
    confrelid: Option<PgClassOid>,
    confkey: Vec<i16>,
    deferrable: bool,
    include: Vec<i16>,
    period: bool,
    index_only: bool,
) -> Result<PgConstraintOid, DdlError> {
    if period && contype != ConType::ForeignKey {
        check_without_overlaps_index(interp, relid, &conkey)?;
    }
    if contype == ConType::PrimaryKey {
        check_no_primary_key(interp, relid)?;
    }
    if matches!(contype, ConType::PrimaryKey | ConType::Unique) {
        let label = if contype == ConType::PrimaryKey {
            "PRIMARY KEY"
        } else {
            "UNIQUE"
        };
        let collations: Vec<_> = conkey
            .iter()
            .map(|&a| {
                interp
                    .attributes_of(relid)
                    .iter()
                    .find(|att| att.attnum == a)
                    .and_then(|att| att.attcollation)
            })
            .collect();
        check_unique_covers_partition_key(interp, relid, &conkey, &collations, label)?;
    }
    if matches!(
        contype,
        ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
    ) {
        use crate::ddl::indexes::{IndexUse, check_index_columns};
        let usage = if contype == ConType::PrimaryKey {
            IndexUse::Primary
        } else {
            IndexUse::Constraint
        };
        let columns: Vec<i16> = conkey.iter().chain(&include).copied().collect();
        check_index_columns(interp, relid, &columns, &[], usage)?;
    }
    if matches!(
        contype,
        ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
    ) {
        // index_create: the index is a relation of the schema, and the
        // constraint's name must be free on the table.
        let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
        if let Some(ns) = nsoid
            && interp.class_by_qname.contains_key(&(ns, conname.clone()))
        {
            return Err(DdlError::DuplicateObject(format!(
                "relation \"{conname}\" already exists"
            )));
        }
        if interp
            .pg_constraint
            .values()
            .any(|c| c.conrelid == relid && c.conname == conname)
        {
            return Err(DdlError::DuplicateObject(format!(
                "constraint \"{conname}\" for relation \"{}\" already exists",
                relname_of(interp, relid)
            )));
        }
    }
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: conname.clone(),
        conrelid: relid,
        contype,
        conkey: conkey.clone(),
        confrelid,
        confkey,
        conislocal: true,
        coninhcount: 0,
        conenforced: true,
        convalidated: true,
        connoinherit: false,
        conperiod: period,
    });
    if matches!(
        contype,
        ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
    ) {
        let table_ns = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relnamespace)
            .ok_or_else(|| {
                DdlError::Internal(format!(
                    "constraint backing index expects pg_class row for relid={relid} to be registered"
                ))
            })?;
        let indexrelid = PgClassOid::from_nonzero(interp.alloc_oid()?);
        let relkind = RelKind::index_on(interp.pg_class.get(&relid).map(|c| c.relkind));
        interp.insert_pg_class(PgClass {
            oid: indexrelid,
            relname: conname,
            relnamespace: table_ns,
            relkind,
            reltype: None,
        });
        // The INCLUDE columns follow the key columns.
        let indnkeyatts = conkey.len() as i16;
        let indkey: Vec<i16> = conkey.iter().chain(&include).copied().collect();
        interp.insert_pg_index(PgIndex {
            indexrelid,
            indrelid: relid,
            indnatts: indkey.len() as i16,
            indnkeyatts,
            // An exclusion constraint's index is not a unique one.
            indisunique: contype != ConType::Exclusion,
            indisprimary: matches!(contype, ConType::PrimaryKey),
            // A WITHOUT OVERLAPS key's GiST index backs an exclusion
            // constraint too.
            indisexclusion: contype == ConType::Exclusion || period,
            indkey,
            indexprs: Vec::new(),
            indpred: None,
        });
        // index_create: indimmediate = !deferrable (before the partition
        // clones copy it).
        if deferrable {
            interp.nonimmediate_indexes.insert(indexrelid);
        }
        if !index_only {
            super::partidx::propagate_new_index(interp, relid, indexrelid)?;
        } else if interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned)
            && !super::inherit::children_of(interp, relid).is_empty()
        {
            interp.invalid_indexes.insert(indexrelid);
        }
    }
    Ok(oid)
}

/// index_check_primary_key (index.c): a table has at most one primary key.
fn check_no_primary_key(interp: &PgCatalog, relid: PgClassOid) -> Result<(), DdlError> {
    if interp
        .pg_index
        .values()
        .any(|i| i.indrelid == relid && i.indisprimary)
    {
        return Err(DdlError::Parse(format!(
            "multiple primary keys for table \"{}\" are not allowed",
            relname_of(interp, relid)
        )));
    }
    Ok(())
}

/// An index's key columns, without its INCLUDE columns.
pub(super) fn key_columns(index: &PgIndex) -> &[i16] {
    let n = usize::try_from(index.indnkeyatts).unwrap_or(0);
    &index.indkey[..n.min(index.indkey.len())]
}

/// DefineIndex (indexcmds.c): a unique index on a partitioned table must
/// contain every partition key column, under the key's collation, and the
/// key may not be an expression. `collations` are the index columns'.
pub(crate) fn check_unique_covers_partition_key(
    interp: &PgCatalog,
    relid: PgClassOid,
    key: &[i16],
    collations: &[Option<crate::oid::PgCollationOid>],
    label: &str,
) -> Result<(), DdlError> {
    let Some(part_key) = interp.partition_keys.get(&relid) else {
        return Ok(());
    };
    if part_key.contains(&0) {
        return Err(DdlError::Parse(format!(
            "unsupported {label} constraint with partition key definition"
        )));
    }
    let part_collations = super::partbound::partition_key_collations(interp, relid);
    let covered = |(i, pk): (usize, &i16)| {
        let collation = part_collations.get(i).copied().flatten();
        key.iter()
            .zip(collations)
            .any(|(k, c)| k == pk && *c == collation)
    };
    if !part_key.iter().enumerate().all(covered) {
        return Err(DdlError::Parse(
            "unique constraint on partitioned table must include all partitioning columns".into(),
        ));
    }
    Ok(())
}

/// The key columns of an `EXCLUDE (elem WITH op, ...)` constraint: each
/// element's attnum (`0` for an expression) and the names PG's
/// `ChooseIndexColumnNames` uses for the default constraint name.
fn exclusion_keys(
    interp: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
) -> Result<(Vec<i16>, Vec<String>), DdlError> {
    let mut attnums = Vec::new();
    let mut names = Vec::new();
    for pair in &c.exclusions {
        let Some(node::Node::List(l)) = pair.node.as_ref() else {
            continue;
        };
        let Some(node::Node::IndexElem(elem)) = l.items.first().and_then(|n| n.node.as_ref())
        else {
            continue;
        };
        if elem.name.is_empty() {
            attnums.push(0);
            // ChooseIndexColumnNames → FigureIndexColname.
            names.push(crate::ddl::indexes::figure_index_colname(
                elem.expr.as_deref(),
            ));
        } else {
            let attnum = interp
                .attribute_by_name(relid, &elem.name)
                .map(|a| a.attnum)
                .ok_or_else(|| {
                    DdlError::Parse(format!(
                        "column \"{}\" named in key does not exist",
                        elem.name
                    ))
                })?;
            attnums.push(attnum);
            names.push(elem.name.clone());
        }
    }
    // DefineIndex: nor may its expressions or predicate read a virtual
    // generated column.
    let exprs: Vec<&typedpg_pg_query::protobuf::Node> = c
        .exclusions
        .iter()
        .filter_map(|pair| match pair.node.as_ref()? {
            node::Node::List(l) => match l.items.first()?.node.as_ref()? {
                node::Node::IndexElem(elem) => elem.expr.as_deref(),
                _ => None,
            },
            _ => None,
        })
        .chain(c.where_clause.as_deref())
        .collect();
    crate::ddl::indexes::check_index_columns(
        interp,
        relid,
        &[],
        &exprs,
        crate::ddl::indexes::IndexUse::Constraint,
    )?;
    Ok((attnums, names))
}

/// The relation's persistence: `p` (permanent), `u` (unlogged) or `t` (temporary).
pub(crate) fn persistence(interp: &PgCatalog, relid: PgClassOid) -> char {
    interp.relpersistence.get(&relid).copied().unwrap_or('p')
}

/// ATAddForeignKeyConstraint: a permanent table references only
/// permanent tables, an unlogged one permanent or unlogged ones, a
/// temporary one only temporary ones.
pub(super) fn check_fk_persistence(
    interp: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
) -> Result<(), DdlError> {
    // The referenced table (its errors are resolve_fk_target's to report).
    let Some(target) = c
        .pktable
        .as_ref()
        .and_then(|rv| crate::ddl::util::lookup_relation(interp, rv).ok())
        .map(|(_, oid)| oid)
    else {
        return Ok(());
    };
    let msg = match (persistence(interp, relid), persistence(interp, target)) {
        ('p', t) if t != 'p' => {
            "constraints on permanent tables may reference only permanent tables"
        }
        ('u', 't') => {
            "constraints on unlogged tables may reference only permanent or unlogged tables"
        }
        ('t', t) if t != 't' => {
            "constraints on temporary tables may reference only temporary tables"
        }
        _ => return Ok(()),
    };
    Err(DdlError::UnsupportedDdl(msg.into()))
}

/// ATAddForeignKeyConstraint: what a foreign key may do over generated
/// columns — no action that writes the referencing column (per the SQL
/// standard), and no virtual column at all.
pub(super) fn check_fk_generated_columns(
    interp: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
    fk_attnums: &[i16],
) -> Result<(), DdlError> {
    for &attnum in fk_attnums {
        let generated = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .and_then(|a| a.attgenerated);
        if generated.is_some() {
            if matches!(c.fk_upd_action.as_str(), "n" | "d" | "c") {
                return Err(DdlError::Parse(
                    "invalid ON UPDATE action for foreign key constraint containing generated \
                     column"
                        .into(),
                ));
            }
            if matches!(c.fk_del_action.as_str(), "n" | "d") {
                return Err(DdlError::Parse(
                    "invalid ON DELETE action for foreign key constraint containing generated \
                     column"
                        .into(),
                ));
            }
        }
        if generated == Some(crate::pg_catalog::AttGenerated::Virtual) {
            return Err(DdlError::UnsupportedDdl(
                "foreign key constraints on virtual generated columns are not supported".into(),
            ));
        }
    }
    Ok(())
}

/// Walk every CHECK / `GENERATED STORED` expression in a `CREATE TABLE` and
/// verify that the inferred type is compatible with the role of that
/// expression (boolean for CHECK, the column's type for generated).
///
/// Volatility is checked separately by [`crate::ddl::volatile`] before the
/// table is even built; this pass only runs once the table exists in the
/// catalog so the expression scope can resolve the column references.
pub(crate) fn validate_constraint_expressions(
    interp: &mut PgCatalog,
    class_oid: PgClassOid,
    relname: &str,
    stmt: &CreateStmt,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::pg_catalog::oid;
    use crate::qualified_name::QualifiedName;
    use crate::scope::Scope;

    let table_attrs = interp.attributes_of(class_oid).to_vec();
    let relnamespace = interp
        .pg_class
        .get(&class_oid)
        .map(|c| c.relnamespace)
        .ok_or_else(|| {
            DdlError::Internal(format!(
                "validate_constraint_expressions expects pg_class row for class_oid={class_oid} to be registered"
            ))
        })?;
    let nspname = interp
        .namespace_name(relnamespace)
        .map(str::to_owned)
        .unwrap_or_else(|| "public".to_owned());

    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        relname,
        QualifiedName::new(nspname, relname.to_owned()),
        &table_attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();

    // Column-level constraints — CHECK and GENERATED expressions live on
    // the ColumnDef's `constraints` list.
    for elt in &stmt.table_elts {
        let Some(node::Node::ColumnDef(cd)) = elt.node.as_ref() else {
            continue;
        };
        if let Some(expr) = super::columns::column_default_expr(cd)
            && let Some(attr) = table_attrs.iter().find(|a| a.attname == cd.colname)
        {
            let default_type =
                crate::ddl::defaults::check_default(interp, expr, &cd.colname, attr.atttypid)?;
            interp
                .attr_default_types
                .insert((class_oid, attr.attnum), default_type);
            interp.attr_default_exprs.insert(
                (class_oid, attr.attnum),
                super::check_inherit::StoredExpr::written(expr),
            );
            crate::ddl::defaults::record_default_dependencies(
                interp,
                class_oid,
                attr.attnum,
                Some(expr),
            );
        }
        for c_node in &cd.constraints {
            let Some(node::Node::Constraint(c)) = c_node.node.as_ref() else {
                continue;
            };
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrCheck) => {
                    if let Some(expr) = c.raw_expr.as_deref() {
                        crate::ddl::expr_kind::check_expr_kind(
                            interp,
                            expr,
                            crate::ddl::expr_kind::ExprKind::CheckConstraint,
                        )?;
                        check_system_column_refs(interp, class_oid, expr)?;
                        // Infer with no type goal so a non-bool result
                        // doesn't surface as a TypeMismatch — we want PG's
                        // exact wording (`argument of CHECK must be type
                        // boolean, not type X`).
                        let result = infer_expr(
                            expr,
                            crate::expr::Ctx::new(&scope, &null_ctx, interp),
                            &mut params,
                            TypeGoal::NONE,
                        )
                        .map_err(|e| {
                            // Forward the analyzer's message verbatim so it
                            // can match PG's wording (e.g. `column "ghost"
                            // does not exist`); append the constraint
                            // location as supplementary detail.
                            DdlError::UnsupportedDdl(format!(
                                "{e} (in CHECK constraint on {})",
                                QualifiedName::new(relname, &cd.colname),
                            ))
                        })?;
                        if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
                            let typname = format_type_for_message(interp, result.type_oid);
                            return Err(DdlError::UnsupportedDdl(format!(
                                "argument of CHECK must be type boolean, not type {typname} \
                                 (CHECK constraint on {})",
                                QualifiedName::new(relname, &cd.colname),
                            )));
                        }
                    }
                }
                Ok(ConstrType::ConstrGenerated) => {
                    if let Some(expr) = c.raw_expr.as_deref()
                        && let Some(attr) = table_attrs.iter().find(|a| a.attname == cd.colname)
                    {
                        let cooked = super::generated::cook_generation_expr(
                            interp,
                            class_oid,
                            &cd.colname,
                            attr.atttypid,
                            attr.attgenerated
                                .unwrap_or(crate::pg_catalog::AttGenerated::Stored),
                            expr,
                        )?;
                        cooked.record(interp, class_oid, attr.attnum);
                    }
                }
                _ => {}
            }
        }
    }

    // Table-level CHECK constraints — both `stmt.constraints` and
    // ColumnDef-shaped `Constraint` nodes inside `stmt.table_elts`.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        if let Some(node::Node::Constraint(c)) = elt.node.as_ref()
            && c.contype == ConstrType::ConstrCheck as i32
            && let Some(expr) = c.raw_expr.as_deref()
        {
            crate::ddl::expr_kind::check_expr_kind(
                interp,
                expr,
                crate::ddl::expr_kind::ExprKind::CheckConstraint,
            )?;
            check_system_column_refs(interp, class_oid, expr)?;
            let result = infer_expr(
                expr,
                crate::expr::Ctx::new(&scope, &null_ctx, interp),
                &mut params,
                TypeGoal::NONE,
            )
            .map_err(|e| {
                DdlError::UnsupportedDdl(format!(
                    "{e} (in table-level CHECK constraint on \"{relname}\")"
                ))
            })?;
            if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
                let typname = format_type_for_message(interp, result.type_oid);
                return Err(DdlError::UnsupportedDdl(format!(
                    "argument of CHECK must be type boolean, not type {typname} \
                     (table-level CHECK constraint on \"{relname}\")"
                )));
            }
        }
    }

    Ok(())
}

pub(crate) fn drop_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: super::inherit::Recursion,
) -> Result<(), DdlError> {
    let conname = &cmd.name;
    let found_oid = interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && &c.conname == conname)
        .map(|c| c.oid);
    if let Some(con) = found_oid
        .and_then(|oid| interp.pg_constraint.get(&oid))
        .filter(|c| c.contype == ConType::NotNull)
        .cloned()
    {
        return super::inherit::drop_not_null_constraint(interp, &con, rec);
    }
    if let Some(con) = found_oid
        .and_then(|oid| interp.pg_constraint.get(&oid))
        .filter(|c| c.contype == ConType::Check)
        .cloned()
    {
        return super::check_inherit::drop_check(interp, &con, rec);
    }
    let Some(oid) = found_oid else {
        if cmd.missing_ok {
            return Ok(());
        }
        let relname = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.as_str())
            .unwrap_or("?");
        return Err(DdlError::DependencyError(format!(
            "constraint \"{conname}\" of relation \"{relname}\" does not exist"
        )));
    };

    // A partition's copy of a partitioned table's constraint goes only
    // with the parent's (dropconstraint_internal).
    if let Some(con) = interp.pg_constraint.get(&oid)
        && con.coninhcount > 0
        && !rec.recursing
    {
        return Err(DdlError::DependencyError(format!(
            "cannot drop inherited constraint \"{conname}\" of relation \"{}\"",
            relname_of(interp, relid)
        )));
    }

    // Refuse to drop a UNIQUE/PK constraint that an FK depends on.
    let is_pkey_or_unique = interp
        .pg_constraint
        .get(&oid)
        .is_some_and(|c| matches!(c.contype, ConType::PrimaryKey | ConType::Unique));
    let cascade = matches!(
        DropBehavior::try_from(cmd.behavior),
        Ok(DropBehavior::DropCascade)
    );
    if is_pkey_or_unique && !cascade {
        let target_set: std::collections::BTreeSet<i16> = interp
            .pg_constraint
            .get(&oid)
            .map(|c| c.conkey.iter().copied().collect())
            .unwrap_or_default();
        let dependent: Option<String> = interp.pg_constraint.values().find_map(|c| {
            if matches!(c.contype, ConType::ForeignKey)
                && c.confrelid == Some(relid)
                && c.confkey
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == target_set
            {
                Some(c.conname.clone())
            } else {
                None
            }
        });
        if let Some(dep) = dependent {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|c| c.relname.as_str())
                .unwrap_or("?");
            return Err(DdlError::DependencyError(format!(
                "cannot drop constraint {conname} on table {relname} because other objects \
                 depend on it (foreign key constraint \"{dep}\" depends on this)"
            )));
        }
    }

    // Drop the backing index (`<conname>` shares the relname with the
    // constraint for PK/UNIQUE) before dropping the constraint itself.
    if is_pkey_or_unique {
        let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
        if let Some(nsoid) = nsoid
            && let Some(idx_oid) = interp
                .class_by_qname
                .get(&(nsoid, conname.clone()))
                .copied()
            && matches!(
                interp.pg_class.get(&idx_oid).map(|c| c.relkind),
                Some(RelKind::Index | RelKind::PartitionedIndex)
            )
        {
            // The partitions' copies and their constraints go too.
            for child in super::partidx::child_indexes(interp, idx_oid) {
                let child_name = interp.pg_class.get(&child).map(|c| c.relname.clone());
                let child_table = interp.pg_index.get(&child).map(|i| i.indrelid);
                interp.pg_constraint.retain(|_, c| {
                    Some(c.conrelid) != child_table || Some(&c.conname) != child_name.as_ref()
                });
                interp.remove_pg_index(child);
                interp.remove_pg_class(child);
            }
            interp.remove_pg_index(idx_oid);
            interp.remove_pg_class(idx_oid);
            let obj = crate::oid::PgGenericOid::from_nonzero(idx_oid.into_nonzero());
            interp.remove_dependencies_of(crate::pg_catalog::PG_CLASS_RELID, obj);
            interp.remove_dependencies_on(crate::pg_catalog::PG_CLASS_RELID, obj);
        }
    }

    interp.pg_constraint.remove(&oid);
    super::foreign_keys::drop_fk_clones(interp, oid);
    Ok(())
}

pub(crate) fn add_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: super::inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::Constraint(c)) = def.node.as_ref() else {
        return Ok(());
    };
    // ALTER TABLE ONLY: a constraint index isn't built on the partitions
    // (DefineIndex under ONLY).
    add_constraint_node(interp, relid, c, &cmd.name, rec, !rec.recurse)
}

/// The constraints written inline on an `ALTER TABLE ... ADD COLUMN`
/// definition (PRIMARY KEY, UNIQUE, CHECK, REFERENCES): like
/// `transformColumnDefinition`, attach the column as the constraint's key and
/// add each as if by `ADD CONSTRAINT`.
pub(crate) fn add_column_constraints(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cd: &typedpg_pg_query::protobuf::ColumnDef,
) -> Result<(), DdlError> {
    let colname_node = typedpg_pg_query::protobuf::Node {
        node: Some(node::Node::String(typedpg_pg_query::protobuf::String {
            sval: cd.colname.clone(),
        })),
    };
    let only_here = super::inherit::Recursion {
        recurse: false,
        recursing: false,
    };
    for mut c in fold_constraint_attrs(&cd.constraints)? {
        match ConstrType::try_from(c.contype) {
            Ok(ConstrType::ConstrPrimary | ConstrType::ConstrUnique) => {
                c.keys = vec![colname_node.clone()];
            }
            Ok(ConstrType::ConstrForeign) => c.fk_attrs = vec![colname_node.clone()],
            Ok(ConstrType::ConstrCheck) => {
                // The column reaches the children, and so does its CHECK.
                let rec = super::inherit::Recursion {
                    recurse: true,
                    recursing: false,
                };
                add_constraint_node(interp, relid, &c, &cd.colname, rec, false)?;
                continue;
            }
            _ => continue,
        }
        add_constraint_node(interp, relid, &c, &cd.colname, only_here, false)?;
    }
    Ok(())
}

/// `ADD CONSTRAINT` of one constraint node (`ATExecAddConstraint`).
fn add_constraint_node(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
    cmd_name: &str,
    rec: super::inherit::Recursion,
    index_only: bool,
) -> Result<(), DdlError> {
    // `ADD {PRIMARY KEY | UNIQUE} USING INDEX idx` turns an existing unique
    // index into the constraint (ATExecAddIndexConstraint).
    if !c.indexname.is_empty() {
        return add_index_constraint(interp, relid, c);
    }
    let (include, include_names) = include_attnums(interp, relid, c)?;

    if c.contype == ConstrType::ConstrExclusion as i32 {
        let (keys, names) = exclusion_keys(interp, relid, c)?;
        let conname = ConName::from_explicit(
            &c.conname,
            ConName::Relation {
                addition: crate::ddl::util::index_name_addition(
                    &[names.as_slice(), include_names.as_slice()].concat(),
                ),
                label: "excl",
            },
        )
        .resolve(interp, relid);
        emit_constraint_with_backing_index(
            interp,
            relid,
            conname,
            ConType::Exclusion,
            keys,
            None,
            Vec::new(),
            c.deferrable,
            include.clone(),
            false,
            index_only,
        )?;
    }

    if c.contype == ConstrType::ConstrPrimary as i32 {
        let pk_cols: Vec<String> = c
            .keys
            .iter()
            .filter_map(|k| {
                if let Some(node::Node::String(s)) = k.node.as_ref() {
                    Some(s.sval.clone())
                } else {
                    None
                }
            })
            .collect();
        check_key_column_list(interp, relid, c, &pk_cols)?;
        // ATPrepAddPrimaryKey: a primary key's columns get not-null
        // constraints.
        for col in &pk_cols {
            super::inherit::require_pk_not_null(interp, relid, col, rec.recurse)?;
        }
        let attnums: Vec<i16> = pk_cols
            .iter()
            .filter_map(|n| {
                interp
                    .attributes_of(relid)
                    .iter()
                    .find(|a| &a.attname == n)
                    .map(|a| a.attnum)
            })
            .collect();
        if !attnums.is_empty() {
            let conname = ConName::from_explicit(
                &c.conname,
                ConName::Relation {
                    addition: String::new(),
                    label: "pkey",
                },
            )
            .resolve(interp, relid);
            emit_constraint_with_backing_index(
                interp,
                relid,
                conname,
                ConType::PrimaryKey,
                attnums,
                None,
                Vec::new(),
                c.deferrable,
                include.clone(),
                c.without_overlaps,
                index_only,
            )?;
        }
    }

    if c.contype == ConstrType::ConstrUnique as i32 {
        let cols: Vec<String> = c
            .keys
            .iter()
            .filter_map(|k| match k.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();
        check_key_column_list(interp, relid, c, &cols)?;
        if let Some(missing) = cols
            .iter()
            .find(|n| interp.attribute_by_name(relid, n).is_none())
        {
            return Err(DdlError::Parse(format!(
                "column \"{missing}\" named in key does not exist"
            )));
        }
        let attnums: Vec<i16> = cols
            .iter()
            .filter_map(|n| {
                interp
                    .attributes_of(relid)
                    .iter()
                    .find(|a| &a.attname == n)
                    .map(|a| a.attnum)
            })
            .collect();
        if !attnums.is_empty() {
            let conname = ConName::from_explicit(
                &c.conname,
                ConName::Relation {
                    addition: crate::ddl::util::index_name_addition(
                        &[cols.as_slice(), include_names.as_slice()].concat(),
                    ),
                    label: "key",
                },
            )
            .resolve(interp, relid);
            emit_constraint_with_backing_index(
                interp,
                relid,
                conname,
                ConType::Unique,
                attnums,
                None,
                Vec::new(),
                c.deferrable,
                include.clone(),
                c.without_overlaps,
                index_only,
            )?;
        }
    }

    if c.contype == ConstrType::ConstrNotnull as i32 {
        let col_name = c
            .keys
            .first()
            .and_then(|k| {
                if let Some(node::Node::String(s)) = k.node.as_ref() {
                    Some(s.sval.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| cmd_name.to_owned());
        // transformTableConstraint.
        if c.is_no_inherit
            && interp.pg_class.get(&relid).map(|r| r.relkind) == Some(RelKind::Partitioned)
        {
            return Err(DdlError::UnsupportedDdl(
                "not-null constraints on partitioned tables cannot be NO INHERIT".into(),
            ));
        }
        let spec = super::inherit::NotNullSpec {
            name: (!c.conname.is_empty()).then_some(c.conname.as_str()),
            no_inherit: c.is_no_inherit,
            not_valid: c.skip_validation,
        };
        super::inherit::add_not_null(interp, relid, &col_name, spec, rec)?;
    }

    if c.contype == ConstrType::ConstrCheck as i32
        && let Some(expr) = c.raw_expr.as_deref()
    {
        // No volatility walk for CHECK — PG accepts volatile expressions at
        // DDL time, only the type-must-be-bool check below has teeth.
        validate_check_expression_for_table(interp, relid, expr)?;
        // Emit pg_constraint row for the CHECK so future inspections see it.
        let conname = ConName::from_explicit(
            &c.conname,
            ConName::Constraint {
                addition: check_name_addition(interp, relid, Some(expr)),
                label: "check",
            },
        )
        .resolve(interp, relid);
        super::check_inherit::add_check(
            interp,
            relid,
            &conname,
            expr,
            c.is_no_inherit,
            rec,
            check_conkey(interp, relid, Some(expr)),
            super::check_inherit::CheckFlags {
                enforced: c.is_enforced,
                valid: c.initially_valid,
            },
        )?;
    }

    if c.contype == ConstrType::ConstrForeign as i32 {
        // FK column list lives in `fk_attrs`, not `keys` (which is empty
        // for FK constraints — `keys` is only used by PK/UNIQUE).
        let column_names: Vec<String> = c
            .fk_attrs
            .iter()
            .filter_map(|k| match k.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.clone()),
                _ => None,
            })
            .collect();
        let default_name = ConName::Constraint {
            addition: crate::ddl::util::index_name_addition(&column_names),
            label: "fkey",
        };
        super::foreign_keys::add_foreign_key(
            interp,
            relid,
            c,
            &column_names,
            default_name,
            false,
            rec.recurse,
        )?;
    }

    Ok(())
}

/// `ALTER TABLE t ADD [CONSTRAINT name] {PRIMARY KEY | UNIQUE} USING INDEX
/// idx` (`ATExecAddIndexConstraint`, index.c `index_constraint_create`):
/// the unique index becomes the constraint's index — renamed to the
/// constraint name when one is given — and a PRIMARY KEY marks its columns
/// NOT NULL.
fn add_index_constraint(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
) -> Result<(), DdlError> {
    let nsoid = interp
        .pg_class
        .get(&relid)
        .map(|cls| cls.relnamespace)
        .ok_or_else(|| DdlError::Internal(format!("relation oid {relid} missing")))?;
    let index_oid = interp
        .class_by_qname
        .get(&(nsoid, c.indexname.clone()))
        .copied()
        .filter(|oid| interp.pg_index.contains_key(oid))
        .ok_or_else(|| {
            DdlError::TableNotFound(format!("index \"{}\" does not exist", c.indexname))
        })?;
    let Some(index) = interp.pg_index.get(&index_oid).cloned() else {
        return Ok(());
    };
    if index.indrelid != relid {
        return Err(DdlError::Parse(format!(
            "index \"{}\" does not belong to table \"{}\"",
            c.indexname,
            relname_of(interp, relid)
        )));
    }
    if interp.invalid_indexes.contains(&index_oid) {
        return Err(DdlError::Parse(format!(
            "index \"{}\" is not valid",
            c.indexname
        )));
    }
    if !index.indisunique {
        return Err(DdlError::Parse(format!(
            "\"{}\" is not a unique index",
            c.indexname
        )));
    }
    let is_primary = c.contype == ConstrType::ConstrPrimary as i32;
    if is_primary {
        check_no_primary_key(interp, relid)?;
    }
    let conname = if c.conname.is_empty() {
        c.indexname.clone()
    } else {
        c.conname.clone()
    };
    if conname != c.indexname {
        interp.rename_pg_class(index_oid, conname.clone(), nsoid);
    }
    // The fake UNIQUE row CREATE UNIQUE INDEX records for ON CONFLICT is
    // superseded by the real constraint.
    interp
        .pg_constraint
        .retain(|_, x| !(x.conrelid == relid && x.conname == c.indexname));
    if let Some(idx) = interp.pg_index.get_mut(&index_oid) {
        idx.indisprimary = is_primary;
    }
    // index_constraint_create: a DEFERRABLE constraint clears the adopted
    // index's indimmediate.
    if c.deferrable {
        interp.nonimmediate_indexes.insert(index_oid);
    }
    // transformIndexConstraint names the index's columns as the key, and
    // ATPrepAddPrimaryKey gives them not-null constraints.
    if is_primary {
        for attnum in key_columns(&index).iter().copied().filter(|&an| an > 0) {
            let colname = interp
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum == attnum)
                .map(|a| a.attname.clone())
                .unwrap_or_default();
            super::inherit::require_pk_not_null(interp, relid, &colname, true)?;
        }
    }
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname,
        conrelid: relid,
        contype: if is_primary {
            ConType::PrimaryKey
        } else {
            ConType::Unique
        },
        conkey: key_columns(&index).to_vec(),
        confrelid: None,
        confkey: Vec::new(),
        conislocal: true,
        coninhcount: 0,
        conenforced: true,
        convalidated: true,
        connoinherit: false,
        conperiod: false,
    });
    Ok(())
}

/// scanNSItemForColumn under EXPR_KIND_CHECK_CONSTRAINT: a CHECK
/// constraint may read no system column but `tableoid`.
fn check_system_column_refs(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &typedpg_pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    let Some(inner) = expr.node.as_ref() else {
        return Ok(());
    };
    for (n, ..) in inner.nodes() {
        let typedpg_pg_query::NodeRef::ColumnRef(cr) = n else {
            continue;
        };
        let Some(name) = cr.fields.last().and_then(crate::ddl::util::node_string) else {
            continue;
        };
        if name != "tableoid"
            && interp.attribute_by_name(relid, name).is_none()
            && crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .any(|(n, ..)| *n == name)
        {
            return Err(DdlError::Parse(format!(
                "system column \"{name}\" reference in check constraint is invalid"
            )));
        }
    }
    Ok(())
}

/// Run a CHECK expression through the analyzer in the scope of `relid`
/// and verify its result type is boolean. Used by ALTER TABLE ADD
/// CONSTRAINT (the CREATE TABLE path uses [`validate_constraint_expressions`]
/// which sees the full `CreateStmt` shape).
fn validate_check_expression_for_table(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &typedpg_pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::pg_catalog::oid;
    use crate::qualified_name::QualifiedName;
    use crate::scope::Scope;

    let class = interp
        .pg_class
        .get(&relid)
        .ok_or_else(|| DdlError::TableNotFound(format!("relation oid {relid}")))?;
    let nspname = interp
        .namespace_name(class.relnamespace)
        .map(str::to_owned)
        .unwrap_or_else(|| "public".to_owned());
    let relname = class.relname.clone();
    let attrs = interp.attributes_of(relid).to_vec();

    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        &relname,
        QualifiedName::new(nspname, relname.clone()),
        &attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();

    crate::ddl::expr_kind::check_expr_kind(
        interp,
        expr,
        crate::ddl::expr_kind::ExprKind::CheckConstraint,
    )?;
    check_system_column_refs(interp, relid, expr)?;
    let result = infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(|e| DdlError::UnsupportedDdl(format!("CHECK on \"{relname}\": {e}")))?;
    if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
        let typname = format_type_for_message(interp, result.type_oid);
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of CHECK must be type boolean, not type {typname} \
             (CHECK constraint on \"{relname}\")"
        )));
    }
    Ok(())
}

/// Copy what `LIKE source INCLUDING CONSTRAINTS / INDEXES` asks for onto the
/// freshly-created relation (`transformTableLikeClause` /
/// `expandTableLikeClause`): CHECK constraints keep their names, indexes are
/// cloned (`generateClonedIndexStmt`) under names chosen like
/// `ChooseIndexName` — `<rel>_pkey`, `<rel>_<cols>_key` for constraint
/// indexes, `<rel>_<cols>_idx` otherwise.
pub(crate) fn copy_like_constraints(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    relname: &str,
    like: &super::merge::LikeCopy,
) -> Result<(), DdlError> {
    use super::merge::{LIKE_CONSTRAINTS, LIKE_INDEXES};
    use crate::ddl::util::{choose_relation_name, index_name_addition};

    let source_attrs = interp.attributes_of(like.source).to_vec();
    let new_attnum = |interp: &PgCatalog, attnum: i16| -> i16 {
        source_attrs
            .iter()
            .find(|a| a.attnum == attnum)
            .and_then(|a| interp.attribute_by_name(relid, &a.attname))
            .map(|a| a.attnum)
            .unwrap_or(0)
    };

    if like.options & LIKE_CONSTRAINTS != 0 {
        let mut checks: Vec<PgConstraint> = interp
            .pg_constraint
            .values()
            .filter(|c| c.conrelid == like.source && c.contype == ConType::Check)
            .cloned()
            .collect();
        checks.sort_by_key(|c| c.oid);
        for c in checks {
            let conkey = c.conkey.iter().map(|&an| new_attnum(interp, an)).collect();
            let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
            if let Some(def) = interp.check_defs.get(&c.oid).cloned() {
                interp.check_defs.insert(oid, def);
            }
            interp.insert_pg_constraint(PgConstraint {
                oid,
                conname: c.conname,
                conrelid: relid,
                contype: ConType::Check,
                conkey,
                confrelid: None,
                confkey: Vec::new(),
                conislocal: true,
                coninhcount: 0,
                conenforced: c.conenforced,
                convalidated: c.convalidated,
                connoinherit: c.connoinherit,
                conperiod: false,
            });
        }
    }

    if like.options & LIKE_INDEXES != 0 {
        let nsoid = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relnamespace)
            .ok_or_else(|| DdlError::Internal(format!("LIKE target relid={relid} missing")))?;
        let mut indexes: Vec<PgIndex> = interp
            .pg_index
            .values()
            .filter(|i| i.indrelid == like.source)
            .cloned()
            .collect();
        indexes.sort_by_key(|i| i.indexrelid);
        for idx in indexes {
            let idxname = relname_of(interp, idx.indexrelid);
            let backing = interp
                .pg_constraint
                .values()
                .find(|c| {
                    c.conrelid == like.source
                        && c.conname == idxname
                        && matches!(c.contype, ConType::PrimaryKey | ConType::Unique)
                })
                .map(|c| (c.contype, c.conperiod));
            let indkey: Vec<i16> = idx
                .indkey
                .iter()
                .map(|&an| if an == 0 { 0 } else { new_attnum(interp, an) })
                .collect();
            let colnames: Vec<String> = idx
                .indkey
                .iter()
                .map(|&an| {
                    source_attrs
                        .iter()
                        .find(|a| an != 0 && a.attnum == an)
                        .map(|a| a.attname.clone())
                        .unwrap_or_else(|| "expr".to_owned())
                })
                .collect();
            let addition = index_name_addition(&colnames);
            match backing {
                Some((contype, period)) => {
                    let name = match contype {
                        ConType::PrimaryKey => crate::ddl::util::choose_constraint_index_name(
                            interp, nsoid, relname, "", "pkey",
                        ),
                        _ => crate::ddl::util::choose_constraint_index_name(
                            interp, nsoid, relname, &addition, "key",
                        ),
                    };
                    let nkey = key_columns(&idx).len();
                    emit_constraint_with_backing_index(
                        interp,
                        relid,
                        name,
                        contype,
                        indkey[..nkey].to_vec(),
                        None,
                        Vec::new(),
                        // generateClonedIndexStmt copies the constraint's
                        // deferrability.
                        interp.nonimmediate_indexes.contains(&idx.indexrelid),
                        indkey[nkey..].to_vec(),
                        period,
                        false,
                    )?;
                }
                None => {
                    let name = choose_relation_name(interp, nsoid, relname, &addition, "idx");
                    let indexrelid = PgClassOid::from_nonzero(interp.alloc_oid()?);
                    let relkind = RelKind::index_on(interp.pg_class.get(&relid).map(|c| c.relkind));
                    interp.insert_pg_class(PgClass {
                        oid: indexrelid,
                        relname: name,
                        relnamespace: nsoid,
                        relkind,
                        reltype: None,
                    });
                    interp.insert_pg_index(PgIndex {
                        indexrelid,
                        indrelid: relid,
                        indkey,
                        ..idx
                    });
                }
            }
        }
    }
    Ok(())
}
