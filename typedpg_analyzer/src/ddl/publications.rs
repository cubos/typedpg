//! Publications (logical replication, publicationcmds.c): they don't
//! affect typing, but PG resolves the tables, columns, row filters and
//! schemas they name, validates their options and keeps membership.

use pg_query::protobuf::{
    AlterPublicationAction, AlterPublicationStmt, CreatePublicationStmt, PublicationObjSpecType,
    PublicationTable, node,
};

use super::DdlError;
use crate::oid::{PgClassOid, PgNamespaceOid};
use crate::pg_catalog::{PgCatalog, RelKind};
use crate::qualified_name::QualifiedName;

/// A publication (`pg_publication`, `pg_publication_rel`,
/// `pg_publication_namespace`).
#[derive(Clone, Debug)]
pub(crate) struct Publication {
    pub(crate) name: String,
    tables: Vec<PubRel>,
    schemas: Vec<PgNamespaceOid>,
    /// `pubviaroot`.
    via_root: bool,
}

/// A `pg_publication_rel` row: whether it has a row filter (`prqual`) and
/// a column list (`prattrs`).
#[derive(Clone, Copy, Debug)]
struct PubRel {
    relid: PgClassOid,
    has_filter: bool,
    has_columns: bool,
}

enum Object<'a> {
    Table(PubRel, &'a PublicationTable),
    Schema(PgNamespaceOid),
}

fn missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("publication \"{name}\" does not exist"))
}

/// The options [`check_options`] parsed that membership rules depend on.
#[derive(Default)]
struct Options {
    via_root: Option<bool>,
}

/// defGetBoolean (define.c): no value means true; otherwise 0 / 1 or,
/// case-insensitively, true / false / on / off.
fn def_get_boolean(name: &str, arg: Option<&node::Node>) -> Result<bool, DdlError> {
    let value = match arg {
        None => return Ok(true),
        Some(node::Node::Integer(i)) => match i.ival {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        Some(node::Node::Boolean(b)) => Some(b.boolval),
        Some(other) => match def_get_string(Some(other)).map(|s| s.to_ascii_lowercase()) {
            Some(s) if s == "true" || s == "on" => Some(true),
            Some(s) if s == "false" || s == "off" => Some(false),
            _ => None,
        },
    };
    value.ok_or_else(|| DdlError::Parse(format!("{name} requires a Boolean value")))
}

/// defGetString (define.c): an option value as text — a bare word such as
/// `off` arrives as a type name.
fn def_get_string(arg: Option<&node::Node>) -> Option<String> {
    match arg? {
        node::Node::String(s) => Some(s.sval.clone()),
        node::Node::Integer(i) => Some(i.ival.to_string()),
        node::Node::Float(f) => Some(f.fval.clone()),
        node::Node::Boolean(b) => Some(b.boolval.to_string()),
        node::Node::TypeName(tn) => Some(
            tn.names
                .iter()
                .filter_map(super::util::node_string)
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

/// parse_publication_options.
fn check_options(options: &[pg_query::protobuf::Node]) -> Result<Options, DdlError> {
    let mut out = Options::default();
    let mut seen: Vec<&str> = Vec::new();
    for opt in options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        // errorConflictingDefElem.
        if seen.contains(&de.defname.as_str()) {
            return Err(DdlError::Parse("conflicting or redundant options".into()));
        }
        seen.push(&de.defname);
        let arg = de.arg.as_deref().and_then(|a| a.node.as_ref());
        let value = def_get_string(arg);
        match de.defname.as_str() {
            "publish" => {
                let Some(value) = value else {
                    return Err(DdlError::Parse("publish requires a parameter".into()));
                };
                // SplitIdentifierString downcases the unquoted names.
                for part in value
                    .split(',')
                    .map(|p| p.trim().to_lowercase())
                    .filter(|p| !p.is_empty())
                {
                    if !matches!(part.as_str(), "insert" | "update" | "delete" | "truncate") {
                        return Err(DdlError::Parse(format!(
                            "unrecognized value for publication option \"publish\": \"{part}\""
                        )));
                    }
                }
            }
            "publish_via_partition_root" => {
                out.via_root = Some(def_get_boolean(&de.defname, arg)?);
            }
            // defGetGeneratedColsOption.
            "publish_generated_columns" => {
                let value = value.unwrap_or_default();
                if !value.eq_ignore_ascii_case("none") && !value.eq_ignore_ascii_case("stored") {
                    return Err(DdlError::Parse(format!(
                        "invalid value for publication parameter \"publish_generated_columns\": \
                         \"{value}\" (Valid values are \"none\" and \"stored\".)"
                    )));
                }
            }
            other => {
                return Err(DdlError::Parse(format!(
                    "unrecognized publication parameter: \"{other}\""
                )));
            }
        }
    }
    Ok(out)
}

/// ObjectsInPublicationToOids + OpenTableList: resolve the objects, with
/// continuation entries taking the kind of the one before
/// (preprocess_pubobj_list). A table named twice is kept once, unless
/// either mention has a row filter or a column list.
fn resolve_objects<'a>(
    interp: &PgCatalog,
    objects: &'a [pg_query::protobuf::Node],
) -> Result<Vec<Object<'a>>, DdlError> {
    let mut out: Vec<Object<'a>> = Vec::new();
    let mut kind = PublicationObjSpecType::PublicationobjTable;
    for obj in objects {
        let Some(node::Node::PublicationObjSpec(spec)) = obj.node.as_ref() else {
            continue;
        };
        let t = PublicationObjSpecType::try_from(spec.pubobjtype)
            .unwrap_or(PublicationObjSpecType::Undefined);
        if t != PublicationObjSpecType::PublicationobjContinuation {
            kind = t;
        }
        match kind {
            PublicationObjSpecType::PublicationobjTable => {
                let Some(pt) = spec.pubtable.as_deref() else {
                    continue;
                };
                let Some(rv) = pt.relation.as_ref() else {
                    continue;
                };
                let (_, relid) = super::util::lookup_relation(interp, rv)?;
                let rel = PubRel {
                    relid,
                    has_filter: pt.where_clause.is_some(),
                    has_columns: !pt.columns.is_empty(),
                };
                let earlier = out.iter().find_map(|o| match o {
                    Object::Table(r, _) if r.relid == relid => Some(*r),
                    _ => None,
                });
                if let Some(earlier) = earlier {
                    if rel.has_filter || earlier.has_filter {
                        return Err(DdlError::DuplicateObject(format!(
                            "conflicting or redundant WHERE clauses for table \"{}\"",
                            rv.relname
                        )));
                    }
                    if rel.has_columns || earlier.has_columns {
                        return Err(DdlError::DuplicateObject(format!(
                            "conflicting or redundant column lists for table \"{}\"",
                            rv.relname
                        )));
                    }
                    continue;
                }
                out.push(Object::Table(rel, pt));
            }
            PublicationObjSpecType::PublicationobjTablesInSchema => {
                let Some(ns) = interp.namespace_oid(&spec.name) else {
                    return Err(DdlError::TableNotFound(format!(
                        "schema \"{}\" does not exist",
                        spec.name
                    )));
                };
                out.push(Object::Schema(ns));
            }
            PublicationObjSpecType::PublicationobjTablesInCurSchema => {
                let schema = super::util::creation_schema(interp)?;
                if let Some(ns) = interp.namespace_oid(&schema) {
                    out.push(Object::Schema(ns));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn relname(interp: &PgCatalog, relid: PgClassOid) -> String {
    interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default()
}

fn is_partitioned(interp: &PgCatalog, relid: PgClassOid) -> bool {
    interp.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Partitioned)
}

/// The checks tables being added to publication `pubname` go through:
/// TransformPubWhereClauses, CheckPubRelationColumnList (`with_schemas`:
/// the publication has or gains FOR TABLES IN SCHEMA elements) and
/// PublicationAddTables' check_publication_add_relation /
/// pub_collist_validate.
fn check_tables_to_add(
    interp: &PgCatalog,
    pubname: &str,
    objects: &[Object<'_>],
    via_root: bool,
    with_schemas: bool,
) -> Result<(), DdlError> {
    let tables = || {
        objects.iter().filter_map(|o| match o {
            Object::Table(rel, pt) => Some((rel.relid, *pt)),
            Object::Schema(_) => None,
        })
    };
    for (relid, pt) in tables() {
        let Some(filter) = pt.where_clause.as_deref() else {
            continue;
        };
        if !via_root && is_partitioned(interp, relid) {
            return Err(DdlError::Parse(format!(
                "cannot use publication WHERE clause for relation \"{}\" (WHERE clause cannot be \
                 used for a partitioned table when publish_via_partition_root is false.)",
                relname(interp, relid)
            )));
        }
        check_row_filter(interp, relid, filter)?;
    }
    for (relid, _) in tables().filter(|(_, pt)| !pt.columns.is_empty()) {
        let detail = if with_schemas {
            "Column lists cannot be specified in publications containing FOR TABLES IN SCHEMA \
             elements."
        } else if !via_root && is_partitioned(interp, relid) {
            "Column lists cannot be specified for partitioned tables when \
             publish_via_partition_root is false."
        } else {
            continue;
        };
        let Some(class) = interp.pg_class.get(&relid) else {
            continue;
        };
        let qn = QualifiedName::new(
            interp.namespace_name(class.relnamespace).unwrap_or("?"),
            class.relname.clone(),
        );
        return Err(DdlError::Parse(format!(
            "cannot use column list for relation \"{qn}\" in publication \"{pubname}\" ({detail})"
        )));
    }
    for (relid, pt) in tables() {
        let relname = relname(interp, relid);
        // check_publication_add_relation.
        let kinds = match interp.pg_class.get(&relid).map(|c| c.relkind) {
            Some(RelKind::Table | RelKind::Partitioned) => None,
            Some(RelKind::View) => Some("views"),
            Some(RelKind::MaterializedView) => Some("materialized views"),
            Some(RelKind::Sequence) => Some("sequences"),
            Some(RelKind::ForeignTable) => Some("foreign tables"),
            _ => Some("this relation"),
        };
        if let Some(kinds) = kinds {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot add relation \"{relname}\" to publication (This operation is not \
                 supported for {kinds}.)"
            )));
        }
        // pub_collist_validate.
        let mut seen: Vec<&str> = Vec::new();
        for col in pt.columns.iter().filter_map(super::util::node_string) {
            if interp.attribute_by_name(relid, col).is_none() {
                if crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, ..)| *n == col)
                {
                    return Err(DdlError::Parse(format!(
                        "cannot use system column \"{col}\" in publication column list"
                    )));
                }
                return Err(DdlError::Parse(format!(
                    "column \"{col}\" of relation \"{relname}\" does not exist"
                )));
            }
            if seen.contains(&col) {
                return Err(DdlError::DuplicateObject(format!(
                    "duplicate column \"{col}\" in publication column list"
                )));
            }
            seen.push(col);
        }
    }
    Ok(())
}

/// First OID of objects not created by initdb (`FirstNormalObjectId`).
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// TransformPubWhereClauses: the row filter is transformed as a WHERE
/// clause (`transformWhereClause` with EXPR_KIND_WHERE, coerced to
/// boolean) and then limited by `check_simple_rowfilter_expr`.
fn check_row_filter(
    interp: &PgCatalog,
    relid: PgClassOid,
    filter: &pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    let unsupported = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(e.to_string());
    let used = std::cell::RefCell::new(Vec::new());
    let Some(result) = super::volatile::infer_over_relation(interp, relid, filter, Some(&used))
    else {
        return Ok(());
    };
    let result = result.map_err(unsupported)?;
    crate::clause::check_no_aggregates_or_windows(filter, interp, "WHERE").map_err(unsupported)?;
    crate::resolve::check_no_srf_in_clause(filter, interp, "WHERE").map_err(unsupported)?;
    // coerce_to_boolean.
    if result.type_oid != crate::pg_catalog::oid::BOOL
        && result.type_oid != crate::pg_catalog::oid::UNKNOWN
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of PUBLICATION WHERE must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    check_simple_rowfilter_expr(interp, relid, filter)?;
    // check_functions_in_node(contain_mutable_or_user_functions_checker)
    // over every function the filter runs, operators' included.
    let mutable = used.into_inner().into_iter().any(|oid| {
        oid.get() >= FIRST_NORMAL_OBJECT_ID
            || interp
                .pg_proc
                .get(&oid)
                .is_some_and(|p| p.provolatile != crate::pg_catalog::ProVolatile::Immutable)
    });
    if mutable {
        return Err(invalid_row_filter(
            "User-defined or built-in mutable functions are not allowed.",
        ));
    }
    Ok(())
}

fn invalid_row_filter(detail: &str) -> DdlError {
    DdlError::UnsupportedDdl(format!("invalid publication WHERE expression ({detail})"))
}

/// check_simple_rowfilter_expr_walker, over the raw expression: only the
/// node kinds that transform into the whitelisted executable nodes (Var,
/// Const, OpExpr, FuncExpr, BoolExpr, RelabelType, CollateExpr, CaseExpr,
/// ArrayExpr, RowExpr, CoalesceExpr, MinMaxExpr, XmlExpr, NullTest,
/// BooleanTest, ...), no system columns and no user-defined types. A cast
/// is allowed only when it becomes a cast function or a relabeling (not a
/// CoerceViaIO / ArrayCoerceExpr / CoerceToDomain).
fn check_simple_rowfilter_expr(
    interp: &PgCatalog,
    relid: PgClassOid,
    node: &pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    const ONLY_SIMPLE: &str = "Only columns, constants, built-in operators, built-in data types, \
                               built-in collations, and immutable built-in functions are allowed.";
    let Some(inner) = node.node.as_ref() else {
        return Ok(());
    };
    let type_of = |n: &pg_query::protobuf::Node| {
        super::volatile::infer_over_relation(interp, relid, n, None)
            .and_then(Result::ok)
            .map(|t| t.type_oid)
    };
    let user_type =
        |t: Option<crate::oid::PgTypeOid>| t.is_some_and(|t| t.get() >= FIRST_NORMAL_OBJECT_ID);
    fn opt(n: &Option<Box<pg_query::protobuf::Node>>) -> Vec<&pg_query::protobuf::Node> {
        n.as_deref().into_iter().collect()
    }
    let children: Vec<&pg_query::protobuf::Node> = match inner {
        node::Node::ColumnRef(cr) => {
            let Some(name) = cr.fields.last().and_then(super::util::node_string) else {
                return Err(invalid_row_filter(ONLY_SIMPLE));
            };
            if interp.attribute_by_name(relid, name).is_none()
                && crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, ..)| *n == name)
            {
                return Err(invalid_row_filter("System columns are not allowed."));
            }
            Vec::new()
        }
        node::Node::AConst(_) => Vec::new(),
        node::Node::TypeCast(tc) => {
            let arg = tc.arg.as_deref();
            let target = type_of(node);
            if user_type(target) {
                return Err(invalid_row_filter("User-defined types are not allowed."));
            }
            // A cast literal folds into a Const.
            let literal = matches!(
                arg.and_then(|a| a.node.as_ref()),
                Some(node::Node::AConst(_))
            );
            if !literal
                && let (Some(target), Some(source)) = (target, arg.and_then(type_of))
                && !matches!(
                    crate::coerce::coercion_pathway(
                        target,
                        source,
                        crate::coerce::CoercionContext::Explicit,
                        interp,
                    ),
                    Some(crate::coerce::CoercionPath::Func | crate::coerce::CoercionPath::Relabel)
                )
            {
                return Err(invalid_row_filter(ONLY_SIMPLE));
            }
            arg.into_iter().collect()
        }
        node::Node::AExpr(e) => {
            let mut c: Vec<_> = opt(&e.lexpr);
            c.extend(opt(&e.rexpr));
            c
        }
        node::Node::List(l) => l.items.iter().collect(),
        node::Node::BoolExpr(b) => b.args.iter().collect(),
        node::Node::NullTest(t) => opt(&t.arg),
        node::Node::BooleanTest(t) => opt(&t.arg),
        node::Node::CaseExpr(c) => {
            let mut out: Vec<_> = opt(&c.arg);
            out.extend(c.args.iter());
            out.extend(opt(&c.defresult));
            out
        }
        node::Node::CaseWhen(w) => {
            let mut out: Vec<_> = opt(&w.expr);
            out.extend(opt(&w.result));
            out
        }
        node::Node::CoalesceExpr(c) => c.args.iter().collect(),
        node::Node::MinMaxExpr(m) => m.args.iter().collect(),
        node::Node::RowExpr(r) => r.args.iter().collect(),
        node::Node::AArrayExpr(a) => a.elements.iter().collect(),
        node::Node::XmlExpr(x) => x.named_args.iter().chain(x.args.iter()).collect(),
        node::Node::CollateClause(c) => opt(&c.arg),
        node::Node::FuncCall(f) => f.args.iter().collect(),
        node::Node::NamedArgExpr(n) => opt(&n.arg),
        // ResTarget: an XMLELEMENT / XMLFOREST named argument.
        node::Node::ResTarget(r) => opt(&r.val),
        _ => return Err(invalid_row_filter(ONLY_SIMPLE)),
    };
    if matches!(
        inner,
        node::Node::ColumnRef(_)
            | node::Node::AExpr(_)
            | node::Node::FuncCall(_)
            | node::Node::CaseExpr(_)
            | node::Node::CoalesceExpr(_)
            | node::Node::MinMaxExpr(_)
    ) && user_type(type_of(node))
    {
        return Err(invalid_row_filter("User-defined types are not allowed."));
    }
    for child in children {
        check_simple_rowfilter_expr(interp, relid, child)?;
    }
    Ok(())
}

pub fn create_publication(
    interp: &mut PgCatalog,
    stmt: &CreatePublicationStmt,
) -> Result<(), DdlError> {
    if interp.publications.iter().any(|p| p.name == stmt.pubname) {
        return Err(DdlError::DuplicateObject(format!(
            "publication \"{}\" already exists",
            stmt.pubname
        )));
    }
    let options = check_options(&stmt.options)?;
    let via_root = options.via_root.unwrap_or(false);
    let objects = resolve_objects(interp, &stmt.pubobjects)?;
    let with_schemas = objects.iter().any(|o| matches!(o, Object::Schema(_)));
    check_tables_to_add(interp, &stmt.pubname, &objects, via_root, with_schemas)?;
    let mut publication = Publication {
        name: stmt.pubname.clone(),
        tables: Vec::new(),
        schemas: Vec::new(),
        via_root,
    };
    for object in objects {
        match object {
            Object::Table(rel, _) => publication.tables.push(rel),
            Object::Schema(ns) => publication.schemas.push(ns),
        }
    }
    interp.publications.push(publication);
    Ok(())
}

pub fn alter_publication(
    interp: &mut PgCatalog,
    stmt: &AlterPublicationStmt,
) -> Result<(), DdlError> {
    let Some(index) = interp
        .publications
        .iter()
        .position(|p| p.name == stmt.pubname)
    else {
        return Err(missing(&stmt.pubname));
    };
    let options = check_options(&stmt.options)?;
    // AlterPublicationOptions: a partitioned table's row filter or column
    // list needs publish_via_partition_root.
    if options.via_root == Some(false) {
        let publication = &interp.publications[index];
        for rel in &publication.tables {
            if !is_partitioned(interp, rel.relid) {
                continue;
            }
            let what = if rel.has_filter {
                "a WHERE clause"
            } else if rel.has_columns {
                "a column list"
            } else {
                continue;
            };
            return Err(DdlError::Parse(format!(
                "cannot set parameter \"publish_via_partition_root\" to false for publication \
                 \"{}\" (The publication contains {what} for partitioned table \"{}\", which is \
                 not allowed when \"publish_via_partition_root\" is false.)",
                stmt.pubname,
                relname(interp, rel.relid)
            )));
        }
    }
    if let Some(via_root) = options.via_root {
        interp.publications[index].via_root = via_root;
    }
    let action =
        AlterPublicationAction::try_from(stmt.action).unwrap_or(AlterPublicationAction::Undefined);
    let objects = resolve_objects(interp, &stmt.pubobjects)?;
    let adds_schemas = objects.iter().any(|o| matches!(o, Object::Schema(_)));
    let publication = &interp.publications[index];
    match action {
        AlterPublicationAction::ApAddObjects => {
            let with_schemas = adds_schemas || !publication.schemas.is_empty();
            check_tables_to_add(
                interp,
                &stmt.pubname,
                &objects,
                publication.via_root,
                with_schemas,
            )?;
        }
        AlterPublicationAction::ApSetObjects => {
            check_tables_to_add(
                interp,
                &stmt.pubname,
                &objects,
                publication.via_root,
                adds_schemas,
            )?;
        }
        _ => {}
    }
    let publication = &mut interp.publications[index];
    match action {
        AlterPublicationAction::ApAddObjects => {
            for object in &objects {
                if let Object::Table(rel, pt) = object {
                    if publication.tables.iter().any(|t| t.relid == rel.relid) {
                        let name = pt.relation.as_ref().map_or("", |rv| rv.relname.as_str());
                        return Err(DdlError::DuplicateObject(format!(
                            "relation \"{name}\" is already member of publication \"{}\"",
                            stmt.pubname
                        )));
                    }
                    publication.tables.push(*rel);
                }
            }
            // AlterPublicationSchemas.
            if adds_schemas && publication.tables.iter().any(|t| t.has_columns) {
                return Err(DdlError::Parse(format!(
                    "cannot add schema to publication \"{}\" (Schemas cannot be added if any \
                     tables that specify a column list are already part of the publication.)",
                    stmt.pubname
                )));
            }
            for object in objects {
                if let Object::Schema(ns) = object
                    && !publication.schemas.contains(&ns)
                {
                    publication.schemas.push(ns);
                }
            }
        }
        AlterPublicationAction::ApDropObjects => {
            for object in objects {
                match object {
                    // PublicationDropTables.
                    Object::Table(rel, pt) => {
                        if rel.has_columns {
                            return Err(DdlError::Parse(
                                "column list must not be specified in ALTER PUBLICATION ... DROP"
                                    .into(),
                            ));
                        }
                        if rel.has_filter {
                            return Err(DdlError::Parse(
                                "cannot use a WHERE clause when removing a table from a \
                                 publication"
                                    .into(),
                            ));
                        }
                        if !publication.tables.iter().any(|t| t.relid == rel.relid) {
                            let name = pt.relation.as_ref().map_or("", |rv| rv.relname.as_str());
                            return Err(DdlError::TypeNotFound(format!(
                                "relation \"{name}\" is not part of the publication"
                            )));
                        }
                        publication.tables.retain(|t| t.relid != rel.relid);
                    }
                    Object::Schema(ns) => publication.schemas.retain(|s| *s != ns),
                }
            }
        }
        AlterPublicationAction::ApSetObjects => {
            publication.tables.clear();
            publication.schemas.clear();
            for object in objects {
                match object {
                    Object::Table(rel, _) => publication.tables.push(rel),
                    Object::Schema(ns) => publication.schemas.push(ns),
                }
            }
        }
        AlterPublicationAction::Undefined => {}
    }
    Ok(())
}

/// DROP PUBLICATION [IF EXISTS] name.
pub(crate) fn drop_publication(
    interp: &mut PgCatalog,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(name) = super::util::node_string(obj_node).map(str::to_owned) else {
        return Ok(());
    };
    let before = interp.publications.len();
    interp.publications.retain(|p| p.name != name);
    if interp.publications.len() == before && !missing_ok {
        return Err(missing(&name));
    }
    Ok(())
}

/// ALTER PUBLICATION name RENAME TO new.
pub(crate) fn rename_publication(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(old) = stmt.object.as_deref().and_then(super::util::node_string) else {
        return Ok(());
    };
    let old = old.to_owned();
    if interp.publications.iter().any(|p| p.name == stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "publication \"{}\" already exists",
            stmt.newname
        )));
    }
    let Some(p) = interp.publications.iter_mut().find(|p| p.name == old) else {
        return Err(missing(&old));
    };
    p.name = stmt.newname.clone();
    Ok(())
}

impl Publication {
    /// A dropped relation leaves the publication.
    pub(crate) fn forget_relation(&mut self, relid: PgClassOid) {
        self.tables.retain(|t| t.relid != relid);
    }
}
