//! Access methods, operator families and operator classes: CREATE / DROP
//! ACCESS METHOD, OPERATOR FAMILY, OPERATOR CLASS, and the opclass
//! resolution of index columns (ResolveOpClass / GetDefaultOpClass,
//! indexcmds.c).

use typedpg_pg_query::protobuf::{
    AlterOpFamilyStmt, CreateAmStmt, CreateOpClassItem, CreateOpClassStmt, CreateOpFamilyStmt, node,
};

use super::DdlError;
use super::util::node_string;
use crate::oid::{PgNamespaceOid, PgOpclassOid, PgOperatorOid, PgTypeOid};
use crate::pg_catalog::{PgAm, PgAmop, PgCatalog, PgOpclass, PgOpfamily, TypCategory, TypType};

/// What an index access method supports (the `IndexAmRoutine` flags of
/// the built-in AMs). Unknown AMs (from extensions) are not checked.
pub(crate) struct AmCaps {
    pub(crate) can_unique: bool,
    pub(crate) can_multicol: bool,
    pub(crate) can_include: bool,
    pub(crate) can_order: bool,
    /// `amclusterable`.
    pub(crate) can_cluster: bool,
    /// `amgettuple != NULL`: what an exclusion constraint checks through.
    pub(crate) has_gettuple: bool,
}

pub(crate) fn am_caps(amname: &str) -> Option<AmCaps> {
    let caps =
        |can_unique, can_multicol, can_include, can_order, can_cluster, has_gettuple| AmCaps {
            can_unique,
            can_multicol,
            can_include,
            can_order,
            can_cluster,
            has_gettuple,
        };
    Some(match amname {
        "btree" => caps(true, true, true, true, true, true),
        "hash" => caps(false, false, false, false, false, true),
        "gist" => caps(false, true, true, false, true, true),
        "gin" => caps(false, true, false, false, false, false),
        "brin" => caps(false, true, false, false, false, false),
        "spgist" => caps(false, false, true, false, false, true),
        _ => return None,
    })
}

/// The `pg_opclass` row of `oid`.
pub(crate) fn opclass_by_oid(interp: &PgCatalog, oid: PgOpclassOid) -> Option<&PgOpclass> {
    interp.pg_opclass.iter().find(|c| c.oid == oid)
}

/// get_op_opfamily_strategy: whether `opr` is a search operator of
/// `opclass`'s operator family.
pub(crate) fn opfamily_has_operator(
    interp: &PgCatalog,
    opclass: &PgOpclass,
    opr: PgOperatorOid,
) -> bool {
    interp.pg_amop.iter().any(|o| {
        o.amopopr == opr
            && o.amopfamily == opclass.opcfamily
            && o.amopfamilynamespace == opclass.opcfamilynamespace
            && o.amopmethod == opclass.opcmethod
    })
}

/// get_opfamily_member: the operator of `opclass`'s family with strategy
/// `strategy` over (`opcintype`, `opcintype`).
pub(crate) fn opfamily_member(
    interp: &PgCatalog,
    opclass: &PgOpclass,
    strategy: i16,
) -> Option<PgOperatorOid> {
    interp
        .pg_amop
        .iter()
        .find(|o| {
            o.amopfamily == opclass.opcfamily
                && o.amopfamilynamespace == opclass.opcfamilynamespace
                && o.amopmethod == opclass.opcmethod
                && o.amopstrategy == strategy
                && o.amoplefttype == opclass.opcintype
                && o.amoprighttype == opclass.opcintype
        })
        .map(|o| o.amopopr)
}

/// format_operator: `name(left,right)`, the name schema-qualified when its
/// schema isn't on the search path.
pub(crate) fn format_operator(interp: &PgCatalog, opr: PgOperatorOid) -> String {
    let Some(op) = interp.pg_operator.get(&opr) else {
        return opr.to_string();
    };
    let name = if interp.schemas_for_lookup(None).contains(&op.oprnamespace) {
        op.oprname.clone()
    } else {
        // quote_identifier(nspname) — QualifiedName's quoting of the schema
        // part; the operator name itself is never quoted.
        let schema = interp.namespace_name(op.oprnamespace).unwrap_or_default();
        let quoted = crate::qualified_name::QualifiedName::new(schema, "x").to_string();
        format!("{}.{}", &quoted[..quoted.len() - 2], op.oprname)
    };
    let left = op.oprleft.map_or_else(
        || "NONE".to_owned(),
        |t| super::util::format_type_for_message(interp, t),
    );
    let right = super::util::format_type_for_message(interp, op.oprright);
    format!("{name}({left},{right})")
}

/// format_opfamily: an operator family's name, schema-qualified when its
/// schema isn't on the search path.
pub(crate) fn format_opfamily(interp: &PgCatalog, opclass: &PgOpclass) -> String {
    if interp
        .schemas_for_lookup(None)
        .contains(&opclass.opcfamilynamespace)
    {
        return opclass.opcfamily.clone();
    }
    let schema = interp
        .namespace_name(opclass.opcfamilynamespace)
        .unwrap_or_default();
    crate::qualified_name::QualifiedName::new(schema, &opclass.opcfamily).to_string()
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
fn split_name(names: &[typedpg_pg_query::protobuf::Node]) -> (Option<String>, String) {
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
        .map(|p| (p.oid, p.prorettype));
    let Some((handler_oid, rettype)) = found else {
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
        amhandler: Some(handler_oid),
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
    let family = if !stmt.opfamilyname.is_empty() {
        let (fschema, fname) = split_name(&stmt.opfamilyname);
        match find_opfamily(interp, fschema.as_deref(), &fname, &am) {
            Some(f) => (f.opfname.clone(), f.opfnamespace),
            None => {
                return Err(DdlError::TypeNotFound(format!(
                    "operator family \"{fname}\" does not exist for access method \"{am}\""
                )));
            }
        }
    } else {
        (name.clone(), nsoid)
    };
    if stmt.opfamilyname.is_empty()
        && !interp
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
    let (family_name, family_ns) = family;
    for item in &stmt.items {
        if let Some(node::Node::CreateOpClassItem(item)) = item.node.as_ref() {
            add_family_operator(interp, &family_name, family_ns, &am, item, Some(intype))?;
        }
    }
    let oid = PgOpclassOid::from_nonzero(interp.alloc_oid()?);
    interp.pg_opclass.push(PgOpclass {
        oid,
        opcname: name,
        opcnamespace: nsoid,
        opcmethod: am,
        opcintype: intype,
        opcdefault: stmt.is_default,
        opcfamily: family_name,
        opcfamilynamespace: family_ns,
    });
    Ok(())
}

/// DefineOpClass / AlterOpFamilyAdd: an `OPERATOR n name [(left, right)]`
/// item becomes a search member (`pg_amop`) of the family — its operand
/// types default to the opclass's input type. `FOR ORDER BY` operators and
/// FUNCTION / STORAGE items are not recorded; an operator that doesn't
/// resolve is skipped.
fn add_family_operator(
    interp: &mut PgCatalog,
    family: &str,
    family_ns: PgNamespaceOid,
    am: &str,
    item: &CreateOpClassItem,
    intype: Option<PgTypeOid>,
) -> Result<(), DdlError> {
    // OPCLASS_ITEM_OPERATOR, not FOR ORDER BY.
    if item.itemtype != 1 || !item.order_family.is_empty() {
        return Ok(());
    }
    let Some(owa) = item.name.as_ref() else {
        return Ok(());
    };
    let (left, right) = match owa.objargs.as_slice() {
        [l, r] => {
            let resolve = |n: &typedpg_pg_query::protobuf::Node| match n.node.as_ref() {
                Some(node::Node::TypeName(tn)) => super::util::resolve_type_name(tn, interp),
                _ => None,
            };
            (resolve(l), resolve(r))
        }
        [] => (intype, intype),
        _ => return Ok(()),
    };
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(());
    };
    let (schema, name) = split_name(&owa.objname);
    let Some(opr) = interp
        .schemas_for_lookup(schema.as_deref())
        .into_iter()
        .find_map(|ns| {
            interp.pg_operator.values().find(|o| {
                o.oprnamespace == ns
                    && o.oprname == name
                    && o.oprleft == Some(left)
                    && o.oprright == right
            })
        })
        .map(|o| o.oid)
    else {
        return Ok(());
    };
    interp.pg_amop.push(PgAmop {
        amopfamily: family.to_owned(),
        amopfamilynamespace: family_ns,
        amopmethod: am.to_owned(),
        amoplefttype: left,
        amoprighttype: right,
        amopstrategy: i16::try_from(item.number).unwrap_or_default(),
        amopopr: opr,
    });
    Ok(())
}

/// ALTER OPERATOR FAMILY name USING am ADD | DROP ... (AlterOpFamily): the
/// family's search operators follow the OPERATOR items.
pub fn alter_opfamily(interp: &mut PgCatalog, stmt: &AlterOpFamilyStmt) -> Result<(), DdlError> {
    am_must_exist(interp, &stmt.amname)?;
    let (schema, name) = split_name(&stmt.opfamilyname);
    let Some((family, family_ns)) = find_opfamily(interp, schema.as_deref(), &name, &stmt.amname)
        .map(|f| (f.opfname.clone(), f.opfnamespace))
    else {
        return Err(DdlError::TypeNotFound(format!(
            "operator family \"{name}\" does not exist for access method \"{}\"",
            stmt.amname
        )));
    };
    for item in &stmt.items {
        let Some(node::Node::CreateOpClassItem(item)) = item.node.as_ref() else {
            continue;
        };
        if !stmt.is_drop {
            add_family_operator(interp, &family, family_ns, &stmt.amname, item, None)?;
            continue;
        }
        // AlterOpFamilyDrop: `OPERATOR n (left, right)`.
        if item.itemtype != 1 {
            continue;
        }
        let types: Vec<Option<PgTypeOid>> = item
            .class_args
            .iter()
            .map(|n| match n.node.as_ref() {
                Some(node::Node::TypeName(tn)) => super::util::resolve_type_name(tn, interp),
                _ => None,
            })
            .collect();
        if let [Some(left), Some(right)] = types.as_slice() {
            let strategy = i16::try_from(item.number).unwrap_or_default();
            interp.pg_amop.retain(|o| {
                !(o.amopfamily == family
                    && o.amopfamilynamespace == family_ns
                    && o.amopmethod == stmt.amname
                    && o.amopstrategy == strategy
                    && o.amoplefttype == *left
                    && o.amoprighttype == *right)
            });
        }
    }
    Ok(())
}

/// DROP ACCESS METHOD / OPERATOR CLASS / OPERATOR FAMILY.
pub(crate) fn drop_am_object(
    interp: &mut PgCatalog,
    objtype: typedpg_pg_query::protobuf::ObjectType,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    use typedpg_pg_query::protobuf::ObjectType;
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
                    interp.pg_amop.retain(|o| {
                        !(o.amopfamilynamespace == ns && o.amopfamily == n && o.amopmethod == am)
                    });
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

/// ResolveOpClass for one index column of type `typ`: the operator class
/// named (which must accept the type), else the type's default one. `None`
/// for an unknown-typed column.
pub(crate) fn resolve_index_opclass(
    interp: &PgCatalog,
    opclass: &[typedpg_pg_query::protobuf::Node],
    typ: PgTypeOid,
    am: &str,
) -> Result<Option<PgOpclassOid>, DdlError> {
    if typ == crate::pg_catalog::oid::UNKNOWN {
        return Ok(None);
    }
    let typname = || super::util::format_type_for_message(interp, typ);
    if opclass.is_empty() {
        let Some(found) = default_opclass(interp, typ, am)? else {
            return Err(DdlError::TypeNotFound(format!(
                "data type {} has no default operator class for access method \"{am}\" (You \
                 must specify an operator class for the index or define a default operator \
                 class for the data type.)",
                typname()
            )));
        };
        return Ok(Some(found.oid));
    }
    let (schema, name) = split_name(opclass);
    let Some(found) = find_opclass(interp, schema.as_deref(), &name, am) else {
        return Err(DdlError::TypeNotFound(format!(
            "operator class \"{name}\" does not exist for access method \"{am}\""
        )));
    };
    check_opclass_accepts(interp, found, typ)?;
    Ok(Some(found.oid))
}

/// ResolveOpClass: an explicitly named operator class must accept the
/// column's type (binary-coercibly).
pub(crate) fn check_opclass_accepts(
    interp: &PgCatalog,
    opclass: &PgOpclass,
    typ: PgTypeOid,
) -> Result<(), DdlError> {
    if !binary_coercible(interp, interp.unwrap_domain(typ), opclass.opcintype)
        && !binary_coercible(interp, typ, opclass.opcintype)
    {
        return Err(DdlError::Parse(format!(
            "operator class \"{}\" does not accept data type {}",
            opclass.opcname,
            super::util::format_type_for_message(interp, typ)
        )));
    }
    Ok(())
}

/// GetDefaultOpClass as an identity: the default operator class of `am`
/// for `typ`, if exactly one.
pub(crate) fn default_opclass_id(
    interp: &PgCatalog,
    typ: PgTypeOid,
    am: &str,
) -> Option<PgOpclassOid> {
    default_opclass(interp, typ, am)
        .ok()
        .flatten()
        .map(|c| c.oid)
}

/// The declared input type (`opcintype`) of `typ`'s default operator class
/// for `am`, if it has one.
pub(crate) fn default_opclass_intype(
    interp: &PgCatalog,
    typ: PgTypeOid,
    am: &str,
) -> Option<PgTypeOid> {
    default_opclass(interp, typ, am)
        .ok()
        .flatten()
        .map(|c| c.opcintype)
}

/// Whether `typ` has a default btree operator class (CreateStatistics).
pub(crate) fn has_default_btree_opclass(interp: &PgCatalog, typ: PgTypeOid) -> bool {
    matches!(default_opclass(interp, typ, "btree"), Ok(Some(_)))
}

/// get_table_am_oid: `name` must be a table access method.
pub(crate) fn check_table_am(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    match interp.pg_am.iter().find(|a| a.amname == name) {
        None => Err(DdlError::TypeNotFound(format!(
            "access method \"{name}\" does not exist"
        ))),
        Some(am) if am.amtype != "t" => Err(DdlError::UnsupportedDdl(format!(
            "access method \"{name}\" is not of type TABLE"
        ))),
        Some(_) => Ok(()),
    }
}
