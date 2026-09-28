//! CREATE TYPE / CREATE DOMAIN / ALTER TYPE DDL handlers.

use pg_query::protobuf::{
    AlterEnumStmt, CoercionContext, CompositeTypeStmt, ConstrType, CreateCastStmt,
    CreateDomainStmt, CreateEnumStmt, CreateRangeStmt, DefineStmt, ObjectType, node,
};

use crate::oid::{PgCastOid, PgClassOid, PgEnumOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, PgAttribute, PgCast, PgClass, PgEnum, PgRange, PgType, RelKind,
    TypCategory, TypType,
};

use super::DdlError;
use super::util::{
    ensure_qualified_name, lookup_type_name, names_key, node_string,
    register_composite_to_record_cast, resolve_type_name,
};
use crate::pg_catalog::PgCatalog;

// ─── CREATE DOMAIN ──────────────────────────────────────────────────────────

pub fn create_domain(interp: &mut PgCatalog, stmt: &CreateDomainStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.domainname)?;

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let base_type_name = stmt
        .type_name
        .as_ref()
        .ok_or_else(|| DdlError::TypeNotFound("domain base type".into()))?;
    let base_type_oid = lookup_type_name(base_type_name, interp)?;
    let typtypmod = crate::typmod::encode(interp, base_type_oid, &base_type_name.typmods)?;

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
    for n in &stmt.constraints {
        let Some(node::Node::Constraint(c)) = n.node.as_ref() else {
            continue;
        };
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

    // `CREATE DOMAIN d AS T COLLATE "x"` pins the domain's default
    // collation. PG validates the name and stores the resolved oid in
    // `pg_type.typcollation`; columns of type `d` later inherit this
    // unless they declare their own `COLLATE`.
    let domain_collation = if let Some(coll) = stmt.coll_clause.as_deref() {
        let parts: Vec<&str> = coll
            .collname
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        let (schema, cname) = match parts.as_slice() {
            [n] => (None, *n),
            [s, n] => (Some(*s), *n),
            _ => return Err(DdlError::Parse("malformed COLLATE clause".into())),
        };
        let resolved = interp
            .resolve_collation(schema, cname)
            .ok_or_else(|| DdlError::Parse(format!("collation \"{cname}\" does not exist")))?;
        Some(resolved.oid)
    } else {
        // Inherit the base type's typcollation.
        interp
            .pg_type
            .get(&base_type_oid)
            .and_then(|t| t.typcollation)
    };

    let oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
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
    });

    register_array_type(interp, nsoid, &name, oid)?;
    interp.domain_constraints.insert(oid, constraints);
    Ok(())
}

/// A named domain constraint (`pg_constraint` row with `contypid` set).
#[derive(Clone, Debug)]
pub(crate) struct DomainConstraint {
    pub(crate) name: String,
    pub(crate) kind: DomainConstraintKind,
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
    c: &pg_query::protobuf::Constraint,
    existing: &mut Vec<DomainConstraint>,
) -> Result<(), DdlError> {
    let kind = match ConstrType::try_from(c.contype) {
        Ok(ConstrType::ConstrNotnull) => DomainConstraintKind::NotNull,
        Ok(ConstrType::ConstrCheck) => {
            if let Some(expr) = c.raw_expr.as_deref() {
                check_domain_check_expression(interp, base_type, expr)?;
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
    existing.push(DomainConstraint { name, kind });
    Ok(())
}

/// A domain CHECK expression sees `VALUE` as a value of the base type and
/// must yield boolean (`domainAddCheckConstraint`).
fn check_domain_check_expression(
    interp: &PgCatalog,
    base_type: PgTypeOid,
    expr: &pg_query::protobuf::Node,
) -> Result<(), DdlError> {
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
    let result = infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(|e| DdlError::UnsupportedDdl(format!("{e} (in domain CHECK constraint)")))?;
    if result.type_oid != oid::BOOL && result.type_oid != oid::UNKNOWN {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of CHECK must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(())
}

// ─── ALTER DOMAIN ───────────────────────────────────────────────────────────

/// `ALTER DOMAIN d { SET | DROP } NOT NULL | ADD constraint | DROP
/// CONSTRAINT name | { SET | DROP } DEFAULT | VALIDATE CONSTRAINT name`
/// (`AlterDomainNotNull` / `AlterDomainAddConstraint` /
/// `AlterDomainDropConstraint`, typecmds.c). NOT NULL changes flip
/// `pg_type.typnotnull`, which every column of the domain reads.
pub fn alter_domain(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::AlterDomainStmt,
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
            let nn = pg_query::protobuf::Constraint {
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
        // SET / DROP DEFAULT: no effect on typing.
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

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    let labels: Vec<String> = stmt
        .vals
        .iter()
        .filter_map(|n| node_string(n).map(|s| s.to_owned()))
        .collect();

    let oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
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

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }
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
            let type_oid = lookup_type_name(tn, interp)?;
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
    let type_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);

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
    });

    register_array_type(interp, nsoid, &name, type_oid)?;
    register_composite_to_record_cast(interp, type_oid)?;

    Ok(())
}

// ─── CREATE TYPE AS RANGE ───────────────────────────────────────────────────

pub fn create_range(interp: &mut PgCatalog, stmt: &CreateRangeStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.type_name)?;

    if interp.type_by_qname.contains_key(&(nsoid, name.clone())) {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{name}\" already exists"
        )));
    }

    // DefineRange (typecmds.c): `subtype` is required; `multirange_type_name`
    // overrides the derived multirange name.
    let mut subtype_oid: Option<PgTypeOid> = None;
    let mut multirange_names: Option<Vec<String>> = None;
    for param_node in &stmt.params {
        let Some(node::Node::DefElem(de)) = param_node.node.as_ref() else {
            continue;
        };
        let arg = de.arg.as_deref().and_then(|a| a.node.as_ref());
        match (de.defname.as_str(), arg) {
            ("subtype", Some(node::Node::TypeName(tn))) => {
                subtype_oid = Some(lookup_type_name(tn, interp)?);
            }
            ("multirange_type_name", Some(node::Node::TypeName(tn))) => {
                multirange_names = Some(
                    tn.names
                        .iter()
                        .filter_map(node_string)
                        .map(str::to_owned)
                        .collect(),
                );
            }
            ("multirange_type_name", Some(node::Node::List(l))) => {
                multirange_names = Some(
                    l.items
                        .iter()
                        .filter_map(node_string)
                        .map(str::to_owned)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    let Some(subtype_oid) = subtype_oid else {
        return Err(DdlError::Parse(
            "type attribute \"subtype\" is required".into(),
        ));
    };
    let (mr_nsoid, mr_name) = match multirange_names.as_deref() {
        Some([schema, mr]) => (super::util::ensure_namespace(interp, schema)?, mr.clone()),
        Some([mr]) => (nsoid, mr.clone()),
        _ => (nsoid, make_multirange_type_name(&name)),
    };
    if interp
        .type_by_qname
        .contains_key(&(mr_nsoid, mr_name.clone()))
    {
        return Err(DdlError::DuplicateObject(format!(
            "type \"{mr_name}\" already exists"
        )));
    }

    let oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
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
    });
    let range_array = register_array_type(interp, nsoid, &name, oid)?;

    let mr_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
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
        });
    }
    Ok(())
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

    if !matches!(
        interp.pg_type.get(&oid).map(|t| t.typtype),
        Some(TypType::Enum)
    ) {
        return Ok(());
    }

    let labels = interp.pg_enum.entry(oid).or_default();
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
        labels
            .iter()
            .map(|e| e.enumsortorder)
            .fold(0.0_f32, f32::max)
            + 1.0
    };

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
/// full type definitions (`CREATE TYPE citext (INPUT = ..., OUTPUT = ...)`).
pub fn define_type(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let obj_type = ObjectType::try_from(stmt.kind).unwrap_or(ObjectType::Undefined);
    if obj_type != ObjectType::ObjectType {
        return Ok(());
    }

    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    let oid = match interp.type_by_qname.get(&(nsoid, name.clone())) {
        // Full definition after shell type — the type already exists.
        Some(&oid) => oid,
        None => create_base_type(interp, nsoid, &name)?,
    };
    record_type_options(interp, oid, &stmt.definition);
    Ok(())
}

/// Record the type options the analyzer uses (`SUBSCRIPT = handler`) from a
/// `CREATE TYPE (...)` / `ALTER TYPE ... SET (...)` option list.
fn record_type_options(
    interp: &mut PgCatalog,
    oid: PgTypeOid,
    options: &[pg_query::protobuf::Node],
) {
    for opt in options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if !de.defname.eq_ignore_ascii_case("subscript") {
            continue;
        }
        let handler = match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
            Some(node::Node::TypeName(tn)) => {
                tn.names.iter().rev().find_map(|n| match n.node.as_ref() {
                    Some(node::Node::String(s)) => Some(s.sval.clone()),
                    _ => None,
                })
            }
            Some(node::Node::String(s)) => Some(s.sval.clone()),
            _ => None,
        };
        match handler {
            Some(h) if !h.eq_ignore_ascii_case("none") => {
                interp.type_subscript.insert(oid, h);
            }
            _ => {
                interp.type_subscript.remove(&oid);
            }
        }
    }
}

/// `ALTER TYPE name SET (...)`: only the options the analyzer models
/// (SUBSCRIPT) change anything.
pub fn alter_type(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::AlterTypeStmt,
) -> Result<(), DdlError> {
    let parts: Vec<&str> = stmt
        .type_name
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let (schema, name) = match parts.as_slice() {
        [n] => (None, *n),
        [s, n] => (Some(*s), *n),
        _ => return Ok(()),
    };
    if let Some(t) = interp.resolve_type_by_name(schema, name) {
        let oid = t.oid;
        record_type_options(interp, oid, &stmt.options);
    }
    Ok(())
}

/// Register a user-defined base type (shell or full) with its array type.
pub(crate) fn create_base_type(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: &str,
) -> Result<PgTypeOid, DdlError> {
    let oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_type(PgType {
        oid,
        typname: name.to_owned(),
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::UserDefined,
        typispreferred: false,
        typrelid: None,
        typelem: None,
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
    });
    register_array_type(interp, nsoid, name, oid)?;
    Ok(oid)
}

// ─── CREATE CAST ────────────────────────────────────────────────────────────

pub fn create_cast(interp: &mut PgCatalog, stmt: &CreateCastStmt) -> Result<(), DdlError> {
    let source_oid = stmt
        .sourcetype
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp));
    let target_oid = stmt
        .targettype
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp));

    let (Some(src), Some(tgt)) = (source_oid, target_oid) else {
        return Ok(());
    };

    let castcontext = match CoercionContext::try_from(stmt.context) {
        Ok(CoercionContext::CoercionImplicit) => CastContext::Implicit,
        Ok(CoercionContext::CoercionAssignment) => CastContext::Assignment,
        _ => CastContext::Explicit,
    };

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

    // PG rejects WITHOUT FUNCTION (binary-compatible) casts that touch a
    // domain or enum on either side. Domains carry CHECK constraints that
    // must run at cast time, and enums have an internal ordering that's not
    // safe to bit-cast through. Only WITH FUNCTION supports either case.
    // The two errors come out with different wording on PG's side
    // (SQLSTATE 42P17), so we match each precisely.
    if matches!(castmethod, CastMethod::Binary) {
        let typtype = |oid: PgTypeOid| interp.pg_type.get(&oid).map(|t| t.typtype);
        let src_kind = typtype(src);
        let tgt_kind = typtype(tgt);
        if src_kind == Some(TypType::Domain) || tgt_kind == Some(TypType::Domain) {
            return Err(DdlError::Parse(
                "domain data types must not be marked binary-compatible".into(),
            ));
        }
        if src_kind == Some(TypType::Enum) || tgt_kind == Some(TypType::Enum) {
            return Err(DdlError::Parse(
                "enum data types are not binary-compatible".into(),
            ));
        }
    }

    let cast_oid = PgCastOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_cast(PgCast {
        oid: cast_oid,
        castsource: src,
        casttarget: tgt,
        castcontext,
        castmethod,
    });
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Suppress the auto array name when needed; we always use `_<name>`.
fn array_name(base_name: &str) -> String {
    format!("_{base_name}")
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
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: array_name(base_name),
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
    });
    if let Some(elem) = interp.pg_type.get_mut(&element_oid) {
        elem.typarray = Some(array_oid);
    }
    Ok(array_oid)
}
