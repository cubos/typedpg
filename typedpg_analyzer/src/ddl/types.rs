//! CREATE TYPE / CREATE DOMAIN / ALTER TYPE DDL handlers.

use typedpg_pg_query::protobuf::{
    AlterEnumStmt, CoercionContext, CompositeTypeStmt, ConstrType, CreateCastStmt,
    CreateDomainStmt, CreateEnumStmt, CreateRangeStmt, DefineStmt, ObjectType, TypeName, node,
};

use crate::oid::{PgCastOid, PgClassOid, PgEnumOid, PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, PgAttribute, PgCast, PgClass, PgEnum, PgRange, PgType, RelKind,
    TypAlign, TypCategory, TypStorage, TypType,
};

use super::DdlError;
use super::util::{
    ensure_qualified_name, names_key, node_string, register_composite_to_record_cast,
};
use crate::pg_catalog::PgCatalog;

// ─── CREATE DOMAIN ──────────────────────────────────────────────────────────

pub fn create_domain(interp: &mut PgCatalog, stmt: &CreateDomainStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.domainname)?;
    let shell = claim_type_name(interp, nsoid, &name)?;

    let base_type_name = stmt
        .type_name
        .as_ref()
        .ok_or_else(|| DdlError::TypeNotFound("domain base type".into()))?;
    let base_type_oid = super::functions::typename_type_id(interp, base_type_name)?;
    let typtypmod = crate::typmod::encode(interp, base_type_oid, &base_type_name.typmods)?;
    // DefineDomain: a domain is over a base, composite, enum, range or
    // multirange type or another domain — never a pseudo-type.
    if interp.pg_type.get(&base_type_oid).map(|t| t.typtype) == Some(TypType::Pseudo) {
        return Err(DdlError::UnsupportedDdl(format!(
            "\"{}\" is not a valid base type for a domain",
            super::util::type_name_to_string(base_type_name)
        )));
    }
    // The collation: COLLATE needs a collatable base type.
    let base_collation = interp
        .pg_type
        .get(&base_type_oid)
        .and_then(|t| t.typcollation);
    let domain_collation = match stmt.coll_clause.as_deref() {
        Some(coll) => {
            let parts: Vec<&str> = coll.collname.iter().filter_map(node_string).collect();
            let (schema, cname) = match parts.as_slice() {
                [n] => (None, *n),
                [s, n] => (Some(*s), *n),
                _ => return Err(DdlError::Parse("malformed COLLATE clause".into())),
            };
            let resolved = interp
                .resolve_collation(schema, cname)
                .ok_or_else(|| DdlError::Parse(format!("collation \"{cname}\" does not exist")))?;
            if base_collation.is_none() {
                return Err(DdlError::UnsupportedDdl(format!(
                    "collations are not supported by type {}",
                    super::util::format_type_for_message(interp, base_type_oid)
                )));
            }
            Some(resolved.oid)
        }
        None => base_collation,
    };

    // Domains inherit category/preferred from their base type.
    let (typcategory, typispreferred) = interp
        .pg_type
        .get(&base_type_oid)
        .map(|t| (t.typcategory, t.typispreferred))
        .unwrap_or((TypCategory::UserDefined, false));

    // `CREATE DOMAIN d AS T NOT NULL` lands in `stmt.constraints` as a
    // `Constraint { contype = CONSTR_NOTNULL }`. PG also forbids null defaults
    // on a NOT NULL domain, but the analyzer doesn't model defaults yet.
    let mut constraints: Vec<DomainConstraint> = Vec::new();
    let mut saw_default = false;
    let mut null_defined = false;
    let mut typ_not_null = false;
    for n in &stmt.constraints {
        let Some(node::Node::Constraint(c)) = n.node.as_ref() else {
            continue;
        };
        // DefineDomain: which clauses a domain takes, and how often.
        let fail = |msg: &str| Err(DdlError::Parse(msg.to_owned()));
        match ConstrType::try_from(c.contype) {
            Ok(ConstrType::ConstrDefault) => {
                if saw_default {
                    return fail("multiple default expressions");
                }
                saw_default = true;
            }
            Ok(ConstrType::ConstrNotnull) => {
                if null_defined {
                    return fail(if typ_not_null {
                        "redundant NOT NULL constraint definition"
                    } else {
                        "conflicting NULL/NOT NULL constraints"
                    });
                }
                if c.is_no_inherit {
                    return fail("not-null constraints for domains cannot be marked NO INHERIT");
                }
                typ_not_null = true;
                null_defined = true;
            }
            Ok(ConstrType::ConstrNull) => {
                if null_defined && typ_not_null {
                    return fail("conflicting NULL/NOT NULL constraints");
                }
                typ_not_null = false;
                null_defined = true;
            }
            Ok(ConstrType::ConstrCheck) if c.is_no_inherit => {
                return fail("check constraints for domains cannot be marked NO INHERIT");
            }
            Ok(ConstrType::ConstrUnique) => {
                return fail("unique constraints not possible for domains");
            }
            Ok(ConstrType::ConstrPrimary) => {
                return fail("primary key constraints not possible for domains");
            }
            Ok(ConstrType::ConstrExclusion) => {
                return fail("exclusion constraints not possible for domains");
            }
            Ok(ConstrType::ConstrForeign) => {
                return fail("foreign key constraints not possible for domains");
            }
            Ok(
                ConstrType::ConstrAttrDeferrable
                | ConstrType::ConstrAttrNotDeferrable
                | ConstrType::ConstrAttrDeferred
                | ConstrType::ConstrAttrImmediate,
            ) => {
                return fail("specifying constraint deferrability not supported for domains");
            }
            Ok(ConstrType::ConstrGenerated | ConstrType::ConstrIdentity) => {
                return fail("specifying GENERATED not supported for domains");
            }
            Ok(ConstrType::ConstrAttrEnforced | ConstrType::ConstrAttrNotEnforced) => {
                return fail("specifying constraint enforceability not supported for domains");
            }
            _ => {}
        }
        // domainAddDefault: cooked like a column default named after the
        // domain, against the base type.
        if c.contype == ConstrType::ConstrDefault as i32
            && let Some(expr) = c.raw_expr.as_deref()
        {
            super::defaults::check_default(interp, expr, &name, base_type_oid)?;
        }
        add_domain_constraint(interp, &name, base_type_oid, c, &mut constraints)?;
    }
    let typnotnull = constraints
        .iter()
        .any(|c| c.kind == DomainConstraintKind::NotNull);

    // DefineDomain: a domain stores its values like its base type.
    let domain_storage = interp
        .pg_type
        .get(&base_type_oid)
        .map_or(TypStorage::Plain, |t| t.typstorage);

    let oid = new_type_oid(interp, shell)?;
    let phys = TypePhysical::of(interp, base_type_oid);
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Domain,
        typcategory,
        typispreferred,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: Some(base_type_oid),
        typnotnull,
        typtypmod,
        typcollation: domain_collation,
        typstorage: domain_storage,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });

    register_array_type(interp, nsoid, &name, oid)?;
    interp.domain_constraints.insert(oid, constraints);
    record_domain_constraint_dependencies(interp, oid)?;
    Ok(())
}

/// A named domain constraint (`pg_constraint` row with `contypid` set).
#[derive(Clone, Debug)]
pub(crate) struct DomainConstraint {
    pub(crate) name: String,
    pub(crate) kind: DomainConstraintKind,
    /// What a CHECK's expression refers to, until the constraint's
    /// dependencies are recorded.
    pub(crate) refs: Vec<super::depend::Reference>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DomainConstraintKind {
    NotNull,
    Check,
}

/// Validate and record one domain constraint (`domainAddCheckConstraint` /
/// `domainAddNotNullConstraint`, typecmds.c). Unnamed ones get PG's
/// generated names: `<domain>_not_null`, `<domain>_check` (numbered on
/// collision, `ChooseConstraintName`). Other constraint kinds (DEFAULT,
/// NULL) carry no name.
fn add_domain_constraint(
    interp: &PgCatalog,
    domain: &str,
    base_type: PgTypeOid,
    c: &typedpg_pg_query::protobuf::Constraint,
    existing: &mut Vec<DomainConstraint>,
) -> Result<(), DdlError> {
    let mut refs = Vec::new();
    let kind = match ConstrType::try_from(c.contype) {
        Ok(ConstrType::ConstrNotnull) => DomainConstraintKind::NotNull,
        Ok(ConstrType::ConstrCheck) => {
            if let Some(expr) = c.raw_expr.as_deref() {
                refs = check_domain_check_expression(interp, base_type, expr)?;
            }
            DomainConstraintKind::Check
        }
        _ => return Ok(()),
    };
    // A second NOT NULL on a domain that has one is a no-op.
    if kind == DomainConstraintKind::NotNull
        && existing
            .iter()
            .any(|x| x.kind == DomainConstraintKind::NotNull)
    {
        return Ok(());
    }
    let name = if c.conname.is_empty() {
        let label = match kind {
            DomainConstraintKind::NotNull => "not_null",
            DomainConstraintKind::Check => "check",
        };
        let mut pass = 0;
        loop {
            let label = if pass == 0 {
                label.to_owned()
            } else {
                format!("{label}{pass}")
            };
            let candidate = super::util::make_object_name(domain, "", &label);
            if !existing.iter().any(|x| x.name == candidate) {
                break candidate;
            }
            pass += 1;
        }
    } else {
        if existing.iter().any(|x| x.name == c.conname) {
            return Err(DdlError::DuplicateObject(format!(
                "constraint \"{}\" for domain \"{domain}\" already exists",
                c.conname
            )));
        }
        c.conname.clone()
    };
    existing.push(DomainConstraint { name, kind, refs });
    Ok(())
}

/// domainAddCheckConstraint: a domain's new CHECK constraints depend on
/// what their expressions refer to.
fn record_domain_constraint_dependencies(
    interp: &mut PgCatalog,
    typid: PgTypeOid,
) -> Result<(), DdlError> {
    let pending: Vec<(String, Vec<super::depend::Reference>)> = interp
        .domain_constraints
        .get_mut(&typid)
        .into_iter()
        .flatten()
        .filter(|c| !c.refs.is_empty())
        .map(|c| (c.name.clone(), std::mem::take(&mut c.refs)))
        .collect();
    for (name, refs) in pending {
        super::depend::record_named(
            interp,
            super::depend::NamedObject::DomainConstraint { typid, name },
            &refs,
        )?;
    }
    Ok(())
}

/// A domain CHECK expression sees `VALUE` as a value of the base type and
/// must yield boolean (`domainAddCheckConstraint`).
fn check_domain_check_expression(
    interp: &PgCatalog,
    base_type: PgTypeOid,
    expr: &typedpg_pg_query::protobuf::Node,
) -> Result<Vec<super::depend::Reference>, DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::pg_catalog::oid;
    use crate::scope::Scope;

    let value = PgAttribute {
        attrelid: crate::pg_catalog::PG_CLASS_RELID,
        attname: "value".to_owned(),
        atttypid: base_type,
        attnum: 1,
        attnotnull: false,
        atthasdef: false,
        attgenerated: None,
        atttypmod: None,
        attidentity: None,
        attcollation: None,
        attislocal: true,
        attinhcount: 0,
    };
    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        "",
        crate::qualified_name::QualifiedName::new("", ""),
        std::slice::from_ref(&value),
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    super::expr_kind::check_expr_kind(interp, expr, super::expr_kind::ExprKind::CheckConstraint)?;
    // A domain CHECK has no parameters (transformParamRef finds no hook).
    if let Some(number) = expr
        .node
        .iter()
        .flat_map(|n| n.nodes())
        .find_map(|(n, _)| match n {
            typedpg_pg_query::NodeRef::ParamRef(p) => Some(p.number),
            _ => None,
        })
    {
        return Err(DdlError::Parse(format!("there is no parameter ${number}")));
    }
    let (result, refs) = super::depend::collect(|| {
        infer_expr(
            expr,
            crate::expr::Ctx::new(&scope, &null_ctx, interp),
            &mut params,
            TypeGoal::NONE,
        )
    });
    let result = result
        .map_err(|e| DdlError::UnsupportedDdl(format!("{e} (in domain CHECK constraint)")))?;
    if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of CHECK must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(refs)
}

// ─── ALTER DOMAIN ───────────────────────────────────────────────────────────

/// `ALTER DOMAIN d { SET | DROP } NOT NULL | ADD constraint | DROP
/// CONSTRAINT name | { SET | DROP } DEFAULT | VALIDATE CONSTRAINT name`
/// (`AlterDomainNotNull` / `AlterDomainAddConstraint` /
/// `AlterDomainDropConstraint`, typecmds.c). NOT NULL changes flip
/// `pg_type.typnotnull`, which every column of the domain reads.
pub fn alter_domain(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterDomainStmt,
) -> Result<(), DdlError> {
    let parts: Vec<&str> = stmt.type_name.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [schema, name] => (Some(*schema), *name),
        [name] => (None, *name),
        _ => return Ok(()),
    };
    let Some(type_oid) = interp.resolve_type_by_name(schema, name).map(|t| t.oid) else {
        return Err(DdlError::TypeNotFound(format!(
            "type \"{}\" does not exist",
            parts.join(".")
        )));
    };
    if interp.pg_type.get(&type_oid).map(|t| t.typtype) != Some(TypType::Domain) {
        return Err(DdlError::Parse(format!(
            "{} is not a domain",
            super::util::format_type_for_message(interp, type_oid)
        )));
    }
    let base_type = interp
        .pg_type
        .get(&type_oid)
        .and_then(|t| t.typbasetype)
        .unwrap_or(type_oid);
    let mut constraints = interp
        .domain_constraints
        .get(&type_oid)
        .cloned()
        .unwrap_or_default();

    match stmt.subtype.as_str() {
        // SET NOT NULL / DROP NOT NULL
        "O" => {
            let nn = typedpg_pg_query::protobuf::Constraint {
                contype: ConstrType::ConstrNotnull as i32,
                ..Default::default()
            };
            add_domain_constraint(interp, name, base_type, &nn, &mut constraints)?;
        }
        "N" => constraints.retain(|c| c.kind != DomainConstraintKind::NotNull),
        // ADD constraint
        "C" => {
            if let Some(node::Node::Constraint(c)) =
                stmt.def.as_deref().and_then(|d| d.node.as_ref())
            {
                add_domain_constraint(interp, name, base_type, c, &mut constraints)?;
            }
        }
        // DROP CONSTRAINT / VALIDATE CONSTRAINT name
        "X" | "V" => {
            let before = constraints.len();
            if stmt.subtype == "X" {
                constraints.retain(|c| c.name != stmt.name);
            }
            let found = if stmt.subtype == "X" {
                constraints.len() != before
            } else {
                constraints.iter().any(|c| c.name == stmt.name)
            };
            if !found && !stmt.missing_ok {
                return Err(DdlError::TypeNotFound(format!(
                    "constraint \"{}\" of domain \"{name}\" does not exist",
                    stmt.name
                )));
            }
        }
        // SET DEFAULT: cooked like the domain's own DEFAULT
        // (AlterDomainDefault). DROP DEFAULT has no effect on typing.
        "T" => {
            if let Some(expr) = stmt.def.as_deref() {
                super::defaults::check_default(interp, expr, name, base_type)?;
            }
        }
        _ => {}
    }

    let typnotnull = constraints
        .iter()
        .any(|c| c.kind == DomainConstraintKind::NotNull);
    let mut changed = false;
    if let Some(t) = interp.pg_type.get_mut(&type_oid) {
        changed = t.typnotnull != typnotnull;
        t.typnotnull = typnotnull;
    }
    interp.domain_constraints.insert(type_oid, constraints);
    record_domain_constraint_dependencies(interp, type_oid)?;
    if changed {
        // Views over columns of the domain see the change too.
        let relations: Vec<PgClassOid> = interp
            .pg_attribute
            .iter()
            .filter(|(_, attrs)| attrs.iter().any(|a| a.atttypid == type_oid))
            .map(|(&relid, _)| relid)
            .collect();
        for relid in relations {
            super::views::refresh_dependent_view_nullability(interp, relid, !typnotnull);
        }
    }
    Ok(())
}

// ─── CREATE TYPE AS ENUM ────────────────────────────────────────────────────

pub fn create_enum(interp: &mut PgCatalog, stmt: &CreateEnumStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.type_name)?;
    let shell = claim_type_name(interp, nsoid, &name)?;

    let labels: Vec<String> = stmt
        .vals
        .iter()
        .filter_map(|n| node_string(n).map(|s| s.to_owned()))
        .collect();
    // EnumValuesCreate inserts the labels one by one; a repeat trips
    // pg_enum's unique index.
    for (i, label) in labels.iter().enumerate() {
        check_enum_label(label)?;
        if labels[..i].contains(label) {
            return Err(DdlError::DuplicateObject(
                "duplicate key value violates unique constraint \"pg_enum_typid_label_index\""
                    .into(),
            ));
        }
    }

    let oid = new_type_oid(interp, shell)?;
    interp.enums_created_in_transaction.insert(oid);
    let phys = TypePhysical::ENUM;
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Enum,
        typcategory: TypCategory::Enum,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Plain,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });
    for (i, label) in labels.into_iter().enumerate() {
        let enum_oid = PgEnumOid::from_nonzero(interp.alloc_oid()?);
        interp.insert_pg_enum(PgEnum {
            oid: enum_oid,
            enumtypid: oid,
            enumsortorder: (i + 1) as f32,
            enumlabel: label,
        });
    }

    register_array_type(interp, nsoid, &name, oid)?;
    Ok(())
}

// ─── CREATE TYPE AS (composite) ─────────────────────────────────────────────

pub fn create_composite(interp: &mut PgCatalog, stmt: &CompositeTypeStmt) -> Result<(), DdlError> {
    let rv = stmt
        .typevar
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TYPE without name".into()))?;

    let (nsoid, name) = super::util::ensure_range_var(interp, rv)?;

    // DefineCompositeType checks the type name first (for the better
    // message), then DefineRelation the relation name.
    let shell = claim_type_name(interp, nsoid, &name)?;
    // A composite type is backed by a relation (DefineCompositeType →
    // DefineRelation), which must not collide either — e.g. with a
    // sequence, which has no row type.
    super::util::check_relation_name_free(interp, nsoid, &name)?;

    // Collect column definitions before mutating, so we can resolve type
    // names against the catalog without holding a mutable borrow.
    // `(name, type, typmod, not null, collation)` per attribute.
    type FieldDef = (
        String,
        PgTypeOid,
        Option<i32>,
        bool,
        Option<crate::oid::PgCollationOid>,
    );
    let mut field_defs: Vec<FieldDef> = Vec::new();
    for col_node in &stmt.coldeflist {
        if let Some(node::Node::ColumnDef(cd)) = col_node.node.as_ref()
            && let Some(tn) = cd.type_name.as_ref()
        {
            // MergeAttributes.
            if field_defs.iter().any(|f| f.0 == cd.colname) {
                return Err(DdlError::DuplicateObject(format!(
                    "column \"{}\" specified more than once",
                    cd.colname
                )));
            }
            let type_oid = super::functions::typename_type_id(interp, tn)?;
            super::tables::check_attribute_type(interp, &cd.colname, type_oid, None, false)?;
            let typmod = crate::typmod::encode(interp, type_oid, &tn.typmods)?;
            let collation = super::tables::column_collation(interp, cd, type_oid)?
                .or_else(|| super::tables::type_collation(interp, type_oid));
            field_defs.push((
                cd.colname.clone(),
                type_oid,
                typmod,
                cd.is_not_null,
                collation,
            ));
        }
    }

    let class_oid = PgClassOid::from_nonzero(interp.alloc_oid()?);
    let type_oid = new_type_oid(interp, shell)?;

    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name.clone(),
        relnamespace: nsoid,
        relkind: RelKind::CompositeType,
        reltype: Some(type_oid),
    });
    for (i, (fname, ftype, ftypmod, fnotnull, fcollation)) in field_defs.into_iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: fname,
            atttypid: ftype,
            attnum: (i + 1) as i16,
            attnotnull: fnotnull,
            atthasdef: false,
            attgenerated: None,
            atttypmod: ftypmod,
            attidentity: None,
            attcollation: fcollation,
            attislocal: true,
            attinhcount: 0,
        });
    }
    let phys = TypePhysical::COMPOSITE;
    interp.insert_pg_type(PgType {
        oid: type_oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Composite,
        typcategory: TypCategory::Composite,
        typispreferred: false,
        typrelid: Some(class_oid),
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });

    register_array_type(interp, nsoid, &name, type_oid)?;
    register_composite_to_record_cast(interp, type_oid)?;

    Ok(())
}

// ─── CREATE TYPE AS RANGE ───────────────────────────────────────────────────

pub fn create_range(interp: &mut PgCatalog, stmt: &CreateRangeStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.type_name)?;
    let shell = claim_type_name(interp, nsoid, &name)?;

    // DefineRange (typecmds.c): each attribute once; `subtype` is required.
    let conflicting = || DdlError::Parse("conflicting or redundant options".into());
    let qualified = |de: &typedpg_pg_query::protobuf::DefElem| -> Vec<String> {
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
    };
    let mut subtype_oid: Option<PgTypeOid> = None;
    let mut subtype_opclass: Option<Vec<String>> = None;
    let mut collation_name: Option<Vec<String>> = None;
    let mut canonical_name: Option<Vec<String>> = None;
    let mut diff_name: Option<Vec<String>> = None;
    let mut multirange_names: Option<Vec<String>> = None;
    for param_node in &stmt.params {
        let Some(node::Node::DefElem(de)) = param_node.node.as_ref() else {
            continue;
        };
        let slot = match de.defname.as_str() {
            "subtype" => {
                if subtype_oid.is_some() {
                    return Err(conflicting());
                }
                let Some(node::Node::TypeName(tn)) =
                    de.arg.as_deref().and_then(|a| a.node.as_ref())
                else {
                    continue;
                };
                subtype_oid = Some(super::functions::typename_type_id(interp, tn)?);
                continue;
            }
            "subtype_opclass" => &mut subtype_opclass,
            "collation" => &mut collation_name,
            "canonical" => &mut canonical_name,
            "subtype_diff" => &mut diff_name,
            "multirange_type_name" => &mut multirange_names,
            other => {
                return Err(DdlError::Parse(format!(
                    "type attribute \"{other}\" not recognized"
                )));
            }
        };
        if slot.replace(qualified(de)).is_some() {
            return Err(conflicting());
        }
    }
    let _ = subtype_opclass;
    let Some(subtype_oid) = subtype_oid else {
        return Err(DdlError::Parse(
            "type attribute \"subtype\" is required".into(),
        ));
    };
    if interp.pg_type.get(&subtype_oid).map(|t| t.typtype) == Some(TypType::Pseudo) {
        return Err(DdlError::Parse(format!(
            "range subtype cannot be {}",
            super::util::format_type_for_message(interp, subtype_oid)
        )));
    }
    let collatable = interp
        .pg_type
        .get(&subtype_oid)
        .is_some_and(|t| t.typcollation.is_some());
    if let Some(coll) = collation_name.as_deref() {
        if !collatable {
            return Err(DdlError::UnsupportedDdl(
                "range collation specified but subtype does not support collation".into(),
            ));
        }
        let (schema, cname) = match coll {
            [s, n] => (Some(s.as_str()), n.as_str()),
            [n] => (None, n.as_str()),
            _ => (None, ""),
        };
        if interp.resolve_collation(schema, cname).is_none() {
            return Err(DdlError::Parse(format!(
                "collation \"{cname}\" for encoding \"UTF8\" does not exist"
            )));
        }
    }
    // The support functions (findRangeCanonicalFunction /
    // findRangeSubtypeDiffFunction).
    let mut support: Vec<PgProcOid> = Vec::new();
    if let Some(canonical) = canonical_name.as_deref() {
        let Some(range_oid) = shell else {
            return Err(DdlError::Parse(
                "cannot specify a canonical function without a pre-created shell type (Create \
                 the type as a shell type, then create its canonicalization function, then do \
                 a full CREATE TYPE.)"
                    .into(),
            ));
        };
        let proc = range_support_function(interp, canonical, &[range_oid])?;
        let p = interp.pg_proc.get(&proc);
        let signature = super::functions::func_signature_string(interp, canonical, &[range_oid]);
        if p.map(|p| p.prorettype) != Some(range_oid) {
            return Err(DdlError::Parse(format!(
                "range canonical function {signature} must return range type"
            )));
        }
        if p.map(|p| p.provolatile) != Some(crate::pg_catalog::ProVolatile::Immutable) {
            return Err(DdlError::Parse(format!(
                "range canonical function {signature} must be immutable"
            )));
        }
        support.push(proc);
    }
    if let Some(diff) = diff_name.as_deref() {
        let args = [subtype_oid, subtype_oid];
        let proc = range_support_function(interp, diff, &args)?;
        let p = interp.pg_proc.get(&proc);
        let signature = super::functions::func_signature_string(interp, diff, &args);
        if p.map(|p| p.prorettype) != Some(crate::pg_catalog::oid::FLOAT8) {
            return Err(DdlError::Parse(format!(
                "range subtype diff function {signature} must return type double precision"
            )));
        }
        if p.map(|p| p.provolatile) != Some(crate::pg_catalog::ProVolatile::Immutable) {
            return Err(DdlError::Parse(format!(
                "range subtype diff function {signature} must be immutable"
            )));
        }
        support.push(proc);
    }
    let (mr_nsoid, mr_name) = match multirange_names.as_deref() {
        Some([schema, mr]) => (super::util::existing_namespace(interp, schema)?, mr.clone()),
        Some([mr]) => (nsoid, mr.clone()),
        _ => (nsoid, make_multirange_type_name(&name)),
    };
    if claim_type_name(interp, mr_nsoid, &mr_name)?.is_some() {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{mr_name}\" already exists"
        )));
    }

    let oid = new_type_oid(interp, shell)?;
    let phys = TypePhysical::range(interp, subtype_oid);
    interp.insert_pg_type(PgType {
        oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Range,
        typcategory: TypCategory::Range,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });
    let range_array = register_array_type(interp, nsoid, &name, oid)?;

    let mr_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
    let phys = TypePhysical::range(interp, subtype_oid);
    interp.insert_pg_type(PgType {
        oid: mr_oid,
        typname: mr_name.clone(),
        typnamespace: mr_nsoid,
        typtype: TypType::Multirange,
        typcategory: TypCategory::Range,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });
    register_array_type(interp, mr_nsoid, &mr_name, mr_oid)?;
    interp.insert_pg_range(PgRange {
        rngtypid: oid,
        rngsubtype: subtype_oid,
        rngmultitypid: Some(mr_oid),
    });

    // makeRangeConstructors / makeMultirangeConstructors: `name(sub, sub)`
    // and `name(sub, sub, text)` (not strict — NULL bounds mean infinite),
    // `mr()`, `mr(range)` and `mr(VARIADIC range[])` (strict).
    use crate::pg_catalog::oid as builtin;
    let constructors = [
        (
            &name,
            nsoid,
            vec![subtype_oid, subtype_oid],
            oid,
            false,
            false,
        ),
        (
            &name,
            nsoid,
            vec![subtype_oid, subtype_oid, builtin::TEXT],
            oid,
            false,
            false,
        ),
        (&mr_name, mr_nsoid, vec![], mr_oid, true, false),
        (&mr_name, mr_nsoid, vec![oid], mr_oid, true, false),
        (&mr_name, mr_nsoid, vec![range_array], mr_oid, true, true),
    ];
    for (proname, pronamespace, proargtypes, prorettype, proisstrict, variadic) in constructors {
        let proc_oid = crate::oid::PgProcOid::from_nonzero(interp.alloc_oid()?);
        interp.insert_pg_proc(crate::pg_catalog::PgProc {
            oid: proc_oid,
            proname: proname.to_owned(),
            pronamespace,
            prokind: crate::pg_catalog::ProKind::Function,
            proallargtypes: if variadic {
                proargtypes.clone()
            } else {
                Vec::new()
            },
            proargmodes: if variadic {
                vec![crate::pg_catalog::ArgMode::Variadic]
            } else {
                Vec::new()
            },
            proargtypes,
            prorettype,
            proretset: false,
            provariadic: variadic.then_some(oid),
            proisstrict,
            pronargdefaults: 0,
            proargnames: Vec::new(),
            provolatile: crate::pg_catalog::ProVolatile::Immutable,
            proargdefaulttypes: Vec::new(),
            prolang: crate::pg_catalog::INTERNAL_LANGUAGE,
        });
    }
    super::depend::record(
        interp,
        super::depend::ObjectAddress::type_(oid),
        support.into_iter().map(super::depend::ObjectAddress::proc),
        crate::pg_catalog::DepType::Normal,
    );
    Ok(())
}

/// A range support function `names(args)` (LookupFuncName with exact
/// argument types).
fn range_support_function(
    interp: &PgCatalog,
    names: &[String],
    args: &[PgTypeOid],
) -> Result<PgProcOid, DdlError> {
    super::functions::lookup_func_name(interp, names, Some(args))?.ok_or_else(|| {
        DdlError::TypeNotFound(format!(
            "function {} does not exist",
            super::functions::func_signature_string(interp, names, args)
        ))
    })
}

/// PG's `makeMultirangeTypeName`: the first `range` in the range type's
/// name becomes `multirange` (`floatrange` → `floatmultirange`); a name
/// without it gets `_multirange` appended (`fr` → `fr_multirange`).
fn make_multirange_type_name(range_name: &str) -> String {
    match range_name.find("range") {
        Some(pos) => format!("{}multi{}", &range_name[..pos], &range_name[pos..]),
        None => super::util::make_object_name(range_name, "", "multirange"),
    }
}

// ─── ALTER TYPE ... ADD VALUE (enum) ────────────────────────────────────────

pub fn alter_enum(interp: &mut PgCatalog, stmt: &AlterEnumStmt) -> Result<(), DdlError> {
    let key = names_key(&stmt.type_name, interp);
    let nsoid = match interp.namespace_oid(&key.schema) {
        Some(oid) => oid,
        None => {
            return Err(DdlError::TypeNotFound(format!(
                "type \"{}\" does not exist",
                key.name
            )));
        }
    };

    let Some(&oid) = interp.type_by_qname.get(&(nsoid, key.name.clone())) else {
        return Err(DdlError::TypeNotFound(format!(
            "type \"{}\" does not exist",
            key.name
        )));
    };

    // checkEnumOwner.
    if !matches!(
        interp.pg_type.get(&oid).map(|t| t.typtype),
        Some(TypType::Enum)
    ) {
        return Err(DdlError::Parse(format!(
            "{} is not an enum",
            super::util::format_type_for_message(interp, oid)
        )));
    }

    // AddEnumLabel / RenameEnumLabel check the new label's length first.
    check_enum_label(&stmt.new_val)?;
    let not_a_label =
        |label: &str| DdlError::Parse(format!("\"{label}\" is not an existing enum label"));
    let labels = interp.pg_enum.entry(oid).or_default();

    // `RENAME VALUE old TO new` (RenameEnumLabel).
    if !stmt.old_val.is_empty() {
        if labels.iter().any(|e| e.enumlabel == stmt.new_val) {
            return Err(DdlError::DuplicateObject(format!(
                "enum label \"{}\" already exists",
                stmt.new_val
            )));
        }
        let Some(label) = labels.iter_mut().find(|e| e.enumlabel == stmt.old_val) else {
            return Err(not_a_label(&stmt.old_val));
        };
        label.enumlabel = stmt.new_val.clone();
        if interp
            .uncommitted_enum_labels
            .remove(&(oid, stmt.old_val.clone()))
        {
            interp
                .uncommitted_enum_labels
                .insert((oid, stmt.new_val.clone()));
        }
        return Ok(());
    }

    if labels.iter().any(|e| e.enumlabel == stmt.new_val) {
        if stmt.skip_if_new_val_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "enum label \"{}\" already exists",
            stmt.new_val
        )));
    }

    let new_sortorder = if stmt.new_val_neighbor.is_empty() {
        labels
            .iter()
            .map(|e| e.enumsortorder)
            .fold(0.0_f32, f32::max)
            + 1.0
    } else if let Some(neighbor) = labels.iter().find(|e| e.enumlabel == stmt.new_val_neighbor) {
        let neighbor_order = neighbor.enumsortorder;
        if stmt.new_val_is_after {
            // Insert immediately after: midpoint with the next-higher
            // sortorder, or neighbor + 1 if neighbor is last.
            let next = labels
                .iter()
                .filter(|e| e.enumsortorder > neighbor_order)
                .map(|e| e.enumsortorder)
                .fold(f32::INFINITY, f32::min);
            if next.is_finite() {
                (neighbor_order + next) / 2.0
            } else {
                neighbor_order + 1.0
            }
        } else {
            // Insert immediately before: midpoint with the previous-lower
            // sortorder, or neighbor - 1 if neighbor is first.
            let prev = labels
                .iter()
                .filter(|e| e.enumsortorder < neighbor_order)
                .map(|e| e.enumsortorder)
                .fold(f32::NEG_INFINITY, f32::max);
            if prev.is_finite() {
                (neighbor_order + prev) / 2.0
            } else {
                neighbor_order - 1.0
            }
        }
    } else {
        return Err(not_a_label(&stmt.new_val_neighbor));
    };

    // check_safe_enum_use: until the transaction commits, the new label
    // is unusable unless the type is new too.
    if !interp.enums_created_in_transaction.contains(&oid) {
        interp
            .uncommitted_enum_labels
            .insert((oid, stmt.new_val.clone()));
    }
    let enum_oid = PgEnumOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_enum(PgEnum {
        oid: enum_oid,
        enumtypid: oid,
        enumsortorder: new_sortorder,
        enumlabel: stmt.new_val.clone(),
    });

    Ok(())
}

// ─── DefineStmt: CREATE TYPE name / CREATE TYPE name (...) ──────────────────

/// Handle `DefineStmt` which covers shell types (`CREATE TYPE citext;`) and
/// full type definitions (`CREATE TYPE citext (INPUT = ..., OUTPUT = ...)`)
/// (DefineType): a full definition fills in a shell made beforehand.
pub fn define_type(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let obj_type = ObjectType::try_from(stmt.kind).unwrap_or(ObjectType::Undefined);
    if obj_type != ObjectType::ObjectType {
        return Ok(());
    }

    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;
    let already_exists = || DdlError::DuplicateObject(format!("type \"{name}\" already exists"));
    let mut typoid = interp.type_by_qname.get(&(nsoid, name.clone())).copied();
    if let Some(oid) = typoid
        && interp.pg_type.get(&oid).is_some_and(|t| t.typisdefined)
    {
        if !move_array_type_name(interp, oid, &name, nsoid) {
            return Err(already_exists());
        }
        typoid = None;
    }
    if stmt.definition.is_empty() {
        if typoid.is_some() {
            return Err(already_exists());
        }
        create_base_type(interp, nsoid, &name)?;
        return Ok(());
    }
    let Some(oid) = typoid else {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" does not exist (Create the type as a shell type, then create its \
             I/O functions, then do a full CREATE TYPE.)"
        )));
    };
    define_base_type(interp, nsoid, &name, oid, &stmt.definition)
}

/// The value of a `CREATE TYPE` option, as DefineType's `defGetString` reads
/// it (a name's last component, a string or a number), lowercased.
fn option_word(de: &typedpg_pg_query::protobuf::DefElem) -> Option<String> {
    match de.arg.as_deref().and_then(|a| a.node.as_ref())? {
        node::Node::TypeName(tn) => tn
            .names
            .last()
            .and_then(super::util::node_string)
            .map(str::to_ascii_lowercase),
        node::Node::String(s) => Some(s.sval.to_ascii_lowercase()),
        node::Node::Integer(i) => Some(i.ival.to_string()),
        _ => None,
    }
}

/// `defGetQualifiedName` of a support-function option.
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

/// One of a base type's support functions (`findTypeInputFunction` and
/// friends): `names` taking one of `signatures` (the first is the one error
/// messages show), returning `rettype`.
fn find_type_support_function(
    interp: &PgCatalog,
    what: &str,
    names: &[String],
    signatures: &[&[PgTypeOid]],
    rettype: PgTypeOid,
) -> Result<PgProcOid, DdlError> {
    let mut found = None;
    for args in signatures {
        if let Some(oid) = super::functions::lookup_func_name(interp, names, Some(args))? {
            if found.is_some() {
                return Err(DdlError::Parse(format!(
                    "type {what} function {} has multiple matches",
                    names.join(".")
                )));
            }
            found = Some(oid);
        }
    }
    let Some(oid) = found else {
        return Err(DdlError::TypeNotFound(format!(
            "function {} does not exist",
            super::functions::func_signature_string(interp, names, signatures[0])
        )));
    };
    if interp.pg_proc.get(&oid).map(|p| p.prorettype) != Some(rettype) {
        return Err(DdlError::Parse(format!(
            "type {what} function {} must return type {}",
            names.join("."),
            super::util::format_type_for_message(interp, rettype)
        )));
    }
    Ok(oid)
}

/// The full `CREATE TYPE name (...)` of a base type (DefineType +
/// TypeCreate): each option once; INPUT and OUTPUT are required, and every
/// support function takes and returns what its role needs; the physical
/// layout — `LIKE` first, then INTERNALLENGTH, PASSEDBYVALUE, ALIGNMENT and
/// STORAGE over it — must be consistent. The shell becomes a defined base
/// type depending on its functions, and gets its array type.
fn define_base_type(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: &str,
    oid: PgTypeOid,
    options: &[typedpg_pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    let defs: Vec<&typedpg_pg_query::protobuf::DefElem> = options
        .iter()
        .filter_map(|o| match o.node.as_ref() {
            Some(node::Node::DefElem(de)) => Some(de.as_ref()),
            _ => None,
        })
        .collect();
    const KNOWN: &[&str] = &[
        "like",
        "internallength",
        "input",
        "output",
        "receive",
        "send",
        "typmod_in",
        "typmod_out",
        "analyze",
        "analyse",
        "subscript",
        "category",
        "preferred",
        "delimiter",
        "element",
        "default",
        "passedbyvalue",
        "alignment",
        "storage",
        "collatable",
    ];
    let canonical = |n: &str| if n == "analyse" { "analyze" } else { n }.to_owned();
    let mut seen: Vec<String> = Vec::new();
    for de in &defs {
        if !KNOWN.contains(&de.defname.as_str()) {
            // Only a WARNING in PG.
            continue;
        }
        let key = canonical(&de.defname);
        if seen.contains(&key) {
            return Err(DdlError::Parse("conflicting or redundant options".into()));
        }
        seen.push(key);
    }
    let get = |n: &str| defs.iter().copied().find(|d| canonical(&d.defname) == n);

    let mut phys = TypePhysical::VARIABLE_BASE;
    let mut storage = TypStorage::Plain;
    if let Some(like) = get("like")
        && let Some(node::Node::TypeName(tn)) = like.arg.as_deref().and_then(|a| a.node.as_ref())
    {
        let like_oid = super::functions::typename_type_id(interp, tn)?;
        if let Some(t) = interp.pg_type.get(&like_oid) {
            phys = TypePhysical {
                typlen: t.typlen,
                typbyval: t.typbyval,
                typalign: t.typalign,
                typsubscript: None,
            };
            storage = t.typstorage;
        }
    }
    if let Some(de) = get("internallength") {
        phys.typlen = match option_word(de).as_deref() {
            Some("variable") | None => -1,
            Some(n) => n.parse().unwrap_or(-1),
        };
    }
    if let Some(de) = get("element")
        && let Some(node::Node::TypeName(tn)) = de.arg.as_deref().and_then(|a| a.node.as_ref())
    {
        let elem = super::functions::typename_type_id(interp, tn)?;
        if interp.pg_type.get(&elem).map(|t| t.typtype) == Some(TypType::Pseudo) {
            return Err(DdlError::Parse(format!(
                "array element type cannot be {}",
                super::util::format_type_for_message(interp, elem)
            )));
        }
    }
    if get("passedbyvalue").is_some() {
        phys.typbyval = true;
    }
    if let Some(de) = get("alignment") {
        phys.typalign = match option_word(de).as_deref() {
            Some("double" | "float8") => TypAlign::Double,
            Some("int4" | "integer") => TypAlign::Int,
            Some("int2" | "smallint") => TypAlign::Short,
            Some("char" | "bpchar") => TypAlign::Char,
            other => {
                return Err(DdlError::Parse(format!(
                    "alignment \"{}\" not recognized",
                    other.unwrap_or_default()
                )));
            }
        };
    }
    if let Some(de) = get("storage") {
        storage = parse_storage(de).ok_or_else(|| {
            DdlError::Parse(format!(
                "storage \"{}\" not recognized",
                option_word(de).unwrap_or_default()
            ))
        })?;
    }
    let names_of = |n: &str| get(n).map(option_names);
    let Some(input) = names_of("input") else {
        return Err(DdlError::Parse(
            "type input function must be specified".into(),
        ));
    };
    let Some(output) = names_of("output") else {
        return Err(DdlError::Parse(
            "type output function must be specified".into(),
        ));
    };
    if get("typmod_in").is_none() && get("typmod_out").is_some() {
        return Err(DdlError::Parse(
            "type modifier output function is useless without a type modifier input function"
                .into(),
        ));
    }
    use crate::pg_catalog::oid as builtin;
    const CSTRING: PgTypeOid = PgTypeOid::from_raw(2275);
    const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);
    const CSTRING_ARRAY: PgTypeOid = PgTypeOid::from_raw(1263);
    let mut functions = vec![
        find_type_support_function(
            interp,
            "input",
            &input,
            &[&[CSTRING], &[CSTRING, builtin::OID, builtin::INT4]],
            oid,
        )?,
        find_type_support_function(interp, "output", &output, &[&[oid]], CSTRING)?,
    ];
    if let Some(receive) = names_of("receive") {
        functions.push(find_type_support_function(
            interp,
            "receive",
            &receive,
            &[&[INTERNAL], &[INTERNAL, builtin::OID, builtin::INT4]],
            oid,
        )?);
    }
    if let Some(send) = names_of("send") {
        functions.push(find_type_support_function(
            interp,
            "send",
            &send,
            &[&[oid]],
            PgTypeOid::from_raw(17), // bytea
        )?);
    }
    if let Some(typmod_in) = names_of("typmod_in") {
        functions.push(find_type_support_function(
            interp,
            "modifier input",
            &typmod_in,
            &[&[CSTRING_ARRAY]],
            builtin::INT4,
        )?);
    }
    if let Some(typmod_out) = names_of("typmod_out") {
        functions.push(find_type_support_function(
            interp,
            "modifier output",
            &typmod_out,
            &[&[builtin::INT4]],
            CSTRING,
        )?);
    }
    if let Some(analyze) = names_of("analyze") {
        functions.push(find_type_support_function(
            interp,
            "analyze",
            &analyze,
            &[&[INTERNAL]],
            builtin::BOOL,
        )?);
    }
    let subscript = match get("subscript") {
        Some(de) => subscript_handler(interp, de)?,
        None => None,
    };
    functions.extend(subscript);

    // TypeCreate's checks of the physical layout.
    if !(phys.typlen > 0 || phys.typlen == -1 || phys.typlen == -2) {
        return Err(DdlError::Parse(format!(
            "invalid type internal size {}",
            phys.typlen
        )));
    }
    if phys.typbyval {
        let align_for = match phys.typlen {
            1 => Some(TypAlign::Char),
            2 => Some(TypAlign::Short),
            4 => Some(TypAlign::Int),
            8 => Some(TypAlign::Double),
            _ => None,
        };
        match align_for {
            None => {
                return Err(DdlError::Parse(format!(
                    "internal size {} is invalid for passed-by-value type",
                    phys.typlen
                )));
            }
            Some(a) if a != phys.typalign => {
                return Err(DdlError::Parse(format!(
                    "alignment \"{}\" is invalid for passed-by-value type of size {}",
                    char::from(phys.typalign.as_char()),
                    phys.typlen
                )));
            }
            _ => {}
        }
    } else if (phys.typlen == -1 && !matches!(phys.typalign, TypAlign::Int | TypAlign::Double))
        || (phys.typlen == -2 && phys.typalign != TypAlign::Char)
    {
        return Err(DdlError::Parse(format!(
            "alignment \"{}\" is invalid for variable-length type",
            char::from(phys.typalign.as_char())
        )));
    }
    if storage != TypStorage::Plain && phys.typlen != -1 {
        return Err(DdlError::Parse(
            "fixed-size types must have storage PLAIN".into(),
        ));
    }

    if let Some(t) = interp.pg_type.get_mut(&oid) {
        t.typtype = TypType::Base;
        t.typcategory = TypCategory::UserDefined;
        t.typisdefined = true;
        t.typlen = phys.typlen;
        t.typbyval = phys.typbyval;
        t.typalign = phys.typalign;
        t.typstorage = storage;
        t.typsubscript = subscript;
    }
    // GenerateTypeDependencies: the type depends on its support functions.
    super::depend::record(
        interp,
        super::depend::ObjectAddress::type_(oid),
        functions
            .into_iter()
            .map(super::depend::ObjectAddress::proc),
        crate::pg_catalog::DepType::Normal,
    );
    let has_array = interp
        .pg_type
        .get(&oid)
        .is_some_and(|t| t.typarray.is_some());
    if !has_array {
        register_array_type(interp, nsoid, name, oid)?;
    }
    Ok(())
}

/// DefineType's `STORAGE = plain | external | extended | main`.
fn parse_storage(de: &typedpg_pg_query::protobuf::DefElem) -> Option<TypStorage> {
    Some(match option_word(de)?.as_str() {
        "external" => TypStorage::External,
        "extended" => TypStorage::Extended,
        "main" => TypStorage::Main,
        "plain" => TypStorage::Plain,
        _ => return None,
    })
}

/// findTypeSubscriptingFunction: `SUBSCRIPT = handler` names a function
/// taking `internal`; `none` clears it; the built-in array handlers are
/// reserved for true arrays.
fn subscript_handler(
    interp: &PgCatalog,
    de: &typedpg_pg_query::protobuf::DefElem,
) -> Result<Option<PgProcOid>, DdlError> {
    let names: Vec<&str> = match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
        Some(node::Node::TypeName(tn)) => tn
            .names
            .iter()
            .filter_map(super::util::node_string)
            .collect(),
        Some(node::Node::String(s)) => vec![s.sval.as_str()],
        _ => return Ok(None),
    };
    let (schema, name) = match names.as_slice() {
        [n] => (None, *n),
        [s, n] => (Some(*s), *n),
        _ => return Ok(None),
    };
    if schema.is_none() && name.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);
    let Some(proc) = interp
        .find_functions(schema, name)
        .into_iter()
        .find(|p| p.proargtypes == [INTERNAL])
    else {
        return Err(DdlError::TypeNotFound(format!(
            "function {}(internal) does not exist",
            names.join(".")
        )));
    };
    if matches!(
        proc.proname.as_str(),
        "array_subscript_handler" | "raw_array_subscript_handler"
    ) && interp.namespace_name(proc.pronamespace) == Some("pg_catalog")
    {
        return Err(DdlError::Parse(format!(
            "user-defined types cannot use subscripting function {}",
            proc.proname
        )));
    }
    Ok(Some(proc.oid))
}

/// `ALTER TYPE name SET (...)` (AlterType): the type must be a (non-array)
/// base type; STORAGE is checked against its layout and, with SUBSCRIPT,
/// changes its row; the support functions must fit their roles; the other
/// CREATE TYPE attributes can't be changed.
pub fn alter_type(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterTypeStmt,
) -> Result<(), DdlError> {
    let tn = TypeName {
        names: stmt.type_name.clone(),
        typemod: -1,
        ..Default::default()
    };
    let oid = super::functions::typename_type_id(interp, &tn)?;
    let Some(t) = interp.pg_type.get(&oid).cloned() else {
        return Ok(());
    };
    let mut storage = None;
    let mut subscript = None;
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        // An option without a value means NONE.
        let has_value = de.arg.is_some();
        match de.defname.as_str() {
            "storage" => {
                let requested = parse_storage(de).ok_or_else(|| {
                    DdlError::Parse(format!(
                        "storage \"{}\" not recognized",
                        option_word(de).unwrap_or_default()
                    ))
                })?;
                if requested != TypStorage::Plain && t.typlen != -1 {
                    return Err(DdlError::Parse(
                        "fixed-size types must have storage PLAIN".into(),
                    ));
                }
                if requested == TypStorage::Plain && t.typstorage != TypStorage::Plain {
                    return Err(DdlError::Parse(
                        "cannot change type's storage to PLAIN".into(),
                    ));
                }
                storage = Some(requested);
            }
            "receive" | "send" | "typmod_in" | "typmod_out" | "analyze" | "analyse"
                if has_value =>
            {
                use crate::pg_catalog::oid as builtin;
                const CSTRING: PgTypeOid = PgTypeOid::from_raw(2275);
                const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);
                const CSTRING_ARRAY: PgTypeOid = PgTypeOid::from_raw(1263);
                let names = option_names(de);
                match de.defname.as_str() {
                    "receive" => find_type_support_function(
                        interp,
                        "receive",
                        &names,
                        &[&[INTERNAL], &[INTERNAL, builtin::OID, builtin::INT4]],
                        oid,
                    ),
                    "send" => find_type_support_function(
                        interp,
                        "send",
                        &names,
                        &[&[oid]],
                        PgTypeOid::from_raw(17), // bytea
                    ),
                    "typmod_in" => find_type_support_function(
                        interp,
                        "modifier input",
                        &names,
                        &[&[CSTRING_ARRAY]],
                        builtin::INT4,
                    ),
                    "typmod_out" => find_type_support_function(
                        interp,
                        "modifier output",
                        &names,
                        &[&[builtin::INT4]],
                        CSTRING,
                    ),
                    _ => find_type_support_function(
                        interp,
                        "analyze",
                        &names,
                        &[&[INTERNAL]],
                        builtin::BOOL,
                    ),
                }?;
            }
            "receive" | "send" | "typmod_in" | "typmod_out" | "analyze" | "analyse" => {}
            "subscript" => subscript = Some(subscript_handler(interp, de)?),
            "internallength" | "input" | "output" | "category" | "preferred" | "default"
            | "element" | "delimiter" | "collatable" => {
                return Err(DdlError::Parse(format!(
                    "type attribute \"{}\" cannot be changed",
                    de.defname
                )));
            }
            other => {
                return Err(DdlError::Parse(format!(
                    "type attribute \"{other}\" not recognized"
                )));
            }
        }
    }
    let is_true_array = t.typelem.is_some_and(|e| {
        t.typcategory == TypCategory::Array && interp.array_type_of(e) == Some(oid)
    });
    if t.typtype != TypType::Base || is_true_array {
        return Err(DdlError::UnsupportedDdl(format!(
            "{} is not a base type",
            super::util::format_type_for_message(interp, oid)
        )));
    }
    if let Some(row) = interp.pg_type.get_mut(&oid) {
        if let Some(storage) = storage {
            row.typstorage = storage;
        }
        if let Some(subscript) = subscript {
            row.typsubscript = subscript;
        }
    }
    Ok(())
}

/// Register a shell type (`CREATE TYPE name;`, or the not-yet-defined result
/// type of a C function).
pub(crate) fn create_base_type(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: &str,
) -> Result<PgTypeOid, DdlError> {
    // TypeShellMake: a pseudo-type placeholder, not yet defined and without
    // an array type (the full CREATE TYPE adds it).
    let oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
    let phys = TypePhysical::SHELL;
    interp.insert_pg_type(PgType {
        oid,
        typname: name.to_owned(),
        typnamespace: nsoid,
        typtype: TypType::Pseudo,
        typcategory: TypCategory::Pseudo,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Plain,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: false,
    });
    Ok(oid)
}

// ─── CREATE CAST ────────────────────────────────────────────────────────────

/// `CREATE CAST (source AS target) ...` (CreateCast, functioncmds.c, and
/// CastCreate): no pseudo-types; a cast function takes the source (or
/// something it is binary-coercible to), an optional int4 typmod and bool
/// explicit flag, and returns the target (or something binary-coercible to
/// it) — a plain, non-set function; WITHOUT FUNCTION needs physically
/// identical, binary-compatible types.
pub fn create_cast(interp: &mut PgCatalog, stmt: &CreateCastStmt) -> Result<(), DdlError> {
    let (Some(source), Some(target)) = (stmt.sourcetype.as_ref(), stmt.targettype.as_ref()) else {
        return Ok(());
    };
    let src = super::functions::typename_type_id(interp, source)?;
    let tgt = super::functions::typename_type_id(interp, target)?;
    let typtype = |oid: PgTypeOid| interp.pg_type.get(&oid).map(|t| t.typtype);
    let shown = |oid: PgTypeOid| super::util::format_type_for_message(interp, oid);
    let invalid = |msg: &str| Err(DdlError::Parse(msg.to_owned()));
    if typtype(src) == Some(TypType::Pseudo) {
        return Err(DdlError::Parse(format!(
            "source data type {} is a pseudo-type",
            shown(src)
        )));
    }
    if typtype(tgt) == Some(TypType::Pseudo) {
        return Err(DdlError::Parse(format!(
            "target data type {} is a pseudo-type",
            shown(tgt)
        )));
    }

    // Map `CREATE CAST` syntax to pg_cast.castmethod:
    // - WITH FUNCTION f(...)  → 'f' (Function)
    // - WITH INOUT            → 'i' (InOut)
    // - WITHOUT FUNCTION      → 'b' (Binary)
    let castmethod = if stmt.inout {
        CastMethod::InOut
    } else if stmt.func.is_some() {
        CastMethod::Function
    } else {
        CastMethod::Binary
    };
    let mut nargs = 0;
    let mut castfunc = None;
    if let Some(func) = stmt.func.as_ref() {
        let Some(oid) = super::functions::lookup_func_with_args(
            interp,
            ObjectType::ObjectFunction,
            func,
            false,
        )?
        else {
            return Ok(());
        };
        let Some(proc) = interp.pg_proc.get(&oid) else {
            return Ok(());
        };
        nargs = proc.proargtypes.len();
        if !(1..=3).contains(&nargs) {
            return invalid("cast function must take one to three arguments");
        }
        if !super::functions::is_binary_coercible(interp, src, proc.proargtypes[0]) {
            return invalid(
                "argument of cast function must match or be binary-coercible from source data \
                 type",
            );
        }
        if nargs > 1 && proc.proargtypes[1] != crate::pg_catalog::oid::INT4 {
            return invalid("second argument of cast function must be type integer");
        }
        if nargs > 2 && proc.proargtypes[2] != crate::pg_catalog::oid::BOOL {
            return invalid("third argument of cast function must be type boolean");
        }
        if !super::functions::is_binary_coercible(interp, proc.prorettype, tgt) {
            return invalid(
                "return data type of cast function must match or be binary-coercible to target \
                 data type",
            );
        }
        if proc.prokind != crate::pg_catalog::ProKind::Function {
            return invalid("cast function must be a normal function");
        }
        if proc.proretset {
            return invalid("cast function must not return a set");
        }
        castfunc = Some(oid);
    }
    if matches!(castmethod, CastMethod::Binary) {
        let physical = |oid: PgTypeOid| {
            interp
                .pg_type
                .get(&oid)
                .map(|t| (t.typlen, t.typbyval, t.typalign))
        };
        if physical(src) != physical(tgt) {
            return invalid("source and target data types are not physically compatible");
        }
        let kinds = [typtype(src), typtype(tgt)];
        if kinds.contains(&Some(TypType::Composite)) {
            return invalid("composite data types are not binary-compatible");
        }
        if crate::coerce::element_type(src, interp).is_some()
            || crate::coerce::element_type(tgt, interp).is_some()
        {
            return invalid("array data types are not binary-compatible");
        }
        if kinds.contains(&Some(TypType::Range)) || kinds.contains(&Some(TypType::Multirange)) {
            return invalid("range data types are not binary-compatible");
        }
        if kinds.contains(&Some(TypType::Enum)) {
            return invalid("enum data types are not binary-compatible");
        }
        if kinds.contains(&Some(TypType::Domain)) {
            return invalid("domain data types must not be marked binary-compatible");
        }
    }
    // Only a length coercion function (more than one argument) may cast a
    // type to itself.
    if src == tgt && nargs < 2 {
        return invalid("source data type and target data type are the same");
    }

    let castcontext = match CoercionContext::try_from(stmt.context) {
        Ok(CoercionContext::CoercionImplicit) => CastContext::Implicit,
        Ok(CoercionContext::CoercionAssignment) => CastContext::Assignment,
        _ => CastContext::Explicit,
    };
    // CastCreate.
    if interp.cast_by_pair.contains_key(&(src, tgt)) {
        return Err(DdlError::DuplicateObject(format!(
            "cast from type {} to type {} already exists",
            shown(src),
            shown(tgt)
        )));
    }
    let cast_oid = PgCastOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_cast(PgCast {
        oid: cast_oid,
        castsource: src,
        casttarget: tgt,
        castcontext,
        castmethod,
        castfunc,
    });
    // The cast depends on its function and both types.
    super::depend::record(
        interp,
        super::depend::ObjectAddress::cast(cast_oid),
        castfunc
            .map(super::depend::ObjectAddress::proc)
            .into_iter()
            .chain([
                super::depend::ObjectAddress::type_(src),
                super::depend::ObjectAddress::type_(tgt),
            ]),
        crate::pg_catalog::DepType::Normal,
    );
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// `pg_type`'s physical columns, as PostgreSQL assigns them to each kind of
/// type it creates.
pub(crate) struct TypePhysical {
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: TypAlign,
    pub typsubscript: Option<PgProcOid>,
}

impl TypePhysical {
    /// DefineEnum: a 4-byte OID, passed by value.
    pub(crate) const ENUM: Self = Self {
        typlen: 4,
        typbyval: true,
        typalign: TypAlign::Int,
        typsubscript: None,
    };
    /// A row type (CREATE TYPE AS, a table's or view's): a varlena record.
    pub(crate) const COMPOSITE: Self = Self {
        typlen: -1,
        typbyval: false,
        typalign: TypAlign::Double,
        typsubscript: None,
    };
    /// TypeShellMake: a placeholder until the full CREATE TYPE.
    pub(crate) const SHELL: Self = Self {
        typlen: 4,
        typbyval: true,
        typalign: TypAlign::Int,
        typsubscript: None,
    };
    /// DefineType's defaults for a full base type: INTERNALLENGTH =
    /// VARIABLE, not PASSEDBYVALUE, ALIGNMENT = int4.
    pub(crate) const VARIABLE_BASE: Self = Self {
        typlen: -1,
        typbyval: false,
        typalign: TypAlign::Int,
        typsubscript: None,
    };

    /// A domain stores its values like its base type (DefineDomain), but has
    /// no subscripting handler of its own: subscripting a domain goes through
    /// its base type.
    pub(crate) fn of(interp: &PgCatalog, base: PgTypeOid) -> Self {
        interp
            .pg_type
            .get(&base)
            .map_or(Self::VARIABLE_BASE, |t| Self {
                typlen: t.typlen,
                typbyval: t.typbyval,
                typalign: t.typalign,
                typsubscript: None,
            })
    }

    /// A range or multirange over `subtype` (DefineRange).
    pub(crate) fn range(interp: &PgCatalog, subtype: PgTypeOid) -> Self {
        Self {
            typalign: Self::of(interp, subtype).typalign.of_container(),
            ..Self::VARIABLE_BASE
        }
    }

    /// The array type of `element`: a varlena whose alignment follows the
    /// element's, subscripted by `array_subscript_handler`.
    pub(crate) fn array(interp: &PgCatalog, element: PgTypeOid) -> Self {
        Self {
            typalign: Self::of(interp, element).typalign.of_container(),
            typsubscript: interp
                .find_functions(Some("pg_catalog"), "array_subscript_handler")
                .first()
                .map(|p| p.oid),
            ..Self::VARIABLE_BASE
        }
    }
}

/// Register an array type (`_name`) for a base type, and back-link the
/// element type's `typarray` to it so `array_type_of(element)` resolves.
fn register_array_type(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    base_name: &str,
    element_oid: PgTypeOid,
) -> Result<PgTypeOid, DdlError> {
    let array_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
    let phys = TypePhysical::array(interp, element_oid);
    let typname = make_array_type_name(interp, nsoid, base_name);
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname,
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::Array,
        typispreferred: false,
        typrelid: None,
        typelem: Some(element_oid),
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });
    if let Some(elem) = interp.pg_type.get_mut(&element_oid) {
        elem.typarray = Some(array_oid);
    }
    Ok(array_oid)
}

/// PG's `makeArrayTypeName` (pg_type.c): `_name` (truncated to fit an
/// identifier), with `_1`, `_2`, ... appended until no type of the schema
/// has it.
pub(crate) fn make_array_type_name(
    interp: &PgCatalog,
    nsoid: PgNamespaceOid,
    type_name: &str,
) -> String {
    let mut pass = 0;
    loop {
        let label = if pass == 0 {
            String::new()
        } else {
            pass.to_string()
        };
        let name = super::util::make_object_name("", type_name, &label);
        if !interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
            return name;
        }
        pass += 1;
    }
}

/// `moveArrayTypeName` (pg_type.c): an autogenerated array type in the way
/// of a new type named `type_name` is renamed out of the way. `false` when
/// `type_oid` isn't one (a shell needs nothing and counts as moved).
fn move_array_type_name(
    interp: &mut PgCatalog,
    type_oid: PgTypeOid,
    type_name: &str,
    nsoid: PgNamespaceOid,
) -> bool {
    let Some(t) = interp.pg_type.get(&type_oid) else {
        return false;
    };
    if !t.typisdefined {
        return true;
    }
    let is_autogenerated_array = t.typelem.is_some_and(|elem| {
        t.typcategory == TypCategory::Array && interp.array_type_of(elem) == Some(type_oid)
    });
    if !is_autogenerated_array {
        return false;
    }
    let new_name = make_array_type_name(interp, nsoid, type_name);
    interp.rename_pg_type(type_oid, new_name, nsoid);
    true
}

/// The name check every CREATE of a type makes (`TypeCreate` / the
/// `moveArrayTypeName` calls of DefineEnum, DefineRange, ...): an existing
/// type of that name is an error — unless it is an autogenerated array
/// type, which is renamed away, or a shell, which the new type fills in
/// (keeping its OID, returned here).
pub(crate) fn claim_type_name(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: &str,
) -> Result<Option<PgTypeOid>, DdlError> {
    let Some(&existing) = interp.type_by_qname.get(&(nsoid, name.to_owned())) else {
        return Ok(None);
    };
    if !move_array_type_name(interp, existing, name, nsoid) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }
    let shell = interp
        .pg_type
        .get(&existing)
        .is_some_and(|t| !t.typisdefined);
    if shell {
        interp.remove_pg_type(existing);
        return Ok(Some(existing));
    }
    Ok(None)
}

/// A new type's OID: the shell's it fills in, or a fresh one.
fn new_type_oid(interp: &mut PgCatalog, shell: Option<PgTypeOid>) -> Result<PgTypeOid, DdlError> {
    match shell {
        Some(oid) => Ok(oid),
        None => Ok(PgTypeOid::from_nonzero(interp.alloc_oid()?)),
    }
}

/// The `"x" is not a domain` check of the commands that name a DOMAIN
/// (DROP / COMMENT ON / GRANT ... ON DOMAIN, ALTER DOMAIN): `names` must
/// resolve to a domain. Other lookup failures are left to the caller.
pub(crate) fn check_is_domain(
    interp: &PgCatalog,
    names: &[typedpg_pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    let parts: Vec<&str> = names.iter().filter_map(node_string).collect();
    let (schema, name) = match parts.as_slice() {
        [n] => (None, *n),
        [s, n] => (Some(*s), *n),
        _ => return Ok(()),
    };
    if let Some(t) = interp.resolve_type_by_name(schema, name)
        && t.typtype != TypType::Domain
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "\"{}\" is not a domain",
            parts.join(".")
        )));
    }
    Ok(())
}

/// An enum label is a `name`: at most NAMEDATALEN - 1 bytes
/// (`EnumValuesCreate`, `AddEnumLabel`, `RenameEnumLabel`).
fn check_enum_label(label: &str) -> Result<(), DdlError> {
    if label.len() > 63 {
        return Err(DdlError::Parse(format!(
            "invalid enum label \"{label}\" (Labels must be 63 bytes or less.)"
        )));
    }
    Ok(())
}

/// The type an `ALTER TYPE / DOMAIN name ...` names (`typenameTypeId`), and
/// the checks RenameType / AlterTypeNamespace make on it: a domain for
/// ALTER DOMAIN, not a table's row type, not an array type. `Ok(None)` for
/// a missing type with `missing_ok`.
fn alterable_type(
    interp: &PgCatalog,
    object: Option<&typedpg_pg_query::protobuf::Node>,
    objtype: ObjectType,
    missing_ok: bool,
) -> Result<Option<PgTypeOid>, DdlError> {
    let tn = match object.and_then(|o| o.node.as_ref()) {
        Some(node::Node::TypeName(tn)) => tn.clone(),
        Some(node::Node::List(list)) => TypeName {
            names: list.items.clone(),
            typemod: -1,
            ..Default::default()
        },
        _ => return Ok(None),
    };
    let oid = match super::functions::typename_type_id(interp, &tn) {
        Ok(oid) => oid,
        Err(_) if missing_ok => return Ok(None),
        Err(e) => return Err(e),
    };
    let Some(t) = interp.pg_type.get(&oid) else {
        return Ok(None);
    };
    let shown = |t: PgTypeOid| super::util::format_type_for_message(interp, t);
    if objtype == ObjectType::ObjectDomain && t.typtype != TypType::Domain {
        return Err(DdlError::UnsupportedDdl(format!(
            "{} is not a domain",
            shown(oid)
        )));
    }
    if t.typtype == TypType::Composite
        && t.typrelid
            .and_then(|r| interp.pg_class.get(&r))
            .is_some_and(|c| c.relkind != RelKind::CompositeType)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "{} is a table's row type (Use ALTER TABLE instead.)",
            shown(oid)
        )));
    }
    if let Some(elem) = t.typelem
        && t.typcategory == TypCategory::Array
        && interp.array_type_of(elem) == Some(oid)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter array type {} (You can alter type {}, which will alter the array type \
             as well.)",
            shown(oid),
            shown(elem)
        )));
    }
    Ok(Some(oid))
}

/// `RenameTypeInternal` (pg_type.c): the new name must be free — an
/// autogenerated array type in the way moves aside — and the array type
/// follows with a name derived from the new one.
fn rename_type_internal(
    interp: &mut PgCatalog,
    oid: PgTypeOid,
    new_name: &str,
    nsoid: PgNamespaceOid,
) -> Result<(), DdlError> {
    if let Some(&existing) = interp.type_by_qname.get(&(nsoid, new_name.to_owned()))
        && existing != oid
    {
        let defined = interp
            .pg_type
            .get(&existing)
            .is_some_and(|t| t.typisdefined);
        if !(defined && move_array_type_name(interp, existing, new_name, nsoid)) {
            return Err(DdlError::DuplicateObject(format!(
                "type \"{new_name}\" already exists"
            )));
        }
    }
    interp.rename_pg_type(oid, new_name.to_owned(), nsoid);
    if let Some(array) = interp.pg_type.get(&oid).and_then(|t| t.typarray) {
        let array_name = make_array_type_name(interp, nsoid, new_name);
        interp.rename_pg_type(array, array_name, nsoid);
    }
    Ok(())
}

/// `ALTER TYPE / DOMAIN name RENAME TO new` (RenameType). A free-standing
/// composite type's relation is renamed with it.
pub(crate) fn rename_type(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let objtype = ObjectType::try_from(stmt.rename_type).unwrap_or(ObjectType::ObjectType);
    let Some(oid) = alterable_type(interp, stmt.object.as_deref(), objtype, stmt.missing_ok)?
    else {
        return Ok(());
    };
    let Some(t) = interp.pg_type.get(&oid).cloned() else {
        return Ok(());
    };
    if let Some(relid) = t.typrelid {
        super::util::check_relation_name_free(interp, t.typnamespace, &stmt.newname)?;
        interp.rename_pg_class(relid, stmt.newname.clone(), t.typnamespace);
    }
    rename_type_internal(interp, oid, &stmt.newname, t.typnamespace)
}

/// `ALTER TYPE / DOMAIN name SET SCHEMA s` (AlterTypeNamespace): no type of
/// that name may be in the new schema; the array type (and a composite's
/// relation) move along.
pub(crate) fn set_type_schema(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterObjectSchemaStmt,
    new_nsoid: PgNamespaceOid,
) -> Result<(), DdlError> {
    let objtype = ObjectType::try_from(stmt.object_type).unwrap_or(ObjectType::ObjectType);
    let Some(oid) = alterable_type(interp, stmt.object.as_deref(), objtype, stmt.missing_ok)?
    else {
        return Ok(());
    };
    let Some(t) = interp.pg_type.get(&oid).cloned() else {
        return Ok(());
    };
    if t.typnamespace == new_nsoid {
        return Ok(());
    }
    let taken = |interp: &PgCatalog, name: &str| {
        interp
            .type_by_qname
            .contains_key(&(new_nsoid, name.to_owned()))
    };
    let already_exists = |name: &str| {
        DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists in schema \"{}\"",
            interp.namespace_name(new_nsoid).unwrap_or_default()
        ))
    };
    if taken(interp, &t.typname) {
        return Err(already_exists(&t.typname));
    }
    let array = t
        .typarray
        .and_then(|a| interp.pg_type.get(&a))
        .map(|a| (a.oid, a.typname.clone()));
    if let Some((_, array_name)) = &array
        && taken(interp, array_name)
    {
        return Err(already_exists(array_name));
    }
    if let Some(relid) = t.typrelid {
        interp.rename_pg_class(relid, t.typname.clone(), new_nsoid);
    }
    interp.rename_pg_type(oid, t.typname.clone(), new_nsoid);
    if let Some((array_oid, array_name)) = array {
        interp.rename_pg_type(array_oid, array_name, new_nsoid);
    }
    Ok(())
}
