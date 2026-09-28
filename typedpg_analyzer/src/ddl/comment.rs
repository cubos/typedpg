//! COMMENT ON — the comment itself doesn't matter to static analysis, but
//! PG resolves the target object first (`get_object_address`), so a
//! migration commenting on something that doesn't exist fails.

use pg_query::protobuf::{CommentStmt, ObjectType, node};

use super::DdlError;
use super::util::node_string;
use crate::pg_catalog::{PgCatalog, RelKind};

pub fn comment_on(interp: &PgCatalog, stmt: &CommentStmt) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_deref().and_then(|o| o.node.as_ref()) else {
        return Ok(());
    };
    let objtype = ObjectType::try_from(stmt.objtype).unwrap_or(ObjectType::Undefined);
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
            let Some((column, rel)) = parts.split_last() else {
                return Ok(());
            };
            let oid = relation(rel)?;
            if interp.attribute_by_name(oid, column).is_none() {
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
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure => {
            let object = Some(Box::new(pg_query::protobuf::Node {
                node: Some(object.clone()),
            }));
            let Some((schema, name, arg_oids)) = super::alter::extract_func_target(&object, interp)
            else {
                return Ok(());
            };
            let found = super::alter::find_proc(interp, schema.as_deref(), &name, &|p| {
                p.proargtypes == arg_oids
            });
            if found.is_none() {
                let args = arg_oids
                    .iter()
                    .map(|&t| super::util::format_type_for_message(interp, t))
                    .collect::<Vec<_>>()
                    .join(", ");
                let kind = if objtype == ObjectType::ObjectProcedure {
                    "procedure"
                } else {
                    "function"
                };
                return Err(DdlError::DependencyError(format!(
                    "{kind} {name}({args}) does not exist"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}
