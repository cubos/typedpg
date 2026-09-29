//! Dependencies between catalog objects (`pg_depend`, catalog/dependency.c
//! and pg_depend.c): recording what an object depends on, and the checks a
//! DROP makes against what depends on its targets.

use typedpg_pg_query::protobuf::{ObjectType, node};

use crate::pg_catalog::PgCatalog;

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
