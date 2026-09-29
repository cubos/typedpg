//! Text search configurations, dictionaries, parsers and templates
//! (tsearchcmds.c): created, altered and dropped by name, each naming the
//! others it's built from. Also the lookups behind `regconfig` /
//! `regdictionary` input.

use typedpg_pg_query::protobuf::{AlterTsConfigurationStmt, DefineStmt, ObjectType, node};

use std::collections::HashMap;

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

/// What the catalog keeps about the text search objects migrations
/// create, beyond their names: a configuration's parser, a dictionary's
/// template and options (`pg_ts_config.cfgparser`, `pg_ts_dict`).
#[derive(Clone, Debug, Default)]
pub(crate) struct TsDefinitions {
    /// Whether a configuration's parser is the built-in `default` one, by
    /// `(namespace, name)`. The seeded configurations all use it.
    configs: HashMap<(PgNamespaceOid, String), bool>,
    /// A dictionary's template (as `(namespace, name)`) and options.
    dicts: HashMap<(PgNamespaceOid, String), DictDefinition>,
}

#[derive(Clone, Debug)]
struct DictDefinition {
    template: (PgNamespaceOid, String),
    options: Vec<(String, Option<String>)>,
}

/// The token types of the built-in `default` parser (wparser_def.c
/// `tok_alias`).
const DEFAULT_PARSER_TOKENS: &[&str] = &[
    "asciiword",
    "word",
    "numword",
    "email",
    "url",
    "host",
    "sfloat",
    "version",
    "hword_numpart",
    "hword_part",
    "hword_asciipart",
    "blank",
    "tag",
    "protocol",
    "numhword",
    "asciihword",
    "hword",
    "url_path",
    "file",
    "float",
    "int",
    "uint",
    "entity",
];

/// The files a stock PostgreSQL 18 server has in `tsearch_data`, as
/// `(basename, extension)`. Nothing tells the analyzer about files added to
/// a server, so it assumes the stock set.
const TSEARCH_DATA_FILES: &[(&str, &str)] = &[
    ("danish", "stop"),
    ("dutch", "stop"),
    ("english", "stop"),
    ("finnish", "stop"),
    ("french", "stop"),
    ("german", "stop"),
    ("hungarian", "stop"),
    ("italian", "stop"),
    ("nepali", "stop"),
    ("norwegian", "stop"),
    ("portuguese", "stop"),
    ("russian", "stop"),
    ("spanish", "stop"),
    ("swedish", "stop"),
    ("turkish", "stop"),
    ("hunspell_sample", "affix"),
    ("hunspell_sample_long", "affix"),
    ("hunspell_sample_long", "dict"),
    ("hunspell_sample_num", "affix"),
    ("hunspell_sample_num", "dict"),
    ("ispell_sample", "affix"),
    ("ispell_sample", "dict"),
    ("synonym_sample", "syn"),
    ("thesaurus_sample", "ths"),
];

/// The server's `tsearch_data` directory (get_share_path), as the stock
/// Debian / PGDG packages and the official image install it.
const TSEARCH_DATA_DIR: &str = "/usr/share/postgresql/18/tsearch_data";

/// The Snowball stemmers for a UTF8 database (dict_snowball.c
/// `stemmer_modules`).
const SNOWBALL_LANGUAGES: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "catalan",
    "danish",
    "dutch",
    "english",
    "estonian",
    "finnish",
    "french",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "lithuanian",
    "nepali",
    "norwegian",
    "porter",
    "portuguese",
    "romanian",
    "russian",
    "serbian",
    "spanish",
    "swedish",
    "tamil",
    "turkish",
    "yiddish",
];

/// defGetString of a definition option's value; `None` without one.
fn option_value(de: &typedpg_pg_query::protobuf::DefElem) -> Option<String> {
    match de.arg.as_deref().and_then(|a| a.node.as_ref())? {
        node::Node::String(s) => Some(s.sval.clone()),
        node::Node::Integer(i) => Some(i.ival.to_string()),
        node::Node::Float(f) => Some(f.fval.clone()),
        node::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.to_owned()),
        node::Node::TypeName(tn) => Some(
            tn.names
                .iter()
                .filter_map(node_string)
                .collect::<Vec<_>>()
                .join("."),
        ),
        node::Node::List(l) => Some(
            l.items
                .iter()
                .filter_map(node_string)
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

/// get_tsearch_config_filename + opening the file: the base name is
/// limited to `[a-z0-9_]`, and the file must be in `tsearch_data`.
fn check_file(basename: &str, extension: &str, what: &str) -> Result<(), DdlError> {
    if !basename
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(DdlError::Parse(format!(
            "invalid text search configuration file name \"{basename}\""
        )));
    }
    if !TSEARCH_DATA_FILES.contains(&(basename, extension)) {
        return Err(DdlError::Parse(format!(
            "could not open {what} file \"{TSEARCH_DATA_DIR}/{basename}.{extension}\": No such \
             file or directory"
        )));
    }
    Ok(())
}

/// defGetBoolean.
fn check_boolean(name: &str, value: Option<&str>) -> Result<(), DdlError> {
    let ok = match value {
        None => true,
        Some(v) => matches!(
            v.to_ascii_lowercase().as_str(),
            "true" | "false" | "on" | "off" | "1" | "0"
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(DdlError::Parse(format!("{name} requires a Boolean value")))
    }
}

/// verify_dictoptions: the template's init method checks the options — for
/// the built-in templates, dsimple_init, dsnowball_init, dispell_init,
/// dsynonym_init and thesaurus_init (their files included; a thesaurus'
/// entries aren't checked against its subdictionary).
fn verify_dict_options(
    interp: &PgCatalog,
    template: &(PgNamespaceOid, String),
    options: &[(String, Option<String>)],
) -> Result<(), DdlError> {
    if interp.namespace_name(template.0) != Some("pg_catalog") {
        return Ok(());
    }
    let invalid = |msg: String| Err(DdlError::Parse(msg));
    let string = |value: &Option<String>, name: &str| {
        value
            .clone()
            .ok_or_else(|| DdlError::Parse(format!("{name} requires a parameter")))
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut once = |name: &'static str, label: &str| -> Result<(), DdlError> {
        if seen.contains(&name) {
            return Err(DdlError::Parse(format!("multiple {label} parameters")));
        }
        seen.push(name);
        Ok(())
    };
    match template.1.as_str() {
        "simple" => {
            for (name, value) in options {
                match name.as_str() {
                    "stopwords" => {
                        once("stopwords", "StopWords")?;
                        check_file(&string(value, name)?, "stop", "stop-word")?;
                    }
                    "accept" => {
                        once("accept", "Accept")?;
                        check_boolean(name, value.as_deref())?;
                    }
                    _ => {
                        return invalid(format!(
                            "unrecognized simple dictionary parameter: \"{name}\""
                        ));
                    }
                }
            }
        }
        "snowball" => {
            let mut language = None;
            for (name, value) in options {
                match name.as_str() {
                    "stopwords" => {
                        once("stopwords", "StopWords")?;
                        check_file(&string(value, name)?, "stop", "stop-word")?;
                    }
                    "language" => {
                        once("language", "Language")?;
                        language = Some(string(value, name)?);
                    }
                    _ => return invalid(format!("unrecognized Snowball parameter: \"{name}\"")),
                }
            }
            let Some(language) = language else {
                return invalid("missing Language parameter".into());
            };
            if !SNOWBALL_LANGUAGES
                .iter()
                .any(|l| l.eq_ignore_ascii_case(&language))
            {
                return Err(DdlError::TypeNotFound(format!(
                    "no Snowball stemmer available for language \"{language}\" and encoding \
                     \"UTF8\""
                )));
            }
        }
        "ispell" => {
            let (mut dict, mut aff) = (false, false);
            for (name, value) in options {
                match name.as_str() {
                    "dictfile" => {
                        once("dictfile", "DictFile")?;
                        check_file(&string(value, name)?, "dict", "dictionary")?;
                        dict = true;
                    }
                    "afffile" => {
                        once("afffile", "AffFile")?;
                        check_file(&string(value, name)?, "affix", "affix")?;
                        aff = true;
                    }
                    "stopwords" => {
                        once("stopwords", "StopWords")?;
                        check_file(&string(value, name)?, "stop", "stop-word")?;
                    }
                    _ => return invalid(format!("unrecognized Ispell parameter: \"{name}\"")),
                }
            }
            if !aff {
                return invalid("missing AffFile parameter".into());
            }
            if !dict {
                return invalid("missing DictFile parameter".into());
            }
        }
        "synonym" => {
            let mut file = None;
            for (name, value) in options {
                match name.as_str() {
                    "synonyms" => file = Some(string(value, name)?),
                    "casesensitive" => check_boolean(name, value.as_deref())?,
                    _ => return invalid(format!("unrecognized synonym parameter: \"{name}\"")),
                }
            }
            let Some(file) = file else {
                return invalid("missing Synonyms parameter".into());
            };
            check_file(&file, "syn", "synonym")?;
        }
        "thesaurus" => {
            let (mut file, mut subdict) = (None, None);
            for (name, value) in options {
                match name.as_str() {
                    "dictfile" => {
                        once("dictfile", "DictFile")?;
                        file = Some(string(value, name)?);
                    }
                    "dictionary" => {
                        once("dictionary", "Dictionary")?;
                        subdict = Some(string(value, name)?);
                    }
                    _ => return invalid(format!("unrecognized Thesaurus parameter: \"{name}\"")),
                }
            }
            let Some(file) = file else {
                return invalid("missing DictFile parameter".into());
            };
            let Some(subdict) = subdict else {
                return invalid("missing Dictionary parameter".into());
            };
            check_file(&file, "ths", "thesaurus")?;
            let names: Vec<&str> = subdict.split('.').collect();
            find(interp, "d", &names)?;
        }
        _ => {}
    }
    Ok(())
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
    // A configuration's parser (named, or the copied configuration's), a
    // dictionary's template and options.
    let mut default_parser = None;
    let mut template = None;
    let mut dict_options = Vec::new();
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let referenced = match (kind, de.defname.as_str()) {
            ("c", "parser") => "p",
            ("c", "copy") => "c",
            ("c", other) => {
                return Err(DdlError::Parse(format!(
                    "text search configuration parameter \"{other}\" not recognized"
                )));
            }
            ("d", "template") => "t",
            ("d", _) => {
                dict_options.push((de.defname.clone(), option_value(de)));
                continue;
            }
            _ => continue,
        };
        let names = option_names(de);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let at = position(interp, referenced, &names)?;
        let object = &interp.pg_ts_objects[at];
        match referenced {
            "p" => {
                default_parser = Some(
                    object.name == "default"
                        && interp.namespace_name(object.namespace) == Some("pg_catalog"),
                );
            }
            "c" => default_parser = Some(config_uses_default_parser(interp, at)),
            _ => template = Some((object.namespace, object.name.clone())),
        }
    }
    if kind == "c" {
        // DefineTSConfiguration.
        let given = |name: &str| {
            stmt.definition.iter().any(
                |o| matches!(o.node.as_ref(), Some(node::Node::DefElem(de)) if de.defname == name),
            )
        };
        if given("parser") && given("copy") {
            return Err(DdlError::Parse(
                "cannot specify both PARSER and COPY options".into(),
            ));
        }
        if default_parser.is_none() {
            return Err(DdlError::Parse("text search parser is required".into()));
        }
    }
    if kind == "d" {
        // DefineTSDictionary.
        let Some(template) = template.as_ref() else {
            return Err(DdlError::Parse("text search template is required".into()));
        };
        verify_dict_options(interp, template, &dict_options)?;
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
    match (kind, default_parser, template) {
        ("c", Some(default_parser), _) => {
            interp
                .ts_definitions
                .configs
                .insert((nsoid, name), default_parser);
        }
        ("d", _, Some(template)) => {
            interp.ts_definitions.dicts.insert(
                (nsoid, name),
                DictDefinition {
                    template,
                    options: dict_options,
                },
            );
        }
        _ => {}
    }
    Ok(())
}

/// Whether configuration `pg_ts_objects[at]` uses the built-in parser —
/// the seeded ones all do.
fn config_uses_default_parser(interp: &PgCatalog, at: usize) -> bool {
    let object = &interp.pg_ts_objects[at];
    interp
        .ts_definitions
        .configs
        .get(&(object.namespace, object.name.clone()))
        .copied()
        .unwrap_or(true)
}

/// ALTER TEXT SEARCH CONFIGURATION ... ADD / ALTER / DROP MAPPING
/// (MakeConfigurationMapping / DropConfigurationMapping): the token types
/// must be the parser's (getTokenTypes), the dictionaries must exist.
pub fn alter_configuration(
    interp: &mut PgCatalog,
    stmt: &AlterTsConfigurationStmt,
) -> Result<(), DdlError> {
    let at = position(interp, "c", &names_of(&stmt.cfgname))?;
    if config_uses_default_parser(interp, at) {
        for token in stmt.tokentype.iter().filter_map(node_string) {
            if !DEFAULT_PARSER_TOKENS.contains(&token) {
                return Err(DdlError::Parse(format!(
                    "token type \"{token}\" does not exist"
                )));
            }
        }
    }
    for dict in &stmt.dicts {
        if let Some(node::Node::List(l)) = dict.node.as_ref() {
            find(interp, "d", &names_of(&l.items))?;
        }
    }
    // The configuration depends on the dictionaries it maps to.
    crate::ddl::depend::record_ts_mapping(interp, &names_of(&stmt.cfgname), &stmt.dicts)
}

/// ALTER TEXT SEARCH DICTIONARY name (...) (AlterTSDictionary): the options
/// given replace (or, without a value, remove) the dictionary's, and the
/// result goes through the template's checks again.
pub fn alter_dictionary(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterTsDictionaryStmt,
) -> Result<(), DdlError> {
    let at = position(interp, "d", &names_of(&stmt.dictname))?;
    let object = &interp.pg_ts_objects[at];
    let key = (object.namespace, object.name.clone());
    let Some(mut definition) = interp.ts_definitions.dicts.get(&key).cloned() else {
        return Ok(());
    };
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let value = option_value(de);
        let existing = definition
            .options
            .iter()
            .position(|(name, _)| *name == de.defname);
        match (existing, value) {
            (Some(i), None) => {
                definition.options.remove(i);
            }
            (Some(i), Some(value)) => definition.options[i].1 = Some(value),
            (None, None) => {}
            (None, Some(value)) => definition.options.push((de.defname.clone(), Some(value))),
        }
    }
    verify_dict_options(interp, &definition.template, &definition.options)?;
    interp.ts_definitions.dicts.insert(key, definition);
    Ok(())
}

/// DROP TEXT SEARCH CONFIGURATION / DICTIONARY / PARSER / TEMPLATE.
pub(crate) fn drop(
    interp: &mut PgCatalog,
    objtype: ObjectType,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
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
        crate::ddl::depend::drop_ts_object(interp, kind, &name, ns, cascade)?;
        interp
            .pg_ts_objects
            .retain(|o| !(o.kind == kind && o.name == name && o.namespace == ns));
    }
    Ok(())
}

impl TsDefinitions {
    /// The object `(namespace, name)` of `kind` is now `new`.
    fn rekey(&mut self, kind: &str, old: (PgNamespaceOid, String), new: (PgNamespaceOid, String)) {
        match kind {
            "c" => {
                if let Some(v) = self.configs.remove(&old) {
                    self.configs.insert(new, v);
                }
            }
            "d" => {
                if let Some(v) = self.dicts.remove(&old) {
                    self.dicts.insert(new, v);
                }
            }
            _ => {}
        }
    }
}

/// The kind letter of a text search object type.
fn kind_of(objtype: ObjectType) -> Option<&'static str> {
    match objtype {
        ObjectType::ObjectTsconfiguration => Some("c"),
        ObjectType::ObjectTsdictionary => Some("d"),
        ObjectType::ObjectTsparser => Some("p"),
        ObjectType::ObjectTstemplate => Some("t"),
        _ => None,
    }
}

/// The index in `pg_ts_objects` of the object `names` resolves to.
fn position(interp: &PgCatalog, kind: &str, names: &[&str]) -> Result<usize, DdlError> {
    find(interp, kind, names)?;
    let (schema, name) = split(names);
    interp
        .schemas_for_lookup(schema.as_deref())
        .into_iter()
        .find_map(|ns| {
            interp
                .pg_ts_objects
                .iter()
                .position(|o| o.kind == kind && o.name == name && o.namespace == ns)
        })
        .ok_or_else(|| DdlError::Internal("text search object vanished".into()))
}

/// AlterObjectRename_internal / AlterObjectNamespace_internal: the name
/// must be free in the target schema (report_namespace_conflict).
fn check_free(
    interp: &PgCatalog,
    kind: &str,
    name: &str,
    ns: PgNamespaceOid,
) -> Result<(), DdlError> {
    if interp
        .pg_ts_objects
        .iter()
        .any(|o| o.kind == kind && o.name == name && o.namespace == ns)
    {
        return Err(DdlError::DuplicateObject(format!(
            "{} \"{name}\" already exists in schema \"{}\"",
            what(kind),
            interp.namespace_name(ns).unwrap_or_default()
        )));
    }
    Ok(())
}

/// `ALTER TEXT SEARCH { CONFIGURATION | DICTIONARY | PARSER | TEMPLATE }
/// name RENAME TO new`.
pub(crate) fn rename(
    interp: &mut PgCatalog,
    objtype: ObjectType,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let (Some(kind), Some(node::Node::List(l))) = (
        kind_of(objtype),
        stmt.object.as_deref().and_then(|o| o.node.as_ref()),
    ) else {
        return Ok(());
    };
    let at = position(interp, kind, &names_of(&l.items))?;
    let ns = interp.pg_ts_objects[at].namespace;
    check_free(interp, kind, &stmt.newname, ns)?;
    let old = std::mem::replace(&mut interp.pg_ts_objects[at].name, stmt.newname.clone());
    interp
        .ts_definitions
        .rekey(kind, (ns, old), (ns, stmt.newname.clone()));
    Ok(())
}

/// `ALTER TEXT SEARCH ... name SET SCHEMA s`.
pub(crate) fn set_schema(
    interp: &mut PgCatalog,
    objtype: ObjectType,
    stmt: &typedpg_pg_query::protobuf::AlterObjectSchemaStmt,
    new_ns: PgNamespaceOid,
) -> Result<(), DdlError> {
    let (Some(kind), Some(node::Node::List(l))) = (
        kind_of(objtype),
        stmt.object.as_deref().and_then(|o| o.node.as_ref()),
    ) else {
        return Ok(());
    };
    let names = names_of(&l.items);
    let at = match position(interp, kind, &names) {
        Ok(at) => at,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    if interp.pg_ts_objects[at].namespace == new_ns {
        return Ok(());
    }
    let name = interp.pg_ts_objects[at].name.clone();
    check_free(interp, kind, &name, new_ns)?;
    let old_ns = std::mem::replace(&mut interp.pg_ts_objects[at].namespace, new_ns);
    interp
        .ts_definitions
        .rekey(kind, (old_ns, name.clone()), (new_ns, name));
    Ok(())
}
