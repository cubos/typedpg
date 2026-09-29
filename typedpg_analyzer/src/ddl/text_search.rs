//! Text search configurations, dictionaries, parsers and templates
//! (tsearchcmds.c): created, altered and dropped by name, each naming the
//! others it's built from. Also the lookups behind `regconfig` /
//! `regdictionary` input.

use typedpg_pg_query::protobuf::{AlterTsConfigurationStmt, DefineStmt, ObjectType, node};

use super::DdlError;
use super::util::node_string;
use crate::oid::PgNamespaceOid;
use crate::pg_catalog::{PgCatalog, PgTsObject};

fn what(kind: &str) -> &'static str {
    match kind {
        "c" => "text search configuration",
        "d" => "text search dictionary",
        "p" => "text search parser",
        _ => "text search template",
    }
}

fn split(names: &[&str]) -> (Option<String>, String) {
    match names {
        [schema, name] => (Some((*schema).to_owned()), (*name).to_owned()),
        [.., name] => (None, (*name).to_owned()),
        [] => (None, String::new()),
    }
}

/// get_ts_config_oid & co.: find a text search object, along the search
/// path when unqualified.
pub(crate) fn find(interp: &PgCatalog, kind: &str, names: &[&str]) -> Result<(), DdlError> {
    let (schema, name) = split(names);
    if let Some(s) = schema.as_deref()
        && interp.namespace_oid(s).is_none()
    {
        return Err(DdlError::TableNotFound(format!(
            "schema \"{s}\" does not exist"
        )));
    }
    let found = interp
        .schemas_for_lookup(schema.as_deref())
        .into_iter()
        .any(|ns| {
            interp
                .pg_ts_objects
                .iter()
                .any(|o| o.kind == kind && o.name == name && o.namespace == ns)
        });
    if found {
        return Ok(());
    }
    Err(DdlError::TypeNotFound(format!(
        "{} \"{}\" does not exist",
        what(kind),
        names.join(".")
    )))
}

fn names_of(nodes: &[typedpg_pg_query::protobuf::Node]) -> Vec<&str> {
    nodes.iter().filter_map(node_string).collect()
}

/// A definition option naming another text search object
/// (`PARSER = p`, `COPY = c`, `TEMPLATE = t`).
fn option_names(de: &typedpg_pg_query::protobuf::DefElem) -> Vec<String> {
    match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
        Some(node::Node::TypeName(tn)) => tn
            .names
            .iter()
            .filter_map(node_string)
            .map(str::to_owned)
            .collect(),
        Some(node::Node::List(l)) => l
            .items
            .iter()
            .filter_map(node_string)
            .map(str::to_owned)
            .collect(),
        Some(node::Node::String(s)) => vec![s.sval.clone()],
        _ => Vec::new(),
    }
}

/// CREATE TEXT SEARCH CONFIGURATION / DICTIONARY / PARSER / TEMPLATE.
pub fn define(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let kind = match ObjectType::try_from(stmt.kind) {
        Ok(ObjectType::ObjectTsconfiguration) => "c",
        Ok(ObjectType::ObjectTsdictionary) => "d",
        Ok(ObjectType::ObjectTsparser) => "p",
        Ok(ObjectType::ObjectTstemplate) => "t",
        _ => return Ok(()),
    };
    let (nsoid, name) = super::util::ensure_qualified_name(interp, &stmt.defnames)?;
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let referenced = match (kind, de.defname.as_str()) {
            ("c", "parser") => "p",
            ("c", "copy") => "c",
            ("d", "template") => "t",
            _ => continue,
        };
        let names = option_names(de);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        find(interp, referenced, &names)?;
    }
    let object = PgTsObject {
        kind: kind.to_owned(),
        name: name.clone(),
        namespace: nsoid,
    };
    if interp.pg_ts_objects.contains(&object) {
        // The catalog's unique index reports it.
        let index = match kind {
            "c" => "pg_ts_config_cfgname_index",
            "d" => "pg_ts_dict_dictname_index",
            "p" => "pg_ts_parser_prsname_index",
            _ => "pg_ts_template_tmplname_index",
        };
        return Err(DdlError::DuplicateObject(format!(
            "duplicate key value violates unique constraint \"{index}\""
        )));
    }
    interp.pg_ts_objects.push(object);
    Ok(())
}

/// ALTER TEXT SEARCH CONFIGURATION ... ADD / ALTER / DROP MAPPING.
pub fn alter_configuration(
    interp: &PgCatalog,
    stmt: &AlterTsConfigurationStmt,
) -> Result<(), DdlError> {
    find(interp, "c", &names_of(&stmt.cfgname))?;
    for dict in &stmt.dicts {
        if let Some(node::Node::List(l)) = dict.node.as_ref() {
            find(interp, "d", &names_of(&l.items))?;
        }
    }
    Ok(())
}

/// ALTER TEXT SEARCH DICTIONARY name (...).
pub fn alter_dictionary(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterTsDictionaryStmt,
) -> Result<(), DdlError> {
    find(interp, "d", &names_of(&stmt.dictname))
}

/// DROP TEXT SEARCH CONFIGURATION / DICTIONARY / PARSER / TEMPLATE.
pub(crate) fn drop(
    interp: &mut PgCatalog,
    objtype: ObjectType,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let kind = match objtype {
        ObjectType::ObjectTsconfiguration => "c",
        ObjectType::ObjectTsdictionary => "d",
        ObjectType::ObjectTsparser => "p",
        _ => "t",
    };
    let Some(node::Node::List(l)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let names = names_of(&l.items);
    match find(interp, kind, &names) {
        Ok(()) => {}
        Err(_) if missing_ok => return Ok(()),
        Err(e) => return Err(e),
    }
    let (schema, name) = split(&names);
    let namespaces: Vec<PgNamespaceOid> = interp.schemas_for_lookup(schema.as_deref());
    if let Some(ns) = namespaces.into_iter().find(|ns| {
        interp
            .pg_ts_objects
            .iter()
            .any(|o| o.kind == kind && o.name == name && o.namespace == *ns)
    }) {
        interp
            .pg_ts_objects
            .retain(|o| !(o.kind == kind && o.name == name && o.namespace == ns));
    }
    Ok(())
}

/// `regconfig` / `regdictionary` input of a bare name: an existing
/// configuration / dictionary.
pub(crate) fn check_reg_input(interp: &PgCatalog, kind: &str, name: &str) -> Result<(), String> {
    find(interp, kind, &[name]).map_err(|e| e.to_string())
}
