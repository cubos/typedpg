use super::*;

/// Resolve every parent in an `INHERITS (…)` clause, copy each parent's
/// columns into the child's `pg_attribute` (skipping names already present
/// on the child), and emit one `pg_inherits` row per parent.
pub(crate) fn apply_inherits(
    interp: &mut PgCatalog,
    child_oid: PgClassOid,
    parents: &[pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    let mut seq: i32 = 1;
    for parent_node in parents {
        let Some(node::Node::RangeVar(rv)) = parent_node.node.as_ref() else {
            continue;
        };
        let (parent_schema, parent_name) = range_var_names(rv, interp);
        let Some(parent_nsoid) = interp.namespace_oid(&parent_schema) else {
            return Err(DdlError::TableNotFound(
                QualifiedName::new(&parent_schema, &parent_name).to_string(),
            ));
        };
        let parent_oid = interp
            .class_by_qname
            .get(&(parent_nsoid, parent_name.clone()))
            .copied()
            .ok_or_else(|| {
                DdlError::TableNotFound(format!("relation \"{parent_name}\" does not exist"))
            })?;

        // Snapshot existing child column names; PG merges any same-named
        // parent column into the child's existing column rather than adding
        // a duplicate.
        let existing: std::collections::HashSet<String> = interp
            .attributes_of(child_oid)
            .iter()
            .map(|a| a.attname.clone())
            .collect();
        let next_attnum = interp
            .attributes_of(child_oid)
            .iter()
            .map(|a| a.attnum)
            .max()
            .unwrap_or(0);
        let parent_attrs: Vec<PgAttribute> = interp.attributes_of(parent_oid).to_vec();

        let mut attnum = next_attnum;
        for pa in parent_attrs {
            if existing.contains(&pa.attname) {
                continue;
            }
            attnum += 1;
            interp.insert_pg_attribute(PgAttribute {
                attrelid: child_oid,
                attname: pa.attname,
                atttypid: pa.atttypid,
                attnum,
                attnotnull: pa.attnotnull,
                atthasdef: pa.atthasdef,
                attgenerated: pa.attgenerated,
                atttypmod: pa.atttypmod,
                attidentity: pa.attidentity,
                attcollation: pa.attcollation,
            });
        }

        interp.pg_inherits.push(PgInherits {
            inhrelid: child_oid,
            inhparent: parent_oid,
            inhseqno: seq,
        });
        seq += 1;
    }
    Ok(())
}

/// The integer type behind a `smallserial` / `serial` / `bigserial` column
/// type, or `None` for any other type name. Mirrors the check at the top of
/// `transformColumnDefinition` (`parse_utilcmd.c`): the name must be
/// unqualified or `pg_catalog`-qualified, with no array bounds.
pub(crate) fn serial_base_type(tn: &pg_query::protobuf::TypeName) -> Option<PgTypeOid> {
    use crate::pg_catalog::oid;
    if !tn.array_bounds.is_empty() || tn.pct_type {
        return None;
    }
    let parts: Vec<&str> = tn
        .names
        .iter()
        .filter_map(super::super::util::node_string)
        .collect();
    let name = match parts.as_slice() {
        [name] | ["pg_catalog", name] => *name,
        _ => return None,
    };
    match name {
        "smallserial" | "serial2" => Some(oid::INT2),
        "serial" | "serial4" => Some(oid::INT4),
        "bigserial" | "serial8" => Some(oid::INT8),
        _ => None,
    }
}

/// Parse a `ColumnDef` AST node into a `ParsedColumn` (shared between
/// CREATE TABLE and ALTER TABLE ADD COLUMN paths).
pub(crate) fn parse_column_def(
    interp: &PgCatalog,
    cd: &pg_query::protobuf::ColumnDef,
    pk_columns: &[String],
) -> Result<ParsedColumn, DdlError> {
    // Detect SERIAL/BIGSERIAL/SMALLSERIAL from type name — pg_query keeps the
    // original name and does NOT rewrite to int4 + nextval(...).
    let serial_type = cd.type_name.as_ref().and_then(serial_base_type);
    let is_serial = serial_type.is_some();

    let type_oid = match (serial_type, cd.type_name.as_ref()) {
        (Some(oid), _) => oid,
        (None, Some(tn)) => lookup_type_name(tn, interp)?,
        (None, None) => crate::pg_catalog::oid::UNKNOWN,
    };

    // Encode any `(n)` / `(p,s)` modifier sitting next to the type name.
    // Empty `typmods` (`varchar` plain) yields `None`.
    let typmod = match cd.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(interp, type_oid, &tn.typmods)?,
        None => None,
    };

    let mut not_null = cd.is_not_null;
    let mut has_default = cd.raw_default.is_some() || cd.cooked_default.is_some();
    let mut is_generated = false;

    if is_serial {
        has_default = true;
    }

    let mut identity: Option<AttIdentity> = None;
    if !cd.identity.is_empty() {
        has_default = true;
        not_null = true;
        identity = match cd.identity.as_str() {
            "a" => Some(AttIdentity::Always),
            "d" => Some(AttIdentity::ByDefault),
            _ => None,
        };
    }

    if !cd.generated.is_empty() {
        has_default = true;
        is_generated = true;
    }

    for c_node in &cd.constraints {
        if let Some(node::Node::Constraint(c)) = c_node.node.as_ref() {
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrNotnull) => not_null = true,
                Ok(ConstrType::ConstrPrimary) => {
                    not_null = true;
                }
                Ok(ConstrType::ConstrDefault) => {
                    has_default = true;
                }
                Ok(ConstrType::ConstrIdentity) => {
                    has_default = true;
                    not_null = true;
                    if identity.is_none() {
                        identity = match c.generated_when.as_str() {
                            // PG `ATTRIBUTE_IDENTITY_ALWAYS` is `'a'`,
                            // `ATTRIBUTE_IDENTITY_BY_DEFAULT` is `'d'`.
                            "a" => Some(AttIdentity::Always),
                            "d" => Some(AttIdentity::ByDefault),
                            // Default to BY DEFAULT when unspecified, matching
                            // `GENERATED AS IDENTITY` shorthand semantics in
                            // some grammars; PG itself always emits one of the
                            // two so this is just a defensive fallback.
                            _ => Some(AttIdentity::ByDefault),
                        };
                    }
                }
                Ok(ConstrType::ConstrGenerated) => {
                    has_default = true;
                    is_generated = true;
                    if let Some(expr) = c.raw_expr.as_deref() {
                        crate::ddl::volatile::check_no_volatile(
                            expr,
                            crate::ddl::volatile::ExprLocation::Generated,
                            interp,
                        )?;
                    }
                }
                // No CHECK volatility check here — PG accepts the DDL even
                // when the predicate calls a VOLATILE function (it only
                // complains at runtime).
                _ => {}
            }
        }
    }

    if pk_columns.iter().any(|pk| pk == &cd.colname) {
        not_null = true;
    }

    // `COLLATE "name"` decoration on the column. PG rejects unknown names
    // up front; we mirror that. Collation oid lands on pg_attribute.
    let collation = if let Some(coll) = cd.coll_clause.as_deref() {
        let parts: Vec<&str> = coll
            .collname
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        let (schema, name) = match parts.as_slice() {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            _ => return Err(DdlError::Parse("malformed COLLATE clause".into())),
        };
        let resolved = interp.resolve_collation(schema, name).ok_or_else(|| {
            // PG includes the encoding in the message: `collation "X" for
            // encoding "UTF8" does not exist`. We don't model encoding so
            // we hardcode UTF8 (real PG uses the database's encoding).
            DdlError::Parse(format!(
                "collation \"{name}\" for encoding \"UTF8\" does not exist"
            ))
        })?;
        Some(resolved.oid)
    } else {
        None
    };

    Ok(ParsedColumn {
        name: cd.colname.clone(),
        type_oid,
        typmod,
        not_null,
        has_default,
        is_generated,
        identity,
        collation,
    })
}

pub(crate) fn set_identity(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };

    // `ALTER COLUMN x ADD GENERATED <kind> AS IDENTITY` parses with
    // `def = Constraint{contype=Identity, generated_when=…}`. The
    // `SET GENERATED <kind>` form parses with `def = List<DefElem>` where
    // one DefElem has `defname = "generated"` and `arg = Integer('a' | 'd')`.
    let new_identity = match def.node.as_ref() {
        Some(node::Node::Constraint(c)) if c.contype == ConstrType::ConstrIdentity as i32 => {
            match c.generated_when.as_str() {
                "a" => Some(AttIdentity::Always),
                "d" => Some(AttIdentity::ByDefault),
                _ => Some(AttIdentity::ByDefault),
            }
        }
        Some(node::Node::List(list)) => {
            let mut found = None;
            for item in &list.items {
                if let Some(node::Node::DefElem(de)) = item.node.as_ref()
                    && de.defname == "generated"
                    && let Some(arg) = de.arg.as_deref()
                    && let Some(node::Node::Integer(i)) = arg.node.as_ref()
                {
                    found = Some(match i.ival as u8 as char {
                        'a' => AttIdentity::Always,
                        _ => AttIdentity::ByDefault,
                    });
                    break;
                }
            }
            found
        }
        _ => return Ok(()),
    };

    let rel = relname_of(interp, relid);
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!(
            "relation \"{rel}\" does not exist"
        )));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name) else {
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation \"{rel}\" does not exist",
            cmd.name
        )));
    };
    if let Some(new_identity) = new_identity {
        col.attidentity = Some(new_identity);
        col.attnotnull = true;
        col.atthasdef = true;
    }
    Ok(())
}

pub(crate) fn drop_identity(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let rel = relname_of(interp, relid);
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!(
            "relation \"{rel}\" does not exist"
        )));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name) else {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation \"{rel}\" does not exist",
            cmd.name
        )));
    };
    col.attidentity = None;
    Ok(())
}

pub(crate) fn add_column(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };

    if interp
        .attributes_of(relid)
        .iter()
        .any(|a| a.attname == cd.colname)
    {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(column_exists_msg(
            interp,
            relid,
            &cd.colname,
        )));
    }

    let col = parse_column_def(interp, cd, &[])?;
    let next_attnum = interp
        .attributes_of(relid)
        .iter()
        .map(|a| a.attnum)
        .max()
        .unwrap_or(0)
        + 1;
    interp.insert_pg_attribute(PgAttribute {
        attrelid: relid,
        attname: col.name.clone(),
        atttypid: col.type_oid,
        attnum: next_attnum,
        attnotnull: col.not_null,
        atthasdef: col.has_default,
        attgenerated: col.is_generated.then_some(AttGenerated::Stored),
        atttypmod: col.typmod,
        attidentity: col.identity,
        attcollation: col.collation,
    });
    Ok(())
}

pub(crate) fn drop_column(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    if !interp
        .attributes_of(relid)
        .iter()
        .any(|a| a.attname == cmd.name)
    {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(column_not_found_msg(
            interp, relid, &cmd.name,
        )));
    }

    let cascade = matches!(
        DropBehavior::try_from(cmd.behavior),
        Ok(DropBehavior::DropCascade)
    );

    // Find dependent views from pg_depend.
    let dependent_views = views::find_views_depending_on_column(interp, relid, &cmd.name);
    if !dependent_views.is_empty() && !cascade {
        let view_names: Vec<String> = dependent_views
            .iter()
            .filter_map(|&v| {
                let c = interp.pg_class.get(&v)?;
                let nsname = interp.namespace_name(c.relnamespace)?;
                Some(QualifiedName::new(nsname, &c.relname).to_string())
            })
            .collect();
        let relname = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot drop column {} of table {relname} because other objects depend on it \
             (view(s) {} depend on this column)",
            cmd.name,
            view_names.join(", "),
        )));
    }

    // Resolve the column's attnum *before* we touch anything — both the
    // FK protection and the children-cascade need it.
    let target_attnum = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attname == cmd.name)
        .map(|a| a.attnum);

    // PG also blocks DROP COLUMN when a `pg_constraint` row references
    // it (PK/UNIQUE on this relation, or an FK on another relation whose
    // target column matches), or when a `pg_index` row's `indkey` contains
    // the attnum. CHECK constraints local to the column are *not* blockers
    // — PG silently drops them along with the column. Without CASCADE we
    // surface the same error PG does.
    if let Some(an) = target_attnum {
        // PG only blocks DROP COLUMN when there's an *external* dependency
        // — an FK in some *other* table that points at this column (or any
        // view, handled above). Everything local to the column on the
        // *same* relation (PK, UNIQUE, CHECK, FK source side, indexes) is
        // dropped silently along with the column, no CASCADE needed. This
        // matches PG's `dropdb` cascade-only-external semantics.
        let mut blockers: Vec<String> = Vec::new();
        for c in interp.pg_constraint.values() {
            // FKs in *other* tables referencing this column.
            if matches!(c.contype, ConType::ForeignKey)
                && c.confrelid == Some(relid)
                && c.confkey.contains(&an)
                && c.conrelid != relid
            {
                blockers.push(c.conname.clone());
            }
        }
        let dependent_indexes: Vec<PgClassOid> = interp
            .pg_index
            .values()
            .filter(|i| i.indrelid == relid && i.indkey.contains(&an))
            .map(|i| i.indexrelid)
            .collect();
        if !blockers.is_empty() && !cascade {
            let relname = interp
                .pg_class
                .get(&relid)
                .map(|c| c.relname.clone())
                .unwrap_or_default();
            return Err(DdlError::DependencyError(format!(
                "cannot drop column {} of table {relname} because other objects depend on it \
                 (constraint(s) {} depend on this column)",
                cmd.name,
                blockers.join(", "),
            )));
        }
        // Drop everything local to this column on this relation —
        // PK/UNIQUE/CHECK constraints, FK source side, and indexes —
        // regardless of CASCADE. PG treats these as part of the column.
        // External FKs (in other tables) only fall away under CASCADE.
        let always_drop: Vec<_> = interp
            .pg_constraint
            .values()
            .filter(|c| c.conrelid == relid && c.conkey.contains(&an))
            .map(|c| c.oid)
            .collect();
        for oid in always_drop {
            interp.pg_constraint.remove(&oid);
        }
        for &idx_oid in &dependent_indexes {
            interp.remove_pg_index(idx_oid);
            interp.remove_pg_class(idx_oid);
            let obj = crate::oid::PgGenericOid::from_nonzero(idx_oid.into_nonzero());
            interp.remove_dependencies_of(crate::pg_catalog::PG_CLASS_RELID, obj);
            interp.remove_dependencies_on(crate::pg_catalog::PG_CLASS_RELID, obj);
        }

        if cascade && !blockers.is_empty() {
            // CASCADE: drop the external FKs (in other tables) that
            // reference this column. Local stuff was already removed above.
            let to_drop_constraints: Vec<_> = interp
                .pg_constraint
                .values()
                .filter(|c| {
                    matches!(c.contype, ConType::ForeignKey)
                        && c.confrelid == Some(relid)
                        && c.confkey.contains(&an)
                        && c.conrelid != relid
                })
                .map(|c| c.oid)
                .collect();
            for oid in to_drop_constraints {
                interp.pg_constraint.remove(&oid);
            }
        }
    }

    if !dependent_views.is_empty() {
        views::drop_views(interp, &dependent_views);
    }

    if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
        attrs.retain(|a| a.attname != cmd.name);
    }

    // Cascade DROP COLUMN through pg_inherits: every direct (and transitive)
    // child that inherited the same column name loses it too. PG actually
    // tracks this with `attinhcount`; we approximate by walking pg_inherits
    // and removing the matching column from each descendant.
    cascade_drop_column_to_children(interp, relid, &cmd.name);

    Ok(())
}

/// Recursively remove a dropped column from every child relation that
/// inherits from `parent_oid`. Skips children that don't carry the column —
/// they may have it locally or have already had it dropped earlier in the
/// same operation.
fn cascade_drop_column_to_children(interp: &mut PgCatalog, parent_oid: PgClassOid, col_name: &str) {
    let direct_children: Vec<PgClassOid> = interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhparent == parent_oid)
        .map(|i| i.inhrelid)
        .collect();
    for child in direct_children {
        let had_col = interp
            .attributes_of(child)
            .iter()
            .any(|a| a.attname == col_name);
        if !had_col {
            continue;
        }
        if let Some(attrs) = interp.pg_attribute.get_mut(&child) {
            attrs.retain(|a| a.attname != col_name);
        }
        cascade_drop_column_to_children(interp, child, col_name);
    }
}

pub(crate) fn set_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col_name: &str,
    not_null: bool,
) -> Result<(), DdlError> {
    let rel = relname_of(interp, relid);
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!(
            "relation \"{rel}\" does not exist"
        )));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == col_name) else {
        return Err(DdlError::Parse(format!(
            "column \"{col_name}\" of relation \"{rel}\" does not exist"
        )));
    };
    col.attnotnull = not_null;
    Ok(())
}

pub(crate) fn set_default(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let rel = relname_of(interp, relid);
    let Some(attrs) = interp.pg_attribute.get_mut(&relid) else {
        return Err(DdlError::TableNotFound(format!(
            "relation \"{rel}\" does not exist"
        )));
    };
    let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name) else {
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation \"{rel}\" does not exist",
            cmd.name
        )));
    };
    col.atthasdef = cmd.def.is_some();
    Ok(())
}

pub(crate) fn alter_column_type(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };

    let new_type_oid = match cd.type_name.as_ref() {
        Some(tn) => lookup_type_name(tn, interp)?,
        None => return Ok(()),
    };

    let new_typmod = match cd.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(interp, new_type_oid, &tn.typmods)?,
        None => None,
    };

    let old_type_oid = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attname == cmd.name)
        .map(|a| a.atttypid)
        .ok_or_else(|| DdlError::Parse(column_not_found_msg(interp, relid, &cmd.name)))?;

    let dependent_views = views::find_views_depending_on_column(interp, relid, &cmd.name);
    if !dependent_views.is_empty() {
        // Match PG (SQLSTATE 0A000): any dependent view blocks `ALTER COLUMN
        // TYPE`, even when the change is binary-coercible. PG has no exemption
        // for "new type is the base of the old domain", so we don't either —
        // otherwise migrations the analyzer accepts would still fail at
        // production runtime.
        let view_names: Vec<String> = dependent_views
            .iter()
            .filter_map(|&v| {
                let c = interp.pg_class.get(&v)?;
                let nsname = interp.namespace_name(c.relnamespace)?;
                Some(QualifiedName::new(nsname, &c.relname).to_string())
            })
            .collect();
        let relname = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return Err(DdlError::DependencyError(format!(
            "cannot alter type of a column used by a view or rule: column {relname}.{} \
             is referenced by view(s) {} (hint: drop the view(s) first, alter the column, \
             then recreate)",
            cmd.name,
            view_names.join(", "),
        )));
    }

    if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(col) = attrs.iter_mut().find(|c| c.attname == cmd.name)
    {
        col.atttypid = new_type_oid;
        col.atttypmod = new_typmod;
    }

    let _ = old_type_oid;
    Ok(())
}
