use super::*;

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

/// The DEFAULT expression written on a column definition, if any.
pub(crate) fn column_default_expr(
    cd: &pg_query::protobuf::ColumnDef,
) -> Option<&pg_query::protobuf::Node> {
    cd.raw_default.as_deref().or_else(|| {
        cd.constraints.iter().find_map(|n| match n.node.as_ref()? {
            node::Node::Constraint(c) if c.contype == ConstrType::ConstrDefault as i32 => {
                c.raw_expr.as_deref()
            }
            _ => None,
        })
    })
}

/// Parse a `ColumnDef` AST node into a `ParsedColumn` (shared between
/// CREATE TABLE and ALTER TABLE ADD COLUMN paths).
pub(crate) fn parse_column_def(
    interp: &PgCatalog,
    relname: &str,
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

    // `transformColumnDefinition`: a serial column is its integer type
    // with `DEFAULT nextval(...)` from an owned sequence and NOT NULL.
    if is_serial {
        has_default = true;
        not_null = true;
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

    let mut nn_name: Option<String> = None;
    let mut saw_null = false;
    let mut saw_not_null = cd.is_not_null;
    for c_node in &cd.constraints {
        if let Some(node::Node::Constraint(c)) = c_node.node.as_ref() {
            match ConstrType::try_from(c.contype) {
                Ok(ConstrType::ConstrNotnull) => {
                    not_null = true;
                    saw_not_null = true;
                    if !c.conname.is_empty() {
                        nn_name = Some(c.conname.clone());
                    }
                }
                Ok(ConstrType::ConstrNull) => saw_null = true,
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

    // transformColumnDefinition rejects `NULL` together with `NOT NULL`.
    if saw_null && saw_not_null {
        return Err(DdlError::Parse(format!(
            "conflicting NULL/NOT NULL declarations for column \"{}\" of table \"{relname}\"",
            cd.colname
        )));
    }

    if pk_columns.iter().any(|pk| pk == &cd.colname) {
        not_null = true;
    }

    // `COLLATE "name"` decoration on the column: PG rejects unknown names
    // and non-collatable types. The collation oid lands on pg_attribute.
    let collation = column_collation(interp, cd, type_oid)?;

    Ok(ParsedColumn {
        name: cd.colname.clone(),
        type_oid,
        typmod,
        not_null,
        has_default,
        is_generated,
        identity,
        collation,
        owned_sequence: if identity.is_some() {
            Some(crate::pg_catalog::DepType::Internal)
        } else if is_serial {
            Some(crate::pg_catalog::DepType::Auto)
        } else {
            None
        },
        nn_local: not_null,
        nn_name,
        nn_inhcount: 0,
        nn_inh_name: None,
        is_local: true,
        inhcount: 0,
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

    let is_add = matches!(def.node.as_ref(), Some(node::Node::Constraint(_)));
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
    // ATExecAddIdentity / ATExecSetIdentity (tablecmds.c).
    if is_add {
        if !col.attnotnull {
            return Err(DdlError::Parse(format!(
                "column \"{}\" of relation \"{rel}\" must be declared NOT NULL before \
                 identity can be added",
                cmd.name
            )));
        }
        if col.attidentity.is_some() {
            return Err(DdlError::Parse(format!(
                "column \"{}\" of relation \"{rel}\" is already an identity column",
                cmd.name
            )));
        }
        if col.atthasdef {
            return Err(DdlError::Parse(format!(
                "column \"{}\" of relation \"{rel}\" already has a default value",
                cmd.name
            )));
        }
    } else if col.attidentity.is_none() {
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation \"{rel}\" is not an identity column",
            cmd.name
        )));
    }
    let attnum = col.attnum;
    if let Some(new_identity) = new_identity {
        col.attidentity = Some(new_identity);
        col.atthasdef = true;
    }
    if is_add {
        crate::ddl::sequences::create_owned_sequence(
            interp,
            relid,
            attnum,
            crate::pg_catalog::DepType::Internal,
        )?;
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
    if col.attidentity.take().is_none() {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(format!(
            "column \"{}\" of relation \"{rel}\" is not an identity column",
            cmd.name
        )));
    }
    col.atthasdef = false;
    let attnum = col.attnum;
    // The identity sequence is internal to the column and goes with it.
    for seq in crate::ddl::sequences::identity_sequences(interp, relid, attnum) {
        crate::ddl::drop::drop_relation_by_oid(interp, seq);
    }
    Ok(())
}

/// `ALTER TABLE ... ADD COLUMN` (`ATExecAddColumn`). Recurses into the
/// children, where a same-named column is merged (the types must agree)
/// instead of added; `ONLY` is refused while children exist.
pub(crate) fn add_column(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };
    let children = inherit::children_of(interp, relid);
    if !rec.recurse && !children.is_empty() {
        return Err(DdlError::Parse(
            "column must be added to child tables too".into(),
        ));
    }

    let col = parse_column_def(interp, &relname_of(interp, relid), cd, &[])?;
    let default_type = match column_default_expr(cd) {
        Some(expr) => Some(crate::ddl::defaults::check_default(
            interp,
            expr,
            &cd.colname,
            col.type_oid,
        )?),
        // serial: `DEFAULT nextval(...)`, a bigint.
        None if col.owned_sequence == Some(crate::pg_catalog::DepType::Auto) => {
            Some(crate::pg_catalog::oid::INT8)
        }
        None => None,
    };

    if let Some(existing) = interp.attribute_by_name(relid, &cd.colname).cloned() {
        if rec.recursing {
            // Merge into the child's own column.
            if existing.atttypid != col.type_oid || existing.atttypmod != col.typmod {
                return Err(DdlError::Parse(format!(
                    "child table \"{}\" has different type for column \"{}\"",
                    relname_of(interp, relid),
                    cd.colname
                )));
            }
            if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
                && let Some(a) = attrs.iter_mut().find(|a| a.attnum == existing.attnum)
            {
                a.attinhcount += 1;
            }
            if col.not_null {
                inherit::set_not_null(interp, relid, &cd.colname, None, rec)?;
            }
            return Ok(());
        }
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(column_exists_msg(
            interp,
            relid,
            &cd.colname,
        )));
    }

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
        attnotnull: false,
        atthasdef: col.has_default,
        attgenerated: col.is_generated.then_some(AttGenerated::Stored),
        atttypmod: col.typmod,
        // Identity is not inherited by regular children.
        attidentity: col.identity.filter(|_| !rec.recursing),
        attcollation: col.collation,
        attislocal: !rec.recursing,
        attinhcount: i16::from(rec.recursing),
    });
    if let Some(default_type) = default_type {
        interp
            .attr_default_types
            .insert((relid, next_attnum), default_type);
    }
    if col.not_null {
        let local = inherit::Recursion {
            recurse: false,
            recursing: rec.recursing,
        };
        inherit::set_not_null(interp, relid, &col.name, col.nn_name.as_deref(), local)?;
    }
    if !rec.recursing
        && let Some(deptype) = col.owned_sequence
    {
        crate::ddl::sequences::create_owned_sequence(interp, relid, next_attnum, deptype)?;
    }
    if !rec.recursing {
        add_column_constraints(interp, relid, cd)?;
    }
    for child in children {
        add_column(interp, child, cmd, rec.child())?;
    }
    Ok(())
}

pub(crate) fn drop_column(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(target) = interp.attribute_by_name(relid, &cmd.name).cloned() else {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(column_not_found_msg(
            interp, relid, &cmd.name,
        )));
    };
    // ATExecDropColumn: an inherited column only goes away with its parent.
    if !rec.recursing && target.attinhcount > 0 {
        return Err(DdlError::Parse(format!(
            "cannot drop inherited column \"{}\"",
            cmd.name
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

    if let Some(an) = target_attnum {
        for seq in crate::ddl::sequences::owned_sequences(interp, relid, Some(an)) {
            crate::ddl::drop::drop_relation_by_oid(interp, seq);
        }
    }

    interp.attr_default_types.remove(&(relid, target.attnum));
    if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
        attrs.retain(|a| a.attname != cmd.name);
    }

    // Children (ATExecDropColumn's recursion): a column inherited only from
    // this parent goes too; one that is also local or inherited from another
    // parent stays, with one inheritance fewer. Under ONLY the children's
    // copies become local.
    for child in inherit::children_of(interp, relid) {
        let Some(child_col) = interp.attribute_by_name(child, &cmd.name).cloned() else {
            continue;
        };
        if rec.recurse && child_col.attinhcount == 1 && !child_col.attislocal {
            drop_column(interp, child, cmd, rec.child())?;
        } else if let Some(attrs) = interp.pg_attribute.get_mut(&child)
            && let Some(a) = attrs.iter_mut().find(|a| a.attnum == child_col.attnum)
        {
            a.attinhcount = (a.attinhcount - 1).max(0);
            if !rec.recurse {
                a.attislocal = true;
            }
        }
    }

    Ok(())
}

/// `ALTER COLUMN c { SET | DROP } DEFAULT`, recursing into the children
/// unless `ONLY` (`ATSimpleRecursion`).
pub(crate) fn set_default(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let attr = interp.attribute_by_name(relid, &cmd.name).cloned();
    if let Some(attr) = &attr {
        match cmd.def.as_deref() {
            Some(expr) => {
                let default_type =
                    crate::ddl::defaults::check_default(interp, expr, &cmd.name, attr.atttypid)?;
                interp
                    .attr_default_types
                    .insert((relid, attr.attnum), default_type);
            }
            None => {
                interp.attr_default_types.remove(&(relid, attr.attnum));
            }
        }
    }
    if rec.recurse {
        for child in inherit::children_of(interp, relid) {
            if interp.attribute_by_name(child, &cmd.name).is_some() {
                set_default(interp, child, cmd, rec.child())?;
            }
        }
    }
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

/// `ALTER COLUMN c TYPE t` (`ATPrepAlterColumnType`): an inherited column
/// is only retyped through its parent, and the parent's change reaches every
/// child (`ONLY` is refused while a child has the column).
pub(crate) fn alter_column_type(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(def) = cmd.def.as_deref() else {
        return Ok(());
    };
    let Some(node::Node::ColumnDef(cd)) = def.node.as_ref() else {
        return Ok(());
    };
    if let Some(attr) = interp.attribute_by_name(relid, &cmd.name)
        && !rec.recursing
        && attr.attinhcount > 0
    {
        return Err(DdlError::Parse(format!(
            "cannot alter inherited column \"{}\"",
            cmd.name
        )));
    }
    let children: Vec<PgClassOid> = inherit::children_of(interp, relid)
        .into_iter()
        .filter(|&c| interp.attribute_by_name(c, &cmd.name).is_some())
        .collect();
    if !rec.recurse && !children.is_empty() {
        return Err(DdlError::Parse(format!(
            "type of inherited column \"{}\" must be changed in child tables too",
            cmd.name
        )));
    }
    let new_type_oid = match cd.type_name.as_ref() {
        Some(tn) => lookup_type_name(tn, interp)?,
        None => return Ok(()),
    };

    let new_typmod = match cd.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(interp, new_type_oid, &tn.typmods)?,
        None => None,
    };

    let attr = interp
        .attribute_by_name(relid, &cmd.name)
        .cloned()
        .ok_or_else(|| DdlError::Parse(column_not_found_msg(interp, relid, &cmd.name)))?;
    // GetColumnDefCollation: an explicit COLLATE, else the new type's
    // default — the old column's collation does not carry over.
    let new_collation = column_collation(interp, cd, new_type_oid)?;

    // ATPrepAlterColumnType: the old value (or the USING expression) must
    // be assignment-coercible to the new type.
    if !rec.recursing {
        match cd.raw_default.as_deref() {
            Some(using) => check_using_expression(interp, relid, &attr, using, new_type_oid)?,
            None => {
                if !crate::coerce::can_coerce(
                    attr.atttypid,
                    new_type_oid,
                    crate::coerce::CoercionContext::Assignment,
                    interp,
                ) {
                    return Err(DdlError::Parse(format!(
                        "column \"{}\" cannot be cast automatically to type {}",
                        cmd.name,
                        format_type_for_message(interp, new_type_oid)
                    )));
                }
            }
        }
    }
    // ATExecAlterColumnType: an existing default is re-coerced too.
    if let Some(&default_type) = interp.attr_default_types.get(&(relid, attr.attnum))
        && !crate::coerce::can_coerce(
            default_type,
            new_type_oid,
            crate::coerce::CoercionContext::Assignment,
            interp,
        )
    {
        return Err(DdlError::Parse(format!(
            "default for column \"{}\" cannot be cast automatically to type {}",
            cmd.name,
            format_type_for_message(interp, new_type_oid)
        )));
    }

    for child in children {
        alter_column_type(interp, child, cmd, rec.child())?;
    }
    let old_type_oid = attr.atttypid;

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
        col.attcollation = new_collation;
    }

    let _ = old_type_oid;
    Ok(())
}

/// The USING expression of ALTER COLUMN TYPE: evaluated over the table's
/// row, its result must be assignment-coercible to the new type (an untyped
/// literal goes through the type's input).
fn check_using_expression(
    interp: &PgCatalog,
    relid: PgClassOid,
    attr: &PgAttribute,
    expr: &pg_query::protobuf::Node,
    new_type: PgTypeOid,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::Scope;

    let relname = relname_of(interp, relid);
    let nspname = interp
        .pg_class
        .get(&relid)
        .and_then(|c| interp.namespace_name(c.relnamespace))
        .unwrap_or("public")
        .to_owned();
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
    let wrap = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(format!("{e}"));
    let result = infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(wrap)?;
    if result.type_oid == crate::pg_catalog::oid::UNKNOWN {
        infer_expr(
            expr,
            crate::expr::Ctx::new(&scope, &null_ctx, interp),
            &mut params,
            TypeGoal::assignment(new_type),
        )
        .map_err(wrap)?;
        return Ok(());
    }
    if !crate::coerce::can_coerce(
        result.type_oid,
        new_type,
        crate::coerce::CoercionContext::Assignment,
        interp,
    ) {
        return Err(DdlError::Parse(format!(
            "result of USING clause for column \"{}\" cannot be cast automatically to type {}",
            attr.attname,
            format_type_for_message(interp, new_type)
        )));
    }
    Ok(())
}

/// A column's collation (`GetColumnDefCollation`, parse_type.c): the
/// explicit `COLLATE` — which the type must support — or none, meaning the
/// type's default.
pub(crate) fn column_collation(
    interp: &PgCatalog,
    cd: &pg_query::protobuf::ColumnDef,
    type_oid: PgTypeOid,
) -> Result<Option<crate::oid::PgCollationOid>, DdlError> {
    let Some(coll) = cd.coll_clause.as_deref() else {
        return Ok(None);
    };
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
    let collatable = interp
        .pg_type
        .get(&interp.unwrap_domain(type_oid))
        .is_some_and(|t| t.typcollation.is_some());
    if !collatable && type_oid != crate::pg_catalog::oid::UNKNOWN {
        return Err(DdlError::Parse(format!(
            "collations are not supported by type {}",
            format_type_for_message(interp, type_oid)
        )));
    }
    Ok(Some(resolved.oid))
}

/// The generated column `name` of `relid`, or PG's `column "x" of relation
/// "t" is not a generated column` (`ATExecDropExpression` /
/// `ATExecSetExpression`).
fn generated_column(
    interp: &PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<Option<PgAttribute>, DdlError> {
    let attr = interp
        .attribute_by_name(relid, name)
        .cloned()
        .ok_or_else(|| DdlError::Parse(column_not_found_msg(interp, relid, name)))?;
    Ok(attr.attgenerated.is_some().then_some(attr))
}

fn not_generated_msg(interp: &PgCatalog, relid: PgClassOid, name: &str) -> String {
    format!(
        "column \"{name}\" of relation \"{}\" is not a generated column",
        relname_of(interp, relid)
    )
}

/// `ALTER COLUMN c DROP EXPRESSION [IF EXISTS]`: the column keeps its
/// current values and becomes an ordinary column without a default; the
/// change reaches the children too.
pub(crate) fn drop_expression(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(attr) = generated_column(interp, relid, &cmd.name)? else {
        if cmd.missing_ok {
            return Ok(());
        }
        return Err(DdlError::Parse(not_generated_msg(interp, relid, &cmd.name)));
    };
    if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(a) = attrs.iter_mut().find(|a| a.attnum == attr.attnum)
    {
        a.attgenerated = None;
        a.atthasdef = false;
    }
    interp.attr_default_types.remove(&(relid, attr.attnum));
    if rec.recurse {
        for child in inherit::children_of(interp, relid) {
            if interp.attribute_by_name(child, &cmd.name).is_some() {
                drop_expression(interp, child, cmd, rec.child())?;
            }
        }
    }
    Ok(())
}

/// `ALTER COLUMN c SET EXPRESSION AS (expr)`: only for a generated column;
/// the new expression, evaluated over the row, must suit the column type.
pub(crate) fn set_expression(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let Some(attr) = generated_column(interp, relid, &cmd.name)? else {
        return Err(DdlError::Parse(not_generated_msg(interp, relid, &cmd.name)));
    };
    if !rec.recursing
        && let Some(expr) = cmd.def.as_deref()
    {
        crate::ddl::volatile::check_no_volatile(
            expr,
            crate::ddl::volatile::ExprLocation::Generated,
            interp,
        )?;
        check_generation_expression(interp, relid, &attr, expr)?;
    }
    if rec.recurse {
        for child in inherit::children_of(interp, relid) {
            if interp.attribute_by_name(child, &cmd.name).is_some() {
                set_expression(interp, child, cmd, rec.child())?;
            }
        }
    }
    Ok(())
}

/// A generation expression over the table's row must yield the column's
/// type (`cookDefault` for generated columns): an untyped literal goes
/// through the type's input, anything else must be assignment-coercible.
fn check_generation_expression(
    interp: &PgCatalog,
    relid: PgClassOid,
    attr: &PgAttribute,
    expr: &pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::Scope;

    let relname = relname_of(interp, relid);
    let nspname = interp
        .pg_class
        .get(&relid)
        .and_then(|c| interp.namespace_name(c.relnamespace))
        .unwrap_or("public")
        .to_owned();
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
    let wrap = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(format!("{e}"));
    let result = infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(wrap)?;
    if result.type_oid == crate::pg_catalog::oid::UNKNOWN {
        infer_expr(
            expr,
            crate::expr::Ctx::new(&scope, &null_ctx, interp),
            &mut params,
            TypeGoal::assignment(attr.atttypid),
        )
        .map_err(wrap)?;
        return Ok(());
    }
    if !crate::coerce::can_coerce(
        result.type_oid,
        attr.atttypid,
        crate::coerce::CoercionContext::Assignment,
        interp,
    ) {
        return Err(DdlError::UnsupportedDdl(format!(
            "column \"{}\" is of type {} but default expression is of type {}",
            attr.attname,
            format_type_for_message(interp, attr.atttypid),
            format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(())
}
