//! Dependencies between catalog objects (`pg_depend`, catalog/dependency.c
//! and pg_depend.c): recording what an object depends on, and the checks a
//! DROP makes against what depends on its targets.

use typedpg_pg_query::protobuf::{ObjectType, node};

use crate::oid::{PgCastOid, PgClassOid, PgGenericOid, PgOperatorOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    DepType, PG_CAST_RELID, PG_CLASS_RELID, PG_OPERATOR_RELID, PG_PROC_RELID, PG_TYPE_RELID,
    PgCatalog, PgDepend, RelKind,
};

/// The object a DROP names, as `(kind, oid)`, when it exists — two names of
/// one DROP that resolve to the same object are one target
/// (`RemoveObjects` / `performMultipleDeletions` look every name up before
/// deleting anything).
pub(crate) fn drop_target_identity(
    interp: &PgCatalog,
    obj_type: ObjectType,
    obj_node: &typedpg_pg_query::protobuf::Node,
) -> Option<(i32, u32)> {
    let kind = obj_type as i32;
    match (obj_type, obj_node.node.as_ref()?) {
        (
            ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectSequence
            | ObjectType::ObjectForeignTable
            | ObjectType::ObjectIndex,
            node::Node::List(list),
        ) => {
            let (schema, name) = super::util::extract_names(&list.items, interp);
            let nsoid = interp.namespace_oid(&schema)?;
            let oid = interp.class_by_qname.get(&(nsoid, name))?;
            Some((kind, oid.get()))
        }
        (ObjectType::ObjectType | ObjectType::ObjectDomain, node::Node::TypeName(tn)) => {
            let oid = super::util::resolve_type_name(tn, interp)?;
            Some((kind, oid.get()))
        }
        (
            ObjectType::ObjectFunction
            | ObjectType::ObjectProcedure
            | ObjectType::ObjectRoutine
            | ObjectType::ObjectAggregate,
            node::Node::ObjectWithArgs(owa),
        ) => {
            let oid = super::functions::lookup_func_with_args(interp, obj_type, owa, true)
                .ok()
                .flatten()?;
            Some((kind, oid.get()))
        }
        (ObjectType::ObjectOperator, node::Node::ObjectWithArgs(_)) => {
            let oid = super::drop::operator_target(interp, obj_node)?;
            Some((kind, oid.get()))
        }
        _ => None,
    }
}

/// PG's message when a DROP naming several objects is blocked
/// (`reportDependentObjects` without an original object).
pub(crate) fn multiple_targets_message(err: super::DdlError) -> super::DdlError {
    match err {
        super::DdlError::DependencyError(msg)
            if msg.starts_with("cannot drop ")
                && msg.contains(" because other objects depend on it") =>
        {
            super::DdlError::DependencyError(
                "cannot drop desired object(s) because other objects depend on them".into(),
            )
        }
        other => other,
    }
}

/// An object `pg_depend` rows point at: `(classid, objid, objsubid)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ObjectAddress {
    pub classid: PgClassOid,
    pub objid: PgGenericOid,
    /// A column's `attnum`, 0 for the whole object.
    pub objsubid: i16,
}

impl ObjectAddress {
    fn new(classid: PgClassOid, oid: std::num::NonZeroU32, objsubid: i16) -> Self {
        Self {
            classid,
            objid: PgGenericOid::from_nonzero(oid),
            objsubid,
        }
    }
    pub(crate) fn proc(oid: PgProcOid) -> Self {
        Self::new(PG_PROC_RELID, oid.into_nonzero(), 0)
    }
    pub(crate) fn relation(oid: PgClassOid) -> Self {
        Self::new(PG_CLASS_RELID, oid.into_nonzero(), 0)
    }
    pub(crate) fn column(relid: PgClassOid, attnum: i16) -> Self {
        Self::new(PG_CLASS_RELID, relid.into_nonzero(), attnum)
    }
    pub(crate) fn type_(oid: PgTypeOid) -> Self {
        Self::new(PG_TYPE_RELID, oid.into_nonzero(), 0)
    }
    pub(crate) fn operator(oid: PgOperatorOid) -> Self {
        Self::new(PG_OPERATOR_RELID, oid.into_nonzero(), 0)
    }
    pub(crate) fn cast(oid: PgCastOid) -> Self {
        Self::new(PG_CAST_RELID, oid.into_nonzero(), 0)
    }

    /// The address of what [`drop_target_identity`] found for `obj_type`.
    pub(crate) fn of_drop_target(obj_type: ObjectType, oid: u32) -> Option<Self> {
        let nz = std::num::NonZeroU32::new(oid)?;
        let classid = match obj_type {
            ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectSequence
            | ObjectType::ObjectForeignTable
            | ObjectType::ObjectIndex => PG_CLASS_RELID,
            ObjectType::ObjectType | ObjectType::ObjectDomain => PG_TYPE_RELID,
            ObjectType::ObjectFunction
            | ObjectType::ObjectProcedure
            | ObjectType::ObjectRoutine
            | ObjectType::ObjectAggregate => PG_PROC_RELID,
            ObjectType::ObjectOperator => PG_OPERATOR_RELID,
            _ => return None,
        };
        Some(Self::new(classid, nz, 0))
    }
}

/// `FirstNormalObjectId`: objects below it come with the server (the
/// seed); PG never records dependencies on pinned objects, and none of
/// these can be dropped by a migration anyway.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// `recordDependencyOn` for each of `referenced` (deduplicated, like
/// `record_object_address_dependencies`), skipping built-in objects and
/// the depender itself.
pub(crate) fn record(
    interp: &mut PgCatalog,
    depender: ObjectAddress,
    referenced: impl IntoIterator<Item = ObjectAddress>,
    deptype: DepType,
) {
    let mut refs: Vec<ObjectAddress> = referenced
        .into_iter()
        .filter(|r| r.objid.get() >= FIRST_NORMAL_OBJECT_ID)
        .filter(|r| (r.classid, r.objid) != (depender.classid, depender.objid))
        .collect();
    refs.sort();
    refs.dedup();
    for r in refs {
        interp.add_dependency(PgDepend {
            classid: depender.classid,
            objid: depender.objid,
            objsubid: depender.objsubid,
            refclassid: r.classid,
            refobjid: r.objid,
            refobjsubid: r.objsubid,
            deptype,
        });
    }
}

/// `deleteDependencyRecordsFor(..., skipExtensionDeps = true)`: forget what
/// `depender` depends on — before a CREATE OR REPLACE records it anew —
/// keeping its extension membership.
pub(crate) fn forget_dependencies_of(interp: &mut PgCatalog, depender: ObjectAddress) {
    interp.pg_depend.retain(|d| {
        !(d.classid == depender.classid
            && d.objid == depender.objid
            && !matches!(d.deptype, DepType::Extension))
    });
}

// ─── Expression references ─────────────────────────────────────────────────

/// An object an analyzed expression refers to (what
/// `find_expr_references_walker` would record): a catalog object, or a
/// column of a relation by name (resolved to its attnum when recorded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reference {
    Object(ObjectAddress),
    Column(PgClassOid, String),
    /// An object the analyzer keeps by name (a text search configuration
    /// a `regconfig` literal names, ...).
    Named(NamedObject),
}

thread_local! {
    /// The references collected by the innermost active [`collect`].
    static COLLECTOR: std::cell::RefCell<Option<Vec<Reference>>> =
        const { std::cell::RefCell::new(None) };
}

fn push(reference: Reference) {
    COLLECTOR.with(|c| {
        if let Some(refs) = c.borrow_mut().as_mut() {
            refs.push(reference);
        }
    });
}

/// The analyzer resolved a reference to `addr` (a function, operator,
/// relation, type ...). A no-op outside [`collect`].
pub(crate) fn note(addr: ObjectAddress) {
    push(Reference::Object(addr));
}

/// The analyzer resolved a reference to column `name` of relation `relid`.
pub(crate) fn note_column(relid: PgClassOid, name: &str) {
    let active = COLLECTOR.with(|c| c.borrow().is_some());
    if active {
        push(Reference::Column(relid, name.to_owned()));
    }
}

/// A `regconfig` / `regdictionary` literal named text search object
/// `kind` (`c` / `d`) `name`, found along the search path.
pub(crate) fn note_ts_object(interp: &PgCatalog, kind: &str, schema: Option<&str>, name: &str) {
    let active = COLLECTOR.with(|c| c.borrow().is_some());
    if !active {
        return;
    }
    let found = interp.schemas_for_lookup(schema).into_iter().find(|ns| {
        interp
            .pg_ts_objects
            .iter()
            .any(|o| o.kind == kind && o.name == name && o.namespace == *ns)
    });
    if let Some(namespace) = found {
        push(Reference::Named(NamedObject::TsObject {
            kind: kind.to_owned(),
            name: name.to_owned(),
            namespace,
        }));
    }
}

/// Run `f` (an analysis) and return what it referred to.
pub(crate) fn collect<R>(f: impl FnOnce() -> R) -> (R, Vec<Reference>) {
    let outer = COLLECTOR.with(|c| c.replace(Some(Vec::new())));
    let result = f();
    let refs = COLLECTOR.with(|c| c.replace(outer)).unwrap_or_default();
    (result, refs)
}

/// `recordDependencyOnExpr`: `depender` depends on what an expression
/// referred to (`refs`, from [`collect`]).
pub(crate) fn record_references(
    interp: &mut PgCatalog,
    depender: ObjectAddress,
    refs: &[Reference],
    deptype: DepType,
) {
    let mut addrs: Vec<ObjectAddress> = Vec::new();
    for r in refs {
        match r {
            Reference::Object(a) => addrs.push(*a),
            Reference::Column(relid, name) => addrs.extend(
                interp
                    .attribute_by_name(*relid, name)
                    .map(|a| ObjectAddress::column(*relid, a.attnum)),
            ),
            Reference::Named(object) => addrs.extend(named_address(interp, object.clone())),
        }
    }
    record(interp, depender, addrs, deptype);
}

// ─── Objects without an OID in the analyzer ────────────────────────────────

/// `pg_constraint`.
pub(crate) const PG_CONSTRAINT_RELID: PgClassOid = PgClassOid::from_raw(2606);
/// `pg_policy`.
pub(crate) const PG_POLICY_RELID: PgClassOid = PgClassOid::from_raw(3256);
/// `pg_rewrite`.
pub(crate) const PG_REWRITE_RELID: PgClassOid = PgClassOid::from_raw(2618);
/// `pg_trigger`.
pub(crate) const PG_TRIGGER_RELID: PgClassOid = PgClassOid::from_raw(2620);
/// `pg_publication_rel`.
pub(crate) const PG_PUBLICATION_REL_RELID: PgClassOid = PgClassOid::from_raw(6106);
/// `pg_opclass`.
pub(crate) const PG_OPCLASS_RELID: PgClassOid = PgClassOid::from_raw(2616);
/// `pg_ts_config`.
pub(crate) const PG_TS_CONFIG_RELID: PgClassOid = PgClassOid::from_raw(3602);
/// `pg_ts_dict`.
pub(crate) const PG_TS_DICT_RELID: PgClassOid = PgClassOid::from_raw(3600);

/// A catalog object the analyzer keeps by name rather than by OID. It gets
/// an OID here so `pg_depend` rows can name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NamedObject {
    Policy {
        relid: PgClassOid,
        name: String,
    },
    Rule {
        relid: PgClassOid,
        name: String,
    },
    Trigger {
        relid: PgClassOid,
        name: String,
    },
    DomainConstraint {
        typid: PgTypeOid,
        name: String,
    },
    PublicationRel {
        publication: String,
        relid: PgClassOid,
    },
    /// An operator class.
    Opclass {
        name: String,
        namespace: crate::oid::PgNamespaceOid,
        method: String,
    },
    /// A text search configuration (`c`) or dictionary (`d`).
    TsObject {
        kind: String,
        name: String,
        namespace: crate::oid::PgNamespaceOid,
    },
}

impl NamedObject {
    fn classid(&self) -> PgClassOid {
        match self {
            NamedObject::Policy { .. } => PG_POLICY_RELID,
            NamedObject::Rule { .. } => PG_REWRITE_RELID,
            NamedObject::Trigger { .. } => PG_TRIGGER_RELID,
            NamedObject::DomainConstraint { .. } => PG_CONSTRAINT_RELID,
            NamedObject::PublicationRel { .. } => PG_PUBLICATION_REL_RELID,
            NamedObject::Opclass { .. } => PG_OPCLASS_RELID,
            NamedObject::TsObject { kind, .. } if kind == "c" => PG_TS_CONFIG_RELID,
            NamedObject::TsObject { .. } => PG_TS_DICT_RELID,
        }
    }

    /// Whether the object still exists (its own DROP / RENAME may have
    /// removed it without telling `pg_depend`).
    fn exists(&self, interp: &PgCatalog) -> bool {
        match self {
            NamedObject::Policy { relid, name } => interp
                .policies
                .get(relid)
                .is_some_and(|p| p.iter().any(|p| &p.name == name)),
            NamedObject::Rule { relid, name } => interp
                .rules
                .get(relid)
                .is_some_and(|r| r.iter().any(|r| &r.name == name)),
            NamedObject::Trigger { relid, name } => interp
                .triggers
                .get(relid)
                .is_some_and(|t| t.iter().any(|t| &t.name == name)),
            NamedObject::DomainConstraint { typid, name } => interp
                .domain_constraints
                .get(typid)
                .is_some_and(|c| c.iter().any(|c| &c.name == name)),
            NamedObject::PublicationRel { publication, relid } => interp
                .publications
                .iter()
                .any(|p| &p.name == publication && p.has_relation(*relid)),
            NamedObject::Opclass {
                name,
                namespace,
                method,
            } => interp.pg_opclass.iter().any(|c| {
                &c.opcname == name && c.opcnamespace == *namespace && &c.opcmethod == method
            }),
            NamedObject::TsObject {
                kind,
                name,
                namespace,
            } => interp
                .pg_ts_objects
                .iter()
                .any(|o| &o.kind == kind && &o.name == name && o.namespace == *namespace),
        }
    }
}

/// The address of `object`, registering it (with a fresh OID) the first
/// time. `None` for a built-in object (in `pg_catalog`), which nothing
/// records dependencies on.
fn named_address(interp: &mut PgCatalog, object: NamedObject) -> Option<ObjectAddress> {
    if let NamedObject::TsObject { namespace, .. } | NamedObject::Opclass { namespace, .. } =
        &object
        && interp.namespace_name(*namespace) == Some("pg_catalog")
    {
        return None;
    }
    let existing = interp
        .named_objects
        .iter()
        .find(|(_, o)| **o == object)
        .map(|(oid, _)| *oid);
    let classid = object.classid();
    let oid = match existing {
        Some(oid) => oid,
        None => {
            let oid = interp.alloc_oid().ok()?;
            interp.named_objects.insert(oid, object);
            oid
        }
    };
    Some(ObjectAddress::new(classid, oid, 0))
}

/// The address `object` was registered under, if it was.
pub(crate) fn named_address_of(interp: &PgCatalog, object: &NamedObject) -> Option<ObjectAddress> {
    interp
        .named_objects
        .iter()
        .find(|(_, o)| *o == object)
        .map(|(oid, o)| ObjectAddress::new(o.classid(), *oid, 0))
}

/// The object the analyzer keeps by name at `addr`.
pub(crate) fn named_object_at(interp: &PgCatalog, addr: ObjectAddress) -> Option<NamedObject> {
    interp
        .named_objects
        .get(&addr.objid.into_nonzero())
        .filter(|o| o.classid() == addr.classid)
        .cloned()
}

/// ALTER POLICY / TRIGGER / RULE ... RENAME TO: the object keeps its
/// address (and dependencies) under its new name.
pub(crate) fn rename_named(interp: &mut PgCatalog, old: &NamedObject, new_name: &str) {
    for object in interp.named_objects.values_mut() {
        if object != old {
            continue;
        }
        match object {
            NamedObject::Policy { name, .. }
            | NamedObject::Rule { name, .. }
            | NamedObject::Trigger { name, .. } => *name = new_name.to_owned(),
            _ => {}
        }
    }
}

/// Whether the object at `addr` still exists.
pub(crate) fn object_exists(interp: &PgCatalog, addr: ObjectAddress) -> bool {
    exists(interp, addr)
}

/// The address of `object` as the depender of new dependencies: a
/// re-created object of the same name starts over — what the old one
/// depended on is forgotten.
pub(crate) fn named_object(
    interp: &mut PgCatalog,
    object: NamedObject,
) -> Result<ObjectAddress, super::DdlError> {
    let addr = named_address(interp, object)
        .ok_or_else(|| super::DdlError::Internal("no address for a built-in object".into()))?;
    forget_dependencies_of(interp, addr);
    Ok(addr)
}

/// DROP TEXT SEARCH CONFIGURATION / DICTIONARY: what depends on it (a
/// configuration's mapping, an index's `regconfig`) needs CASCADE.
pub(crate) fn drop_ts_object(
    interp: &mut PgCatalog,
    kind: &str,
    name: &str,
    namespace: crate::oid::PgNamespaceOid,
    cascade: bool,
) -> Result<(), super::DdlError> {
    let object = NamedObject::TsObject {
        kind: kind.to_owned(),
        name: name.to_owned(),
        namespace,
    };
    let Some(oid) = interp
        .named_objects
        .iter()
        .find(|(_, o)| **o == object)
        .map(|(oid, _)| *oid)
    else {
        return Ok(());
    };
    let addr = ObjectAddress::new(object.classid(), oid, 0);
    let desc = describe(interp, addr);
    drop_dependents(interp, addr, &desc, cascade)?;
    interp.named_objects.remove(&oid);
    interp.remove_dependencies_of(addr.classid, addr.objid);
    Ok(())
}

// ─── DROP ──────────────────────────────────────────────────────────────────

/// Whether the object `addr` names still exists.
fn exists(interp: &PgCatalog, addr: ObjectAddress) -> bool {
    let oid = addr.objid.get();
    match addr.classid {
        c if c == PG_PROC_RELID => {
            PgProcOid::new(oid).is_some_and(|o| interp.pg_proc.contains_key(&o))
        }
        c if c == PG_OPERATOR_RELID => {
            PgOperatorOid::new(oid).is_some_and(|o| interp.pg_operator.contains_key(&o))
        }
        c if c == PG_CAST_RELID => {
            PgCastOid::new(oid).is_some_and(|o| interp.pg_cast.contains_key(&o))
        }
        c if c == PG_TYPE_RELID => {
            PgTypeOid::new(oid).is_some_and(|o| interp.pg_type.contains_key(&o))
        }
        c if c == PG_CLASS_RELID => PgClassOid::new(oid).is_some_and(|relid| {
            interp.pg_class.contains_key(&relid)
                && (addr.objsubid == 0
                    || interp
                        .attributes_of(relid)
                        .iter()
                        .any(|a| a.attnum == addr.objsubid))
        }),
        c if c == PG_CONSTRAINT_RELID => {
            crate::oid::PgConstraintOid::new(oid)
                .is_some_and(|o| interp.pg_constraint.contains_key(&o))
                || interp
                    .named_objects
                    .get(&addr.objid.into_nonzero())
                    .is_some_and(|o| o.exists(interp))
        }
        _ => interp
            .named_objects
            .get(&addr.objid.into_nonzero())
            .is_some_and(|o| o.exists(interp)),
    }
}

/// `getObjectDescription` (objectaddress.c), for the objects dependencies
/// connect.
pub(crate) fn describe(interp: &PgCatalog, addr: ObjectAddress) -> String {
    let oid = addr.objid.get();
    let type_name = |t: PgTypeOid| super::util::format_type_for_message(interp, t);
    let relation = |relid: PgClassOid| -> (String, String) {
        let Some(c) = interp.pg_class.get(&relid) else {
            return ("relation".into(), String::new());
        };
        let kind = match c.relkind {
            RelKind::View => "view",
            RelKind::MaterializedView => "materialized view",
            RelKind::Sequence => "sequence",
            RelKind::Index | RelKind::PartitionedIndex => "index",
            RelKind::CompositeType => "composite type",
            RelKind::ForeignTable => "foreign table",
            _ => "table",
        };
        (kind.into(), c.relname.clone())
    };
    match addr.classid {
        c if c == PG_PROC_RELID => {
            let Some(p) = PgProcOid::new(oid).and_then(|o| interp.pg_proc.get(&o)) else {
                return "function".into();
            };
            let args: Vec<String> = p.proargtypes.iter().map(|&t| type_name(t)).collect();
            let kind = if p.prokind == crate::pg_catalog::ProKind::Procedure {
                "procedure"
            } else {
                "function"
            };
            format!("{kind} {}({})", p.proname, args.join(","))
        }
        c if c == PG_OPERATOR_RELID => {
            let Some(o) = PgOperatorOid::new(oid).and_then(|o| interp.pg_operator.get(&o)) else {
                return "operator".into();
            };
            let left = o.oprleft.map_or_else(|| "NONE".to_owned(), type_name);
            format!("operator {}({left},{})", o.oprname, type_name(o.oprright))
        }
        c if c == PG_CAST_RELID => match PgCastOid::new(oid).and_then(|o| interp.pg_cast.get(&o)) {
            Some(cast) => format!(
                "cast from {} to {}",
                type_name(cast.castsource),
                type_name(cast.casttarget)
            ),
            None => "cast".into(),
        },
        c if c == PG_TYPE_RELID => match PgTypeOid::new(oid) {
            Some(t) => format!("type {}", type_name(t)),
            None => "type".into(),
        },
        c if c == PG_CLASS_RELID => {
            let Some(relid) = PgClassOid::new(oid) else {
                return "relation".into();
            };
            let (kind, name) = relation(relid);
            if addr.objsubid == 0 {
                return format!("{kind} {name}");
            }
            let column = interp
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum == addr.objsubid)
                .map(|a| a.attname.clone())
                .unwrap_or_default();
            format!("column {column} of {kind} {name}")
        }
        c if c == PG_CONSTRAINT_RELID => {
            if let Some(con) =
                crate::oid::PgConstraintOid::new(oid).and_then(|o| interp.pg_constraint.get(&o))
            {
                let (_, table) = relation(con.conrelid);
                return format!("constraint {} on table {table}", con.conname);
            }
            match interp.named_objects.get(&addr.objid.into_nonzero()) {
                Some(NamedObject::DomainConstraint { name, .. }) => format!("constraint {name}"),
                _ => "constraint".into(),
            }
        }
        _ => match interp.named_objects.get(&addr.objid.into_nonzero()) {
            Some(NamedObject::Policy { relid, name }) => {
                format!("policy {name} on table {}", relation(*relid).1)
            }
            Some(NamedObject::Rule { relid, name }) => {
                format!(
                    "rule {name} on {} {}",
                    relation(*relid).0,
                    relation(*relid).1
                )
            }
            Some(NamedObject::Trigger { relid, name }) => {
                format!(
                    "trigger {name} on {} {}",
                    relation(*relid).0,
                    relation(*relid).1
                )
            }
            Some(NamedObject::PublicationRel { publication, relid }) => format!(
                "publication of table {} in publication {publication}",
                relation(*relid).1
            ),
            Some(NamedObject::Opclass { name, method, .. }) => {
                format!("operator class {name} for access method {method}")
            }
            Some(NamedObject::TsObject { kind, name, .. }) => format!(
                "text search {} {name}",
                if kind == "c" {
                    "configuration"
                } else {
                    "dictionary"
                }
            ),
            _ => "object".into(),
        },
    }
}

/// A column default (`classid = pg_class`, `objsubid = attnum`) is described
/// as PG's pg_attrdef row is — unless the column is generated, in which case
/// the column itself is the dependent.
fn describe_dependent(interp: &PgCatalog, addr: ObjectAddress) -> String {
    if addr.classid == PG_CLASS_RELID
        && addr.objsubid > 0
        && let Some(relid) = PgClassOid::new(addr.objid.get())
        && let Some(attr) = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == addr.objsubid)
        && attr.attgenerated.is_none()
    {
        let table = interp
            .pg_class
            .get(&relid)
            .map(|c| c.relname.clone())
            .unwrap_or_default();
        return format!("default value for column {} of table {table}", attr.attname);
    }
    describe(interp, addr)
}

thread_local! {
    /// Every object the current DROP statement names: dependencies among
    /// them need no CASCADE (performMultipleDeletions).
    static DROP_TARGETS: std::cell::RefCell<Vec<ObjectAddress>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Run a DROP statement's body with `targets` as the objects it names.
pub(crate) fn with_drop_targets<R>(targets: Vec<ObjectAddress>, f: impl FnOnce() -> R) -> R {
    let outer = DROP_TARGETS.with(|t| t.replace(targets));
    let result = f();
    DROP_TARGETS.with(|t| t.replace(outer));
    result
}

thread_local! {
    /// The objects a DROP is deleting right now (the target and the
    /// dependents being cascaded to), so a dependency cycle — a serial
    /// column's default and its sequence — ends.
    static IN_PROGRESS: std::cell::RefCell<Vec<ObjectAddress>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn in_progress(addr: ObjectAddress) -> bool {
    IN_PROGRESS.with(|p| {
        p.borrow()
            .iter()
            .any(|x| x.classid == addr.classid && x.objid == addr.objid)
    })
}

/// Run `f` with `addr` marked as being deleted.
fn while_deleting<R>(addr: ObjectAddress, f: impl FnOnce() -> R) -> R {
    IN_PROGRESS.with(|p| p.borrow_mut().push(addr));
    let result = f();
    IN_PROGRESS.with(|p| {
        p.borrow_mut().pop();
    });
    result
}

fn is_drop_target(addr: ObjectAddress) -> bool {
    DROP_TARGETS.with(|t| {
        t.borrow()
            .iter()
            .any(|x| x.classid == addr.classid && x.objid == addr.objid)
    })
}

/// The `pg_depend` rows of objects depending on `target` (on a column of
/// it, for a whole relation), minus those whose dependent no longer
/// exists — which are forgotten.
fn dependents_of(interp: &mut PgCatalog, target: ObjectAddress) -> Vec<(ObjectAddress, DepType)> {
    let rows: Vec<(ObjectAddress, DepType)> = interp
        .pg_depend
        .iter()
        .filter(|d| {
            d.refclassid == target.classid
                && d.refobjid == target.objid
                && (target.objsubid == 0 || d.refobjsubid == target.objsubid)
                && !matches!(d.deptype, DepType::Extension | DepType::Pin)
        })
        .map(|d| {
            (
                ObjectAddress {
                    classid: d.classid,
                    objid: d.objid,
                    objsubid: d.objsubid,
                },
                d.deptype,
            )
        })
        .filter(|(dep, _)| (dep.classid, dep.objid) != (target.classid, target.objid))
        .collect();
    let mut live = Vec::new();
    for (dep, deptype) in rows {
        if exists(interp, dep) {
            if !live.iter().any(|(d, _)| *d == dep) {
                live.push((dep, deptype));
            }
        } else {
            forget_dependencies_of(interp, dep);
        }
    }
    live
}

/// `findDependentObjects` / `reportDependentObjects` for a DROP of
/// `target` (described as `target_desc`): without CASCADE, an object that
/// depends on it normally blocks the DROP — unless the same DROP names it;
/// with CASCADE it is dropped first, recursively. Automatic and internal
/// dependents always go along.
pub(crate) fn drop_dependents(
    interp: &mut PgCatalog,
    target: ObjectAddress,
    target_desc: &str,
    cascade: bool,
) -> Result<(), super::DdlError> {
    let dependents: Vec<(ObjectAddress, DepType)> = dependents_of(interp, target)
        .into_iter()
        .filter(|(d, _)| !in_progress(*d))
        .collect();
    // An object that belongs to the target (automatically or internally —
    // a table's policies, triggers and rules) goes with it, whatever else
    // of the target it depends on normally.
    let owned = |d: &ObjectAddress| {
        target.objsubid == 0
            && interp.pg_depend.iter().any(|row| {
                row.classid == d.classid
                    && row.objid == d.objid
                    && row.refclassid == target.classid
                    && row.refobjid == target.objid
                    && row.refobjsubid == 0
                    && matches!(row.deptype, DepType::Auto | DepType::Internal)
            })
    };
    if !cascade
        && let Some((blocker, _)) = dependents
            .iter()
            .find(|(d, t)| *t == DepType::Normal && !is_drop_target(*d) && !owned(d))
    {
        return Err(super::DdlError::DependencyError(format!(
            "cannot drop {target_desc} because other objects depend on it ({} depends on \
             {target_desc})",
            describe_dependent(interp, *blocker)
        )));
    }
    while_deleting(target, || {
        for (dependent, _) in dependents {
            if is_drop_target(dependent) || in_progress(dependent) || !exists(interp, dependent) {
                continue;
            }
            delete_object(interp, dependent)?;
        }
        Ok(())
    })
}

/// `findDependentObjects`' owner check for a DROP of `target`: an object
/// that is part of another one (an identity column's sequence, an
/// extension's member) can only go with it.
pub(crate) fn check_not_owned(
    interp: &PgCatalog,
    target: ObjectAddress,
    target_desc: &str,
) -> Result<(), super::DdlError> {
    let owner = interp.pg_depend.iter().find(|d| {
        d.classid == target.classid
            && d.objid == target.objid
            && matches!(d.deptype, DepType::Internal | DepType::Extension)
    });
    let Some(owner) = owner else {
        return Ok(());
    };
    let owner_addr = ObjectAddress {
        classid: owner.refclassid,
        objid: owner.refobjid,
        objsubid: owner.refobjsubid,
    };
    if is_drop_target(owner_addr) {
        return Ok(());
    }
    let owner_desc = if owner.refclassid == crate::pg_catalog::PG_EXTENSION_RELID {
        crate::oid::PgExtensionOid::new(owner.refobjid.get())
            .and_then(|e| interp.pg_extension.get(&e))
            .map(|e| format!("extension {}", e.extname))
            .unwrap_or_else(|| "extension".into())
    } else {
        describe(interp, owner_addr)
    };
    Err(super::DdlError::DependencyError(format!(
        "cannot drop {target_desc} because {owner_desc} requires it (You can drop {owner_desc} \
         instead.)"
    )))
}

/// Drop one dependent object in a cascade (`deleteOneObject`), after what
/// depends on it in turn.
fn delete_object(interp: &mut PgCatalog, addr: ObjectAddress) -> Result<(), super::DdlError> {
    while_deleting(addr, || delete_object_now(interp, addr))
}

fn delete_object_now(interp: &mut PgCatalog, addr: ObjectAddress) -> Result<(), super::DdlError> {
    let desc = describe(interp, addr);
    drop_dependents(interp, addr, &desc, true)?;
    let oid = addr.objid.get();
    match addr.classid {
        c if c == PG_PROC_RELID => {
            if let Some(proc) = PgProcOid::new(oid) {
                let views = super::views::find_views_depending_on_function(interp, proc);
                super::views::drop_views(interp, &views);
                interp.remove_pg_proc(proc);
            }
        }
        c if c == PG_OPERATOR_RELID => {
            if let Some(op) = PgOperatorOid::new(oid) {
                interp.remove_pg_operator(op);
            }
        }
        c if c == PG_CAST_RELID => {
            if let Some(cast) = PgCastOid::new(oid) {
                interp.remove_pg_cast(cast);
            }
        }
        c if c == PG_TYPE_RELID => {
            if let Some(t) = PgTypeOid::new(oid) {
                super::drop::drop_type_cascade(interp, t);
            }
        }
        c if c == PG_CLASS_RELID => {
            let Some(relid) = PgClassOid::new(oid) else {
                return Ok(());
            };
            if addr.objsubid > 0 {
                drop_column_dependent(interp, relid, addr.objsubid);
            } else {
                match interp.pg_class.get(&relid).map(|c| c.relkind) {
                    Some(RelKind::View | RelKind::MaterializedView) => {
                        super::views::drop_views(interp, &[relid]);
                    }
                    Some(RelKind::Index | RelKind::PartitionedIndex) => {
                        let table = interp.pg_index.get(&relid).map(|i| i.indrelid);
                        let name = interp.pg_class.get(&relid).map(|c| c.relname.clone());
                        interp.remove_pg_index(relid);
                        interp.remove_pg_class(relid);
                        if let (Some(table), Some(name)) = (table, name) {
                            interp
                                .pg_constraint
                                .retain(|_, c| !(c.conrelid == table && c.conname == name));
                        }
                    }
                    _ => super::drop::drop_relation_by_oid(interp, relid),
                }
            }
        }
        c if c == PG_CONSTRAINT_RELID
            && crate::oid::PgConstraintOid::new(oid)
                .is_some_and(|o| interp.pg_constraint.contains_key(&o)) =>
        {
            if let Some(con) = crate::oid::PgConstraintOid::new(oid) {
                interp.pg_constraint.remove(&con);
            }
        }
        _ => {
            if let Some(object) = interp.named_objects.remove(&addr.objid.into_nonzero()) {
                delete_named(interp, &object);
            }
        }
    }
    interp.remove_dependencies_of(addr.classid, addr.objid);
    Ok(())
}

/// A dependent column-level object: a generated column goes, a column
/// default is removed (the column stays).
fn drop_column_dependent(interp: &mut PgCatalog, relid: PgClassOid, attnum: i16) {
    let generated = interp
        .attributes_of(relid)
        .iter()
        .any(|a| a.attnum == attnum && a.attgenerated.is_some());
    interp.attr_default_types.remove(&(relid, attnum));
    interp.attr_default_exprs.remove(&(relid, attnum));
    super::defaults::forget_default_dependencies(interp, relid, attnum);
    if generated {
        interp.generated_refs.remove(&(relid, attnum));
        super::statistics::drop_column_statistics(interp, relid, attnum);
        if let Some(attrs) = interp.pg_attribute.get_mut(&relid) {
            attrs.retain(|a| a.attnum != attnum);
        }
    } else if let Some(attrs) = interp.pg_attribute.get_mut(&relid)
        && let Some(a) = attrs.iter_mut().find(|a| a.attnum == attnum)
    {
        a.atthasdef = false;
    }
}

fn delete_named(interp: &mut PgCatalog, object: &NamedObject) {
    match object {
        NamedObject::Policy { relid, name } => {
            interp
                .column_deps
                .policy_quals
                .remove(&(*relid, name.clone()));
            if let Some(p) = interp.policies.get_mut(relid) {
                p.retain(|n| &n.name != name);
            }
        }
        NamedObject::Rule { relid, name } => {
            if let Some(r) = interp.rules.get_mut(relid) {
                r.retain(|r| &r.name != name);
            }
        }
        NamedObject::Trigger { relid, name } => {
            if let Some(t) = interp.triggers.get_mut(relid) {
                t.retain(|t| &t.name != name);
            }
        }
        NamedObject::DomainConstraint { typid, name } => {
            if let Some(c) = interp.domain_constraints.get_mut(typid) {
                c.retain(|c| &c.name != name);
            }
        }
        NamedObject::PublicationRel { publication, relid } => {
            for p in &mut interp.publications {
                if &p.name == publication {
                    p.forget_relation(*relid);
                }
            }
        }
        NamedObject::Opclass {
            name,
            namespace,
            method,
        } => interp.pg_opclass.retain(|c| {
            !(&c.opcname == name && c.opcnamespace == *namespace && &c.opcmethod == method)
        }),
        NamedObject::TsObject {
            kind,
            name,
            namespace,
        } => interp
            .pg_ts_objects
            .retain(|o| !(&o.kind == kind && &o.name == name && o.namespace == *namespace)),
    }
}

/// DROP EXTENSION: remove a member the analyzer keeps by name.
pub(crate) fn delete_member(interp: &mut PgCatalog, classid: PgClassOid, objid: PgGenericOid) {
    if let Some(object) = interp.named_objects.remove(&objid.into_nonzero()) {
        delete_named(interp, &object);
    }
    interp.remove_dependencies_of(classid, objid);
    interp.remove_dependencies_on(classid, objid);
}

/// An extension's new objects the analyzer keeps by name (operator
/// classes, text search objects), as addresses for its membership rows.
pub(crate) fn extension_named_members(
    interp: &mut PgCatalog,
    opclasses_before: usize,
    ts_before: usize,
) -> Vec<ObjectAddress> {
    let mut objects: Vec<NamedObject> = interp.pg_opclass
        [opclasses_before.min(interp.pg_opclass.len())..]
        .iter()
        .map(|c| NamedObject::Opclass {
            name: c.opcname.clone(),
            namespace: c.opcnamespace,
            method: c.opcmethod.clone(),
        })
        .collect();
    objects.extend(
        interp.pg_ts_objects[ts_before.min(interp.pg_ts_objects.len())..]
            .iter()
            .filter(|o| o.kind == "c" || o.kind == "d")
            .map(|o| NamedObject::TsObject {
                kind: o.kind.clone(),
                name: o.name.clone(),
                namespace: o.namespace,
            }),
    );
    objects
        .into_iter()
        .filter_map(|o| named_address(interp, o))
        .collect()
}

/// An index depends on the (non-built-in) operator classes its columns
/// name.
pub(crate) fn record_index_opclasses(
    interp: &mut PgCatalog,
    index: PgClassOid,
    params: &[typedpg_pg_query::protobuf::Node],
    am: &str,
) {
    let mut refs = Vec::new();
    for param in params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
        let parts: Vec<&str> = elem
            .opclass
            .iter()
            .filter_map(super::util::node_string)
            .collect();
        let (schema, name) = match parts.as_slice() {
            [s, n] => (Some(*s), *n),
            [n] => (None, *n),
            _ => continue,
        };
        if let Some(c) = super::opclass::find_opclass(interp, schema, name, am) {
            refs.push(Reference::Named(NamedObject::Opclass {
                name: c.opcname.clone(),
                namespace: c.opcnamespace,
                method: c.opcmethod.clone(),
            }));
        }
    }
    record_references(
        interp,
        ObjectAddress::relation(index),
        &refs,
        DepType::Normal,
    );
}

/// ALTER TABLE / TYPE ... DROP COLUMN / ATTRIBUTE: what depends on the
/// column (SQL function bodies, rules, policies, triggers, views over a
/// composite's field, ...) needs CASCADE (`ATExecDropColumn` →
/// `performDeletion`).
pub(crate) fn drop_column_dependents(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    cascade: bool,
) -> Result<(), super::DdlError> {
    let addr = ObjectAddress::column(relid, attnum);
    let desc = describe(interp, addr);
    drop_dependents(interp, addr, &desc, cascade)
}

// ─── Recording what stored expressions refer to ────────────────────────────

/// What expression `expr` refers to, analyzed over the columns of relation
/// `relid` (when given). An expression that no longer analyzes refers to
/// nothing the analyzer can tell.
pub(crate) fn expression_references(
    interp: &PgCatalog,
    expr: &typedpg_pg_query::protobuf::Node,
    relid: Option<PgClassOid>,
) -> Vec<Reference> {
    let mut scope = crate::scope::Scope::default();
    if let Some(relid) = relid
        && let Some(class) = interp.pg_class.get(&relid)
    {
        let schema = interp
            .namespace_name(class.relnamespace)
            .unwrap_or_default();
        let attrs = interp.attributes_of(relid).to_vec();
        scope.add_dml_target(
            interp,
            &class.relname,
            crate::qualified_name::QualifiedName::new(schema, &class.relname),
            &attrs,
        );
    }
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let (_, refs) = collect(|| {
        crate::expr::infer_expr(
            expr,
            crate::expr::Ctx::new(&scope, &null_ctx, interp),
            &mut params,
            crate::expr::TypeGoal::NONE,
        )
    });
    refs
}

/// `recordDependencyOnSingleRelExpr`: `depender` depends on what `exprs`
/// refer to — normally, except for the columns of its own relation
/// `relid`, which it depends on automatically (it goes with them).
fn record_expressions(
    interp: &mut PgCatalog,
    depender: ObjectAddress,
    exprs: &[&typedpg_pg_query::protobuf::Node],
    relid: Option<PgClassOid>,
) {
    let refs: Vec<Reference> = exprs
        .iter()
        .flat_map(|e| expression_references(interp, e, relid))
        .collect();
    let own = |r: &Reference| match r {
        Reference::Column(rel, _) => Some(*rel) == relid,
        Reference::Object(a) => {
            a.classid == PG_CLASS_RELID && relid.is_some_and(|rel| rel.get() == a.objid.get())
        }
        Reference::Named(_) => false,
    };
    let (own_refs, other_refs): (Vec<Reference>, Vec<Reference>) = refs.into_iter().partition(own);
    record_references(interp, depender, &other_refs, DepType::Normal);
    // The relation itself is implied; only its columns are recorded.
    let own_columns: Vec<Reference> = own_refs
        .into_iter()
        .filter(|r| matches!(r, Reference::Column(..)))
        .collect();
    record_references(interp, depender, &own_columns, DepType::Auto);
}

/// A CHECK constraint (`check_defs`) depends on what its expression
/// refers to beyond its own table (StoreRelCheck → recordDependencyOnExpr).
pub(crate) fn record_check_constraint(interp: &mut PgCatalog, oid: crate::oid::PgConstraintOid) {
    let Some(relid) = interp.pg_constraint.get(&oid).map(|c| c.conrelid) else {
        return;
    };
    let Some(super::tables::check_inherit::StoredExpr::Written(expr)) =
        interp.check_defs.get(&oid).map(|d| d.expr.clone())
    else {
        return;
    };
    let addr = ObjectAddress::new(PG_CONSTRAINT_RELID, oid.into_nonzero(), 0);
    forget_dependencies_of(interp, addr);
    record_expressions(interp, addr, &[&expr], Some(relid));
}

/// A column default or generation expression (`attr_default_exprs`)
/// depends on what it refers to (StoreAttrDefault).
pub(crate) fn record_column_expression(interp: &mut PgCatalog, relid: PgClassOid, attnum: i16) {
    let Some(super::tables::check_inherit::StoredExpr::Written(expr)) =
        interp.attr_default_exprs.get(&(relid, attnum)).cloned()
    else {
        return;
    };
    record_expressions(
        interp,
        ObjectAddress::column(relid, attnum),
        &[&expr],
        Some(relid),
    );
}

/// An index depends on what its expressions and predicate refer to
/// (index_create → recordDependencyOnSingleRelExpr).
pub(crate) fn record_index_expressions(interp: &mut PgCatalog, index: PgClassOid) {
    use prost::Message;
    let Some(row) = interp.pg_index.get(&index) else {
        return;
    };
    let relid = row.indrelid;
    let exprs: Vec<typedpg_pg_query::protobuf::Node> = row
        .indexprs
        .iter()
        .chain(row.indpred.iter())
        .filter_map(|ast| typedpg_pg_query::protobuf::Node::decode(ast.ast.as_slice()).ok())
        .collect();
    let refs: Vec<&typedpg_pg_query::protobuf::Node> = exprs.iter().collect();
    record_expressions(interp, ObjectAddress::relation(index), &refs, Some(relid));
}

/// A named object (a rule, a trigger, ...) depends on `refs`.
pub(crate) fn record_named(
    interp: &mut PgCatalog,
    object: NamedObject,
    refs: &[Reference],
) -> Result<(), super::DdlError> {
    let addr = named_object(interp, object)?;
    record_references(interp, addr, refs, DepType::Normal);
    Ok(())
}

/// A publication's table depends on the columns of its column list and on
/// what its row filter refers to (publication_add_relation).
pub(crate) fn record_publication_rel(
    interp: &mut PgCatalog,
    publication: &str,
    relid: PgClassOid,
    table: &typedpg_pg_query::protobuf::PublicationTable,
) -> Result<(), super::DdlError> {
    let mut refs: Vec<Reference> = table
        .columns
        .iter()
        .filter_map(super::util::node_string)
        .map(|c| Reference::Column(relid, c.to_owned()))
        .collect();
    if let Some(filter) = table.where_clause.as_deref() {
        refs.extend(expression_references(interp, filter, Some(relid)));
    }
    let addr = named_object(
        interp,
        NamedObject::PublicationRel {
            publication: publication.to_owned(),
            relid,
        },
    )?;
    // The table's own columns matter here (unlike a table's expressions).
    record_references(interp, addr, &refs, DepType::Normal);
    Ok(())
}

/// ALTER TEXT SEARCH CONFIGURATION ... MAPPING ... WITH dicts: the
/// configuration depends on the dictionaries (MakeConfigurationMapping).
pub(crate) fn record_ts_mapping(
    interp: &mut PgCatalog,
    config: &[&str],
    dicts: &[typedpg_pg_query::protobuf::Node],
) -> Result<(), super::DdlError> {
    let find = |interp: &PgCatalog, kind: &str, names: &[&str]| -> Option<NamedObject> {
        let (schema, name) = match names {
            [s, n] => (Some(*s), *n),
            [.., n] => (None, *n),
            [] => return None,
        };
        let namespace = interp.schemas_for_lookup(schema).into_iter().find(|ns| {
            interp
                .pg_ts_objects
                .iter()
                .any(|o| o.kind == kind && o.name == name && o.namespace == *ns)
        })?;
        Some(NamedObject::TsObject {
            kind: kind.to_owned(),
            name: name.to_owned(),
            namespace,
        })
    };
    let Some(config) = find(interp, "c", config) else {
        return Ok(());
    };
    let Some(depender) = named_address(interp, config) else {
        return Ok(());
    };
    let refs: Vec<Reference> = dicts
        .iter()
        .filter_map(|d| match d.node.as_ref() {
            Some(node::Node::List(l)) => {
                let names: Vec<&str> = l
                    .items
                    .iter()
                    .filter_map(super::util::node_string)
                    .collect();
                find(interp, "d", &names).map(Reference::Named)
            }
            _ => None,
        })
        .collect();
    record_references(interp, depender, &refs, DepType::Normal);
    Ok(())
}
