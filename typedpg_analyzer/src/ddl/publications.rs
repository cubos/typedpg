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
                if check_filters
                    && let Some(filter) = pt.where_clause.as_deref()
                    && let Some(Err(e)) =
                        super::volatile::infer_over_relation(interp, relid, filter, None)
                {
                    return Err(DdlError::UnsupportedDdl(e.to_string()));
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
