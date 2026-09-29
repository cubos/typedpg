//! Publications (logical replication, publicationcmds.c): they don't
//! affect typing, but PG resolves the tables, columns, row filters and
//! schemas they name, validates their options and keeps membership.

use pg_query::protobuf::{
    AlterPublicationAction, AlterPublicationStmt, CreatePublicationStmt, PublicationObjSpecType,
    node,
};

use super::DdlError;
use crate::oid::{PgClassOid, PgNamespaceOid};
use crate::pg_catalog::{PgCatalog, RelKind};

/// A publication (`pg_publication`, `pg_publication_rel`,
/// `pg_publication_namespace`).
#[derive(Clone, Debug)]
pub(crate) struct Publication {
    pub(crate) name: String,
    tables: Vec<PgClassOid>,
    schemas: Vec<PgNamespaceOid>,
}

enum Object {
    Table(PgClassOid, String),
    Schema(PgNamespaceOid),
}

fn missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("publication \"{name}\" does not exist"))
}

/// parse_publication_options.
fn check_options(options: &[pg_query::protobuf::Node]) -> Result<(), DdlError> {
    for opt in options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let value = match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
            Some(node::Node::String(s)) => s.sval.clone(),
            _ => String::new(),
        };
        match de.defname.as_str() {
            "publish" => {
                for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    if !matches!(part, "insert" | "update" | "delete" | "truncate") {
                        return Err(DdlError::Parse(format!(
                            "unrecognized value for publication option \"publish\": \"{part}\""
                        )));
                    }
                }
            }
            "publish_via_partition_root" | "publish_generated_columns" => {}
            other => {
                return Err(DdlError::Parse(format!(
                    "unrecognized publication parameter: \"{other}\""
                )));
            }
        }
    }
    Ok(())
}

/// ObjectsInPublicationToOids: resolve the objects, with continuation
/// entries taking the kind of the one before (preprocess_pubobj_list).
fn resolve_objects(
    interp: &PgCatalog,
    objects: &[pg_query::protobuf::Node],
    check_filters: bool,
) -> Result<Vec<Object>, DdlError> {
    let mut out = Vec::new();
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
                        "cannot add relation \"{}\" to publication (This operation is not \
                         supported for {kinds}.)",
                        rv.relname
                    )));
                }
                for col in pt.columns.iter().filter_map(super::util::node_string) {
                    if interp.attribute_by_name(relid, col).is_none() {
                        return Err(DdlError::Parse(format!(
                            "column \"{col}\" of relation \"{}\" does not exist",
                            rv.relname
                        )));
                    }
                }
                if check_filters && let Some(filter) = pt.where_clause.as_deref() {
                    check_row_filter(interp, relid, filter)?;
                }
                out.push(Object::Table(relid, rv.relname.clone()));
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
    check_options(&stmt.options)?;
    let objects = resolve_objects(interp, &stmt.pubobjects, true)?;
    let mut publication = Publication {
        name: stmt.pubname.clone(),
        tables: Vec::new(),
        schemas: Vec::new(),
    };
    for object in objects {
        match object {
            Object::Table(relid, _) => publication.tables.push(relid),
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
    check_options(&stmt.options)?;
    let action =
        AlterPublicationAction::try_from(stmt.action).unwrap_or(AlterPublicationAction::Undefined);
    let objects = resolve_objects(
        interp,
        &stmt.pubobjects,
        action != AlterPublicationAction::ApDropObjects,
    )?;
    let publication = &mut interp.publications[index];
    match action {
        AlterPublicationAction::ApAddObjects => {
            for object in objects {
                match object {
                    Object::Table(relid, name) => {
                        if publication.tables.contains(&relid) {
                            return Err(DdlError::DuplicateObject(format!(
                                "relation \"{name}\" is already member of publication \"{}\"",
                                stmt.pubname
                            )));
                        }
                        publication.tables.push(relid);
                    }
                    Object::Schema(ns) => {
                        if !publication.schemas.contains(&ns) {
                            publication.schemas.push(ns);
                        }
                    }
                }
            }
        }
        AlterPublicationAction::ApDropObjects => {
            for object in objects {
                match object {
                    Object::Table(relid, name) => {
                        if !publication.tables.contains(&relid) {
                            return Err(DdlError::TypeNotFound(format!(
                                "relation \"{name}\" is not part of the publication"
                            )));
                        }
                        publication.tables.retain(|t| *t != relid);
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
                    Object::Table(relid, _) => publication.tables.push(relid),
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
        self.tables.retain(|t| *t != relid);
    }
}
