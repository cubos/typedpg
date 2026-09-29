//! Inheritance-aware ALTER TABLE plumbing and PG 18 not-null constraints.
//!
//! PG recurses most ALTER TABLE subcommands from a parent into its
//! inheritance children and partitions (`ATSimpleRecursion`,
//! `ATPrepAddColumn`, …) unless `ONLY` is given, and tracks per column /
//! per constraint whether it is defined locally (`attislocal`,
//! `conislocal`) and how many parents it comes from (`attinhcount`,
//! `coninhcount`). A NOT NULL is a `pg_constraint` row with `contype = 'n'`
//! whose presence `attnotnull` mirrors.

use super::*;

/// How an ALTER TABLE subcommand reaches a relation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Recursion {
    /// Apply to the children too (no `ONLY`).
    pub(crate) recurse: bool,
    /// This invocation is the recursion into a child.
    pub(crate) recursing: bool,
}

impl Recursion {
    /// The invocation for a child reached from this one.
    pub(crate) fn child(self) -> Self {
        Self {
            recurse: true,
            recursing: true,
        }
    }
}

/// Direct inheritance children / partitions of `relid`, in creation order.
pub(crate) fn children_of(interp: &PgCatalog, relid: PgClassOid) -> Vec<PgClassOid> {
    interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhparent == relid)
        .map(|i| i.inhrelid)
        .collect()
}

/// The not-null constraint row of column `attnum`, if any.
pub(crate) fn not_null_constraint(
    interp: &PgCatalog,
    relid: PgClassOid,
    attnum: i16,
) -> Option<&PgConstraint> {
    interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && c.contype == ConType::NotNull && c.conkey == [attnum])
}

/// Name of the not-null constraint of a NOT NULL column (the generated
/// `<table>_<column>_not_null` when the catalog has no row for it, e.g. a
/// seed relation).
pub(crate) fn not_null_name(interp: &PgCatalog, relid: PgClassOid, attnum: i16) -> String {
    if let Some(c) = not_null_constraint(interp, relid, attnum) {
        return c.conname.clone();
    }
    let colname = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .map(|a| a.attname.clone())
        .unwrap_or_default();
    crate::ddl::util::make_object_name(&relname_of(interp, relid), &colname, "not_null")
}

/// PG's `ChooseConstraintName`: `name1[_name2]_label`, numbered until no
/// constraint of a relation in the same schema uses it.
pub(crate) fn choose_constraint_name(
    interp: &PgCatalog,
    relid: PgClassOid,
    name2: &str,
    label: &str,
) -> String {
    let relname = relname_of(interp, relid);
    let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
    let taken = |name: &str| {
        interp.pg_constraint.values().any(|c| {
            c.conname == name && interp.pg_class.get(&c.conrelid).map(|r| r.relnamespace) == nsoid
        })
    };
    let mut pass = 0;
    loop {
        let modlabel = if pass == 0 {
            label.to_owned()
        } else {
            format!("{label}{pass}")
        };
        let name = crate::ddl::util::make_object_name(&relname, name2, &modlabel);
        if !taken(&name) {
            return name;
        }
        pass += 1;
    }
}

/// Record the not-null constraint of a freshly-created column
/// (AddRelationNotNullConstraints): local when the column declares one
/// (named explicitly or `<table>_<column>_not_null`), otherwise inherited
/// under the first parent's constraint name. It is valid — the table is
/// empty — and NO INHERIT only as declared, which an inherited not-null
/// constraint can't be.
pub(super) fn record_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    col: &ParsedColumn,
) -> Result<(), DdlError> {
    let local = col.nn_local || col.nn_inhcount == 0;
    if local && col.nn_no_inherit && col.nn_inhcount > 0 {
        return Err(DdlError::Parse(format!(
            "cannot define not-null constraint with NO INHERIT on column \"{}\" (The column \
             has an inherited not-null constraint.)",
            col.name
        )));
    }
    let name = match (&col.nn_name, local) {
        (Some(name), _) => name.clone(),
        (None, true) => choose_constraint_name(interp, relid, &col.name, "not_null"),
        (None, false) => col
            .nn_inh_name
            .clone()
            .unwrap_or_else(|| choose_constraint_name(interp, relid, &col.name, "not_null")),
    };
    insert_not_null(
        interp,
        relid,
        attnum,
        NotNullRow {
            name,
            conislocal: local,
            coninhcount: col.nn_inhcount,
            convalidated: true,
            connoinherit: local && col.nn_no_inherit,
        },
    )
}

/// The catalog fields of a new not-null constraint row.
struct NotNullRow {
    name: String,
    conislocal: bool,
    coninhcount: i16,
    convalidated: bool,
    connoinherit: bool,
}

/// StoreRelNotNull, plus the `attnotnull` flag it implies — set for a NOT
/// VALID constraint too.
fn insert_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    row: NotNullRow,
) -> Result<(), DdlError> {
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: row.name,
        conrelid: relid,
        contype: ConType::NotNull,
        conkey: vec![attnum],
        confrelid: None,
        confkey: Vec::new(),
        conislocal: row.conislocal,
        coninhcount: row.coninhcount,
        conenforced: true,
        convalidated: row.convalidated,
        connoinherit: row.connoinherit,
        conperiod: false,
    });
    set_attnotnull(interp, relid, attnum, true);
    Ok(())
}

fn set_attnotnull(interp: &mut PgCatalog, relid: PgClassOid, attnum: i16, value: bool) {
    let mut changed = false;
    if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(a) = attrs.iter_mut().find(|a| a.attnum == attnum)
    {
        changed = a.attnotnull != value;
        a.attnotnull = value;
    }
    if changed {
        views::refresh_dependent_view_nullability(interp, relid, !value);
    }
}

fn attnum_or_err(interp: &PgCatalog, relid: PgClassOid, col: &str) -> Result<i16, DdlError> {
    interp
        .attribute_by_name(relid, col)
        .map(|a| a.attnum)
        .ok_or_else(|| DdlError::Parse(column_not_found_msg(interp, relid, col)))
}

fn is_system_column(name: &str) -> bool {
    crate::pg_catalog::SYSTEM_COLUMNS
        .iter()
        .any(|(n, ..)| *n == name)
}

fn column_name(interp: &PgCatalog, relid: PgClassOid, attnum: i16) -> String {
    interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .map(|a| a.attname.clone())
        .unwrap_or_default()
}

/// `ALTER TABLE ... ALTER COLUMN c SET NOT NULL` (`ATExecSetNotNull`). A
/// column that already has a not-null constraint keeps it — becoming local
/// when set directly, gaining an inheritance count when reached from a
/// parent, getting validated when NOT VALID — and the command stops there.
/// Otherwise a constraint is created and the children get an inherited copy
/// under the same name; under ONLY with children, a regular parent's
/// constraint is NO INHERIT and a partitioned table's is refused.
pub(crate) fn set_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col: &str,
    rec: Recursion,
) -> Result<(), DdlError> {
    set_not_null_named(interp, relid, col, None, rec)
}

fn set_not_null_named(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col: &str,
    conname: Option<&str>,
    rec: Recursion,
) -> Result<(), DdlError> {
    let attnum = match interp.attribute_by_name(relid, col) {
        Some(a) => a.attnum,
        None if is_system_column(col) => {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot alter system column \"{col}\""
            )));
        }
        None => return Err(DdlError::Parse(column_not_found_msg(interp, relid, col))),
    };
    let relname = relname_of(interp, relid);
    if let Some(existing) = not_null_constraint(interp, relid, attnum).cloned() {
        if existing.connoinherit && rec.recurse {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \
                 \"{relname}\"",
                existing.conname
            )));
        }
        if rec.recursing {
            if let Some(c) = interp.pg_constraint.get_mut(&existing.oid) {
                c.coninhcount += 1;
            }
        } else if !existing.conislocal {
            if let Some(c) = interp.pg_constraint.get_mut(&existing.oid) {
                c.conislocal = true;
            }
        } else if !existing.convalidated {
            return validate_not_null(interp, &existing, rec);
        }
        return Ok(());
    }
    let children = children_of(interp, relid);
    let mut no_inherit = false;
    if !rec.recurse && !children.is_empty() {
        if interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned) {
            return Err(DdlError::Parse(
                "constraint must be added to child tables too (Do not specify the ONLY \
                 keyword.)"
                    .into(),
            ));
        }
        no_inherit = true;
    }
    let name = match conname {
        Some(n) => n.to_owned(),
        None => choose_constraint_name(interp, relid, col, "not_null"),
    };
    insert_not_null(
        interp,
        relid,
        attnum,
        NotNullRow {
            name: name.clone(),
            conislocal: !rec.recursing,
            coninhcount: i16::from(rec.recursing),
            convalidated: true,
            connoinherit: no_inherit,
        },
    )?;
    if rec.recurse {
        for child in children {
            if interp.attribute_by_name(child, col).is_some() {
                set_not_null_named(interp, child, col, Some(&name), rec.child())?;
            }
        }
    }
    Ok(())
}

/// A `NOT NULL` constraint to add: `ADD [CONSTRAINT name] NOT NULL col [NOT
/// VALID] [NO INHERIT]`, a column's `NOT NULL` in ADD COLUMN, or a primary
/// key column's implied one.
#[derive(Clone, Copy, Default)]
pub(crate) struct NotNullSpec<'a> {
    pub(crate) name: Option<&'a str>,
    pub(crate) no_inherit: bool,
    pub(crate) not_valid: bool,
}

/// `ATAddCheckNNConstraint` → `AddRelationNewConstraints` for a not-null
/// constraint. A column that already has one keeps it
/// (`AdjustNotNullInheritance`: the NO INHERIT flags must agree, a NOT VALID
/// one can't stand for a valid one, and a locally given name must be its
/// name) and nothing recurses. Otherwise the constraint is created — NOT
/// VALID ones too set `attnotnull` — and, unless NO INHERIT, reaches every
/// child (ONLY is refused while there are any).
pub(crate) fn add_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col: &str,
    spec: NotNullSpec<'_>,
    rec: Recursion,
) -> Result<(), DdlError> {
    let relname = relname_of(interp, relid);
    let attnum = match interp.attribute_by_name(relid, col) {
        Some(a) => a.attnum,
        None if is_system_column(col) => {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot add not-null constraint on system column \"{col}\""
            )));
        }
        None => return Err(DdlError::Parse(column_not_found_msg(interp, relid, col))),
    };
    let is_local = !rec.recursing;
    if let Some(existing) = not_null_constraint(interp, relid, attnum).cloned() {
        if spec.no_inherit != existing.connoinherit {
            return Err(DdlError::Parse(format!(
                "cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \
                 \"{relname}\" (You might need to make the existing constraint inheritable \
                 using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT.)",
                existing.conname
            )));
        }
        if !spec.not_valid && !existing.convalidated {
            return Err(DdlError::Parse(format!(
                "incompatible NOT VALID constraint \"{}\" on relation \"{relname}\" (You might \
                 need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT.)",
                existing.conname
            )));
        }
        if is_local
            && let Some(name) = spec.name
            && name != existing.conname
        {
            return Err(DdlError::Parse(format!(
                "cannot create not-null constraint \"{name}\" on column \"{col}\" of table \
                 \"{relname}\" (A not-null constraint named \"{}\" already exists for this \
                 column.)",
                existing.conname
            )));
        }
        if let Some(c) = interp.pg_constraint.get_mut(&existing.oid) {
            if is_local {
                c.conislocal = true;
            } else {
                c.coninhcount += 1;
            }
        }
        return Ok(());
    }
    let name = match spec.name {
        Some(name) => {
            if interp
                .pg_constraint
                .values()
                .any(|c| c.conrelid == relid && c.conname == name)
            {
                return Err(DdlError::DuplicateObject(format!(
                    "constraint \"{name}\" for relation \"{relname}\" already exists"
                )));
            }
            name.to_owned()
        }
        None => choose_constraint_name(interp, relid, col, "not_null"),
    };
    insert_not_null(
        interp,
        relid,
        attnum,
        NotNullRow {
            name: name.clone(),
            conislocal: is_local,
            coninhcount: i16::from(!is_local),
            convalidated: !spec.not_valid,
            connoinherit: spec.no_inherit,
        },
    )?;
    if spec.no_inherit {
        return Ok(());
    }
    let children = children_of(interp, relid);
    if !rec.recurse && !children.is_empty() {
        return Err(DdlError::Parse(
            "constraint must be added to child tables too".into(),
        ));
    }
    for child in children {
        let child_spec = NotNullSpec {
            name: Some(&name),
            ..spec
        };
        add_not_null(interp, child, col, child_spec, rec.child())?;
    }
    Ok(())
}

/// ATPrepAddPrimaryKey: a primary key column needs a not-null constraint
/// that is inheritable and valid (verifyNotNullPKCompatible); a missing one
/// is added — to the children too — and under ONLY every child must
/// already have one.
pub(crate) fn require_pk_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col: &str,
    recurse: bool,
) -> Result<(), DdlError> {
    let incompatible = |interp: &PgCatalog, con: &PgConstraint| -> Result<(), DdlError> {
        let (marked, hint) = if con.connoinherit {
            (
                "NO INHERIT",
                "You might need to make the existing constraint inheritable using ALTER TABLE \
                 ... ALTER CONSTRAINT ... INHERIT.",
            )
        } else if !con.convalidated {
            (
                "NOT VALID",
                "You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT.",
            )
        } else {
            return Ok(());
        };
        Err(DdlError::Parse(format!(
            "cannot create primary key on column \"{col}\" (The constraint \"{}\" on column \
             \"{col}\" of table \"{}\", marked {marked}, is incompatible with a primary key. \
             {hint})",
            con.conname,
            relname_of(interp, con.conrelid)
        )))
    };
    let Some(attnum) = interp.attribute_by_name(relid, col).map(|a| a.attnum) else {
        // AddRelationNewConstraints on the queued not-null constraint.
        return Err(DdlError::Parse(column_not_found_msg(interp, relid, col)));
    };
    if let Some(con) = not_null_constraint(interp, relid, attnum) {
        return incompatible(interp, con);
    }
    if !recurse {
        for child in children_of(interp, relid) {
            let child_con = interp
                .attribute_by_name(child, col)
                .and_then(|a| not_null_constraint(interp, child, a.attnum));
            match child_con {
                None => {
                    return Err(DdlError::Parse(format!(
                        "column \"{col}\" of table \"{}\" is not marked NOT NULL",
                        relname_of(interp, child)
                    )));
                }
                Some(con) => incompatible(interp, con)?,
            }
        }
    }
    add_not_null(
        interp,
        relid,
        col,
        NotNullSpec::default(),
        Recursion {
            recurse: true,
            recursing: false,
        },
    )
}

/// QueueNNConstraintValidation: validating a not-null constraint validates
/// the children's copies first (all of them — ONLY is refused while there
/// are any), unless it is NO INHERIT.
pub(crate) fn validate_not_null(
    interp: &mut PgCatalog,
    con: &PgConstraint,
    rec: Recursion,
) -> Result<(), DdlError> {
    let relid = con.conrelid;
    let attnum = con.conkey.first().copied().unwrap_or(0);
    let colname = column_name(interp, relid, attnum);
    if !rec.recursing && !con.connoinherit {
        for child in all_inheritors(interp, relid) {
            if child == relid {
                continue;
            }
            if !rec.recurse {
                return Err(DdlError::Parse(
                    "constraint must be validated on child tables too".into(),
                ));
            }
            let Some(child_con) = interp
                .attribute_by_name(child, &colname)
                .and_then(|a| not_null_constraint(interp, child, a.attnum))
                .cloned()
            else {
                continue;
            };
            if !child_con.convalidated {
                let only_here = Recursion {
                    recurse: false,
                    recursing: true,
                };
                validate_not_null(interp, &child_con, only_here)?;
            }
        }
    }
    if let Some(c) = interp.pg_constraint.get_mut(&con.oid) {
        c.convalidated = true;
    }
    Ok(())
}

/// `relid` and all its descendants (find_all_inheritors).
pub(crate) fn all_inheritors(interp: &PgCatalog, relid: PgClassOid) -> Vec<PgClassOid> {
    let mut tree = vec![relid];
    let mut i = 0;
    while i < tree.len() {
        for child in children_of(interp, tree[i]) {
            if !tree.contains(&child) {
                tree.push(child);
            }
        }
        i += 1;
    }
    tree
}

/// `ALTER TABLE ... ALTER CONSTRAINT name [NO] INHERIT` on a not-null
/// constraint (ATExecAlterConstrInheritability): the children one level
/// down lose their inherited copy's link (keeping it as their own) or get
/// one, as by SET NOT NULL.
pub(crate) fn alter_not_null_inheritability(
    interp: &mut PgCatalog,
    con: &PgConstraint,
    no_inherit: bool,
) -> Result<(), DdlError> {
    if con.connoinherit == no_inherit {
        return Ok(());
    }
    if let Some(c) = interp.pg_constraint.get_mut(&con.oid) {
        c.connoinherit = no_inherit;
    }
    let relid = con.conrelid;
    let colname = column_name(interp, relid, con.conkey.first().copied().unwrap_or(0));
    for child in children_of(interp, relid) {
        if no_inherit {
            let child_con = interp
                .attribute_by_name(child, &colname)
                .and_then(|a| not_null_constraint(interp, child, a.attnum))
                .map(|c| c.oid);
            if let Some(c) = child_con.and_then(|oid| interp.pg_constraint.get_mut(&oid)) {
                c.coninhcount = (c.coninhcount - 1).max(0);
                c.conislocal = true;
            }
        } else if interp.attribute_by_name(child, &colname).is_some() {
            set_not_null_named(
                interp,
                child,
                &colname,
                Some(&con.conname),
                Recursion {
                    recurse: true,
                    recursing: true,
                },
            )?;
        }
    }
    Ok(())
}

/// `ALTER TABLE ... ALTER COLUMN c DROP NOT NULL` (`ATExecDropNotNull` →
/// `dropconstraint_internal`).
pub(crate) fn drop_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col: &str,
    rec: Recursion,
) -> Result<(), DdlError> {
    let attnum = attnum_or_err(interp, relid, col)?;
    let rel = relname_of(interp, relid);
    let Some(attr) = interp.attribute_by_name(relid, col).cloned() else {
        return Ok(());
    };
    // Already nullable: nothing to do.
    if !attr.attnotnull {
        return Ok(());
    }
    if attr.attidentity.is_some() {
        return Err(DdlError::Parse(format!(
            "column \"{col}\" of relation \"{rel}\" is an identity column"
        )));
    }
    // A partition keeps a NOT NULL its parent has.
    if let Some(parent) = interp
        .pg_inherits
        .iter()
        .find(|i| i.inhrelid == relid)
        .map(|i| i.inhparent)
        .filter(|p| interp.pg_class.get(p).map(|c| c.relkind) == Some(RelKind::Partitioned))
        && interp
            .attribute_by_name(parent, col)
            .is_some_and(|a| a.attnotnull)
    {
        return Err(DdlError::Parse(format!(
            "column \"{col}\" is marked NOT NULL in parent table"
        )));
    }
    let Some(con) = not_null_constraint(interp, relid, attnum).cloned() else {
        // A column NOT NULL without a constraint row (seed relations) just
        // loses the flag.
        set_attnotnull(interp, relid, attnum, false);
        return Ok(());
    };
    drop_not_null_constraint(interp, &con, rec)
}

/// Drop a not-null constraint row the PG way (dropconstraint_internal): a
/// primary-key, replica-identity or identity column keeps its NOT NULL; an
/// inherited constraint can only go away through its parent; children lose
/// their inherited copy unless they also define one locally or inherit it
/// from another parent — or keep it as their own under ONLY. A NO INHERIT
/// constraint has no copies.
pub(crate) fn drop_not_null_constraint(
    interp: &mut PgCatalog,
    con: &PgConstraint,
    rec: Recursion,
) -> Result<(), DdlError> {
    let relid = con.conrelid;
    let attnum = con.conkey.first().copied().unwrap_or(0);
    let colname = column_name(interp, relid, attnum);
    if con.coninhcount > 0 && !rec.recursing {
        return Err(DdlError::Parse(format!(
            "cannot drop inherited constraint \"{}\" of relation \"{}\"",
            con.conname,
            relname_of(interp, relid)
        )));
    }
    let in_pk = interp.pg_constraint.values().any(|c| {
        c.conrelid == relid && c.contype == ConType::PrimaryKey && c.conkey.contains(&attnum)
    });
    if in_pk {
        return Err(DdlError::Parse(format!(
            "column \"{colname}\" is in a primary key"
        )));
    }
    if super::object_refs::replica_identity_columns(interp, relid).contains(&attnum) {
        return Err(DdlError::Parse(format!(
            "column \"{colname}\" is in index used as replica identity"
        )));
    }
    if interp
        .attributes_of(relid)
        .iter()
        .any(|a| a.attnum == attnum && a.attidentity.is_some())
    {
        return Err(DdlError::Parse(format!(
            "column \"{colname}\" of relation \"{}\" is an identity column",
            relname_of(interp, relid)
        )));
    }
    interp.pg_constraint.remove(&con.oid);
    set_attnotnull(interp, relid, attnum, false);
    if con.connoinherit {
        return Ok(());
    }
    for child in children_of(interp, relid) {
        let Some(child_attnum) = interp.attribute_by_name(child, &colname).map(|a| a.attnum) else {
            continue;
        };
        let Some(child_con) = not_null_constraint(interp, child, child_attnum).cloned() else {
            continue;
        };
        if rec.recurse {
            if child_con.coninhcount == 1 && !child_con.conislocal {
                drop_not_null_constraint(interp, &child_con, rec.child())?;
            } else if let Some(c) = interp.pg_constraint.get_mut(&child_con.oid) {
                c.coninhcount = (c.coninhcount - 1).max(0);
            }
        } else if let Some(c) = interp.pg_constraint.get_mut(&child_con.oid) {
            // ONLY: the children keep the constraint as their own.
            c.coninhcount = (c.coninhcount - 1).max(0);
            c.conislocal = true;
        }
    }
    Ok(())
}
