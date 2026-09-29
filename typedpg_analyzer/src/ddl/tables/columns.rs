use super::*;

/// The integer type behind a `smallserial` / `serial` / `bigserial` column
/// type, or `None` for any other type name. Mirrors the check at the top of
/// `transformColumnDefinition` (`parse_utilcmd.c`): the name must be
/// unqualified or `pg_catalog`-qualified, with no array bounds.
pub(crate) fn serial_base_type(tn: &typedpg_pg_query::protobuf::TypeName) -> Option<PgTypeOid> {
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

/// find_composite_type_dependencies (tablecmds.c): the relation `orig`'s
/// row type (or an attribute type of composite type `orig`) can't change
/// while a stored column holds values of it — directly, or through an
/// array, domain, range or multirange over it, or a view's or composite
/// type's row type that contains it. A column of a relation with storage
/// or partitions blocks; other relations only pass the question on.
pub(crate) fn find_composite_type_dependencies(
    interp: &PgCatalog,
    type_oid: PgTypeOid,
    orig: PgClassOid,
) -> Result<(), DdlError> {
    let mut visited = Vec::new();
    composite_dependencies_of(interp, type_oid, orig, &mut visited)
}

fn composite_dependencies_of(
    interp: &PgCatalog,
    type_oid: PgTypeOid,
    orig: PgClassOid,
    visited: &mut Vec<PgTypeOid>,
) -> Result<(), DdlError> {
    if visited.contains(&type_oid) {
        return Ok(());
    }
    visited.push(type_oid);
    // Types containing it.
    let mut containers: Vec<PgTypeOid> = interp
        .pg_type
        .values()
        .filter(|t| {
            (t.typcategory == TypCategory::Array && t.typelem == Some(type_oid))
                || t.typbasetype == Some(type_oid)
        })
        .map(|t| t.oid)
        .collect();
    containers.extend(
        interp
            .pg_range
            .values()
            .filter(|r| r.rngsubtype == type_oid)
            .map(|r| r.rngtypid),
    );
    containers.extend(interp.pg_range.get(&type_oid).and_then(|r| r.rngmultitypid));
    containers.sort();
    for container in containers {
        composite_dependencies_of(interp, container, orig, visited)?;
    }
    // Relations with a column of it.
    let mut users: Vec<(PgClassOid, String)> = interp
        .pg_attribute
        .iter()
        .filter_map(|(rel, attrs)| {
            attrs
                .iter()
                .find(|a| a.atttypid == type_oid)
                .map(|a| (*rel, a.attname.clone()))
        })
        .collect();
    users.sort();
    for (rel, column) in users {
        let Some(class) = interp.pg_class.get(&rel) else {
            continue;
        };
        if matches!(
            class.relkind,
            RelKind::Table | RelKind::MaterializedView | RelKind::Partitioned
        ) {
            let orig_class = interp.pg_class.get(&orig);
            let orig_name = relname_of(interp, orig);
            let user = format!("{}.{column}", class.relname);
            return Err(DdlError::UnsupportedDdl(
                match orig_class.map(|c| c.relkind) {
                    Some(RelKind::CompositeType) => {
                        format!(
                            "cannot alter type \"{orig_name}\" because column \"{user}\" uses it"
                        )
                    }
                    Some(RelKind::ForeignTable) => format!(
                        "cannot alter foreign table \"{orig_name}\" because column \"{user}\" uses \
                         its row type"
                    ),
                    _ => format!(
                        "cannot alter table \"{orig_name}\" because column \"{user}\" uses its row \
                         type"
                    ),
                },
            ));
        }
        if let Some(row_type) = class.reltype {
            composite_dependencies_of(interp, row_type, orig, visited)?;
        }
    }
    Ok(())
}

/// DomainHasConstraints: a domain in `typ`'s chain carries a CHECK or NOT
/// NULL constraint.
fn domain_has_constraints(interp: &PgCatalog, typ: PgTypeOid) -> bool {
    let mut current = typ;
    while let Some(t) = interp.pg_type.get(&current) {
        if t.typtype != TypType::Domain {
            return false;
        }
        if t.typnotnull
            || interp
                .domain_constraints
                .get(&current)
                .is_some_and(|c| !c.is_empty())
        {
            return true;
        }
        match t.typbasetype {
            Some(base) => current = base,
            None => return false,
        }
    }
    false
}

/// has_partition_attrs: whether the table's partition key reads column
/// `attnum` (as a key column or in a key expression).
fn in_partition_key(interp: &PgCatalog, relid: PgClassOid, attnum: i16) -> bool {
    interp
        .partition_key_attrs
        .get(&relid)
        .is_some_and(|attrs| attrs.contains(&attnum))
}

/// The canonical text of a serial column's `nextval(...)` default: it
/// names the column's own sequence, so it matches no other default.
pub(crate) fn serial_default_text(relid: PgClassOid, attnum: i16) -> String {
    format!("nextval(<sequence of {relid}.{attnum}>)")
}

/// The DEFAULT expression written on a column definition, if any.
pub(crate) fn column_default_expr(
    cd: &typedpg_pg_query::protobuf::ColumnDef,
) -> Option<&typedpg_pg_query::protobuf::Node> {
    cd.raw_default.as_deref().or_else(|| {
        cd.constraints.iter().find_map(|n| match n.node.as_ref()? {
            node::Node::Constraint(c) if c.contype == ConstrType::ConstrDefault as i32 => {
                c.raw_expr.as_deref()
            }
            _ => None,
        })
    })
}

/// Where a column definition is being transformed
/// (`CreateStmtContext`): a typed table (`OF type`), a partition, a
/// partitioned table.
#[derive(Clone, Copy, Default)]
pub(crate) struct ColumnContext {
    pub(crate) of_type: bool,
    pub(crate) partbound: bool,
    pub(crate) partitioned: bool,
}

/// Parse a `ColumnDef` AST node into a `ParsedColumn` (shared between
/// CREATE TABLE and ALTER TABLE ADD COLUMN paths). Ports
/// `transformColumnDefinition` (parse_utilcmd.c): the column's constraint
/// clauses are processed in order, and each conflicting combination is
/// reported where PG reports it.
pub(crate) fn parse_column_def(
    interp: &PgCatalog,
    relname: &str,
    cd: &typedpg_pg_query::protobuf::ColumnDef,
    pk_columns: &[String],
    cx: ColumnContext,
) -> Result<ParsedColumn, DdlError> {
    use crate::pg_catalog::oid;
    // Detect SERIAL/BIGSERIAL/SMALLSERIAL from type name — typedpg_pg_query keeps the
    // original name and does NOT rewrite to int4 + nextval(...).
    let serial_type = cd.type_name.as_ref().and_then(serial_base_type);
    let is_serial = serial_type.is_some();
    if let Some(tn) = cd.type_name.as_ref()
        && !tn.array_bounds.is_empty()
        && serial_base_type(&typedpg_pg_query::protobuf::TypeName {
            array_bounds: Vec::new(),
            ..tn.clone()
        })
        .is_some()
    {
        return Err(DdlError::UnsupportedDdl(
            "array of serial is not implemented".into(),
        ));
    }

    let type_oid = match (serial_type, cd.type_name.as_ref()) {
        (Some(oid), _) => oid,
        (None, Some(tn)) => lookup_type_name(tn, interp)?,
        (None, None) => oid::UNKNOWN,
    };

    // Encode any `(n)` / `(p,s)` modifier sitting next to the type name.
    // Empty `typmods` (`varchar` plain) yields `None`.
    let typmod = match cd.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(interp, type_oid, &tn.typmods)?,
        None => None,
    };

    // transformConstraintAttrs, then the constraints; a serial column gets
    // a trailing `DEFAULT nextval(...)` so a conflicting DEFAULT /
    // GENERATED clause is detected.
    let mut constraints = fold_constraint_attrs(&cd.constraints)?;
    let mut need_notnull = false;
    let mut disallow_noinherit_notnull = false;
    if is_serial {
        constraints.push(typedpg_pg_query::protobuf::Constraint {
            contype: ConstrType::ConstrDefault as i32,
            ..Default::default()
        });
        need_notnull = true;
        disallow_noinherit_notnull = true;
    }
    if constraints.iter().any(|c| {
        matches!(
            ConstrType::try_from(c.contype),
            Ok(ConstrType::ConstrIdentity | ConstrType::ConstrPrimary)
        )
    }) {
        disallow_noinherit_notnull = true;
    }

    let colname = &cd.colname;
    let conflicting_null = || {
        DdlError::Parse(format!(
            "conflicting NULL/NOT NULL declarations for column \"{colname}\" of table \"{relname}\""
        ))
    };
    let conflicting_no_inherit = || {
        DdlError::Parse(format!(
            "conflicting NO INHERIT declarations for not-null constraints on column \"{colname}\""
        ))
    };
    let both = |what: &str| {
        DdlError::Parse(format!(
            "both {what} specified for column \"{colname}\" of table \"{relname}\""
        ))
    };

    let mut is_not_null = false;
    let mut saw_nullable = false;
    let mut saw_default = false;
    let mut saw_identity = false;
    let mut saw_generated = false;
    // The column's not-null constraint: (name, NO INHERIT).
    let mut notnull: Option<(Option<String>, bool)> = None;
    let mut identity: Option<AttIdentity> = None;
    let mut generated: Option<AttGenerated> = None;
    let mut identity_options = Vec::new();
    for c in &constraints {
        match ConstrType::try_from(c.contype) {
            Ok(ConstrType::ConstrNull) => {
                if (saw_nullable && is_not_null) || need_notnull {
                    return Err(conflicting_null());
                }
                is_not_null = false;
                saw_nullable = true;
            }
            Ok(ConstrType::ConstrNotnull) => {
                if cx.partitioned && c.is_no_inherit {
                    return Err(DdlError::UnsupportedDdl(
                        "not-null constraints on partitioned tables cannot be NO INHERIT".into(),
                    ));
                }
                if saw_nullable && !is_not_null {
                    return Err(conflicting_null());
                }
                if disallow_noinherit_notnull && c.is_no_inherit {
                    return Err(conflicting_no_inherit());
                }
                let name = (!c.conname.is_empty()).then(|| c.conname.clone());
                match notnull.as_mut() {
                    None => {
                        is_not_null = true;
                        saw_nullable = true;
                        need_notnull = false;
                        notnull = Some((name, c.is_no_inherit));
                    }
                    Some((existing, no_inherit)) => {
                        if let (Some(a), Some(b)) = (existing.as_ref(), name.as_ref())
                            && a != b
                        {
                            return Err(DdlError::Parse(format!(
                                "conflicting not-null constraint names \"{a}\" and \"{b}\""
                            )));
                        }
                        if *no_inherit != c.is_no_inherit {
                            return Err(conflicting_no_inherit());
                        }
                        if existing.is_none() {
                            *existing = name;
                        }
                    }
                }
            }
            Ok(ConstrType::ConstrDefault) => {
                if saw_default {
                    return Err(DdlError::Parse(format!(
                        "multiple default values specified for column \"{colname}\" of table \"{relname}\""
                    )));
                }
                saw_default = true;
            }
            Ok(ConstrType::ConstrIdentity) => {
                if cx.of_type {
                    return Err(DdlError::UnsupportedDdl(
                        "identity columns are not supported on typed tables".into(),
                    ));
                }
                if cx.partbound {
                    return Err(DdlError::UnsupportedDdl(
                        "identity columns are not supported on partitions".into(),
                    ));
                }
                if saw_identity {
                    return Err(DdlError::Parse(format!(
                        "multiple identity specifications for column \"{colname}\" of table \"{relname}\""
                    )));
                }
                // generateSerialExtraStmts → init_params: the sequence's type.
                if !matches!(type_oid, oid::INT2 | oid::INT4 | oid::INT8) {
                    return Err(DdlError::Parse(
                        "identity column type must be smallint, integer, or bigint".into(),
                    ));
                }
                identity_options.clone_from(&c.options);
                // PG `ATTRIBUTE_IDENTITY_ALWAYS` is `'a'`,
                // `ATTRIBUTE_IDENTITY_BY_DEFAULT` is `'d'`.
                identity = Some(match c.generated_when.as_str() {
                    "a" => AttIdentity::Always,
                    _ => AttIdentity::ByDefault,
                });
                saw_identity = true;
                if !saw_nullable {
                    need_notnull = true;
                } else if !is_not_null {
                    return Err(conflicting_null());
                }
            }
            Ok(ConstrType::ConstrGenerated) => {
                if cx.of_type {
                    return Err(DdlError::UnsupportedDdl(
                        "generated columns are not supported on typed tables".into(),
                    ));
                }
                if saw_generated {
                    return Err(DdlError::Parse(format!(
                        "multiple generation clauses specified for column \"{colname}\" of table \"{relname}\""
                    )));
                }
                // gram.y's opt_virtual_or_stored: VIRTUAL unless STORED.
                generated = Some(if c.generated_kind == "s" {
                    AttGenerated::Stored
                } else {
                    AttGenerated::Virtual
                });
                if let Some(expr) = c.raw_expr.as_deref() {
                    crate::ddl::volatile::check_no_volatile(
                        expr,
                        crate::ddl::volatile::ExprLocation::Generated,
                        interp,
                    )?;
                }
                saw_generated = true;
            }
            Ok(ConstrType::ConstrPrimary) => {
                if saw_nullable && !is_not_null {
                    return Err(conflicting_null());
                }
                need_notnull = true;
            }
            // CHECK, UNIQUE and FOREIGN KEY are processed with the table's
            // constraints.
            _ => {}
        }
        if saw_default && saw_identity {
            return Err(both("default and identity"));
        }
        if saw_default && saw_generated {
            return Err(both("default and generation expression"));
        }
        if saw_identity && saw_generated {
            return Err(both("identity and generation expression"));
        }
    }
    // A not-null constraint for PRIMARY KEY, SERIAL or IDENTITY.
    if need_notnull && !(saw_nullable && is_not_null) {
        is_not_null = true;
        notnull = Some((None, false));
    }

    let mut not_null = is_not_null;
    if pk_columns.iter().any(|pk| pk == colname) {
        not_null = true;
    }
    let (nn_name, nn_no_inherit) = notnull.unwrap_or((None, false));

    // `COLLATE "name"` decoration on the column: PG rejects unknown names
    // and non-collatable types. The collation oid lands on pg_attribute.
    let collation =
        column_collation(interp, cd, type_oid)?.or_else(|| type_collation(interp, type_oid));

    Ok(ParsedColumn {
        name: cd.colname.clone(),
        type_oid,
        typmod,
        not_null,
        has_default: saw_default || saw_identity || saw_generated,
        generated,
        identity,
        collation,
        owned_sequence: if identity.is_some() {
            Some(crate::pg_catalog::DepType::Internal)
        } else if is_serial {
            Some(crate::pg_catalog::DepType::Auto)
        } else {
            None
        },
        identity_options,
        nn_local: not_null,
        nn_name,
        nn_no_inherit,
        nn_inhcount: 0,
        nn_inh_name: None,
        is_local: true,
        inhcount: 0,
        local_default: saw_default || saw_generated,
        inherited_default: None,
        bogus_default: false,
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
    // ATExecAddIdentity: the column's not-null constraint must be valid.
    if is_add
        && let Some(attr) = interp.attribute_by_name(relid, &cmd.name)
        && attr.attnotnull
        && let Some(con) = inherit::not_null_constraint(interp, relid, attr.attnum)
        && !con.convalidated
    {
        return Err(DdlError::Parse(format!(
            "incompatible NOT VALID constraint \"{}\" on relation \"{rel}\" (You might need to \
             validate it using ALTER TABLE ... VALIDATE CONSTRAINT.)",
            con.conname
        )));
    }
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
    // ATExecSetIdentity: the other options alter the identity sequence
    // (AlterSequence's init_params).
    if let Some(node::Node::List(list)) = def.node.as_ref() {
        let options: Vec<typedpg_pg_query::protobuf::Node> = list
            .items
            .iter()
            .filter(|o| {
                !matches!(o.node.as_ref(), Some(node::Node::DefElem(de)) if de.defname == "generated")
            })
            .cloned()
            .collect();
        for seq in crate::ddl::sequences::identity_sequences(interp, relid, attnum) {
            let current = interp.sequence_params.get(&seq).copied();
            let params = crate::ddl::seqparams::init_params(
                interp,
                &options,
                Some(
                    current.unwrap_or(crate::ddl::seqparams::SeqParams::defaults(
                        crate::pg_catalog::oid::INT8,
                    )),
                ),
            )?;
            interp.sequence_params.insert(seq, params);
        }
    }
    if is_add {
        let options = match def.node.as_ref() {
            Some(node::Node::Constraint(c)) => c.options.clone(),
            _ => Vec::new(),
        };
        crate::ddl::sequences::create_owned_sequence(
            interp,
            relid,
            attnum,
            crate::pg_catalog::DepType::Internal,
            &options,
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
    add_column_to(interp, relid, cmd, rec)
}

/// [`add_column`] for one relation of the recursion.
fn add_column_to(
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

    let cx = ColumnContext {
        partitioned: interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned),
        ..ColumnContext::default()
    };
    let col = parse_column_def(interp, &relname_of(interp, relid), cd, &[], cx)?;
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

    // ATExecAddColumn: a column with a default — or a constrained domain
    // type, whose NULL default is checked — rewrites a table's rows.
    if interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Table)
        && (col.has_default || domain_has_constraints(interp, col.type_oid))
        && let Some(row_type) = interp.pg_class.get(&relid).and_then(|c| c.reltype)
    {
        find_composite_type_dependencies(interp, row_type, relid)?;
    }
    // ATExecAddColumn: CheckAttributeType.
    if col.generated == Some(AttGenerated::Virtual) {
        super::generated::check_virtual_column_type(interp, &col.name, col.type_oid)?;
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
        attgenerated: col.generated,
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
        interp.attr_default_exprs.insert(
            (relid, next_attnum),
            match column_default_expr(cd) {
                Some(expr) => super::check_inherit::check_expr_text(expr),
                None => serial_default_text(relid, next_attnum),
            },
        );
    }
    crate::ddl::defaults::record_default_dependencies(
        interp,
        relid,
        next_attnum,
        column_default_expr(cd),
    );
    if !rec.recursing
        && let Some(deptype) = col.owned_sequence
    {
        crate::ddl::sequences::create_owned_sequence(
            interp,
            relid,
            next_attnum,
            deptype,
            &col.identity_options,
        )?;
    }
    // The generation expression, now that the column exists.
    for c_node in &cd.constraints {
        if let Some(node::Node::Constraint(c)) = c_node.node.as_ref()
            && c.contype == ConstrType::ConstrGenerated as i32
            && let Some(expr) = c.raw_expr.as_deref()
            && let Some(kind) = col.generated
        {
            let cooked = super::generated::cook_generation_expr(
                interp,
                relid,
                &col.name,
                col.type_oid,
                kind,
                expr,
            )?;
            cooked.record(interp, relid, next_attnum);
        }
    }
    for child in children {
        add_column_to(interp, child, cmd, rec.child())?;
    }
    // The column's constraints run as later subcommands
    // (transformAlterTableStmt), once every child has the column: its
    // not-null constraint first (AT_PASS_ADD_CONSTR), reaching the children.
    if !rec.recursing {
        if col.not_null {
            inherit::add_not_null(
                interp,
                relid,
                &col.name,
                inherit::NotNullSpec {
                    name: col.nn_name.as_deref(),
                    no_inherit: col.nn_no_inherit,
                    not_valid: false,
                },
                inherit::Recursion {
                    recurse: rec.recurse,
                    recursing: false,
                },
            )?;
        }
        add_column_constraints(interp, relid, cd)?;
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
    if in_partition_key(interp, relid, target.attnum) {
        return Err(DdlError::Parse(format!(
            "cannot drop column \"{}\" because it is part of the partition key of relation \
             \"{}\"",
            cmd.name,
            relname_of(interp, relid)
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

    // A generated column reading this one depends on it (through its
    // pg_attrdef entry): it needs CASCADE, and goes with it.
    let generated_dependents: Vec<String> = {
        let mut deps: Vec<(i16, String)> = interp
            .generated_refs
            .iter()
            .filter(|((rel, attnum), refs)| {
                *rel == relid && *attnum != target.attnum && refs.contains(&target.attnum)
            })
            .filter_map(|((_, attnum), _)| {
                interp
                    .attributes_of(relid)
                    .iter()
                    .find(|a| a.attnum == *attnum)
                    .map(|a| (a.attnum, a.attname.clone()))
            })
            .collect();
        deps.sort();
        deps.into_iter().map(|(_, name)| name).collect()
    };
    if let Some(first) = generated_dependents.first()
        && !cascade
    {
        let relname = relname_of(interp, relid);
        return Err(DdlError::DependencyError(format!(
            "cannot drop column {} of table {relname} because other objects depend on it \
             (column {first} of table {relname} depends on column {} of table {relname})",
            cmd.name, cmd.name,
        )));
    }
    for dependent in &generated_dependents {
        let drop_dependent = AlterTableCmd {
            name: dependent.clone(),
            missing_ok: true,
            ..cmd.clone()
        };
        drop_column(interp, relid, &drop_dependent, rec)?;
    }
    interp.generated_refs.remove(&(relid, target.attnum));

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
        // A constraint whose index goes (through an INCLUDE column) goes
        // too.
        let index_names: Vec<String> = dependent_indexes
            .iter()
            .filter_map(|i| interp.pg_class.get(i).map(|c| c.relname.clone()))
            .collect();
        let always_drop: Vec<_> = interp
            .pg_constraint
            .values()
            .filter(|c| {
                c.conrelid == relid
                    && (c.conkey.contains(&an)
                        || (matches!(
                            c.contype,
                            ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
                        ) && index_names.contains(&c.conname)))
            })
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
    interp.attr_default_exprs.remove(&(relid, target.attnum));
    crate::ddl::statistics::drop_column_statistics(interp, relid, target.attnum);
    crate::ddl::defaults::forget_default_dependencies(interp, relid, target.attnum);
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
    // ATExecColumnDefault: an identity or generated column's default is
    // its identity / generation expression.
    if let Some(attr) = &attr {
        let relname = relname_of(interp, relid);
        if attr.attidentity.is_some() {
            let hint = if cmd.def.is_none() {
                " (Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.)"
            } else {
                ""
            };
            return Err(DdlError::Parse(format!(
                "column \"{}\" of relation \"{relname}\" is an identity column{hint}",
                cmd.name
            )));
        }
        if let Some(kind) = attr.attgenerated {
            let hint = match (cmd.def.is_some(), kind) {
                (true, _) => " (Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead.)",
                (false, AttGenerated::Stored) => {
                    " (Use ALTER TABLE ... ALTER COLUMN ... DROP EXPRESSION instead.)"
                }
                (false, AttGenerated::Virtual) => "",
            };
            return Err(DdlError::Parse(format!(
                "column \"{}\" of relation \"{relname}\" is a generated column{hint}",
                cmd.name
            )));
        }
    }
    if let Some(attr) = &attr {
        match cmd.def.as_deref() {
            Some(expr) => {
                let default_type =
                    crate::ddl::defaults::check_default(interp, expr, &cmd.name, attr.atttypid)?;
                interp
                    .attr_default_types
                    .insert((relid, attr.attnum), default_type);
                interp.attr_default_exprs.insert(
                    (relid, attr.attnum),
                    super::check_inherit::check_expr_text(expr),
                );
            }
            None => {
                interp.attr_default_types.remove(&(relid, attr.attnum));
                interp.attr_default_exprs.remove(&(relid, attr.attnum));
            }
        }
        crate::ddl::defaults::record_default_dependencies(
            interp,
            relid,
            attr.attnum,
            cmd.def.as_deref(),
        );
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
    // ATPrepAlterColumnType: USING would contradict the generation
    // expression.
    if let Some(attr) = interp.attribute_by_name(relid, &cmd.name)
        && attr.attgenerated.is_some()
        && cd.raw_default.is_some()
    {
        return Err(DdlError::Parse(format!(
            "cannot specify USING when altering type of generated column (Column \"{}\" is a \
             generated column.)",
            cmd.name
        )));
    }
    if let Some(attr) = interp.attribute_by_name(relid, &cmd.name)
        && !rec.recursing
        && attr.attinhcount > 0
    {
        return Err(DdlError::Parse(format!(
            "cannot alter inherited column \"{}\"",
            cmd.name
        )));
    }
    if let Some(attr) = interp.attribute_by_name(relid, &cmd.name)
        && in_partition_key(interp, relid, attr.attnum)
    {
        return Err(DdlError::Parse(format!(
            "cannot alter column \"{}\" because it is part of the partition key of relation \
             \"{}\"",
            cmd.name,
            relname_of(interp, relid)
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
    let new_collation = column_collation(interp, cd, new_type_oid)?
        .or_else(|| type_collation(interp, new_type_oid));

    // CheckAttributeType(..., CHKATYPE_IS_VIRTUAL).
    if attr.attgenerated == Some(AttGenerated::Virtual) {
        super::generated::check_virtual_column_type(interp, &cmd.name, new_type_oid)?;
    }
    // ATPrepAlterColumnType: the old value (or the USING expression) must
    // be assignment-coercible to the new type — a virtual column stores
    // none.
    if !rec.recursing && attr.attgenerated != Some(AttGenerated::Virtual) {
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
        let what = if attr.attgenerated.is_some() {
            "generation expression"
        } else {
            "default"
        };
        return Err(DdlError::Parse(format!(
            "{what} for column \"{}\" cannot be cast automatically to type {}",
            cmd.name,
            format_type_for_message(interp, new_type_oid)
        )));
    }
    // RememberAllDependentForRebuilding: a generated column reading this
    // one pins its type.
    if let Some(((_, generated), _)) = interp
        .generated_refs
        .iter()
        .filter(|((rel, gen_attnum), refs)| {
            *rel == relid && *gen_attnum != attr.attnum && refs.contains(&attr.attnum)
        })
        .min_by_key(|((_, gen_attnum), _)| *gen_attnum)
    {
        let generated_name = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == *generated)
            .map(|a| a.attname.clone())
            .unwrap_or_default();
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter type of a column used by a generated column (Column \"{}\" is used \
             by generated column \"{generated_name}\".)",
            cmd.name
        )));
    }

    // The table's rewrite (or, without storage, this check now): no stored
    // column may hold its row type.
    if let Some(row_type) = interp.pg_class.get(&relid).and_then(|c| c.reltype) {
        find_composite_type_dependencies(interp, row_type, relid)?;
    }
    // ATPrepAlterColumnType's recursion: a descendant's column may not
    // also come from a parent outside the altered tree.
    if rec.recurse && !rec.recursing {
        let tree = inherit::all_inheritors(interp, relid);
        for &descendant in &tree[1..] {
            let numparents = interp
                .pg_inherits
                .iter()
                .filter(|h| h.inhrelid == descendant && tree.contains(&h.inhparent))
                .count() as i16;
            if interp
                .attribute_by_name(descendant, &cmd.name)
                .is_some_and(|a| a.attinhcount > numparents)
            {
                return Err(DdlError::Parse(format!(
                    "cannot alter inherited column \"{}\" of relation \"{}\"",
                    cmd.name,
                    relname_of(interp, descendant)
                )));
            }
        }
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
    expr: &typedpg_pg_query::protobuf::Node,
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

/// A type's default collation (`typcollation`): a domain's own, else its
/// base type's; `None` for non-collatable types.
pub(crate) fn type_collation(
    interp: &PgCatalog,
    type_oid: PgTypeOid,
) -> Option<crate::oid::PgCollationOid> {
    interp.pg_type.get(&type_oid).and_then(|t| t.typcollation)
}

/// A column's collation (`GetColumnDefCollation`, parse_type.c): the
/// explicit `COLLATE` — which the type must support — or none, meaning the
/// type's default.
pub(crate) fn column_collation(
    interp: &PgCatalog,
    cd: &typedpg_pg_query::protobuf::ColumnDef,
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

/// The column `name` of `relid` for an ALTER COLUMN subcommand: PG's
/// `column "x" of relation "t" does not exist`, or `cannot alter system
/// column` for a system column.
fn alter_target_column(
    interp: &PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<PgAttribute, DdlError> {
    if let Some(attr) = interp.attribute_by_name(relid, name) {
        return Ok(attr.clone());
    }
    if crate::pg_catalog::SYSTEM_COLUMNS
        .iter()
        .any(|(n, ..)| *n == name)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter system column \"{name}\""
        )));
    }
    Err(DdlError::Parse(column_not_found_msg(interp, relid, name)))
}

fn not_generated_msg(interp: &PgCatalog, relid: PgClassOid, name: &str) -> String {
    format!(
        "column \"{name}\" of relation \"{}\" is not a generated column",
        relname_of(interp, relid)
    )
}

/// `ALTER COLUMN c DROP EXPRESSION [IF EXISTS]` (`ATPrepDropExpression` /
/// `ATExecDropExpression`): the column keeps its current values and becomes
/// an ordinary column without a default; the change reaches the children
/// too, and `ONLY` is refused while there are any. An inherited column
/// keeps its parent's expression, and a virtual column has no values to
/// keep.
pub(crate) fn drop_expression(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let children = inherit::children_of(interp, relid);
    if !rec.recurse && !rec.recursing && !children.is_empty() {
        return Err(DdlError::UnsupportedDdl(
            "ALTER TABLE / DROP EXPRESSION must be applied to child tables too".into(),
        ));
    }
    if !rec.recursing {
        let attr = interp
            .attribute_by_name(relid, &cmd.name)
            .ok_or_else(|| DdlError::Parse(column_not_found_msg(interp, relid, &cmd.name)))?;
        if attr.attinhcount > 0 {
            return Err(DdlError::Parse(
                "cannot drop generation expression from inherited column".into(),
            ));
        }
    }
    let attr = alter_target_column(interp, relid, &cmd.name)?;
    let relname = relname_of(interp, relid);
    match attr.attgenerated {
        Some(AttGenerated::Virtual) => {
            return Err(DdlError::UnsupportedDdl(format!(
                "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns \
                 (Column \"{}\" of relation \"{relname}\" is a virtual generated column.)",
                cmd.name
            )));
        }
        None if cmd.missing_ok => {}
        None => return Err(DdlError::Parse(not_generated_msg(interp, relid, &cmd.name))),
        Some(AttGenerated::Stored) => {
            if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
                && let Some(a) = attrs.iter_mut().find(|a| a.attnum == attr.attnum)
            {
                a.attgenerated = None;
                a.atthasdef = false;
            }
            interp.attr_default_types.remove(&(relid, attr.attnum));
            interp.attr_default_exprs.remove(&(relid, attr.attnum));
            interp.generated_refs.remove(&(relid, attr.attnum));
        }
    }
    if rec.recurse {
        for child in children {
            if interp.attribute_by_name(child, &cmd.name).is_some() {
                drop_expression(interp, child, cmd, rec.child())?;
            }
        }
    }
    Ok(())
}

/// `ALTER COLUMN c SET EXPRESSION AS (expr)` (`ATExecSetExpression`): only
/// for a generated column, and — until PG rechecks the constraints and row
/// filters over the new values — not for a virtual one of a table with CHECK
/// constraints or in a publication. The new expression is cooked like the
/// original one (`cookDefault`); the change reaches the children.
pub(crate) fn set_expression(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let attr = alter_target_column(interp, relid, &cmd.name)?;
    let Some(kind) = attr.attgenerated else {
        return Err(DdlError::Parse(not_generated_msg(interp, relid, &cmd.name)));
    };
    let relname = relname_of(interp, relid);
    let virtual_detail = || {
        format!(
            "(Column \"{}\" of relation \"{relname}\" is a virtual generated column.)",
            cmd.name
        )
    };
    if kind == AttGenerated::Virtual
        && interp
            .pg_constraint
            .values()
            .any(|c| c.conrelid == relid && c.contype == ConType::Check)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in \
             tables with check constraints {}",
            virtual_detail()
        )));
    }
    if kind == AttGenerated::Virtual
        && crate::ddl::publications::relation_in_publication(interp, relid)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in \
             tables that are part of a publication {}",
            virtual_detail()
        )));
    }
    if kind == AttGenerated::Stored
        && interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Table)
        && let Some(row_type) = interp.pg_class.get(&relid).and_then(|c| c.reltype)
    {
        find_composite_type_dependencies(interp, row_type, relid)?;
    }
    if let Some(expr) = cmd.def.as_deref() {
        let cooked = super::generated::cook_generation_expr(
            interp,
            relid,
            &attr.attname,
            attr.atttypid,
            kind,
            expr,
        )?;
        cooked.record(interp, relid, attr.attnum);
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
