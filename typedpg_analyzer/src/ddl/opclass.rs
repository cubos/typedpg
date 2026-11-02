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
    // OperatorIsVisible: never one in the temporary schema.
    let name = if interp
        .schemas_for_function_lookup(None)
        .contains(&op.oprnamespace)
    {
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

pub(crate) fn find_opfamily<'a>(
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
    let oid = crate::oid::PgGenericOid::from_nonzero(interp.alloc_oid()?);
    interp.access_method_oids.insert(stmt.amname.clone(), oid);
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
    insert_opfamily(interp, name, nsoid, &stmt.amname)
}

/// A new operator family, which depends on its access method and goes
/// with it (CreateOpFamily: DEPENDENCY_AUTO) — its classes, which depend
/// on it, too.
fn insert_opfamily(
    interp: &mut PgCatalog,
    name: String,
    nsoid: crate::oid::PgNamespaceOid,
    am: &str,
) -> Result<(), DdlError> {
    let oid = crate::oid::PgGenericOid::from_nonzero(interp.alloc_oid()?);
    interp.pg_opfamily.push(PgOpfamily {
        oid,
        opfname: name,
        opfnamespace: nsoid,
        opfmethod: am.to_owned(),
    });
    // A built-in method is pinned: nothing records dependencies on it.
    let method = super::depend::named_address_of(
        interp,
        &super::depend::NamedObject::AccessMethod {
            name: am.to_owned(),
        },
    );
    super::depend::record(
        interp,
        super::depend::ObjectAddress::new(super::depend::PG_OPFAMILY_RELID, oid.into_nonzero(), 0),
        method,
        crate::pg_catalog::DepType::Auto,
    );
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
                    "operator family \"{}\" does not exist for access method \"{am}\"",
                    written(&stmt.opfamilyname)
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
        insert_opfamily(interp, name.clone(), nsoid, &am)?;
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
    let Some(family_key) =
        super::opfamily_members::FamilyKey::of(interp, &family_name, family_ns, &am)
    else {
        return Err(DdlError::Internal("operator family vanished".into()));
    };
    // storeOperators without the ADD check: an operator the family already
    // has trips pg_amop's unique index.
    let ops = family_operators(
        interp,
        &family_name,
        family_ns,
        &am,
        &stmt.items,
        Some(intype),
    )?;
    for op in &ops {
        if interp.pg_amop.iter().any(|o| same_member(o, op)) {
            return Err(DdlError::DuplicateObject(
                "duplicate key value violates unique constraint \"pg_amop_fam_strat_index\"".into(),
            ));
        }
    }
    let funcs =
        super::opfamily_members::function_members(interp, &family_key, &stmt.items, Some(intype))?;
    // storeProcedures: a support function the family already has trips
    // pg_amproc's unique index.
    if funcs.iter().any(|f| {
        super::opfamily_members::find(interp, &family_key, true, f.number, f.left, f.right)
            .is_some()
    }) {
        return Err(DdlError::DuplicateObject(
            "duplicate key value violates unique constraint \"pg_amproc_fam_proc_index\"".into(),
        ));
    }
    let oid = PgOpclassOid::from_nonzero(interp.alloc_oid()?);
    // The class depends on its family, and goes with it (DEPENDENCY_AUTO).
    let family = super::depend::named_address_of(
        interp,
        &super::depend::NamedObject::Opfamily {
            name: family_name.clone(),
            namespace: family_ns,
            method: am.clone(),
        },
    );
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
    let me =
        super::depend::ObjectAddress::new(super::depend::PG_OPCLASS_RELID, oid.into_nonzero(), 0);
    super::depend::record(interp, me, family, crate::pg_catalog::DepType::Auto);
    // DefineOpClass: the class depends on its input type (a built-in one
    // is pinned).
    super::depend::record(
        interp,
        me,
        [super::depend::ObjectAddress::type_(intype)],
        crate::pg_catalog::DepType::Normal,
    );
    // Its operators and support functions: hard members tie the class to
    // them (see opfamily_members).
    let mut members: Vec<super::opfamily_members::FamilyMember> = ops
        .iter()
        .map(|op| super::opfamily_members::operator_member(&family_key, op))
        .chain(funcs)
        .collect();
    super::opfamily_members::adjust(
        interp,
        super::opfamily_members::Path::CreateClass { class: me, intype },
        &mut members,
    );
    interp.pg_amop.extend(ops);
    super::opfamily_members::store(interp, members);
    Ok(())
}

/// DefineOpClass / AlterOpFamilyAdd: an `OPERATOR n name [(left, right)]`
/// item becomes a search member (`pg_amop`) of the family — its operand
/// types default to the opclass's input type. `FOR ORDER BY` operators and
/// FUNCTION / STORAGE items are not recorded. The strategy number must be
/// in the access method's range, and the operator must exist.
fn family_operator(
    interp: &PgCatalog,
    family: &str,
    family_ns: PgNamespaceOid,
    am: &str,
    item: &CreateOpClassItem,
    intype: Option<PgTypeOid>,
) -> Result<Option<PgAmop>, DdlError> {
    // OPCLASS_ITEM_OPERATOR.
    if item.itemtype != 1 {
        return Ok(None);
    }
    if let Some(max) = am_strategies(interp, am)
        && (item.number <= 0 || item.number > max)
    {
        return Err(DdlError::Parse(format!(
            "invalid operator number {}, must be between 1 and {max}",
            item.number
        )));
    }
    // Not FOR ORDER BY.
    if !item.order_family.is_empty() {
        return Ok(None);
    }
    let Some(owa) = item.name.as_ref() else {
        return Ok(None);
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
        _ => return Ok(None),
    };
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(None);
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
        // LookupOperName.
        return Err(DdlError::TypeNotFound(format!(
            "operator does not exist: {} {} {}",
            super::util::format_type_for_message(interp, left),
            written(&owa.objname),
            super::util::format_type_for_message(interp, right),
        )));
    };
    Ok(Some(PgAmop {
        amopfamily: family.to_owned(),
        amopfamilynamespace: family_ns,
        amopmethod: am.to_owned(),
        amoplefttype: left,
        amoprighttype: right,
        amopstrategy: i16::try_from(item.number).unwrap_or_default(),
        amopopr: opr,
    }))
}

/// The access method's `amstrategies`, when it fixes one: btree's 5,
/// hash's and bloom's 1 (BTMaxStrategyNumber, HTMaxStrategyNumber,
/// BLOOM_NSTRATEGIES), by its handler. The others (GiST, GIN, SP-GiST,
/// BRIN) take any positive number.
fn am_strategies(interp: &PgCatalog, am: &str) -> Option<i32> {
    let handler = interp
        .pg_am
        .iter()
        .find(|a| a.amname == am)?
        .amhandler
        .and_then(|h| interp.pg_proc.get(&h))
        .map(|p| p.proname.as_str())?;
    match handler {
        "bthandler" => Some(5),
        "hashhandler" | "blhandler" => Some(1),
        _ => None,
    }
}

/// Whether `a` and `b` are the same member of a family: strategy number
/// and operand types (pg_amop_fam_strat_index).
fn same_member(a: &PgAmop, b: &PgAmop) -> bool {
    a.amopfamily == b.amopfamily
        && a.amopfamilynamespace == b.amopfamilynamespace
        && a.amopmethod == b.amopmethod
        && a.amopstrategy == b.amopstrategy
        && a.amoplefttype == b.amoplefttype
        && a.amoprighttype == b.amoprighttype
}

/// The OPERATOR items of a CREATE OPERATOR CLASS / ALTER OPERATOR FAMILY
/// ADD, resolved (addFamilyMember: a strategy number is given once per
/// operand types).
fn family_operators(
    interp: &PgCatalog,
    family: &str,
    family_ns: PgNamespaceOid,
    am: &str,
    items: &[typedpg_pg_query::protobuf::Node],
    intype: Option<PgTypeOid>,
) -> Result<Vec<PgAmop>, DdlError> {
    let mut ops: Vec<PgAmop> = Vec::new();
    for item in items {
        let Some(node::Node::CreateOpClassItem(item)) = item.node.as_ref() else {
            continue;
        };
        let Some(op) = family_operator(interp, family, family_ns, am, item, intype)? else {
            continue;
        };
        if ops.iter().any(|o| same_member(o, &op)) {
            return Err(DdlError::Parse(format!(
                "operator number {} for ({},{}) appears more than once",
                op.amopstrategy,
                super::util::format_type_for_message(interp, op.amoplefttype),
                super::util::format_type_for_message(interp, op.amoprighttype),
            )));
        }
        ops.push(op);
    }
    Ok(ops)
}

/// The name list as written (NameListToString).
fn written(names: &[typedpg_pg_query::protobuf::Node]) -> String {
    names
        .iter()
        .filter_map(node_string)
        .collect::<Vec<_>>()
        .join(".")
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
            "operator family \"{}\" does not exist for access method \"{}\"",
            written(&stmt.opfamilyname),
            stmt.amname
        )));
    };
    let Some(family_key) =
        super::opfamily_members::FamilyKey::of(interp, &family, family_ns, &stmt.amname)
    else {
        return Err(DdlError::Internal("operator family vanished".into()));
    };
    if !stmt.is_drop {
        // AlterOpFamilyAdd → storeOperators(isAdd).
        let ops = family_operators(interp, &family, family_ns, &stmt.amname, &stmt.items, None)?;
        for op in &ops {
            if interp.pg_amop.iter().any(|o| same_member(o, op)) {
                return Err(DdlError::DuplicateObject(format!(
                    "operator {}({},{}) already exists in operator family \"{}\"",
                    op.amopstrategy,
                    super::util::format_type_for_message(interp, op.amoplefttype),
                    super::util::format_type_for_message(interp, op.amoprighttype),
                    written(&stmt.opfamilyname),
                )));
            }
        }
        let funcs =
            super::opfamily_members::function_members(interp, &family_key, &stmt.items, None)?;
        for f in &funcs {
            if super::opfamily_members::find(interp, &family_key, true, f.number, f.left, f.right)
                .is_some()
            {
                return Err(DdlError::DuplicateObject(format!(
                    "function {}({},{}) already exists in operator family \"{}\"",
                    f.number,
                    super::util::format_type_for_message(interp, f.left),
                    super::util::format_type_for_message(interp, f.right),
                    written(&stmt.opfamilyname),
                )));
            }
        }
        let mut members: Vec<super::opfamily_members::FamilyMember> = ops
            .iter()
            .map(|op| super::opfamily_members::operator_member(&family_key, op))
            .chain(funcs)
            .collect();
        super::opfamily_members::adjust(
            interp,
            super::opfamily_members::Path::AlterAdd,
            &mut members,
        );
        interp.pg_amop.extend(ops);
        super::opfamily_members::store(interp, members);
        return Ok(());
    }
    // AlterOpFamilyDrop: `OPERATOR n (left, right)` and `FUNCTION n (left,
    // right)` items.
    let mut dropped: Vec<(bool, i16, PgTypeOid, PgTypeOid)> = Vec::new();
    for item in &stmt.items {
        let Some(node::Node::CreateOpClassItem(item)) = item.node.as_ref() else {
            continue;
        };
        let is_func = match item.itemtype {
            1 => false,
            2 => true,
            _ => continue,
        };
        let types: Vec<Option<PgTypeOid>> = item
            .class_args
            .iter()
            .map(|n| match n.node.as_ref() {
                Some(node::Node::TypeName(tn)) => super::util::resolve_type_name(tn, interp),
                _ => None,
            })
            .collect();
        // processTypesSpec: one type names both sides.
        let (left, right) = match types.as_slice() {
            [Some(t)] => (*t, *t),
            [Some(l), Some(r)] => (*l, *r),
            _ => continue,
        };
        let number = i16::try_from(item.number).unwrap_or_default();
        if dropped.contains(&(is_func, number, left, right)) {
            return Err(DdlError::Parse(format!(
                "{} number {number} for ({},{}) appears more than once",
                if is_func { "function" } else { "operator" },
                super::util::format_type_for_message(interp, left),
                super::util::format_type_for_message(interp, right),
            )));
        }
        dropped.push((is_func, number, left, right));
    }
    // dropOperators / dropProcedures: a member tied to a class goes only
    // with it.
    let built_in = interp.namespace_name(family_ns) == Some("pg_catalog");
    for (is_func, number, left, right) in dropped {
        let member =
            super::opfamily_members::find(interp, &family_key, is_func, number, left, right);
        if let Some(member) = &member {
            super::opfamily_members::drop_member(interp, member)?;
        }
        if is_func {
            // The analyzer keeps the support functions of the families
            // migrations create only.
            if member.is_none() && !built_in {
                return Err(DdlError::TypeNotFound(format!(
                    "function {number}({},{}) does not exist in operator family \"{}\"",
                    super::util::format_type_for_message(interp, left),
                    super::util::format_type_for_message(interp, right),
                    written(&stmt.opfamilyname),
                )));
            }
            continue;
        }
        let before = interp.pg_amop.len();
        interp.pg_amop.retain(|o| {
            !(o.amopfamily == family
                && o.amopfamilynamespace == family_ns
                && o.amopmethod == stmt.amname
                && o.amopstrategy == number
                && o.amoplefttype == left
                && o.amoprighttype == right)
        });
        if interp.pg_amop.len() == before {
            return Err(DdlError::TypeNotFound(format!(
                "operator {number}({},{}) does not exist in operator family \"{}\"",
                super::util::format_type_for_message(interp, left),
                super::util::format_type_for_message(interp, right),
                written(&stmt.opfamilyname),
            )));
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
    cascade: bool,
) -> Result<(), DdlError> {
    use typedpg_pg_query::protobuf::ObjectType;
    match objtype {
        ObjectType::ObjectAccessMethod => {
            let Some(name) = node_string(obj_node).map(str::to_owned) else {
                return Ok(());
            };
            if !am_exists(interp, &name) {
                if missing_ok {
                    return Ok(());
                }
                return Err(DdlError::TypeNotFound(format!(
                    "access method \"{name}\" does not exist"
                )));
            }
            // An extension's member goes only with it.
            let object = super::depend::NamedObject::AccessMethod { name: name.clone() };
            if let Some(addr) = super::depend::named_address_of(interp, &object) {
                let desc = format!("access method {name}");
                super::depend::check_not_owned(interp, addr, &desc)?;
                // Its operator families (and their classes) go with it;
                // what uses them needs CASCADE.
                super::depend::drop_dependents(interp, addr, &desc, cascade)?;
                interp.remove_dependencies_on(addr.classid, addr.objid);
            }
            interp.pg_am.retain(|a| a.amname != name);
            interp.access_method_oids.remove(&name);
        }
        ObjectType::ObjectOpclass | ObjectType::ObjectOpfamily => {
            let is_class = objtype == ObjectType::ObjectOpclass;
            if let Some(object) = resolve_am_object(interp, is_class, obj_node, missing_ok)? {
                // What depends on it (an index on the class, the classes of
                // the family and their indexes) needs CASCADE.
                super::depend::drop_named(interp, object, cascade)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The operator class or family `[am, name...]` names, resolved along the
/// search path. `Ok(None)` when it (or its schema, with `missing_ok`)
/// doesn't exist.
fn resolve_am_object(
    interp: &PgCatalog,
    is_class: bool,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<Option<super::depend::NamedObject>, DdlError> {
    use super::depend::NamedObject;
    let Some(node::Node::List(l)) = obj_node.node.as_ref() else {
        return Ok(None);
    };
    let Some((am, names)) = l.items.split_first() else {
        return Ok(None);
    };
    let am = node_string(am).unwrap_or_default().to_owned();
    am_must_exist(interp, &am)?;
    let (schema, name) = split_name(names);
    if let Some(schema) = schema.as_deref()
        && interp.namespace_oid(schema).is_none()
    {
        if missing_ok {
            return Ok(None);
        }
        return Err(DdlError::TypeNotFound(format!(
            "schema \"{schema}\" does not exist"
        )));
    }
    let found = if is_class {
        find_opclass(interp, schema.as_deref(), &name, &am).map(|c| NamedObject::Opclass {
            name: c.opcname.clone(),
            namespace: c.opcnamespace,
            method: am.clone(),
        })
    } else {
        find_opfamily(interp, schema.as_deref(), &name, &am).map(|f| NamedObject::Opfamily {
            name: f.opfname.clone(),
            namespace: f.opfnamespace,
            method: am.clone(),
        })
    };
    if found.is_none() && !missing_ok {
        let written = written(names);
        let what = if is_class {
            "operator class"
        } else {
            "operator family"
        };
        return Err(DdlError::TypeNotFound(format!(
            "{what} \"{written}\" does not exist for access method \"{am}\""
        )));
    }
    Ok(found)
}

/// ALTER OPERATOR CLASS / FAMILY ... RENAME TO / SET SCHEMA
/// (AlterObjectRename_internal / AlterObjectNamespace_internal): the
/// object keeps its OID — and its dependencies — under its new name. A
/// family's operators and classes, which name it, follow it.
pub(crate) fn rename_or_move_am_object(
    interp: &mut PgCatalog,
    is_class: bool,
    obj_node: &typedpg_pg_query::protobuf::Node,
    new_name: Option<&str>,
    new_namespace: Option<PgNamespaceOid>,
) -> Result<(), DdlError> {
    use super::depend::NamedObject;
    let (NamedObject::Opclass {
        name,
        namespace,
        method,
    }
    | NamedObject::Opfamily {
        name,
        namespace,
        method,
    }) = (match resolve_am_object(interp, is_class, obj_node, false)? {
        Some(object) => object,
        None => return Ok(()),
    })
    else {
        return Ok(());
    };
    let to_name = new_name.map_or_else(|| name.clone(), str::to_owned);
    let to_ns = new_namespace.unwrap_or(namespace);
    if to_name == name && to_ns == namespace {
        return Ok(());
    }
    // report_namespace_conflict.
    let taken = if is_class {
        interp
            .pg_opclass
            .iter()
            .any(|c| c.opcname == to_name && c.opcnamespace == to_ns && c.opcmethod == method)
    } else {
        interp
            .pg_opfamily
            .iter()
            .any(|f| f.opfname == to_name && f.opfnamespace == to_ns && f.opfmethod == method)
    };
    if taken {
        let what = if is_class {
            "operator class"
        } else {
            "operator family"
        };
        return Err(DdlError::DuplicateObject(format!(
            "{what} \"{to_name}\" for access method \"{method}\" already exists in schema \"{}\"",
            interp.namespace_name(to_ns).unwrap_or_default()
        )));
    }
    if is_class {
        if let Some(c) = interp
            .pg_opclass
            .iter_mut()
            .find(|c| c.opcname == name && c.opcnamespace == namespace && c.opcmethod == method)
        {
            c.opcname = to_name;
            c.opcnamespace = to_ns;
        }
        return Ok(());
    }
    for f in &mut interp.pg_opfamily {
        if f.opfname == name && f.opfnamespace == namespace && f.opfmethod == method {
            f.opfname.clone_from(&to_name);
            f.opfnamespace = to_ns;
        }
    }
    for o in &mut interp.pg_amop {
        if o.amopfamily == name && o.amopfamilynamespace == namespace && o.amopmethod == method {
            o.amopfamily.clone_from(&to_name);
            o.amopfamilynamespace = to_ns;
        }
    }
    for c in &mut interp.pg_opclass {
        if c.opcfamily == name && c.opcfamilynamespace == namespace && c.opcmethod == method {
            c.opcfamily.clone_from(&to_name);
            c.opcfamilynamespace = to_ns;
        }
    }
    Ok(())
}

/// IsBinaryCoercible, including the polymorphic opclass input types.
fn binary_coercible(interp: &PgCatalog, src: PgTypeOid, target: PgTypeOid) -> bool {
    if src == target || interp.is_binary_coercible(src, target) {
        return true;
    }
    let t = interp.pg_type.get(&src);
    // type_is_array: a true array type — record[] (`_record`) too, though a
    // pseudo-type.
    let is_array = t.is_some_and(|t| {
        t.typelem.is_some() && (t.typcategory == TypCategory::Array || t.typtype == TypType::Pseudo)
    });
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
    resolve_opclass(interp, opclass, typ, am).map(Some)
}

/// ResolveOpClass for a column of type `typ`, `unknown` included — which
/// has no operator class: `data type unknown has no default operator class
/// …`, or the one named `does not accept data type unknown`.
pub(crate) fn resolve_opclass(
    interp: &PgCatalog,
    opclass: &[typedpg_pg_query::protobuf::Node],
    typ: PgTypeOid,
    am: &str,
) -> Result<PgOpclassOid, DdlError> {
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
        return Ok(found.oid);
    }
    let (schema, name) = split_name(opclass);
    let Some(found) = find_opclass(interp, schema.as_deref(), &name, am) else {
        return Err(DdlError::TypeNotFound(format!(
            "operator class \"{name}\" does not exist for access method \"{am}\""
        )));
    };
    check_opclass_accepts(interp, found, typ)?;
    Ok(found.oid)
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
