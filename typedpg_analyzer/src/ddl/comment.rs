//! COMMENT ON and ALTER ... OWNER TO — the comment doesn't matter to static
//! analysis, but PG resolves the target object first
//! (`get_object_address`), and the owner is modeled (object privileges are
//! checked against it), so a migration naming something that doesn't exist
//! fails.

use typedpg_pg_query::ast::{self, AlterOwnerStmt, CommentStmt, ObjectType};

use super::DdlError;
use super::acl::{self, AclObject};
use super::util::node_string;
use crate::pg_catalog::{PgCatalog, RelKind};

/// COMMENT ON (CommentObject): the object must exist, and a column's
/// relation must be one whose columns can carry comments.
pub fn comment_on(interp: &PgCatalog, stmt: &CommentStmt) -> Result<(), DdlError> {
    let Some(object) = stmt.object.as_ref() else {
        return Ok(());
    };
    let objtype = stmt.objtype;
    resolve_object(interp, objtype, object)?;
    if objtype == ObjectType::ObjectColumn
        && let ast::Node::List(l) = object
    {
        let parts: Vec<&str> = l
            .items
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .collect();
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

/// `ALTER <object> ... OWNER TO role` (ExecAlterOwnerStmt): the target must
/// exist and the new owner is recorded. Roles live in the cluster, outside
/// what the migrations build, so a role the catalog doesn't know is taken to
/// exist (see `ddl::session::role_may_exist`).
pub fn alter_owner(interp: &mut PgCatalog, stmt: &AlterOwnerStmt) -> Result<(), DdlError> {
    let objtype = stmt.object_type;
    let new_owner = match stmt.newowner.as_ref() {
        Some(spec) => Some(super::roles::resolve_role_spec(interp, spec)?),
        None => None,
    };
    if let Some(rv) = stmt.relation.as_ref() {
        let (_, relid) = super::util::lookup_relation(interp, rv)?;
        if let Some(owner) = new_owner {
            acl::change_relation_owner(interp, relid, owner);
        }
        return Ok(());
    }
    let Some(object) = stmt.object.as_ref() else {
        return Ok(());
    };
    resolve_object(interp, objtype, object)?;
    let Some(owner) = new_owner else {
        return Ok(());
    };
    match objtype {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectSequence
        | ObjectType::ObjectForeignTable => {
            let parts = object_names(object);
            if let Some(relid) = relation_oid(interp, &parts) {
                acl::change_relation_owner(interp, relid, owner);
            }
        }
        ObjectType::ObjectIndex => {
            // ATExecChangeOwner's index case: a plain index follows its
            // table (skipped), a partitioned one is refused.
            let parts = object_names(object);
            if let Some(relid) = relation_oid(interp, &parts)
                && interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::PartitionedIndex)
            {
                return Err(DdlError::UnsupportedDdl(format!(
                    "cannot change owner of index \"{}\" (Change the ownership of the index's \
                     table instead.)",
                    parts.last().cloned().unwrap_or_default()
                )));
            }
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            let tn = typedpg_pg_query::ast::TypeName {
                names: match object {
                    ast::Node::List(l) => l.items.clone(),
                    ast::Node::TypeName(tn) => tn.names.clone(),
                    _ => return Ok(()),
                },
                ..Default::default()
            };
            let oid = super::util::lookup_type_name(&tn, interp)?;
            if let Some(t) = interp.get_type(oid)
                && t.typtype == crate::pg_catalog::TypType::Composite
                && let Some(relid) = t.typrelid
                && interp.pg_class.get(&relid).map(|c| c.relkind) != Some(RelKind::CompositeType)
            {
                return Err(DdlError::Parse(format!(
                    "{} is a table's row type",
                    super::util::format_type_for_message(interp, oid)
                )));
            }
            interp.change_object_owner(AclObject::Type(oid), owner);
            if let Some(array) = interp.array_type_of(oid) {
                interp.change_object_owner(AclObject::Type(array), owner);
            }
        }
        ObjectType::ObjectSchema => {
            if let ast::Node::String(s) = object
                && let Some(ns) = interp.namespace_oid(&s.sval)
            {
                interp.change_object_owner(AclObject::Schema(ns), owner);
            }
        }
        ObjectType::ObjectFunction
        | ObjectType::ObjectProcedure
        | ObjectType::ObjectRoutine
        | ObjectType::ObjectAggregate => {
            if let ast::Node::ObjectWithArgs(owa) = object
                && let Some(oid) =
                    super::functions::lookup_func_with_args(interp, objtype, owa, false)?
            {
                interp.change_object_owner(AclObject::Function(oid), owner);
            }
        }
        ObjectType::ObjectLanguage => {
            if let ast::Node::String(s) = object
                && let Some(oid) = interp
                    .pg_language
                    .values()
                    .find(|l| l.lanname == s.sval)
                    .map(|l| l.oid)
            {
                interp.change_object_owner(AclObject::Language(oid), owner);
            }
        }
        _ => {}
    }
    Ok(())
}

fn object_names(object: &ast::Node) -> Vec<String> {
    match object {
        ast::Node::List(l) => l
            .items
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .map(str::to_owned)
            .collect(),
        ast::Node::String(s) => vec![s.sval.clone()],
        _ => Vec::new(),
    }
}

fn relation_oid(interp: &PgCatalog, parts: &[String]) -> Option<crate::oid::PgClassOid> {
    let (schema, name) = match parts {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => return None,
    };
    interp.resolve_table(schema, name).map(|c| c.oid)
}

/// get_object_address: the object `objtype` / `object` names must exist.
pub(crate) fn resolve_object(
    interp: &PgCatalog,
    objtype: ObjectType,
    object: &ast::Node,
) -> Result<(), DdlError> {
    let names = object_names;
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
            // An index's columns are its own (an index has no system
            // columns).
            let exists = if interp.pg_index.contains_key(&oid) {
                super::indexes::index_attnames(interp, oid).contains(column)
            } else {
                interp.attribute_by_name(oid, column).is_some() || system
            };
            if !exists {
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
            // get_object_address_relobject: table_openrv.
            let relname = rel.last().cloned().unwrap_or_default();
            super::util::table_open_check(interp, oid, &relname)?;
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
            if let ast::Node::TypeName(tn) = object {
                super::util::lookup_type_name(tn, interp)?;
                if objtype == ObjectType::ObjectDomain {
                    super::types::check_is_domain(interp, &tn.names)?;
                }
            }
        }
        ObjectType::ObjectSchema => {
            if let ast::Node::String(s) = object
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
            // get_object_address: LookupFuncWithArgs.
            if let ast::Node::ObjectWithArgs(owa) = object {
                super::functions::lookup_func_with_args(interp, objtype, owa, false)?;
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
            if let ast::Node::String(s) = object {
                super::fdw::check_server(interp, &s.sval)?;
            }
        }
        ObjectType::ObjectStatisticExt => {
            if let ast::Node::List(l) = object
                && !super::statistics::statistics_exist(interp, &l.items)
            {
                return Err(super::statistics::not_found(interp, &l.items));
            }
        }
        ObjectType::ObjectCollation => {
            super::collations::lookup_collation(interp, Some(object), false)?;
        }
        ObjectType::ObjectTrigger | ObjectType::ObjectPolicy | ObjectType::ObjectRule => {
            let parts = names(object);
            let Some((sub, rel)) = parts.split_last() else {
                return Ok(());
            };
            let oid = relation(rel)?;
            let relname = rel.last().cloned().unwrap_or_default();
            // get_object_address_relobject: table_openrv.
            super::util::table_open_check(interp, oid, &relname)?;
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
            // LookupOperWithArgs.
            super::drop::lookup_operator(interp, Some(object))?;
        }
        ObjectType::ObjectCast => {
            let ast::Node::List(l) = object else {
                return Ok(());
            };
            let [Some(ast::Node::TypeName(source)), Some(target)] = [
                l.items.first().and_then(|n| n.as_ref()),
                l.items.get(1).and_then(|n| n.as_ref()),
            ] else {
                return Ok(());
            };
            let ast::Node::TypeName(target) = target else {
                return Ok(());
            };
            let source = super::util::lookup_type_name(source, interp)?;
            let target = super::util::lookup_type_name(target, interp)?;
            if interp.catalog_cast(source, target).is_none() {
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
            if let ast::Node::String(s) = object
                && !super::opclass::am_exists(interp, &s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "access method \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectTablespace => {
            if let ast::Node::String(s) = object
                && !super::cluster::tablespace_exists(interp, &s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "tablespace \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectFdw => {
            if let ast::Node::String(s) = object {
                super::fdw::check_fdw(interp, &s.sval)?;
            }
        }
        ObjectType::ObjectLargeobject => {
            let oid = match object {
                ast::Node::Integer(i) => u32::try_from(i.ival).ok(),
                ast::Node::Float(f) => f.fval.parse().ok(),
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
            super::languages::find_transform(interp, Some(object))?;
        }
        ObjectType::ObjectConversion => {
            let parts = names(object);
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            if !super::conversions::conversion_exists(interp, &parts) {
                return Err(super::conversions::not_found(interp, &parts));
            }
        }
        ObjectType::ObjectEventTrigger => {
            if let ast::Node::String(s) = object
                && !interp.event_triggers.iter().any(|(t, _)| *t == s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "event trigger \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectSubscription => {
            if let ast::Node::String(s) = object {
                super::cluster::alter_subscription(interp, &s.sval)?;
            }
        }
        ObjectType::ObjectPublication => {
            if let ast::Node::String(s) = object
                && !super::publications::publication_exists(interp, &s.sval)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "publication \"{}\" does not exist",
                    s.sval
                )));
            }
        }
        ObjectType::ObjectLanguage => {
            if let ast::Node::String(s) = object {
                super::languages::check(interp, &s.sval)?;
            }
        }
        _ => {}
    }
    Ok(())
}
