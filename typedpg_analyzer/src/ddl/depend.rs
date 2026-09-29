//! Dependencies between catalog objects (`pg_depend`, catalog/dependency.c
//! and pg_depend.c): recording what an object depends on, and the checks a
//! DROP makes against what depends on its targets.

use typedpg_pg_query::protobuf::{ObjectType, node};

use crate::oid::{PgCastOid, PgClassOid, PgGenericOid, PgOperatorOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    DepType, PG_CAST_RELID, PG_CLASS_RELID, PG_OPERATOR_RELID, PG_PROC_RELID, PG_TYPE_RELID,
    PgCatalog, PgDepend,
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
