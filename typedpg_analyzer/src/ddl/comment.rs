//! COMMENT ON and ALTER ... OWNER TO — the comment or owner doesn't
//! matter to static analysis, but PG resolves the target object first
//! (`get_object_address`), so a migration naming something that doesn't
//! exist fails.

use typedpg_pg_query::protobuf::{CommentStmt, ObjectType, node};

use super::DdlError;
use super::util::node_string;
use crate::pg_catalog::{PgCatalog, RelKind};

/// COMMENT ON (CommentObject): the object must exist, and a column's
/// relation must be one whose columns can carry comments.
pub fn comment_on(interp: &PgCatalog, stmt: &CommentStmt) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_deref().and_then(|o| o.node.as_ref()) else {
        return Ok(());
    };
    let objtype = ObjectType::try_from(stmt.objtype).unwrap_or(ObjectType::Undefined);
    resolve_object(interp, objtype, object)?;
    if objtype == ObjectType::ObjectColumn
        && let node::Node::List(l) = object
    {
        let parts: Vec<&str> = l.items.iter().filter_map(node_string).collect();
        let (schema, name) = match parts.as_slice() {
            [schema, name, _] => (Some(*schema), *name),
            [name, _] => (None, *name),
            _ => return Ok(()),
        };
        if let Some(class) = interp.resolve_table(schema, name)
            && !matches!(
                class.relkind,
                RelKind::Table
                    | RelKind::View
                    | RelKind::MaterializedView
                    | RelKind::CompositeType
                    | RelKind::ForeignTable
                    | RelKind::Partitioned
            )
        {
            return Err(DdlError::Parse(format!(
                "cannot set comment on relation \"{name}\" (This operation is not supported for \
                 {}.)",
                class.relkind.plural()
            )));
        }
    }
    Ok(())
}

/// `ALTER <object> ... OWNER TO role` (ExecAlterOwnerStmt). The new owner
/// is not checked: roles live in the cluster, outside what migrations
/// build.
pub fn alter_owner(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterOwnerStmt,
) -> Result<(), DdlError> {
    let objtype = ObjectType::try_from(stmt.object_type).unwrap_or(ObjectType::Undefined);
    if let Some(rv) = stmt.relation.as_ref() {
        super::util::lookup_relation(interp, rv)?;
        return Ok(());
    }
    let Some(object) = stmt.object.as_deref().and_then(|o| o.node.as_ref()) else {
        return Ok(());
    };
    // ALTER TYPE / DOMAIN name the type as a plain name list.
    if let (ObjectType::ObjectType | ObjectType::ObjectDomain, node::Node::List(l)) =
        (objtype, object)
    {
        let tn = typedpg_pg_query::protobuf::TypeName {
            names: l.items.clone(),
            ..Default::default()
        };
        return resolve_object(interp, objtype, &node::Node::TypeName(tn));
    }
    resolve_object(interp, objtype, object)
}

/// get_object_address: the object `objtype` / `object` names must exist.
pub(crate) fn resolve_object(
    interp: &PgCatalog,
    objtype: ObjectType,
    object: &node::Node,
) -> Result<(), DdlError> {
    let names = |n: &node::Node| -> Vec<String> {
        match n {
            node::Node::List(l) => l
                .items
                .iter()
                .filter_map(node_string)
                .map(str::to_owned)
                .collect(),
            node::Node::String(s) => vec![s.sval.clone()],
            _ => Vec::new(),
        }
    };
    let relation = |parts: &[String]| -> Result<crate::oid::PgClassOid, DdlError> {
        let (schema, name) = match parts {
            [name] => (None, name.as_str()),
            [schema, name] => (Some(schema.as_str()), name.as_str()),
            _ => return Err(DdlError::Parse("improper relation name".into())),
        };
        if let Some(s) = schema
            && interp.namespace_oid(s).is_none()
        {
            return Err(DdlError::TableNotFound(format!(
                "schema \"{s}\" does not exist"
            )));
        }
        interp
            .resolve_table(schema, name)
            .map(|c| c.oid)
            .ok_or_else(|| {
                DdlError::TableNotFound(format!("relation \"{}\" does not exist", parts.join(".")))
            })
    };

    match objtype {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectSequence
        | ObjectType::ObjectIndex
        | ObjectType::ObjectForeignTable => {
            let parts = names(object);
            let oid = relation(&parts)?;
            let relkind = interp.pg_class.get(&oid).map(|c| c.relkind);
            let (ok, what) = match objtype {
                ObjectType::ObjectTable => (
                    matches!(relkind, Some(RelKind::Table | RelKind::Partitioned)),
                    "a table",
                ),
                ObjectType::ObjectView => (relkind == Some(RelKind::View), "a view"),
                ObjectType::ObjectMatview => (
                    relkind == Some(RelKind::MaterializedView),
                    "a materialized view",
                ),
                ObjectType::ObjectSequence => (relkind == Some(RelKind::Sequence), "a sequence"),
                ObjectType::ObjectIndex => (
                    matches!(relkind, Some(RelKind::Index | RelKind::PartitionedIndex)),
                    "an index",
                ),
                _ => (relkind == Some(RelKind::ForeignTable), "a foreign table"),
            };
            if !ok {
                return Err(DdlError::Parse(format!(
                    "\"{}\" is not {what}",
                    parts.last().cloned().unwrap_or_default()
                )));
            }
        }
        ObjectType::ObjectColumn => {
            let parts = names(object);
            // get_object_address_attribute.
            if parts.len() < 2 {
                return Err(DdlError::Parse("column name must be qualified".into()));
            }
            let Some((column, rel)) = parts.split_last() else {
                return Ok(());
            };
            let oid = relation(rel)?;
            // get_attnum: system columns count.
            let system = crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .any(|(n, ..)| n == column);
            if interp.attribute_by_name(oid, column).is_none() && !system {
                return Err(DdlError::Parse(format!(
                    "column \"{column}\" of relation \"{}\" does not exist",
                    rel.last().cloned().unwrap_or_default()
                )));
            }
        }
        ObjectType::ObjectTabconstraint => {
            let parts = names(object);
            let Some((conname, rel)) = parts.split_last() else {
                return Ok(());
            };
            let oid = relation(rel)?;
            if !interp
                .pg_constraint
                .values()
                .any(|c| c.conrelid == oid && &c.conname == conname)
            {
                return Err(DdlError::Parse(format!(
                    "constraint \"{conname}\" for table \"{}\" does not exist",
                    rel.last().cloned().unwrap_or_default()
                )));
            }
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            if let node::Node::TypeName(tn) = object {
                super::util::lookup_type_name(tn, interp)?;
                if objtype == ObjectType::ObjectDomain {
                    super::types::check_is_domain(interp, &tn.names)?;
                }
            }
        }
        ObjectType::ObjectSchema => {
            if let node::Node::String(s) = object
                && interp.namespace_oid(&s.sval).is_none()
            {
                return Err(DdlError::TableNotFound(format!(
                    "schema \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectFunction
        | ObjectType::ObjectProcedure
        | ObjectType::ObjectRoutine
        | ObjectType::ObjectAggregate => {
            let unspecified = matches!(object, node::Node::ObjectWithArgs(o) if o.args_unspecified);
            let object = Some(Box::new(typedpg_pg_query::protobuf::Node {
                node: Some(object.clone()),
            }));
            let Some((schema, name, arg_oids)) = super::alter::extract_func_target(&object, interp)
            else {
                return Ok(());
            };
            let kind = match objtype {
                ObjectType::ObjectProcedure => "procedure",
                ObjectType::ObjectAggregate => "aggregate",
                _ => "function",
            };
            // LookupFuncWithArgs without an argument list: the name must be
            // unique.
            if unspecified {
                let candidates = interp.find_functions(schema.as_deref(), &name);
                return match candidates.len() {
                    0 => Err(DdlError::TypeNotFound(format!(
                        "could not find a {kind} named \"{name}\""
                    ))),
                    1 => Ok(()),
                    _ => Err(DdlError::DependencyError(format!(
                        "{kind} name \"{name}\" is not unique"
                    ))),
                };
            }
            let found = super::alter::find_proc(interp, schema.as_deref(), &name, &|p| {
                p.proargtypes == arg_oids
            });
            let signature = || {
                let args = arg_oids
                    .iter()
                    .map(|&t| super::util::format_type_for_message(interp, t))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}({args})")
            };
            let Some((_, oid)) = found else {
                return Err(DdlError::DependencyError(format!(
                    "{kind} {} does not exist",
                    signature()
                )));
            };
            // LookupFuncWithArgs: the object type must match the kind.
            let prokind = interp.pg_proc.get(&oid).map(|p| p.prokind);
            use crate::pg_catalog::ProKind;
            match objtype {
                ObjectType::ObjectFunction if prokind == Some(ProKind::Procedure) => {
                    return Err(DdlError::Parse(format!(
                        "{} is not a function",
                        signature()
                    )));
                }
                ObjectType::ObjectProcedure if prokind != Some(ProKind::Procedure) => {
                    return Err(DdlError::Parse(format!(
                        "{} is not a procedure",
                        signature()
                    )));
                }
                ObjectType::ObjectAggregate if prokind != Some(ProKind::Aggregate) => {
                    return Err(DdlError::Parse(format!(
                        "function {} is not an aggregate",
                        signature()
                    )));
                }
                _ => {}
            }
        }
        ObjectType::ObjectTsconfiguration
        | ObjectType::ObjectTsdictionary
        | ObjectType::ObjectTsparser
        | ObjectType::ObjectTstemplate => {
            let kind = match objtype {
                ObjectType::ObjectTsconfiguration => "c",
                ObjectType::ObjectTsdictionary => "d",
                ObjectType::ObjectTsparser => "p",
                _ => "t",
            };
            let parts = names(object);
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            super::text_search::find(interp, kind, &parts)?;
        }
        ObjectType::ObjectForeignServer => {
            if let node::Node::String(s) = object {
                super::fdw::check_server(interp, &s.sval)?;
            }
        }
        ObjectType::ObjectStatisticExt => {
            if let node::Node::List(l) = object
                && !super::statistics::statistics_exist(interp, &l.items)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "statistics object \"{}\" does not exist",
                    names(object).join(".")
                )));
            }
        }
        ObjectType::ObjectCollation => {
            let parts = names(object);
            let (schema, name) = match parts.as_slice() {
                [name] => (None, name.as_str()),
                [schema, name] => (Some(schema.as_str()), name.as_str()),
                _ => return Ok(()),
            };
            if interp.resolve_collation(schema, name).is_none() {
                return Err(DdlError::TypeNotFound(format!(
                    "collation \"{}\" for encoding \"UTF8\" does not exist",
                    parts.join(".")
                )));
            }
        }
        ObjectType::ObjectTrigger | ObjectType::ObjectPolicy | ObjectType::ObjectRule => {
            let parts = names(object);
            let Some((sub, rel)) = parts.split_last() else {
                return Ok(());
            };
            let oid = relation(rel)?;
            let relname = rel.last().cloned().unwrap_or_default();
            let (exists, what, of) = match objtype {
                ObjectType::ObjectTrigger => (
                    interp
                        .triggers
                        .get(&oid)
                        .is_some_and(|ts| ts.iter().any(|t| &t.name == sub)),
                    "trigger",
                    "table",
                ),
                ObjectType::ObjectPolicy => (
                    interp
                        .policies
                        .get(&oid)
                        .is_some_and(|ps| ps.iter().any(|p| &p.name == sub)),
                    "policy",
                    "table",
                ),
                _ => (super::rules::has_rule(interp, oid, sub), "rule", "relation"),
            };
            if !exists {
                return Err(DdlError::TypeNotFound(format!(
                    "{what} \"{sub}\" for {of} \"{relname}\" does not exist"
                )));
            }
        }
        ObjectType::ObjectOperator => {
            let node::Node::ObjectWithArgs(owa) = object else {
                return Ok(());
            };
            let parts: Vec<&str> = owa.objname.iter().filter_map(node_string).collect();
            let (schema, name) = match parts.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => return Ok(()),
            };
            // LookupOperWithArgs: NONE is an empty type name.
            let mut args = Vec::new();
            for arg in &owa.objargs {
                match arg.node.as_ref() {
                    Some(node::Node::TypeName(tn)) if !tn.names.is_empty() => {
                        args.push(Some(super::util::lookup_type_name(tn, interp)?));
                    }
                    _ => args.push(None),
                }
            }
            let [left, Some(right)] = args.as_slice() else {
                return Ok(());
            };
            let (left, right) = (*left, *right);
            let found = super::drop::find_operator(interp, schema, name, &|o| {
                o.oprleft == left && o.oprright == right
            });
            if found.is_none() {
                let left_name = left
                    .map(|t| super::util::format_type_for_message(interp, t))
                    .unwrap_or_else(|| "NONE".into());
                return Err(DdlError::DependencyError(format!(
                    "operator does not exist: {left_name} {name} {}",
                    super::util::format_type_for_message(interp, right)
                )));
            }
        }
        ObjectType::ObjectCast => {
            let node::Node::List(l) = object else {
                return Ok(());
            };
            let [Some(node::Node::TypeName(source)), Some(target)] = [
                l.items.first().and_then(|n| n.node.as_ref()),
                l.items.get(1).and_then(|n| n.node.as_ref()),
            ] else {
                return Ok(());
            };
            let node::Node::TypeName(target) = target else {
                return Ok(());
            };
            let source = super::util::lookup_type_name(source, interp)?;
            let target = super::util::lookup_type_name(target, interp)?;
            if !interp.cast_by_pair.contains_key(&(source, target)) {
                return Err(DdlError::TypeNotFound(format!(
                    "cast from type {} to type {} does not exist",
                    super::util::format_type_for_message(interp, source),
                    super::util::format_type_for_message(interp, target)
                )));
            }
        }
        ObjectType::ObjectOpclass | ObjectType::ObjectOpfamily => {
            // `[access method, name...]`.
            let parts = names(object);
            let Some((am, rest)) = parts.split_first() else {
                return Ok(());
            };
            if !super::opclass::am_exists(interp, am) {
                return Err(DdlError::TypeNotFound(format!(
                    "access method \"{am}\" does not exist"
                )));
            }
            let (schema, name) = match rest {
                [schema, name] => (Some(schema.as_str()), name.as_str()),
                [name] => (None, name.as_str()),
                _ => return Ok(()),
            };
            let (found, what) = if objtype == ObjectType::ObjectOpclass {
                (
                    super::opclass::find_opclass(interp, schema, name, am).is_some(),
                    "operator class",
                )
            } else {
                (
                    super::opclass::find_opfamily(interp, schema, name, am).is_some(),
                    "operator family",
                )
            };
            if !found {
                return Err(DdlError::TypeNotFound(format!(
                    "{what} \"{}\" does not exist for access method \"{am}\"",
                    rest.join(".")
                )));
            }
        }
        ObjectType::ObjectAccessMethod => {
            if let node::Node::String(s) = object
                && !super::opclass::am_exists(interp, &s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "access method \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectTablespace => {
            if let node::Node::String(s) = object
                && !super::cluster::tablespace_exists(interp, &s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "tablespace \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectFdw => {
            if let node::Node::String(s) = object {
                super::fdw::check_fdw(interp, &s.sval)?;
            }
        }
        ObjectType::ObjectLargeobject => {
            let oid = match object {
                node::Node::Integer(i) => u32::try_from(i.ival).ok(),
                node::Node::Float(f) => f.fval.parse().ok(),
                _ => None,
            };
            if let Some(oid) = oid
                && !super::cluster::large_object_exists(interp, oid)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "large object {oid} does not exist"
                )));
            }
        }
        ObjectType::ObjectTransform => {
            let wrapped = typedpg_pg_query::protobuf::Node {
                node: Some(object.clone()),
            };
            super::languages::find_transform(interp, &wrapped)?;
        }
        ObjectType::ObjectConversion => {
            let parts = names(object);
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            if !super::conversions::conversion_exists(interp, &parts) {
                return Err(DdlError::TypeNotFound(format!(
                    "conversion \"{}\" does not exist",
                    parts.join(".")
                )));
            }
        }
        ObjectType::ObjectEventTrigger => {
            if let node::Node::String(s) = object
                && !interp.event_triggers.iter().any(|(t, _)| *t == s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "event trigger \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectSubscription => {
            if let node::Node::String(s) = object {
                super::cluster::alter_subscription(interp, &s.sval)?;
            }
        }
        ObjectType::ObjectPublication => {
            if let node::Node::String(s) = object
                && !super::publications::publication_exists(interp, &s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "publication \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectLanguage => {
            if let node::Node::String(s) = object {
                super::languages::check(interp, &s.sval)?;
            }
        }
        _ => {}
    }
    Ok(())
}
