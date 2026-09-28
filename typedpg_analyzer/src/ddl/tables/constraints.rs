use super::*;

/// Emit `pg_constraint` rows for every PRIMARY KEY / UNIQUE / CHECK /
/// FOREIGN KEY constraint declared on a freshly-built table. FK targets
/// are validated (existence, column existence, type compatibility, and
/// uniqueness coverage on the referenced columns) and recorded with
/// `confrelid`/`confkey` so the dependency graph is traversable.
pub(crate) fn emit_constraints(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    relname: &str,
    stmt: &CreateStmt,
) -> Result<(), DdlError> {
    let attinfo_by_name: std::collections::HashMap<String, (i16, PgTypeOid)> = interp
        .attributes_of(relid)
        .iter()
        .map(|a| (a.attname.clone(), (a.attnum, a.atttypid)))
        .collect();
    let attnum_of = |name: &str| attinfo_by_name.get(name).map(|(an, _)| *an);
    let atttype_of = |name: &str| attinfo_by_name.get(name).map(|(_, t)| *t);

    let mut to_emit: Vec<PendingConstraint> = Vec::new();
    // FOREIGN KEYs are resolved only after this table's own PRIMARY KEY /
    // UNIQUE constraints exist, so a self-reference finds them — PG likewise
    // adds FKs after creating the table and its indexes
    // (transformFKConstraints queues them as ALTER TABLE ADD CONSTRAINT).
    // `(constraint, local columns, their types, their attnums, default name)`.
    type PendingFk<'a> = (
        &'a pg_query::protobuf::Constraint,
        Vec<String>,
        Vec<PgTypeOid>,
        Vec<i16>,
        ConName,
    );
    let mut pending_fks: Vec<PendingFk> = Vec::new();

    // Column-level constraints.
    for elt in &stmt.table_elts {
        let Some(node::Node::ColumnDef(cd)) = elt.node.as_ref() else {
            continue;
        };
        let Some(an) = attnum_of(&cd.colname) else {
            continue;
        };
        let Some(my_type) = atttype_of(&cd.colname) else {
            continue;
        };
        for c_node in &cd.constraints {
            let Some(node::Node::Constraint(c)) = c_node.node.as_ref() else {
                continue;
            };
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
                        vec![an],
                        None,
                        Vec::new(),
                    ));
                }
                Ok(ConstrType::ConstrForeign) => {
                    pending_fks.push((
                        c,
                        vec![cd.colname.clone()],
                        vec![my_type],
                        vec![an],
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
                ));
            }
            Ok(ConstrType::ConstrUnique) if !columns.is_empty() => {
                to_emit.push((
                    ConName::from_explicit(
                        &c.conname,
                        ConName::Relation {
                            addition: crate::ddl::util::index_name_addition(&column_names),
                            label: "key",
                        },
                    ),
                    ConType::Unique,
                    columns,
                    None,
                    Vec::new(),
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
                    columns,
                    None,
                    Vec::new(),
                ));
            }
            Ok(ConstrType::ConstrExclusion) => {
                let (keys, names) = exclusion_keys(interp, relid, c)?;
                to_emit.push((
                    ConName::from_explicit(
                        &c.conname,
                        ConName::Relation {
                            addition: crate::ddl::util::index_name_addition(&names),
                            label: "excl",
                        },
                    ),
                    ConType::Exclusion,
                    keys,
                    None,
                    Vec::new(),
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
                let local_types: Vec<PgTypeOid> =
                    fk_names.iter().filter_map(|n| atttype_of(n)).collect();
                if local_types.len() != fk_names.len() {
                    return Err(DdlError::Parse(format!(
                        "foreign key on {relname} references unknown local column"
                    )));
                }
                let fk_columns: Vec<i16> = fk_names.iter().filter_map(|n| attnum_of(n)).collect();
                let default_name = ConName::Constraint {
                    addition: crate::ddl::util::index_name_addition(&fk_names),
                    label: "fkey",
                };
                pending_fks.push((c, fk_names, local_types, fk_columns, default_name));
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
        let dup = kept
            .iter()
            .position(|k| index_backed(k.1) && index_backed(pending.1) && k.2 == pending.2);
        match dup {
            Some(i) if !pending.0.is_explicit() || !kept[i].0.is_explicit() => {
                if pending.1 == ConType::PrimaryKey && kept[i].1 == ConType::Unique {
                    kept[i] = pending;
                }
            }
            _ => kept.push(pending),
        }
    }
    for (conname, contype, conkey, confrelid, confkey) in kept {
        let conname = conname.resolve(interp, relid);
        emit_constraint_with_backing_index(
            interp, relid, conname, contype, conkey, confrelid, confkey,
        )?;
    }
    for (c, local_names, local_types, conkey, default_name) in pending_fks {
        let (target_oid, target_attnums) =
            resolve_fk_target(interp, c, relname, &local_names, &local_types)?;
        let conname = ConName::from_explicit(&c.conname, default_name).resolve(interp, relid);
        emit_constraint_with_backing_index(
            interp,
            relid,
            conname,
            ConType::ForeignKey,
            conkey,
            Some(target_oid),
            target_attnums,
        )?;
    }
    Ok(())
}

/// Insert a `pg_constraint` row and, for PK/UNIQUE, the backing
/// `pg_class` (relkind = 'i') + `pg_index` rows that PG auto-creates.
///
/// PG conflates the constraint and its backing index — `<table>_pkey` is
/// both a constraint and an index, sharing one name. Mirror that so DROP
/// COLUMN / DROP TABLE cascade through `pg_index` and `ON CONFLICT ON
/// CONSTRAINT name` finds the index by its conname.
fn emit_constraint_with_backing_index(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    conname: String,
    contype: ConType,
    conkey: Vec<i16>,
    confrelid: Option<PgClassOid>,
    confkey: Vec<i16>,
) -> Result<(), DdlError> {
    if matches!(contype, ConType::PrimaryKey | ConType::Unique) {
        let label = if contype == ConType::PrimaryKey {
            "PRIMARY KEY"
        } else {
            "UNIQUE"
        };
        check_unique_covers_partition_key(interp, relid, &conkey, label)?;
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
        interp.insert_pg_class(PgClass {
            oid: indexrelid,
            relname: conname,
            relnamespace: table_ns,
            relkind: RelKind::Index,
            reltype: None,
        });
        let indnatts = conkey.len() as i16;
        interp.insert_pg_index(PgIndex {
            indexrelid,
            indrelid: relid,
            indnatts,
            indnkeyatts: indnatts,
            // An exclusion constraint's index is not a unique one.
            indisunique: contype != ConType::Exclusion,
            indisprimary: matches!(contype, ConType::PrimaryKey),
            indkey: conkey,
            indexprs: Vec::new(),
            indpred: None,
        });
    }
    Ok(())
}

/// DefineIndex (indexcmds.c): a unique index on a partitioned table must
/// contain every partition key column, and the key may not be an
/// expression.
pub(crate) fn check_unique_covers_partition_key(
    interp: &PgCatalog,
    relid: PgClassOid,
    key: &[i16],
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
    if part_key.iter().any(|pk| !key.contains(pk)) {
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
    c: &pg_query::protobuf::Constraint,
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
            names.push("expr".to_owned());
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
    Ok((attnums, names))
}

/// Resolve a `FOREIGN KEY` target: returns `(target_class_oid, target_attnums)`.
///
/// Validates the same things PG does at CREATE/ALTER time:
/// - Target relation exists.
/// - Target columns exist.
/// - When the column list is omitted, defaults to the target's PRIMARY KEY.
/// - Target columns are covered exactly by a `PRIMARY KEY` or `UNIQUE`
///   constraint on the target relation.
/// - Local and target column types match (after domain unwrapping).
fn resolve_fk_target(
    interp: &PgCatalog,
    c: &pg_query::protobuf::Constraint,
    relname: &str,
    local_col_names: &[String],
    local_types: &[PgTypeOid],
) -> Result<(PgClassOid, Vec<i16>), DdlError> {
    // Match PG's auto-naming: explicit `CONSTRAINT <name>` if given, otherwise
    // `<relname>_<col1>_<col2>_..._fkey`. Used as the prefix on error
    // messages so `pglite_sanity` matches PG's `foreign key constraint
    // "<name>" cannot be implemented`.
    let fk_name = constraint_name(&c.conname, || {
        format!("{relname}_{}_fkey", local_col_names.join("_"))
    });
    let pkrv = c
        .pktable
        .as_ref()
        .ok_or_else(|| DdlError::Parse(format!("FOREIGN KEY on {relname} without REFERENCES")))?;
    let (target_schema, target_name) = range_var_names(pkrv, interp);
    let target_nsoid = interp.namespace_oid(&target_schema).ok_or_else(|| {
        DdlError::TableNotFound(format!(
            "relation \"{}\" does not exist (referenced \
             by foreign key constraint \"{fk_name}\")",
            QualifiedName::new(&target_schema, &target_name),
        ))
    })?;
    let target_oid = interp
        .class_by_qname
        .get(&(target_nsoid, target_name.clone()))
        .copied()
        .ok_or_else(|| {
            DdlError::TableNotFound(format!(
                "relation \"{target_name}\" does not exist (referenced by foreign key \
                 constraint \"{fk_name}\")"
            ))
        })?;

    // No explicit column list → default to the target's PRIMARY KEY.
    let target_attnums: Vec<i16> = if c.pk_attrs.is_empty() {
        let pk = interp
            .pg_constraint
            .values()
            .find(|x| x.conrelid == target_oid && matches!(x.contype, ConType::PrimaryKey))
            .ok_or_else(|| {
                DdlError::DependencyError(format!(
                    "there is no primary key for referenced table \"{target_name}\""
                ))
            })?;
        pk.conkey.clone()
    } else {
        let target_attrs = interp.attributes_of(target_oid);
        let mut nums = Vec::new();
        for k in &c.pk_attrs {
            if let Some(node::Node::String(s)) = k.node.as_ref() {
                let Some(an) = target_attrs
                    .iter()
                    .find(|a| a.attname == s.sval)
                    .map(|a| a.attnum)
                else {
                    return Err(DdlError::Parse(format!(
                        "column \"{}\" referenced in foreign key constraint does not exist \
                         on \"{target_name}\"",
                        s.sval
                    )));
                };
                nums.push(an);
            }
        }
        nums
    };

    // transformFkeyCheckAttrs: the referenced columns must be exactly the
    // key of a unique, non-partial, non-expression index (a PRIMARY KEY /
    // UNIQUE constraint's, or a plain CREATE UNIQUE INDEX).
    let target_set: std::collections::BTreeSet<i16> = target_attnums.iter().copied().collect();
    let covered = interp.pg_index.values().any(|idx| {
        idx.indrelid == target_oid
            && idx.indisunique
            && idx.indpred.is_none()
            && idx.indexprs.is_empty()
            && idx.indkey[..usize::try_from(idx.indnkeyatts)
                .unwrap_or(0)
                .min(idx.indkey.len())]
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                == target_set
    }) || interp.pg_constraint.values().any(|x| {
        x.conrelid == target_oid
            && matches!(x.contype, ConType::PrimaryKey | ConType::Unique)
            && x.conkey
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                == target_set
    });
    if !covered {
        return Err(DdlError::DependencyError(format!(
            "there is no unique constraint matching given keys for referenced table \
             \"{target_name}\""
        )));
    }

    // Type compatibility — local vs target after domain unwrapping.
    let target_attrs = interp.attributes_of(target_oid);
    let target_types: Vec<PgTypeOid> = target_attnums
        .iter()
        .filter_map(|&an| {
            target_attrs
                .iter()
                .find(|a| a.attnum == an)
                .map(|a| a.atttypid)
        })
        .collect();
    if target_types.len() != target_attnums.len() {
        return Err(DdlError::Parse(format!(
            "foreign key constraint \"{fk_name}\" cannot be implemented \
             (references an unknown column on \"{target_name}\")"
        )));
    }
    if target_types.len() != local_types.len() {
        return Err(DdlError::Parse(format!(
            "number of referencing and referenced columns for foreign key disagree \
             (constraint \"{fk_name}\": {} local column(s) vs {} on \"{target_name}\")",
            local_types.len(),
            target_types.len()
        )));
    }
    for (lt, tt) in local_types.iter().zip(target_types.iter()) {
        if interp.unwrap_domain(*lt) != interp.unwrap_domain(*tt) {
            let lt_name = format_type_for_message(interp, *lt);
            let tt_name = format_type_for_message(interp, *tt);
            return Err(DdlError::DependencyError(format!(
                "foreign key constraint \"{fk_name}\" cannot be implemented \
                 (key columns of \"{relname}\" and \"{target_name}\" are of incompatible \
                 types: {lt_name} and {tt_name})"
            )));
        }
    }

    Ok((target_oid, target_attnums))
}

/// Pick a constraint name: explicit one when supplied, otherwise the
/// PG-style auto-generated form provided by the caller's closure.
fn constraint_name(explicit: &str, fallback: impl FnOnce() -> String) -> String {
    if explicit.is_empty() {
        fallback()
    } else {
        explicit.to_owned()
    }
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
                    if let Some(expr) = c.raw_expr.as_deref() {
                        crate::ddl::expr_kind::check_expr_kind(
                            interp,
                            expr,
                            crate::ddl::expr_kind::ExprKind::GeneratedColumn,
                        )?;
                        crate::ddl::volatile::check_mutability(
                            interp,
                            class_oid,
                            expr,
                            crate::ddl::volatile::ExprLocation::Generated,
                        )?;
                        // check_nested_generated: a generation expression
                        // may not read another generated column.
                        if let Some(inner) = expr.node.as_ref() {
                            for (n, ..) in inner.nodes() {
                                let pg_query::NodeRef::ColumnRef(cr) = n else {
                                    continue;
                                };
                                let Some(colname) =
                                    cr.fields.last().and_then(crate::ddl::util::node_string)
                                else {
                                    continue;
                                };
                                if table_attrs
                                    .iter()
                                    .any(|a| a.attname == colname && a.attgenerated.is_some())
                                {
                                    return Err(DdlError::Parse(format!(
                                        "cannot use generated column \"{colname}\" in column \
                                         generation expression"
                                    )));
                                }
                            }
                        }
                        let col_type = table_attrs
                            .iter()
                            .find(|a| a.attname == cd.colname)
                            .map(|a| a.atttypid)
                            .ok_or_else(|| {
                                DdlError::Parse(format!(
                                    "generated column \"{}\" of \"{relname}\" not found \
                                     in pg_attribute",
                                    cd.colname
                                ))
                            })?;
                        // Use a no-goal pass so we can compare the expression's
                        // type to the column's type ourselves and emit PG's
                        // exact wording on mismatch (`column "X" is of type T
                        // but default expression is of type U`).
                        let result = infer_expr(
                            expr,
                            crate::expr::Ctx::new(&scope, &null_ctx, interp),
                            &mut params,
                            TypeGoal::NONE,
                        )
                        .map_err(|e| {
                            DdlError::UnsupportedDdl(format!(
                                "{e} (in GENERATED expression on {})",
                                QualifiedName::new(relname, &cd.colname),
                            ))
                        })?;
                        if interp.unwrap_domain(result.type_oid) != interp.unwrap_domain(col_type)
                            && result.type_oid != oid::UNKNOWN
                            && !interp.has_implicit_cast(result.type_oid, col_type)
                        {
                            let col_typname = format_type_for_message(interp, col_type);
                            let expr_typname = format_type_for_message(interp, result.type_oid);
                            return Err(DdlError::UnsupportedDdl(format!(
                                "column \"{}\" is of type {col_typname} but default expression \
                                 is of type {expr_typname} (in GENERATED expression on \
                                 \"{relname}\")",
                                cd.colname
                            )));
                        }
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
                Some(RelKind::Index)
            )
        {
            interp.remove_pg_index(idx_oid);
            interp.remove_pg_class(idx_oid);
            let obj = crate::oid::PgGenericOid::from_nonzero(idx_oid.into_nonzero());
            interp.remove_dependencies_of(crate::pg_catalog::PG_CLASS_RELID, obj);
            interp.remove_dependencies_on(crate::pg_catalog::PG_CLASS_RELID, obj);
        }
    }

    interp.pg_constraint.remove(&oid);
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
    add_constraint_node(interp, relid, c, &cmd.name, rec)
}

/// The constraints written inline on an `ALTER TABLE ... ADD COLUMN`
/// definition (PRIMARY KEY, UNIQUE, CHECK, REFERENCES): like
/// `transformColumnDefinition`, attach the column as the constraint's key and
/// add each as if by `ADD CONSTRAINT`.
pub(crate) fn add_column_constraints(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cd: &pg_query::protobuf::ColumnDef,
) -> Result<(), DdlError> {
    let colname_node = pg_query::protobuf::Node {
        node: Some(node::Node::String(pg_query::protobuf::String {
            sval: cd.colname.clone(),
        })),
    };
    let only_here = super::inherit::Recursion {
        recurse: false,
        recursing: false,
    };
    for c_node in &cd.constraints {
        let Some(node::Node::Constraint(c)) = c_node.node.as_ref() else {
            continue;
        };
        let mut c = c.clone();
        match ConstrType::try_from(c.contype) {
            Ok(ConstrType::ConstrPrimary | ConstrType::ConstrUnique) => {
                c.keys = vec![colname_node.clone()];
            }
            Ok(ConstrType::ConstrForeign) => c.fk_attrs = vec![colname_node.clone()],
            Ok(ConstrType::ConstrCheck) => {}
            _ => continue,
        }
        add_constraint_node(interp, relid, &c, &cd.colname, only_here)?;
    }
    Ok(())
}

/// `ADD CONSTRAINT` of one constraint node (`ATExecAddConstraint`).
fn add_constraint_node(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    c: &pg_query::protobuf::Constraint,
    cmd_name: &str,
    rec: super::inherit::Recursion,
) -> Result<(), DdlError> {
    // `ADD {PRIMARY KEY | UNIQUE} USING INDEX idx` turns an existing unique
    // index into the constraint (ATExecAddIndexConstraint).
    if !c.indexname.is_empty() {
        return add_index_constraint(interp, relid, c);
    }

    if c.contype == ConstrType::ConstrExclusion as i32 {
        let (keys, names) = exclusion_keys(interp, relid, c)?;
        let conname = ConName::from_explicit(
            &c.conname,
            ConName::Relation {
                addition: crate::ddl::util::index_name_addition(&names),
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
        // A primary key's columns get (local) not-null constraints.
        for col in &pk_cols {
            if interp.attribute_by_name(relid, col).is_some() {
                let only_here = super::inherit::Recursion {
                    recurse: false,
                    recursing: false,
                };
                super::inherit::set_not_null(interp, relid, col, None, only_here)?;
            }
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
                    addition: crate::ddl::util::index_name_addition(&cols),
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
        let explicit = (!c.conname.is_empty()).then_some(c.conname.as_str());
        super::inherit::set_not_null(interp, relid, &col_name, explicit, rec)?;
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
        emit_constraint_with_backing_index(
            interp,
            relid,
            conname,
            ConType::Check,
            Vec::new(),
            None,
            Vec::new(),
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
        let attrs = interp.attributes_of(relid).to_vec();
        let local_types: Vec<PgTypeOid> = column_names
            .iter()
            .filter_map(|n| attrs.iter().find(|a| &a.attname == n).map(|a| a.atttypid))
            .collect();
        if local_types.len() != column_names.len() {
            return Err(DdlError::Parse(
                "ALTER TABLE ADD FOREIGN KEY references unknown local column".to_string(),
            ));
        }
        let attnums: Vec<i16> = column_names
            .iter()
            .filter_map(|n| attrs.iter().find(|a| &a.attname == n).map(|a| a.attnum))
            .collect();
        let relname_owned = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        let (target_oid, target_attnums) =
            resolve_fk_target(interp, c, &relname_owned, &column_names, &local_types)?;
        let conname = ConName::from_explicit(
            &c.conname,
            ConName::Constraint {
                addition: crate::ddl::util::index_name_addition(&column_names),
                label: "fkey",
            },
        )
        .resolve(interp, relid);
        emit_constraint_with_backing_index(
            interp,
            relid,
            conname,
            ConType::ForeignKey,
            attnums,
            Some(target_oid),
            target_attnums,
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
    c: &pg_query::protobuf::Constraint,
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
    if !index.indisunique {
        return Err(DdlError::Parse(format!(
            "\"{}\" is not a unique index",
            c.indexname
        )));
    }
    let is_primary = c.contype == ConstrType::ConstrPrimary as i32;
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
    if is_primary {
        let only_here = super::inherit::Recursion {
            recurse: false,
            recursing: false,
        };
        for attnum in index.indkey.iter().copied().filter(|&an| an > 0) {
            let colname = interp
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum == attnum)
                .map(|a| a.attname.clone())
                .unwrap_or_default();
            super::inherit::set_not_null(interp, relid, &colname, None, only_here)?;
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
        conkey: index.indkey,
        confrelid: None,
        confkey: Vec::new(),
        conislocal: true,
        coninhcount: 0,
    });
    Ok(())
}

/// Run a CHECK expression through the analyzer in the scope of `relid`
/// and verify its result type is boolean. Used by ALTER TABLE ADD
/// CONSTRAINT (the CREATE TABLE path uses [`validate_constraint_expressions`]
/// which sees the full `CreateStmt` shape).
fn validate_check_expression_for_table(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &pg_query::protobuf::Node,
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
                .map(|c| c.contype);
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
                Some(contype) => {
                    let name = match contype {
                        ConType::PrimaryKey => {
                            choose_relation_name(interp, nsoid, relname, "", "pkey")
                        }
                        _ => choose_relation_name(interp, nsoid, relname, &addition, "key"),
                    };
                    emit_constraint_with_backing_index(
                        interp,
                        relid,
                        name,
                        contype,
                        indkey,
                        None,
                        Vec::new(),
                    )?;
                }
                None => {
                    let name = choose_relation_name(interp, nsoid, relname, &addition, "idx");
                    let indexrelid = PgClassOid::from_nonzero(interp.alloc_oid()?);
                    interp.insert_pg_class(PgClass {
                        oid: indexrelid,
                        relname: name,
                        relnamespace: nsoid,
                        relkind: RelKind::Index,
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
