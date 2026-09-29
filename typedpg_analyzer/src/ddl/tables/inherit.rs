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

/// Record the not-null constraint of a freshly-created column: local when
/// the column declares one (named explicitly or `<table>_<column>_not_null`),
/// otherwise inherited under the first parent's constraint name.
pub(super) fn record_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    col: &ParsedColumn,
) -> Result<(), DdlError> {
    let local = col.nn_local || col.nn_inhcount == 0;
    let name = match (&col.nn_name, local) {
        (Some(name), _) => name.clone(),
        (None, true) => choose_constraint_name(interp, relid, &col.name, "not_null"),
        (None, false) => col
            .nn_inh_name
            .clone()
            .unwrap_or_else(|| choose_constraint_name(interp, relid, &col.name, "not_null")),
    };
    insert_not_null(interp, relid, attnum, name, local, col.nn_inhcount)
}

fn insert_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    name: String,
    conislocal: bool,
    coninhcount: i16,
) -> Result<(), DdlError> {
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: name,
        conrelid: relid,
        contype: ConType::NotNull,
        conkey: vec![attnum],
        confrelid: None,
        confkey: Vec::new(),
        conislocal,
        coninhcount,
        conenforced: true,
        convalidated: true,
        connoinherit: false,
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

/// `ALTER TABLE ... ALTER COLUMN c SET NOT NULL` and `ADD [CONSTRAINT name]
/// NOT NULL c` (`ATExecSetNotNull` / `AdjustNotNullInheritance`): a column
/// that already has a not-null constraint keeps it (becoming local when set
/// directly, gaining an inheritance count when reached from a parent);
/// otherwise one is created. Children get an inherited copy under the same
/// name.
pub(crate) fn set_not_null(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    col: &str,
    explicit_name: Option<&str>,
    rec: Recursion,
) -> Result<(), DdlError> {
    let attnum = attnum_or_err(interp, relid, col)?;
    set_not_null_at(interp, relid, attnum, explicit_name, rec)
}

fn set_not_null_at(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    name: Option<&str>,
    rec: Recursion,
) -> Result<(), DdlError> {
    let existing = not_null_constraint(interp, relid, attnum).map(|c| c.oid);
    let conname = match existing {
        Some(oid) => {
            if let Some(c) = interp.pg_constraint.get_mut(&oid) {
                if rec.recursing {
                    c.coninhcount += 1;
                } else {
                    c.conislocal = true;
                }
            }
            set_attnotnull(interp, relid, attnum, true);
            interp
                .pg_constraint
                .get(&oid)
                .map(|c| c.conname.clone())
                .unwrap_or_default()
        }
        None => {
            let colname = interp
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum == attnum)
                .map(|a| a.attname.clone())
                .unwrap_or_default();
            let conname = match name {
                Some(n) => n.to_owned(),
                None => choose_constraint_name(interp, relid, &colname, "not_null"),
            };
            insert_not_null(
                interp,
                relid,
                attnum,
                conname.clone(),
                !rec.recursing,
                i16::from(rec.recursing),
            )?;
            conname
        }
    };
    if rec.recurse {
        let colname = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
            .unwrap_or_default();
        for child in children_of(interp, relid) {
            if let Some(child_attnum) = interp.attribute_by_name(child, &colname).map(|a| a.attnum)
            {
                set_not_null_at(interp, child, child_attnum, Some(&conname), rec.child())?;
            }
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
    if interp
        .attribute_by_name(relid, col)
        .is_some_and(|a| a.attidentity.is_some())
    {
        return Err(DdlError::Parse(format!(
            "column \"{col}\" of relation \"{rel}\" is an identity column"
        )));
    }
    let Some(con) = not_null_constraint(interp, relid, attnum).cloned() else {
        // Already nullable; PG reports nothing. A column NOT NULL without a
        // constraint row (seed relations) just loses the flag.
        set_attnotnull(interp, relid, attnum, false);
        return Ok(());
    };
    drop_not_null_constraint(interp, &con, rec)
}

/// Drop a not-null constraint row the PG way: a primary-key column keeps
/// its NOT NULL; an inherited constraint can only go away through its
/// parent; children lose their inherited copy unless they also define one
/// locally or inherit it from another parent.
pub(crate) fn drop_not_null_constraint(
    interp: &mut PgCatalog,
    con: &PgConstraint,
    rec: Recursion,
) -> Result<(), DdlError> {
    let relid = con.conrelid;
    let attnum = con.conkey.first().copied().unwrap_or(0);
    let colname = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .map(|a| a.attname.clone())
        .unwrap_or_default();
    if rec.recursing {
        let Some(c) = interp.pg_constraint.get_mut(&con.oid) else {
            return Ok(());
        };
        c.coninhcount = (c.coninhcount - 1).max(0);
        if c.coninhcount > 0 || c.conislocal {
            return Ok(());
        }
    } else {
        let in_pk = interp.pg_constraint.values().any(|c| {
            c.conrelid == relid && c.contype == ConType::PrimaryKey && c.conkey.contains(&attnum)
        });
        if in_pk {
            return Err(DdlError::Parse(format!(
                "column \"{colname}\" is in a primary key"
            )));
        }
        if con.coninhcount > 0 {
            return Err(DdlError::Parse(format!(
                "cannot drop inherited constraint \"{}\" of relation \"{}\"",
                con.conname,
                relname_of(interp, relid)
            )));
        }
    }
    interp.pg_constraint.remove(&con.oid);
    set_attnotnull(interp, relid, attnum, false);
    for child in children_of(interp, relid) {
        let Some(child_attnum) = interp.attribute_by_name(child, &colname).map(|a| a.attnum) else {
            continue;
        };
        let Some(child_con) = not_null_constraint(interp, child, child_attnum).cloned() else {
            continue;
        };
        if rec.recurse {
            drop_not_null_constraint(interp, &child_con, rec.child())?;
        } else if let Some(c) = interp.pg_constraint.get_mut(&child_con.oid) {
            // ONLY: the children keep the constraint as their own.
            c.coninhcount = (c.coninhcount - 1).max(0);
            c.conislocal = true;
        }
    }
    Ok(())
}
