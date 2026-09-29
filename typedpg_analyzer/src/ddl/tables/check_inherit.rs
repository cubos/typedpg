//! Inheritance of CHECK constraints. A CHECK constraint that isn't `NO
//! INHERIT` propagates to every inheritance child and partition, where it
//! is merged with a same-named constraint of the same expression
//! (`MergeCheckConstraint`, `MergeWithExistingConstraint` in heap.c) and
//! counted in `coninhcount`; dropping and renaming it recurse the same way
//! (`dropconstraint_internal`, `rename_constraint_internal`).

use super::*;

/// The parts of a CHECK constraint's `pg_constraint` row the analyzer
/// needs beyond [`PgConstraint`]: the expression and `connoinherit`.
#[derive(Clone, Debug)]
pub(crate) struct CheckDef {
    pub(crate) expr: StoredExpr,
    pub(crate) no_inherit: bool,
}

/// A CHECK constraint's `conenforced` / `convalidated` as it is added.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CheckFlags {
    pub(crate) enforced: bool,
    pub(crate) valid: bool,
}

impl CheckFlags {
    /// The flags of a constraint written in CREATE TABLE: valid (the table
    /// is empty) unless NOT ENFORCED.
    pub(crate) fn created(enforced: bool) -> Self {
        CheckFlags {
            enforced,
            valid: enforced,
        }
    }
}

/// An expression kept from the DDL that wrote it: a CHECK constraint, a
/// column default or generation expression, a statistics expression. Two
/// compare equal when PG's `equal()` says their parse trees are (source
/// locations aside), so different spellings of one tree match.
#[derive(Clone, Debug)]
pub(crate) enum StoredExpr {
    Written(Box<typedpg_pg_query::protobuf::Node>),
    /// A serial column's `nextval()` of its own sequence.
    Serial(PgClassOid, i16),
}

impl StoredExpr {
    pub(crate) fn written(expr: &typedpg_pg_query::protobuf::Node) -> Self {
        StoredExpr::Written(Box::new(expr.clone()))
    }
}

impl PartialEq for StoredExpr {
    fn eq(&self, other: &Self) -> bool {
        use typedpg_pg_query::Equal;
        match (self, other) {
            (StoredExpr::Written(a), StoredExpr::Written(b)) => a.equal(b),
            (StoredExpr::Serial(r1, a1), StoredExpr::Serial(r2, a2)) => r1 == r2 && a1 == a2,
            _ => false,
        }
    }
}

fn same_expr(a: Option<&CheckDef>, b: Option<&CheckDef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.expr == b.expr,
        // A seeded constraint carries no expression: nothing to compare.
        _ => true,
    }
}

fn constraint_named(interp: &PgCatalog, relid: PgClassOid, name: &str) -> Option<PgConstraint> {
    interp
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && c.conname == name)
        .cloned()
}

/// `relispartition`: the relation is a partition of a partitioned table.
fn is_partition(interp: &PgCatalog, relid: PgClassOid) -> bool {
    interp.pg_inherits.iter().any(|i| {
        i.inhrelid == relid
            && interp
                .pg_class
                .get(&i.inhparent)
                .is_some_and(|c| c.relkind == RelKind::Partitioned)
    })
}

/// Map a CHECK's `conkey` from `from` to `to` by column name.
fn map_conkey(interp: &PgCatalog, from: PgClassOid, to: PgClassOid, conkey: &[i16]) -> Vec<i16> {
    conkey
        .iter()
        .filter_map(|&an| {
            let name = &interp
                .attributes_of(from)
                .iter()
                .find(|a| a.attnum == an)?
                .attname;
            interp.attribute_by_name(to, name).map(|a| a.attnum)
        })
        .collect()
}

/// StoreRelCheck (heap.c): a partitioned table holds no rows of its own,
/// so a NO INHERIT CHECK constraint on it makes no sense.
pub(crate) fn check_no_inherit_allowed(
    interp: &PgCatalog,
    relid: PgClassOid,
    no_inherit: bool,
) -> Result<(), DdlError> {
    if no_inherit && interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned) {
        return Err(DdlError::Parse(format!(
            "cannot add NO INHERIT constraint to partitioned table \"{}\"",
            relname_of(interp, relid)
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // one per pg_constraint column
fn insert_check(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    name: &str,
    conkey: Vec<i16>,
    def: Option<CheckDef>,
    conislocal: bool,
    coninhcount: i16,
    flags: CheckFlags,
) -> Result<(), DdlError> {
    check_no_inherit_allowed(interp, relid, def.as_ref().is_some_and(|d| d.no_inherit))?;
    let oid = PgConstraintOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_constraint(PgConstraint {
        oid,
        conname: name.to_owned(),
        conrelid: relid,
        contype: ConType::Check,
        conkey,
        confrelid: None,
        confkey: Vec::new(),
        conislocal,
        coninhcount,
        conenforced: flags.enforced,
        convalidated: flags.valid,
        connoinherit: def.as_ref().is_some_and(|d| d.no_inherit),
        conperiod: false,
    });
    if let Some(def) = def {
        interp.check_defs.insert(oid, def);
    }
    Ok(())
}

/// A CHECK constraint the parents give a new table: (name, conkey on the
/// table, definition, how many parents contribute it, enforced by any of
/// them).
type InheritedCheck = (String, Vec<i16>, Option<CheckDef>, i16, bool);

/// CREATE TABLE ... INHERITS / PARTITION OF: the parents' inheritable CHECK
/// constraints, merged by name (MergeCheckConstraint), then merged with the
/// table's own ones (MergeWithExistingConstraint).
pub(super) fn inherit_parent_checks(
    interp: &mut PgCatalog,
    relid: PgClassOid,
) -> Result<(), DdlError> {
    let mut parents: Vec<&PgInherits> = interp
        .pg_inherits
        .iter()
        .filter(|i| i.inhrelid == relid)
        .collect();
    parents.sort_by_key(|i| i.inhseqno);
    let parents: Vec<PgClassOid> = parents.iter().map(|i| i.inhparent).collect();

    let mut inherited: Vec<InheritedCheck> = Vec::new();
    for parent in parents {
        let mut checks: Vec<PgConstraint> = interp
            .pg_constraint
            .values()
            .filter(|c| c.conrelid == parent && c.contype == ConType::Check)
            .cloned()
            .collect();
        checks.sort_by_key(|c| c.oid);
        for con in checks {
            let def = interp.check_defs.get(&con.oid).cloned();
            if def.as_ref().is_some_and(|d| d.no_inherit) {
                continue;
            }
            match inherited.iter_mut().find(|e| e.0 == con.conname) {
                Some(e) => {
                    if !same_expr(e.2.as_ref(), def.as_ref()) {
                        return Err(DdlError::DuplicateObject(format!(
                            "check constraint name \"{}\" appears multiple times but with \
                             different expressions",
                            con.conname
                        )));
                    }
                    e.3 += 1;
                    // One ENFORCED parent makes the merged one ENFORCED.
                    e.4 |= con.conenforced;
                }
                None => {
                    let conkey = map_conkey(interp, parent, relid, &con.conkey);
                    inherited.push((con.conname.clone(), conkey, def, 1, con.conenforced));
                }
            }
        }
    }

    let relname = relname_of(interp, relid);
    let partition = is_partition(interp, relid);
    for (name, conkey, def, count, enforced) in inherited {
        let Some(local) = constraint_named(interp, relid, &name) else {
            insert_check(
                interp,
                relid,
                &name,
                conkey,
                def,
                false,
                count,
                CheckFlags::created(enforced),
            )?;
            continue;
        };
        let local_def = interp.check_defs.get(&local.oid);
        if local.contype != ConType::Check || !same_expr(local_def, def.as_ref()) {
            return Err(DdlError::DuplicateObject(format!(
                "constraint \"{name}\" for relation \"{relname}\" already exists"
            )));
        }
        if local_def.is_some_and(|d| d.no_inherit) {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{name}\" conflicts with inherited constraint on relation \
                 \"{relname}\""
            )));
        }
        // The local definition merging into an ENFORCED inherited one may
        // not be NOT ENFORCED.
        if !local.conenforced && enforced {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{name}\" conflicts with NOT ENFORCED constraint on relation \
                 \"{relname}\""
            )));
        }
        if let Some(row) = interp.pg_constraint.get_mut(&local.oid) {
            row.coninhcount = count;
            // A partition's constraints are never local.
            if partition {
                row.conislocal = false;
            }
        }
    }
    Ok(())
}

/// `ALTER TABLE ... ADD [CONSTRAINT name] CHECK (...)` on `relid`
/// (ATAddCheckNNConstraint), recursing to the children unless NO INHERIT.
#[allow(clippy::too_many_arguments)]
pub(super) fn add_check(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    name: &str,
    expr: &typedpg_pg_query::protobuf::Node,
    no_inherit: bool,
    rec: super::inherit::Recursion,
    conkey: Vec<i16>,
    flags: CheckFlags,
) -> Result<(), DdlError> {
    let def = CheckDef {
        expr: StoredExpr::written(expr),
        no_inherit,
    };
    let relname = relname_of(interp, relid);
    if let Some(existing) = constraint_named(interp, relid, name) {
        // A local constraint merges only into a purely inherited one.
        let existing_def = interp.check_defs.get(&existing.oid).cloned();
        let same =
            existing.contype == ConType::Check && same_expr(existing_def.as_ref(), Some(&def));
        if !same || existing.conislocal || is_partition(interp, relid) {
            return Err(DdlError::DuplicateObject(format!(
                "constraint \"{name}\" for relation \"{relname}\" already exists"
            )));
        }
        if existing.coninhcount > 0 && no_inherit {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{name}\" conflicts with inherited constraint on relation \
                 \"{relname}\""
            )));
        }
        check_merge_flags(&existing, flags, true, &relname)?;
        if let Some(row) = interp.pg_constraint.get_mut(&existing.oid) {
            row.conislocal = true;
            if flags.enforced && !row.conenforced {
                row.conenforced = true;
                row.convalidated = true;
            }
        }
        // Merged: the children already have it.
        return Ok(());
    }
    insert_check(
        interp,
        relid,
        name,
        conkey.clone(),
        Some(def.clone()),
        true,
        0,
        flags,
    )?;
    if no_inherit {
        return Ok(());
    }
    let children = super::inherit::children_of(interp, relid);
    if !rec.recurse && !children.is_empty() {
        return Err(DdlError::UnsupportedDdl(
            "constraint must be added to child tables too".into(),
        ));
    }
    for child in children {
        let child_key = map_conkey(interp, relid, child, &conkey);
        add_inherited_check(interp, child, name, &def, child_key, flags)?;
    }
    Ok(())
}

/// MergeWithExistingConstraint's validity and enforcement rules: a NOT
/// VALID existing constraint can't stand for a valid one, and an ENFORCED
/// inherited definition can't merge into a NOT ENFORCED one (nor a local
/// NOT ENFORCED one into an ENFORCED one).
fn check_merge_flags(
    existing: &PgConstraint,
    flags: CheckFlags,
    is_local: bool,
    relname: &str,
) -> Result<(), DdlError> {
    let name = &existing.conname;
    if flags.valid && existing.conenforced && !existing.convalidated {
        return Err(DdlError::UnsupportedDdl(format!(
            "constraint \"{name}\" conflicts with NOT VALID constraint on relation \"{relname}\""
        )));
    }
    if (!is_local && flags.enforced && !existing.conenforced)
        || (is_local && !flags.enforced && existing.conenforced)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "constraint \"{name}\" conflicts with NOT ENFORCED constraint on relation \
             \"{relname}\""
        )));
    }
    Ok(())
}

/// The recursive step of [`add_check`]: `is_local = false`, merging
/// allowed.
fn add_inherited_check(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    name: &str,
    def: &CheckDef,
    conkey: Vec<i16>,
    flags: CheckFlags,
) -> Result<(), DdlError> {
    let relname = relname_of(interp, relid);
    if let Some(existing) = constraint_named(interp, relid, name) {
        let existing_def = interp.check_defs.get(&existing.oid).cloned();
        if existing.contype != ConType::Check || !same_expr(existing_def.as_ref(), Some(def)) {
            return Err(DdlError::DuplicateObject(format!(
                "constraint \"{name}\" for relation \"{relname}\" already exists"
            )));
        }
        if existing_def.is_some_and(|d| d.no_inherit) {
            return Err(DdlError::UnsupportedDdl(format!(
                "constraint \"{name}\" conflicts with non-inherited constraint on relation \
                 \"{relname}\""
            )));
        }
        check_merge_flags(&existing, flags, false, &relname)?;
        if let Some(row) = interp.pg_constraint.get_mut(&existing.oid) {
            row.coninhcount += 1;
        }
        return Ok(());
    }
    insert_check(
        interp,
        relid,
        name,
        conkey.clone(),
        Some(def.clone()),
        false,
        1,
        flags,
    )?;
    for child in super::inherit::children_of(interp, relid) {
        let child_key = map_conkey(interp, relid, child, &conkey);
        add_inherited_check(interp, child, name, def, child_key, flags)?;
    }
    Ok(())
}

/// DROP CONSTRAINT of a CHECK constraint (dropconstraint_internal).
pub(super) fn drop_check(
    interp: &mut PgCatalog,
    con: &PgConstraint,
    rec: super::inherit::Recursion,
) -> Result<(), DdlError> {
    if con.coninhcount > 0 && !rec.recursing {
        return Err(DdlError::DependencyError(format!(
            "cannot drop inherited constraint \"{}\" of relation \"{}\"",
            con.conname,
            relname_of(interp, con.conrelid)
        )));
    }
    interp.pg_constraint.remove(&con.oid);
    let def = interp.check_defs.remove(&con.oid);
    if def.is_some_and(|d| d.no_inherit) {
        return Ok(());
    }
    for child in super::inherit::children_of(interp, con.conrelid) {
        let Some(child_con) = constraint_named(interp, child, &con.conname)
            .filter(|c| c.contype == ConType::Check && c.coninhcount > 0)
        else {
            continue;
        };
        if rec.recurse && child_con.coninhcount == 1 && !child_con.conislocal {
            drop_check(interp, &child_con, rec.child())?;
        } else if let Some(row) = interp.pg_constraint.get_mut(&child_con.oid) {
            row.coninhcount -= 1;
            if !rec.recurse {
                row.conislocal = true;
            }
        }
    }
    Ok(())
}

/// RENAME CONSTRAINT of a CHECK constraint (rename_constraint_internal):
/// an inheritable one is renamed in every descendant too, and an inherited
/// one can only be renamed from its parent.
pub(crate) fn rename_check(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    old: &str,
    new: &str,
    recurse: bool,
) -> Result<(), DdlError> {
    rename_check_at(interp, relid, old, new, recurse, 0)
}

fn rename_check_at(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    old: &str,
    new: &str,
    recurse: bool,
    expected_parents: i16,
) -> Result<(), DdlError> {
    let Some(con) = constraint_named(interp, relid, old) else {
        return Ok(());
    };
    let no_inherit = interp
        .check_defs
        .get(&con.oid)
        .is_some_and(|d| d.no_inherit);
    if !no_inherit {
        if recurse {
            // find_all_inheritors: every descendant, with how many of its
            // parents are in the tree.
            let mut tree = vec![relid];
            let mut i = 0;
            while i < tree.len() {
                for child in super::inherit::children_of(interp, tree[i]) {
                    if !tree.contains(&child) {
                        tree.push(child);
                    }
                }
                i += 1;
            }
            for &child in &tree[1..] {
                let numparents = interp
                    .pg_inherits
                    .iter()
                    .filter(|h| h.inhrelid == child && tree.contains(&h.inhparent))
                    .count() as i16;
                rename_check_at(interp, child, old, new, false, numparents)?;
            }
        } else if expected_parents == 0 && !super::inherit::children_of(interp, relid).is_empty() {
            return Err(DdlError::DependencyError(format!(
                "inherited constraint \"{old}\" must be renamed in child tables too"
            )));
        }
    }
    if con.coninhcount > expected_parents {
        return Err(DdlError::DependencyError(format!(
            "cannot rename inherited constraint \"{old}\""
        )));
    }
    crate::ddl::alter::check_constraint_name_free(interp, relid, new, &relname_of(interp, relid))?;
    if let Some(row) = interp.pg_constraint.get_mut(&con.oid) {
        row.conname = new.to_owned();
    }
    Ok(())
}
