//! GRANT / REVOKE on objects. Privileges don't affect static analysis, but
//! PG resolves every target object first (`ExecuteGrantStmt` →
//! `objectNamesToOids`), so a GRANT naming a missing table, column,
//! sequence, function, schema or type fails the migration.
//!
//! Grantee roles are not checked: roles live in the cluster, outside what
//! the migrations build, and are usually created elsewhere.

use pg_query::protobuf::{GrantStmt, GrantTargetType, ObjectType, node};

use super::DdlError;
use super::util::node_string;
use crate::pg_catalog::{PgCatalog, RelKind};

pub fn grant(interp: &PgCatalog, stmt: &GrantStmt) -> Result<(), DdlError> {
    let targtype = GrantTargetType::try_from(stmt.targtype).unwrap_or(GrantTargetType::Undefined);
    let objtype = ObjectType::try_from(stmt.objtype).unwrap_or(ObjectType::Undefined);
    match targtype {
        GrantTargetType::AclTargetAllInSchema => {
            for obj in &stmt.objects {
                if let Some(schema) = node_string(obj)
                    && interp.namespace_oid(schema).is_none()
                {
                    return Err(DdlError::TableNotFound(format!(
                        "schema \"{schema}\" does not exist"
                    )));
                }
            }
            Ok(())
        }
        GrantTargetType::AclTargetObject => {
            for obj in &stmt.objects {
                check_object(interp, objtype, obj, &stmt.privileges)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_object(
    interp: &PgCatalog,
    objtype: ObjectType,
    obj: &pg_query::protobuf::Node,
    privileges: &[pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    match (objtype, obj.node.as_ref()) {
        (ObjectType::ObjectTable | ObjectType::ObjectSequence, Some(node::Node::RangeVar(rv))) => {
            let (_, relid) = super::util::lookup_relation(interp, rv)?;
            let relkind = interp.pg_class.get(&relid).map(|c| c.relkind);
            if objtype == ObjectType::ObjectSequence && relkind != Some(RelKind::Sequence) {
                return Err(DdlError::Parse(format!(
                    "\"{}\" is not a sequence",
                    rv.relname
                )));
            }
            // Column privileges name existing columns.
            for p in privileges {
                if let Some(node::Node::AccessPriv(ap)) = p.node.as_ref() {
                    for col in ap.cols.iter().filter_map(node_string) {
                        if interp.attribute_by_name(relid, col).is_none() {
                            return Err(DdlError::Parse(format!(
                                "column \"{col}\" of relation \"{}\" does not exist",
                                rv.relname
                            )));
                        }
                    }
                }
            }
        }
        (
            ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectRoutine,
            Some(node::Node::ObjectWithArgs(owa)),
        ) => {
            let object = Some(Box::new(pg_query::protobuf::Node {
                node: Some(node::Node::ObjectWithArgs(owa.clone())),
            }));
            let Some((schema, name, arg_oids)) = super::alter::extract_func_target(&object, interp)
            else {
                return Ok(());
            };
            let kind = match objtype {
                ObjectType::ObjectProcedure => "procedure",
                ObjectType::ObjectRoutine => "routine",
                _ => "function",
            };
            if owa.args_unspecified {
                match interp.find_functions(schema.as_deref(), &name).len() {
                    0 => {
                        return Err(DdlError::TypeNotFound(format!(
                            "could not find a {kind} named \"{name}\""
                        )));
                    }
                    1 => {}
                    _ => {
                        return Err(DdlError::DependencyError(format!(
                            "{kind} name \"{name}\" is not unique"
                        )));
                    }
                }
            } else {
                let wanted = arg_oids.clone();
                let found = super::alter::find_proc(interp, schema.as_deref(), &name, &move |p| {
                    p.proargtypes == wanted
                });
                if found.is_none() {
                    let args = arg_oids
                        .iter()
                        .map(|&t| super::util::format_type_for_message(interp, t))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(DdlError::TypeNotFound(format!(
                        "{kind} {name}({args}) does not exist"
                    )));
                }
            }
        }
        (ObjectType::ObjectSchema, _) => {
            if let Some(schema) = node_string(obj)
                && interp.namespace_oid(schema).is_none()
            {
                return Err(DdlError::TableNotFound(format!(
                    "schema \"{schema}\" does not exist"
                )));
            }
        }
        (ObjectType::ObjectType | ObjectType::ObjectDomain, Some(node::Node::List(l))) => {
            let tn = pg_query::protobuf::TypeName {
                names: l.items.clone(),
                ..Default::default()
            };
            super::util::lookup_type_name(&tn, interp)?;
        }
        _ => {}
    }
    Ok(())
}
