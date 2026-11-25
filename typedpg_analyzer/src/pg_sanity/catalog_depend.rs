//! The catalog oracle's `pg_depend` layer: the analyzer's dependencies as
//! PostgreSQL would record them.
//!
//! The analyzer records in its `pg_depend` the dependencies its DROPs
//! consult that no structure of its own implies, in PG's conventions —
//! but for two dependers it has no row of: a column default's (or
//! generation expression's) dependencies are recorded from the column
//! (there is no `pg_attrdef` row), a view's query's from the view (there
//! is no `_RETURN` rule). The rest of PG's rows it keeps as structure: a
//! relation's namespace (`relnamespace`), a column's type (`atttypid`), a
//! constraint's columns (`conkey`), an index's (`indkey`), an operator
//! family's members (`family_members`)… [`model_depend`] renames the two
//! dependers and adds, from that structure, the rows PG's DDL records
//! for it (heap_create_with_catalog, GenerateTypeDependencies,
//! ProcedureCreate, CreateConstraintEntry, index_create, StoreAttrDefault,
//! storeOperators / storeProcedures, …) — each one derived from a fact
//! the seed-row layer compares already, so a missing or extra row left
//! over is a dependency the analyzer records differently from PG.

use std::collections::{HashMap, HashSet};

use super::catalog_extras::{DepObject, Extras};
use crate::ddl::depend::{self as d, NamedObject, ObjectAddress, named_object_at};
use crate::oid::{PgClassOid, PgGenericOid};
use crate::pg_catalog::{
    ConType, DepType, PG_CLASS_RELID, PG_COLLATION_RELID, PG_OPERATOR_RELID, PG_PROC_RELID,
    PgCatalog, PgDepend, RelKind, TypType,
};

const FIRST_NORMAL: u32 = crate::catalog_export::FIRST_NORMAL_OBJECT_ID;
const DEFAULT_COLLATION: u32 = 100;
const PG_ATTRDEF: u32 = 2604;
const PG_REWRITE: u32 = 2618;
const PG_TRIGGER: u32 = 2620;
const PG_AMOP: u32 = 2602;
const PG_AMPROC: u32 = 2603;

/// The rows being built, deduplicated as PG's `pg_depend` is not — two
/// identical rows behave as one.
struct Rows<'a> {
    db: &'a PgCatalog,
    x: &'a mut Extras,
    rows: Vec<PgDepend>,
    seen: HashSet<(u32, u32, i16, u32, u32, i16, u8)>,
    /// The next synthetic OID of an object the analyzer has none for.
    synthetic: u32,
    synthesized: HashMap<(u32, String), u32>,
}

impl Rows<'_> {
    /// `recordDependencyOn`: a pinned referenced object, or the depender
    /// itself, records nothing.
    fn push(&mut self, depender: ObjectAddress, referenced: ObjectAddress, deptype: DepType) {
        if d::is_pinned(referenced)
            || (depender.classid == referenced.classid
                && depender.objid == referenced.objid
                && depender.objsubid == referenced.objsubid)
        {
            return;
        }
        let key = (
            depender.classid.get(),
            depender.objid.get(),
            depender.objsubid,
            referenced.classid.get(),
            referenced.objid.get(),
            referenced.objsubid,
            deptype as u8,
        );
        if self.seen.insert(key) {
            self.rows.push(PgDepend {
                classid: depender.classid,
                objid: depender.objid,
                objsubid: depender.objsubid,
                refclassid: referenced.classid,
                refobjid: referenced.objid,
                refobjsubid: referenced.objsubid,
                deptype,
            });
        }
    }

    /// recordDependencyOn with DEPENDENCY_PARTITION_PRI (`primary`) or
    /// _SEC: the row, under `Normal`, its kind in
    /// [`Extras::partition_deps`] (the public `DepType` has no such kinds).
    fn push_partition(
        &mut self,
        depender: ObjectAddress,
        referenced: ObjectAddress,
        primary: bool,
    ) {
        let key = (
            depender.classid.get(),
            depender.objid.get(),
            depender.objsubid,
            referenced.classid.get(),
            referenced.objid.get(),
            referenced.objsubid,
        );
        if self.x.partition_deps.insert(key, primary).is_none() {
            self.push(depender, referenced, DepType::Normal);
        }
    }

    /// The address of an object the analyzer keeps no row (nor OID) for —
    /// a column's `pg_attrdef` entry, a view's `_RETURN` rule, an
    /// operator family's member — under a synthetic OID.
    fn synthetic(&mut self, classid: u32, object: DepObject) -> ObjectAddress {
        let x = &mut *self.x;
        let next = &mut self.synthetic;
        let oid = *self
            .synthesized
            .entry((classid, format!("{object:?}")))
            .or_insert_with(|| {
                *next -= 1;
                x.dep_objects.insert((classid, *next), object);
                *next
            });
        address(classid, oid, 0)
    }

    fn attrdef(&mut self, relid: PgClassOid, attnum: i16) -> ObjectAddress {
        self.synthetic(
            PG_ATTRDEF,
            DepObject::Attrdef {
                relid: relid.get(),
                attnum,
            },
        )
    }

    fn view_rule(&mut self, relid: PgClassOid) -> ObjectAddress {
        self.synthetic(
            PG_REWRITE,
            DepObject::Rule {
                relid: relid.get(),
                name: "_RETURN".into(),
            },
        )
    }

    /// Record the identity of `(classid, objid)` when it is an object of a
    /// catalog the seed rows don't cover that the analyzer keeps by name.
    fn name(&mut self, classid: PgClassOid, objid: PgGenericOid) {
        let addr = ObjectAddress {
            classid,
            objid,
            objsubid: 0,
        };
        let object = match named_object_at(self.db, addr) {
            Some(NamedObject::Policy { relid, name }) => DepObject::Policy {
                relid: relid.get(),
                name,
            },
            Some(NamedObject::Rule { relid, name }) => DepObject::Rule {
                relid: relid.get(),
                name,
            },
            Some(NamedObject::Trigger { relid, name }) => DepObject::Trigger {
                relid: relid.get(),
                name,
            },
            Some(NamedObject::DomainConstraint { typid, name }) => DepObject::DomainConstraint {
                typid: typid.get(),
                name,
            },
            Some(NamedObject::PublicationRel { publication, relid }) => DepObject::PublicationRel {
                publication,
                relid: relid.get(),
            },
            Some(NamedObject::Statistics { name, namespace }) => DepObject::Statistics {
                nsp: namespace.get(),
                name,
            },
            Some(NamedObject::Transform { typ, lang }) => DepObject::Transform {
                typ: typ.get(),
                lang,
            },
            _ => return,
        };
        self.x
            .dep_objects
            .insert((classid.get(), objid.get()), object);
    }
}

fn address(classid: u32, oid: u32, objsubid: i16) -> ObjectAddress {
    ObjectAddress {
        classid: PgClassOid::from_raw(classid),
        objid: PgGenericOid::from_raw(oid),
        objsubid,
    }
}

fn user(oid: u32) -> bool {
    oid >= FIRST_NORMAL
}

/// The analyzer's `pg_depend` rows as PostgreSQL would hold them, with
/// the identities of the objects they name in catalogs the seed rows don't
/// cover added to `x.dep_objects` (see the module's documentation).
pub(crate) fn model_depend(
    db: &PgCatalog,
    x: &mut Extras,
    role_name: &dyn Fn(crate::ddl::roles::RoleId) -> String,
) -> Vec<PgDepend> {
    let mut rows = Rows {
        db,
        x,
        rows: Vec::new(),
        seen: HashSet::new(),
        synthetic: u32::MAX,
        synthesized: HashMap::new(),
    };
    recorded(&mut rows);
    relations(&mut rows);
    types(&mut rows);
    routines(&mut rows);
    constraints(&mut rows);
    indexes(&mut rows);
    family_members(&mut rows);
    others(&mut rows);
    catalog_objects(&mut rows, role_name);
    rows.rows
}

/// The rows the analyzer recorded, the two dependers it keeps its own way
/// renamed.
fn recorded(r: &mut Rows<'_>) {
    let db = r.db;
    // The rows an operator family member stands for: derived from
    // `family_members` instead (see [`family_members`]).
    let hard_members: HashSet<(u32, u32, u32)> = db
        .family_members
        .iter()
        .filter_map(|m| {
            let owner = m.hard_owner?;
            let classid = if m.is_func {
                PG_PROC_RELID
            } else {
                PG_OPERATOR_RELID
            };
            Some((owner.objid.get(), classid.get(), m.object.get()))
        })
        .collect();
    for row in db.pg_depend.iter() {
        // A row between built-in objects (the seed's) is not compared.
        if !user(row.objid.get()) && !user(row.refobjid.get()) {
            continue;
        }
        // A view's `_RETURN` rule (a CREATE RULE one's dependency on its
        // view), by its `pg_rewrite` row.
        let view_rule = (row.classid == d::PG_REWRITE_RELID
            && named_object_at(
                db,
                ObjectAddress {
                    classid: row.classid,
                    objid: row.objid,
                    objsubid: 0,
                },
            )
            .is_none())
        .then(|| db.pg_rewrite_row(row.objid.get()))
        .flatten()
        .filter(|w| w.rulename == "_RETURN" && user(w.ev_class.get()));
        if let Some(w) = view_rule {
            r.x.dep_objects.insert(
                (row.classid.get(), row.objid.get()),
                DepObject::Rule {
                    relid: w.ev_class.get(),
                    name: w.rulename.clone(),
                },
            );
        }
        // The analyzer forgets the rows of a dropped object lazily, when
        // a DROP next consults them (`dependents_of`): such a row is dead.
        if view_rule.is_none()
            && !d::object_exists(
                db,
                ObjectAddress {
                    classid: row.classid,
                    objid: row.objid,
                    objsubid: row.objsubid,
                },
            )
        {
            continue;
        }
        r.name(row.classid, row.objid);
        r.name(row.refclassid, row.refobjid);
        if row.deptype == DepType::Normal
            && [d::PG_OPCLASS_RELID, d::PG_OPFAMILY_RELID].contains(&row.classid)
            && hard_members.contains(&(row.objid.get(), row.refclassid.get(), row.refobjid.get()))
        {
            continue;
        }
        let mut depender = ObjectAddress {
            classid: row.classid,
            objid: row.objid,
            objsubid: row.objsubid,
        };
        let referenced = ObjectAddress {
            classid: row.refclassid,
            objid: row.refobjid,
            objsubid: row.refobjsubid,
        };
        if row.classid == PG_CLASS_RELID && row.deptype == DepType::Normal {
            let relid = PgClassOid::from_raw(row.objid.get());
            let kind = db.pg_class.get(&relid).map(|c| c.relkind);
            if row.objsubid != 0 {
                // A column's: its default's (StoreAttrDefault).
                depender = r.attrdef(relid, row.objsubid);
            } else if matches!(kind, Some(RelKind::View | RelKind::MaterializedView)) {
                // A view's: its `_RETURN` rule's (DefineViewRules).
                depender = r.view_rule(relid);
            }
        }
        r.push(depender, referenced, row.deptype);
    }
}

/// heap_create_with_catalog, AddNewAttributeTuples, StoreAttrDefault,
/// StoreCatalogInheritance1, StorePartitionKey, DefineViewRules.
fn relations(r: &mut Rows<'_>) {
    let db = r.db;
    let toast = |ns: crate::oid::PgNamespaceOid| {
        db.namespace_name(ns)
            .is_some_and(|n| n == "pg_toast" || n.starts_with("pg_toast_temp"))
    };
    let mut classes: Vec<_> = db
        .pg_class
        .values()
        .filter(|c| user(c.oid.get()) && !toast(c.relnamespace))
        .collect();
    classes.sort_by_key(|c| c.oid);
    for c in classes {
        let me = ObjectAddress::relation(c.oid);
        if !matches!(
            c.relkind,
            RelKind::CompositeType | RelKind::Index | RelKind::PartitionedIndex
        ) {
            r.push(
                me,
                ObjectAddress::namespace(c.relnamespace),
                DepType::Normal,
            );
            if let Some(t) = db.typed_tables.get(&c.oid) {
                r.push(me, ObjectAddress::type_(*t), DepType::Normal);
            }
        }
        if let Some(t) = c.reltype {
            if c.relkind == RelKind::CompositeType {
                r.push(me, ObjectAddress::type_(t), DepType::Internal);
            } else {
                r.push(ObjectAddress::type_(t), me, DepType::Internal);
            }
        }
        if matches!(c.relkind, RelKind::View | RelKind::MaterializedView) {
            let rule = r.view_rule(c.oid);
            r.push(rule, me, DepType::Internal);
        }
        for a in db.attributes_of(c.oid).iter().filter(|a| a.attnum > 0) {
            let col = ObjectAddress::column(c.oid, a.attnum);
            if matches!(c.relkind, RelKind::Index | RelKind::PartitionedIndex) {
                continue;
            }
            r.push(col, ObjectAddress::type_(a.atttypid), DepType::Normal);
            if let Some(coll) = a.attcollation
                && coll.get() != DEFAULT_COLLATION
            {
                r.push(col, ObjectAddress::collation(coll), DepType::Normal);
            }
            if a.attgenerated.is_some() || (a.atthasdef && a.attidentity.is_none()) {
                let def = r.attrdef(c.oid, a.attnum);
                let deptype = if a.attgenerated.is_some() {
                    DepType::Internal
                } else {
                    DepType::Auto
                };
                r.push(def, col, deptype);
                // The columns a generation expression reads (its own
                // relation's, DEPENDENCY_NORMAL as any other reference).
                if let Some(cols) = db.generated_refs.get(&(c.oid, a.attnum)) {
                    for &n in cols {
                        r.push(def, ObjectAddress::column(c.oid, n), DepType::Normal);
                    }
                }
            }
        }
        // StorePartitionKey: the table depends on each key's operator
        // class and collation; the key columns, and the columns the key
        // expressions read, depend internally on it.
        for opc in crate::ddl::tables::partbound::partition_key_opclasses(db, c.oid) {
            r.push(
                me,
                address(d::PG_OPCLASS_RELID.get(), opc.get(), 0),
                DepType::Normal,
            );
        }
        for coll in crate::ddl::tables::partbound::partition_key_collations(db, c.oid)
            .into_iter()
            .flatten()
        {
            if coll.get() != DEFAULT_COLLATION {
                r.push(me, ObjectAddress::collation(coll), DepType::Normal);
            }
        }
        if let Some(keys) = db.partition_keys.get(&c.oid) {
            let mut cols: Vec<i16> = keys.iter().copied().filter(|&k| k != 0).collect();
            cols.extend(
                db.partition_key_attrs
                    .get(&c.oid)
                    .into_iter()
                    .flatten()
                    .copied(),
            );
            for n in cols {
                r.push(ObjectAddress::column(c.oid, n), me, DepType::Internal);
            }
        }
    }
    // StoreCatalogInheritance1: a child depends on its parent (a
    // partition automatically).
    for i in db.pg_inherits.iter() {
        let kind = db.pg_class.get(&i.inhrelid).map(|c| c.relkind);
        if matches!(
            kind,
            Some(RelKind::Index | RelKind::PartitionedIndex) | None
        ) {
            continue;
        }
        let deptype = if db.partition_bounds.contains_key(&i.inhrelid) {
            DepType::Auto
        } else {
            DepType::Normal
        };
        r.push(
            ObjectAddress::relation(i.inhrelid),
            ObjectAddress::relation(i.inhparent),
            deptype,
        );
    }
}

/// GenerateTypeDependencies, RangeCreate.
fn types(r: &mut Rows<'_>) {
    let db = r.db;
    let mut types: Vec<_> = db.pg_type.values().filter(|t| user(t.oid.get())).collect();
    types.sort_by_key(|t| t.oid);
    for t in types {
        let me = ObjectAddress::type_(t.oid);
        let implicit_array = t
            .typelem
            .and_then(|e| db.pg_type.get(&e))
            .is_some_and(|e| e.typarray == Some(t.oid));
        let rowtype = t
            .typrelid
            .and_then(|rel| db.pg_class.get(&rel))
            .is_some_and(|c| c.relkind != RelKind::CompositeType);
        let multirange = t.typtype == TypType::Multirange;
        if !(implicit_array || rowtype) || multirange {
            r.push(
                me,
                ObjectAddress::namespace(t.typnamespace),
                DepType::Normal,
            );
        }
        if let Some(base) = t.typbasetype {
            r.push(me, ObjectAddress::type_(base), DepType::Normal);
        }
        if let Some(coll) = t.typcollation
            && coll.get() != DEFAULT_COLLATION
        {
            r.push(me, ObjectAddress::collation(coll), DepType::Normal);
        }
        if let Some(sub) = t.typsubscript {
            r.push(me, ObjectAddress::proc(sub), DepType::Normal);
        }
        if let Some(elem) = t.typelem {
            let deptype = if implicit_array {
                DepType::Internal
            } else {
                DepType::Normal
            };
            r.push(me, ObjectAddress::type_(elem), deptype);
        }
    }
    let mut ranges: Vec<_> = db
        .pg_range
        .values()
        .filter(|g| user(g.rngtypid.get()))
        .collect();
    ranges.sort_by_key(|g| g.rngtypid);
    for g in ranges {
        let me = ObjectAddress::type_(g.rngtypid);
        r.push(me, ObjectAddress::type_(g.rngsubtype), DepType::Normal);
        if let Some(opc) = g.rngsubopc {
            r.push(
                me,
                address(d::PG_OPCLASS_RELID.get(), opc.get(), 0),
                DepType::Normal,
            );
        }
        if let Some(coll) = g.rngcollation {
            r.push(me, ObjectAddress::collation(coll), DepType::Normal);
        }
        if let Some(mr) = g.rngmultitypid {
            r.push(ObjectAddress::type_(mr), me, DepType::Internal);
        }
    }
}

/// ProcedureCreate, makeOperatorDependencies, CreateCast's structure.
fn routines(r: &mut Rows<'_>) {
    let db = r.db;
    let mut procs: Vec<_> = db.pg_proc.values().filter(|p| user(p.oid.get())).collect();
    procs.sort_by_key(|p| p.oid);
    for p in procs {
        let me = ObjectAddress::proc(p.oid);
        r.push(
            me,
            ObjectAddress::namespace(p.pronamespace),
            DepType::Normal,
        );
        r.push(me, ObjectAddress::language(p.prolang), DepType::Normal);
        r.push(me, ObjectAddress::type_(p.prorettype), DepType::Normal);
        let params = if p.proallargtypes.is_empty() {
            &p.proargtypes
        } else {
            &p.proallargtypes
        };
        for &t in params {
            r.push(me, ObjectAddress::type_(t), DepType::Normal);
        }
        if let Some(s) = p.prosupport {
            r.push(me, ObjectAddress::proc(s), DepType::Normal);
        }
    }
    let mut ops: Vec<_> = db
        .pg_operator
        .values()
        .filter(|o| user(o.oid.get()))
        .collect();
    ops.sort_by_key(|o| o.oid);
    for o in ops {
        let me = ObjectAddress::operator(o.oid);
        r.push(
            me,
            ObjectAddress::namespace(o.oprnamespace),
            DepType::Normal,
        );
        for t in [o.oprleft, Some(o.oprright), o.oprresult]
            .into_iter()
            .flatten()
        {
            r.push(me, ObjectAddress::type_(t), DepType::Normal);
        }
        if let Some(code) = o.oprcode {
            r.push(me, ObjectAddress::proc(code), DepType::Normal);
        }
    }
}

/// CreateConstraintEntry, index_constraint_create.
fn constraints(r: &mut Rows<'_>) {
    let db = r.db;
    let mut cons: Vec<_> = db
        .pg_constraint
        .values()
        .filter(|c| user(c.oid.get()))
        .collect();
    cons.sort_by_key(|c| c.oid);
    let index_of = |c: &crate::pg_catalog::PgConstraint| {
        let ns = db.pg_class.get(&c.conrelid)?.relnamespace;
        let index = *db.class_by_qname.get(&(ns, c.conname.clone()))?;
        db.pg_index
            .get(&index)
            .filter(|i| i.indrelid == c.conrelid)
            .map(|_| index)
    };
    for c in cons {
        let me = address(d::PG_CONSTRAINT_RELID.get(), c.oid.get(), 0);
        let index = matches!(
            c.contype,
            ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
        )
        .then(|| index_of(c))
        .flatten();
        // The key columns (an index constraint's INCLUDE columns too); an
        // expression's 0 is the whole relation, recorded only without a
        // column of it (eliminate_duplicate_dependencies).
        let mut keys: Vec<i16> = match index.and_then(|i| db.pg_index.get(&i)) {
            Some(i) => i.indkey.clone(),
            None => c.conkey.clone(),
        };
        if keys.iter().any(|&k| k != 0) {
            keys.retain(|&k| k != 0);
        } else {
            keys.clear();
        }
        if keys.is_empty() {
            r.push(me, ObjectAddress::relation(c.conrelid), DepType::Auto);
        }
        for k in keys {
            r.push(me, ObjectAddress::column(c.conrelid, k), DepType::Auto);
            // A CHECK's `conkey` is the columns its expression reads, which
            // it depends on normally too (CreateConstraintEntry's
            // recordDependencyOnSingleRelExpr, self-dependencies NORMAL).
            if c.contype == ConType::Check {
                r.push(me, ObjectAddress::column(c.conrelid, k), DepType::Normal);
            }
        }
        if let Some(index) = index {
            r.push(ObjectAddress::relation(index), me, DepType::Internal);
            // A partition's: the parent index's constraint.
            if let Some(parent) = db.index_parents.get(&index)
                && let Some(pc) = db.pg_constraint.values().find(|pc| {
                    pc.conrelid == db.pg_index.get(parent).map_or(pc.conrelid, |i| i.indrelid)
                        && index_of(pc) == Some(*parent)
                })
            {
                r.push_partition(
                    me,
                    address(d::PG_CONSTRAINT_RELID.get(), pc.oid.get(), 0),
                    true,
                );
                r.push_partition(me, ObjectAddress::relation(c.conrelid), false);
            }
        }
        if c.contype == ConType::ForeignKey {
            if let Some(frel) = c.confrelid {
                for &k in &c.confkey {
                    r.push(me, ObjectAddress::column(frel, k), DepType::Normal);
                }
            }
            if let Some(details) = db.fk_details.get(&c.oid) {
                if let Some(index) = details.index {
                    r.push(me, ObjectAddress::relation(index), DepType::Normal);
                }
                if let Some(parent) = details.parent {
                    let parent_addr = address(d::PG_CONSTRAINT_RELID.get(), parent.get(), 0);
                    // addFkConstraint: a row derived for a referenced
                    // partition (on the same referencing table) can only go
                    // with its parent; a clone on a referencing partition
                    // goes with the parent or the partition.
                    if db
                        .pg_constraint
                        .get(&parent)
                        .is_some_and(|p| p.conrelid == c.conrelid)
                    {
                        r.push(me, parent_addr, DepType::Internal);
                    } else {
                        r.push_partition(me, parent_addr, true);
                        r.push_partition(me, ObjectAddress::relation(c.conrelid), false);
                    }
                }
            }
        }
    }
    // A domain's constraints depend on it automatically.
    for (typid, cs) in db.domain_constraints.iter() {
        for c in cs {
            r.push(
                address(d::PG_CONSTRAINT_RELID.get(), c.oid.get(), 0),
                ObjectAddress::type_(*typid),
                DepType::Auto,
            );
            r.name(d::PG_CONSTRAINT_RELID, c.oid);
        }
    }
}

/// index_create.
fn indexes(r: &mut Rows<'_>) {
    let db = r.db;
    let constraint_indexes: HashSet<PgClassOid> = db
        .pg_constraint
        .values()
        .filter(|c| {
            matches!(
                c.contype,
                ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
            )
        })
        .filter_map(|c| {
            let ns = db.pg_class.get(&c.conrelid)?.relnamespace;
            db.class_by_qname.get(&(ns, c.conname.clone())).copied()
        })
        .collect();
    let mut indexes: Vec<_> = db
        .pg_index
        .values()
        .filter(|i| user(i.indexrelid.get()) && user(i.indrelid.get()))
        .collect();
    indexes.sort_by_key(|i| i.indexrelid);
    for i in indexes {
        let me = ObjectAddress::relation(i.indexrelid);
        if !constraint_indexes.contains(&i.indexrelid) {
            let simple: Vec<i16> = i.indkey.iter().copied().filter(|&k| k != 0).collect();
            if simple.is_empty() {
                r.push(me, ObjectAddress::relation(i.indrelid), DepType::Auto);
            }
            for k in simple {
                r.push(me, ObjectAddress::column(i.indrelid, k), DepType::Auto);
            }
        }
        if let Some(parent) = db.index_parents.get(&i.indexrelid) {
            r.push_partition(me, ObjectAddress::relation(*parent), true);
            r.push_partition(me, ObjectAddress::relation(i.indrelid), false);
        }
        let nkeys = i.indnkeyatts.max(0) as usize;
        for coll in i.indcollation.iter().take(nkeys).flatten() {
            if coll.get() != DEFAULT_COLLATION {
                r.push(me, ObjectAddress::collation(*coll), DepType::Normal);
            }
        }
    }
}

/// storeOperators / storeProcedures: each member of an operator family a
/// migration created.
fn family_members(r: &mut Rows<'_>) {
    let db = r.db;
    for m in db.family_members.iter() {
        let classid = if m.is_func { PG_AMPROC } else { PG_AMOP };
        let family = m.family.oid.get();
        let me = r.synthetic(
            classid,
            DepObject::FamilyMember {
                func: m.is_func,
                family,
                number: m.number,
                left: m.left.get(),
                right: m.right.get(),
            },
        );
        let hard = m.hard_owner.is_some();
        let (object_dep, owner_dep) = if hard {
            (DepType::Normal, DepType::Internal)
        } else {
            (DepType::Auto, DepType::Auto)
        };
        let object = ObjectAddress {
            classid: if m.is_func {
                PG_PROC_RELID
            } else {
                PG_OPERATOR_RELID
            },
            objid: m.object,
            objsubid: 0,
        };
        r.push(me, object, object_dep);
        let owner = m
            .hard_owner
            .unwrap_or_else(|| address(d::PG_OPFAMILY_RELID.get(), family, 0));
        r.push(me, owner, owner_dep);
        // typeDepNeeded: a type the member's operator or function doesn't
        // take.
        let inputs: Vec<u32> = if m.is_func {
            crate::oid::PgProcOid::new(m.object.get())
                .and_then(|p| db.pg_proc.get(&p))
                .map(|p| p.proargtypes.iter().map(|t| t.get()).collect())
                .unwrap_or_default()
        } else {
            crate::oid::PgOperatorOid::new(m.object.get())
                .and_then(|o| db.pg_operator.get(&o))
                .map(|o| {
                    o.oprleft
                        .into_iter()
                        .chain([o.oprright])
                        .map(|t| t.get())
                        .collect()
                })
                .unwrap_or_default()
        };
        for (n, t) in [m.left, m.right].into_iter().enumerate() {
            if n == 1 && m.right == m.left {
                continue;
            }
            if !inputs.contains(&t.get()) {
                r.push(me, ObjectAddress::type_(t), object_dep);
            }
        }
    }
}

/// Namespaces of the other kinds of objects, triggers, policies, rules,
/// statistics, extensions.
fn others(r: &mut Rows<'_>) {
    let db = r.db;
    // (A built-in object moved to a schema of the migrations' depends on
    // it: changeDependencyFor.)
    for c in db
        .pg_collation
        .values()
        .filter(|c| user(c.oid.get()) || user(c.collnamespace.get()))
    {
        r.push(
            address(PG_COLLATION_RELID.get(), c.oid.get(), 0),
            ObjectAddress::namespace(c.collnamespace),
            DepType::Normal,
        );
    }
    for c in db
        .conversions
        .iter()
        .filter(|c| user(c.oid.get()) || user(c.namespace.get()))
    {
        r.push(
            address(2607, c.oid.get(), 0),
            ObjectAddress::namespace(c.namespace),
            DepType::Normal,
        );
    }
    for o in db
        .pg_ts_objects
        .iter()
        .filter(|o| user(o.oid.get()) || user(o.namespace.get()))
    {
        let classid = match o.kind.as_str() {
            "c" => d::PG_TS_CONFIG_RELID,
            "d" => d::PG_TS_DICT_RELID,
            "p" => d::PG_TS_PARSER_RELID,
            _ => d::PG_TS_TEMPLATE_RELID,
        };
        r.push(
            address(classid.get(), o.oid.get(), 0),
            ObjectAddress::namespace(o.namespace),
            DepType::Normal,
        );
    }
    for f in db.pg_opfamily.iter().filter(|f| user(f.oid.get())) {
        r.push(
            address(d::PG_OPFAMILY_RELID.get(), f.oid.get(), 0),
            ObjectAddress::namespace(f.opfnamespace),
            DepType::Normal,
        );
    }
    for c in db.pg_opclass.iter().filter(|c| user(c.oid.get())) {
        r.push(
            address(d::PG_OPCLASS_RELID.get(), c.oid.get(), 0),
            ObjectAddress::namespace(c.opcnamespace),
            DepType::Normal,
        );
    }
    for e in db.pg_extension.values().filter(|e| user(e.oid.get())) {
        r.push(
            address(crate::pg_catalog::PG_EXTENSION_RELID.get(), e.oid.get(), 0),
            ObjectAddress::namespace(e.extnamespace),
            DepType::Normal,
        );
    }
    for s in db.statistics.iter() {
        let me = address(d::PG_STATISTIC_EXT_RELID.get(), s.oid.get(), 0);
        r.name(d::PG_STATISTIC_EXT_RELID, s.oid);
        r.push(me, ObjectAddress::namespace(s.namespace), DepType::Normal);
        // A whole-relation dependency without plain columns (whatever
        // the expressions read).
        if s.keys.is_empty() {
            r.push(me, ObjectAddress::relation(s.relid), DepType::Auto);
        }
        for &n in &s.attnums {
            r.push(me, ObjectAddress::column(s.relid, n), DepType::Auto);
        }
    }
    for (relid, triggers) in db.triggers.iter() {
        for t in triggers {
            let me = address(PG_TRIGGER, t.oid.get(), 0);
            r.name(PgClassOid::from_raw(PG_TRIGGER), t.oid);
            r.push(me, ObjectAddress::proc(t.function), DepType::Normal);
            r.push(me, ObjectAddress::relation(*relid), DepType::Auto);
            if let Some(c) = t.constrrel {
                r.push(me, ObjectAddress::relation(c), DepType::Auto);
            }
            if let Some(c) = t.constraint {
                r.push(
                    address(d::PG_CONSTRAINT_RELID.get(), c.get(), 0),
                    me,
                    DepType::Internal,
                );
            }
            if let Some(parent) = t.parent
                && let Some(pt) = db
                    .triggers
                    .get(&parent)
                    .and_then(|ts| ts.iter().find(|p| p.name == t.name))
            {
                r.push_partition(me, address(PG_TRIGGER, pt.oid.get(), 0), true);
                r.push_partition(me, ObjectAddress::relation(*relid), false);
            }
            for col in &t.columns {
                if let Some(a) = db.attribute_by_name(*relid, col) {
                    r.push(me, ObjectAddress::column(*relid, a.attnum), DepType::Normal);
                }
            }
        }
    }
    for (relid, policies) in db.policies.iter() {
        for p in policies {
            let me = address(d::PG_POLICY_RELID.get(), p.oid.get(), 0);
            r.name(d::PG_POLICY_RELID, p.oid);
            r.push(me, ObjectAddress::relation(*relid), DepType::Auto);
        }
    }
    for (relid, rules) in db.rules.iter() {
        for rule in rules {
            let me = address(PG_REWRITE, rule.oid.get(), 0);
            r.name(d::PG_REWRITE_RELID, rule.oid);
            r.push(me, ObjectAddress::relation(*relid), DepType::Auto);
        }
    }
}

/// The objects the analyzer keeps by name, without an OID or a row of
/// `pg_depend`: publications' members, foreign servers and user mappings,
/// foreign tables' servers, event triggers, access methods, subscriptions,
/// required extensions, schemas' default privileges.
fn catalog_objects(r: &mut Rows<'_>, role_name: &dyn Fn(crate::ddl::roles::RoleId) -> String) {
    let db = r.db;
    // publication_add_relation / publication_add_schema: a member goes
    // with its publication or its table (schema).
    for p in db.publications.iter() {
        let publication = r.synthetic(6104, DepObject::Publication(p.name.clone()));
        let (tables, schemas) = p.members();
        for (relid, _except) in tables {
            let Some(oid) = p.relation_oid(relid) else {
                continue;
            };
            let me = address(d::PG_PUBLICATION_REL_RELID.get(), oid.get(), 0);
            r.name(d::PG_PUBLICATION_REL_RELID, oid);
            r.push(me, publication, DepType::Auto);
            r.push(me, ObjectAddress::relation(relid), DepType::Auto);
        }
        for &nsp in schemas {
            let me = r.synthetic(
                6237,
                DepObject::PublicationNamespace {
                    publication: p.name.clone(),
                    nsp: nsp.get(),
                },
            );
            r.push(me, publication, DepType::Auto);
            r.push(me, ObjectAddress::namespace(nsp), DepType::Auto);
        }
    }
    // CreateForeignServer, CreateUserMapping, CreateForeignTable.
    let (servers, mappings) = (
        db.foreign_data.server_wrappers(),
        db.foreign_data.user_mapping_pairs(),
    );
    let server_addr =
        |r: &mut Rows<'_>, name: &str| r.synthetic(1417, DepObject::Server(name.to_owned()));
    for (server, wrapper) in servers {
        let me = server_addr(r, server);
        if let Some(fdw) = db.foreign_data.wrapper_oid(wrapper) {
            r.push(
                me,
                address(d::PG_FOREIGN_DATA_WRAPPER_RELID.get(), fdw.get(), 0),
                DepType::Normal,
            );
        }
    }
    for (user, server) in mappings {
        let me = r.synthetic(
            1418,
            DepObject::UserMapping {
                user: if user == PgCatalog::MIGRATIONS_USER {
                    role_name(db.roles.migrations_user)
                } else {
                    user.clone()
                },
                server: server.clone(),
            },
        );
        let srv = server_addr(r, server);
        r.push(me, srv, DepType::Normal);
    }
    let mut tables: Vec<_> = db.foreign_data.table_servers.iter().collect();
    tables.sort();
    for (relid, server) in tables {
        let srv = server_addr(r, server);
        r.push(ObjectAddress::relation(*relid), srv, DepType::Normal);
    }
    for (name, server) in db.cluster_objects.subscription_servers() {
        if let Some(server) = server {
            let me = r.synthetic(6100, DepObject::Subscription(name.to_owned()));
            let srv = server_addr(r, server);
            r.push(me, srv, DepType::Normal);
        }
    }
    // CreateEventTrigger.
    for (name, function) in db.event_triggers.iter() {
        let me = r.synthetic(3466, DepObject::EventTrigger(name.clone()));
        r.push(me, ObjectAddress::proc(*function), DepType::Normal);
    }
    // CreateAccessMethod.
    for am in db.pg_am.iter() {
        if let (Some(oid), Some(handler)) = (db.access_method_oids.get(&am.amname), am.amhandler) {
            r.push(
                address(d::PG_AM_RELID.get(), oid.get(), 0),
                ObjectAddress::proc(handler),
                DepType::Normal,
            );
        }
    }
    // CreateExtension: an extension depends on the ones it requires.
    for e in db.pg_extension.values().filter(|e| user(e.oid.get())) {
        for required in crate::ddl::extensions::requires(&e.extname) {
            if let Some(req) = db.extension_by_name.get(*required) {
                r.push(
                    address(crate::pg_catalog::PG_EXTENSION_RELID.get(), e.oid.get(), 0),
                    address(crate::pg_catalog::PG_EXTENSION_RELID.get(), req.get(), 0),
                    DepType::Normal,
                );
            }
        }
    }
    // SetDefaultACL: a schema's default privileges go with it.
    for (role, nsp, objtype) in db.acls.schema_defaults() {
        let me = r.synthetic(
            826,
            DepObject::DefaultAcl {
                role: role_name(role),
                nsp: nsp.get(),
                objtype,
            },
        );
        r.push(me, ObjectAddress::namespace(nsp), DepType::Auto);
    }
}
