//! Access methods, operator families and operator classes: CREATE / DROP
//! ACCESS METHOD, OPERATOR FAMILY, OPERATOR CLASS, and the opclass
//! resolution of index columns (ResolveOpClass / GetDefaultOpClass,
//! indexcmds.c).

use pg_query::protobuf::{CreateAmStmt, CreateOpClassStmt, CreateOpFamilyStmt, node};

use super::DdlError;
use super::util::node_string;
use crate::oid::{PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{PgAm, PgCatalog, PgOpclass, PgOpfamily, TypCategory, TypType};

/// What an index access method supports (the `IndexAmRoutine` flags of
/// the built-in AMs). Unknown AMs (from extensions) are not checked.
pub(crate) struct AmCaps {
    pub(crate) can_unique: bool,
    pub(crate) can_multicol: bool,
    pub(crate) can_include: bool,
    pub(crate) can_order: bool,
}

pub(crate) fn am_caps(amname: &str) -> Option<AmCaps> {
    let caps = |can_unique, can_multicol, can_include, can_order| AmCaps {
        can_unique,
        can_multicol,
        can_include,
        can_order,
    };
    Some(match amname {
        "btree" => caps(true, true, true, true),
        "hash" => caps(false, false, false, false),
        "gist" => caps(false, true, true, false),
        "gin" => caps(false, true, false, false),
        "brin" => caps(false, true, false, false),
        "spgist" => caps(false, false, true, false),
        _ => return None,
    })
}

pub(crate) fn am_exists(interp: &PgCatalog, amname: &str) -> bool {
    interp.pg_am.iter().any(|a| a.amname == amname)
}

fn am_must_exist(interp: &PgCatalog, amname: &str) -> Result<(), DdlError> {
    if am_exists(interp, amname) {
        return Ok(());
    }
    Err(DdlError::TypeNotFound(format!(
        "access method \"{amname}\" does not exist"
    )))
}

/// `(schema, name)` of a possibly qualified name list.
fn split_name(names: &[pg_query::protobuf::Node]) -> (Option<String>, String) {
    let parts: Vec<&str> = names.iter().filter_map(node_string).collect();
    match parts.as_slice() {
        [schema, name] => (Some((*schema).to_owned()), (*name).to_owned()),
        [.., name] => (None, (*name).to_owned()),
        [] => (None, String::new()),
    }
}

/// The namespaces an unqualified opclass / opfamily name is looked up in.
fn lookup_namespaces(interp: &PgCatalog, schema: Option<&str>) -> Vec<PgNamespaceOid> {
    interp.schemas_for_lookup(schema)
}

fn find_opfamily<'a>(
    interp: &'a PgCatalog,
    schema: Option<&str>,
    name: &str,
    am: &str,
) -> Option<&'a PgOpfamily> {
    lookup_namespaces(interp, schema)
        .into_iter()
        .find_map(|ns| {
            interp
                .pg_opfamily
                .iter()
                .find(|f| f.opfnamespace == ns && f.opfname == name && f.opfmethod == am)
        })
}

pub(crate) fn find_opclass<'a>(
    interp: &'a PgCatalog,
    schema: Option<&str>,
    name: &str,
    am: &str,
) -> Option<&'a PgOpclass> {
    lookup_namespaces(interp, schema)
        .into_iter()
        .find_map(|ns| {
            interp
                .pg_opclass
                .iter()
                .find(|c| c.opcnamespace == ns && c.opcname == name && c.opcmethod == am)
        })
}

/// QualifiedNameGetCreationNamespace: the named schema, or the creation
/// schema.
fn object_namespace(interp: &PgCatalog, schema: Option<&str>) -> Result<PgNamespaceOid, DdlError> {
    let schema = match schema {
        Some(s) => s.to_owned(),
        None => super::util::creation_schema(interp)?,
    };
    interp
        .namespace_oid(&schema)
        .ok_or_else(|| DdlError::TableNotFound(format!("schema \"{schema}\" does not exist")))
}

/// CREATE ACCESS METHOD name TYPE INDEX | TABLE HANDLER fn
/// (CreateAccessMethod, lookup_am_handler_func).
pub fn create_am(interp: &mut PgCatalog, stmt: &CreateAmStmt) -> Result<(), DdlError> {
    if am_exists(interp, &stmt.amname) {
        return Err(DdlError::DuplicateObject(format!(
            "access method \"{}\" already exists",
            stmt.amname
        )));
    }
    let (schema, handler) = split_name(&stmt.handler_name);
    // `internal`.
    let internal = PgTypeOid::from_raw(2281);
    let found = interp
        .find_functions(schema.as_deref(), &handler)
        .into_iter()
        .find(|p| p.proargtypes == [internal])
        .map(|p| p.prorettype);
    let Some(rettype) = found else {
        return Err(DdlError::TypeNotFound(format!(
            "function {handler}(internal) does not exist"
        )));
    };
    let expected = if stmt.amtype == "t" {
        "table_am_handler"
    } else {
        "index_am_handler"
    };
    if interp.pg_type.get(&rettype).map(|t| t.typname.as_str()) != Some(expected) {
        return Err(DdlError::Parse(format!(
            "function {handler} must return type {expected}"
        )));
    }
    interp.pg_am.push(PgAm {
        amname: stmt.amname.clone(),
        amtype: stmt.amtype.clone(),
    });
    Ok(())
}

/// CREATE OPERATOR FAMILY name USING am (CreateOpFamily).
pub fn create_opfamily(interp: &mut PgCatalog, stmt: &CreateOpFamilyStmt) -> Result<(), DdlError> {
    am_must_exist(interp, &stmt.amname)?;
    let (schema, name) = split_name(&stmt.opfamilyname);
    let nsoid = object_namespace(interp, schema.as_deref())?;
    if interp
        .pg_opfamily
        .iter()
        .any(|f| f.opfnamespace == nsoid && f.opfname == name && f.opfmethod == stmt.amname)
    {
        return Err(DdlError::DuplicateObject(format!(
            "operator family \"{name}\" for access method \"{}\" already exists",
            stmt.amname
        )));
    }
    interp.pg_opfamily.push(PgOpfamily {
        opfname: name,
        opfnamespace: nsoid,
        opfmethod: stmt.amname.clone(),
    });
    Ok(())
}

/// CREATE OPERATOR CLASS name [DEFAULT] FOR TYPE t USING am [FAMILY f]
/// (DefineOpClass). Without FAMILY, a same-named family is used or created.
pub fn create_opclass(interp: &mut PgCatalog, stmt: &CreateOpClassStmt) -> Result<(), DdlError> {
    am_must_exist(interp, &stmt.amname)?;
    let am = stmt.amname.clone();
    let intype = match stmt.datatype.as_ref() {
        Some(tn) => super::util::lookup_type_name(tn, interp)?,
        None => return Ok(()),
    };
    let (schema, name) = split_name(&stmt.opclassname);
    let nsoid = object_namespace(interp, schema.as_deref())?;
    if !stmt.opfamilyname.is_empty() {
        let (fschema, fname) = split_name(&stmt.opfamilyname);
        if find_opfamily(interp, fschema.as_deref(), &fname, &am).is_none() {
            return Err(DdlError::TypeNotFound(format!(
                "operator family \"{fname}\" does not exist for access method \"{am}\""
            )));
        }
    } else if !interp
        .pg_opfamily
        .iter()
        .any(|f| f.opfnamespace == nsoid && f.opfname == name && f.opfmethod == am)
    {
        interp.pg_opfamily.push(PgOpfamily {
            opfname: name.clone(),
            opfnamespace: nsoid,
            opfmethod: am.clone(),
        });
    }
    if interp
        .pg_opclass
        .iter()
        .any(|c| c.opcnamespace == nsoid && c.opcname == name && c.opcmethod == am)
    {
        return Err(DdlError::DuplicateObject(format!(
            "operator class \"{name}\" for access method \"{am}\" already exists"
        )));
    }
    if stmt.is_default
        && let Some(existing) = interp
            .pg_opclass
            .iter()
            .find(|c| c.opcdefault && c.opcmethod == am && c.opcintype == intype)
    {
        return Err(DdlError::DuplicateObject(format!(
            "could not make operator class \"{name}\" be default for type {} (Operator class \
             \"{}\" already is the default.)",
            super::util::type_name_to_string(stmt.datatype.as_ref().unwrap_or(&Default::default())),
            existing.opcname
        )));
    }
    interp.pg_opclass.push(PgOpclass {
        opcname: name,
        opcnamespace: nsoid,
        opcmethod: am,
        opcintype: intype,
        opcdefault: stmt.is_default,
    });
    Ok(())
}

/// DROP ACCESS METHOD / OPERATOR CLASS / OPERATOR FAMILY.
pub(crate) fn drop_am_object(
    interp: &mut PgCatalog,
    objtype: pg_query::protobuf::ObjectType,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    use pg_query::protobuf::ObjectType;
    match objtype {
        ObjectType::ObjectAccessMethod => {
            let Some(name) = node_string(obj_node).map(str::to_owned) else {
                return Ok(());
            };
            let before = interp.pg_am.len();
            interp.pg_am.retain(|a| a.amname != name);
            if interp.pg_am.len() == before && !missing_ok {
                return Err(DdlError::TypeNotFound(format!(
                    "access method \"{name}\" does not exist"
                )));
            }
        }
        ObjectType::ObjectOpclass | ObjectType::ObjectOpfamily => {
            // [am, name...]
            let Some(node::Node::List(l)) = obj_node.node.as_ref() else {
                return Ok(());
            };
            let Some((am, names)) = l.items.split_first() else {
                return Ok(());
            };
            let am = node_string(am).unwrap_or_default().to_owned();
            if !am_exists(interp, &am) {
                return Err(DdlError::TypeNotFound(format!(
                    "access method \"{am}\" does not exist"
                )));
            }
            let (schema, name) = split_name(names);
            let what = if objtype == ObjectType::ObjectOpclass {
                "operator class"
            } else {
                "operator family"
            };
            let found = if objtype == ObjectType::ObjectOpclass {
                find_opclass(interp, schema.as_deref(), &name, &am)
                    .map(|c| (c.opcnamespace, c.opcname.clone()))
            } else {
                find_opfamily(interp, schema.as_deref(), &name, &am)
                    .map(|f| (f.opfnamespace, f.opfname.clone()))
            };
            match found {
                Some((ns, n)) if objtype == ObjectType::ObjectOpclass => interp
                    .pg_opclass
                    .retain(|c| !(c.opcnamespace == ns && c.opcname == n && c.opcmethod == am)),
                Some((ns, n)) => {
                    interp
                        .pg_opfamily
                        .retain(|f| !(f.opfnamespace == ns && f.opfname == n && f.opfmethod == am));
                }
                None if missing_ok => {}
                None => {
                    return Err(DdlError::TypeNotFound(format!(
                        "{what} \"{name}\" does not exist for access method \"{am}\""
                    )));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// IsBinaryCoercible, including the polymorphic opclass input types.
fn binary_coercible(interp: &PgCatalog, src: PgTypeOid, target: PgTypeOid) -> bool {
    if src == target || interp.is_binary_coercible(src, target) {
        return true;
    }
    let t = interp.pg_type.get(&src);
    let is_array = t.is_some_and(|t| t.typcategory == TypCategory::Array && t.typelem.is_some());
    let typtype = t.map(|t| t.typtype);
    match target.get() {
        2276 | 2283 => true,                                 // any, anyelement
        2277 | 5078 => is_array,                             // anyarray, anycompatiblearray
        2776 | 5079 => !is_array,                            // anynonarray, anycompatiblenonarray
        3500 => typtype == Some(TypType::Enum),              // anyenum
        3831 | 5080 => typtype == Some(TypType::Range),      // anyrange, anycompatiblerange
        4537 | 4538 => typtype == Some(TypType::Multirange), // anymultirange, anycompatiblemultirange
        2249 => typtype == Some(TypType::Composite),         // record
        2287 => {
            // record[]
            is_array
                && t.and_then(|t| t.typelem)
                    .and_then(|e| interp.pg_type.get(&e))
                    .is_some_and(|e| e.typtype == TypType::Composite)
        }
        _ => false,
    }
}

/// GetDefaultOpClass: the default opclass of `am` for `typ`.
fn default_opclass<'a>(
    interp: &'a PgCatalog,
    typ: PgTypeOid,
    am: &str,
) -> Result<Option<&'a PgOpclass>, DdlError> {
    let typ = interp.unwrap_domain(typ);
    let category = interp.pg_type.get(&typ).map(|t| t.typcategory);
    let mut exact: Vec<&PgOpclass> = Vec::new();
    let mut preferred: Vec<&PgOpclass> = Vec::new();
    let mut compatible: Vec<&PgOpclass> = Vec::new();
    for c in interp
        .pg_opclass
        .iter()
        .filter(|c| c.opcdefault && c.opcmethod == am)
    {
        if c.opcintype == typ {
            exact.push(c);
        } else if binary_coercible(interp, typ, c.opcintype) {
            let is_preferred = interp
                .pg_type
                .get(&c.opcintype)
                .is_some_and(|t| t.typispreferred && Some(t.typcategory) == category);
            if is_preferred {
                preferred.push(c);
            } else {
                compatible.push(c);
            }
        }
    }
    if exact.len() > 1 {
        return Err(DdlError::DuplicateObject(format!(
            "there are multiple default operator classes for data type {}",
            super::util::format_type_for_message(interp, typ)
        )));
    }
    Ok(
        match (
            exact.as_slice(),
            preferred.as_slice(),
            compatible.as_slice(),
        ) {
            ([one], ..) | ([], [one], _) | ([], [], [one]) => Some(*one),
            _ => None,
        },
    )
}

/// ResolveOpClass for one index column of type `typ`.
pub(crate) fn resolve_index_opclass(
    interp: &PgCatalog,
    opclass: &[pg_query::protobuf::Node],
    typ: PgTypeOid,
    am: &str,
) -> Result<(), DdlError> {
    if typ == crate::pg_catalog::oid::UNKNOWN {
        return Ok(());
    }
    let typname = || super::util::format_type_for_message(interp, typ);
    if opclass.is_empty() {
        if default_opclass(interp, typ, am)?.is_none() {
            return Err(DdlError::TypeNotFound(format!(
                "data type {} has no default operator class for access method \"{am}\" (You \
                 must specify an operator class for the index or define a default operator \
                 class for the data type.)",
                typname()
            )));
        }
        return Ok(());
    }
    let (schema, name) = split_name(opclass);
    let Some(found) = find_opclass(interp, schema.as_deref(), &name, am) else {
        return Err(DdlError::TypeNotFound(format!(
            "operator class \"{name}\" does not exist for access method \"{am}\""
        )));
    };
    if !binary_coercible(interp, interp.unwrap_domain(typ), found.opcintype)
        && !binary_coercible(interp, typ, found.opcintype)
    {
        return Err(DdlError::Parse(format!(
            "operator class \"{name}\" does not accept data type {}",
            typname()
        )));
    }
    Ok(())
}

/// Whether `typ` has a default btree operator class (CreateStatistics).
pub(crate) fn has_default_btree_opclass(interp: &PgCatalog, typ: PgTypeOid) -> bool {
    matches!(default_opclass(interp, typ, "btree"), Ok(Some(_)))
}
