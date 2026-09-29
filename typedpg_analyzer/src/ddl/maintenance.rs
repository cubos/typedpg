//! Maintenance and utility statements that don't change the schema the
//! analyzer reads — TRUNCATE, CLUSTER, REINDEX, VACUUM / ANALYZE, LOCK,
//! SECURITY LABEL, ALTER DEFAULT PRIVILEGES — but that PG runs against
//! named objects which must exist and suit the command.

use typedpg_pg_query::protobuf::{
    ClusterStmt, DropBehavior, LockStmt, ReindexObjectType, ReindexStmt, SecLabelStmt,
    TruncateStmt, VacuumStmt, node,
};

use super::DdlError;
use crate::oid::PgClassOid;
use crate::pg_catalog::{ConType, PgCatalog, RelKind};

fn relkind(interp: &PgCatalog, relid: PgClassOid) -> Option<RelKind> {
    interp.pg_class.get(&relid).map(|c| c.relkind)
}

fn relname(interp: &PgCatalog, relid: PgClassOid) -> String {
    interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default()
}

/// TRUNCATE (ExecuteTruncate): tables only, and no foreign key from a
/// table left out may reference one being truncated
/// (heap_truncate_check_FKs), unless CASCADE.
pub fn truncate(interp: &PgCatalog, stmt: &TruncateStmt) -> Result<(), DdlError> {
    let mut targets: Vec<PgClassOid> = Vec::new();
    for rel in &stmt.relations {
        let Some(node::Node::RangeVar(rv)) = rel.node.as_ref() else {
            continue;
        };
        let (_, relid) = super::util::lookup_relation(interp, rv)?;
        // truncate_check_rel.
        if !matches!(
            relkind(interp, relid),
            Some(RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable)
        ) {
            return Err(DdlError::Parse(format!(
                "\"{}\" is not a table",
                rv.relname
            )));
        }
        targets.push(relid);
        if rv.inh {
            let mut i = targets.len() - 1;
            while i < targets.len() {
                for child in super::tables::inherit::children_of(interp, targets[i]) {
                    if !targets.contains(&child) {
                        targets.push(child);
                    }
                }
                i += 1;
            }
        }
    }
    if stmt.behavior == DropBehavior::DropCascade as i32 {
        return Ok(());
    }
    for &target in &targets {
        if let Some(fk) = interp.pg_constraint.values().find(|c| {
            c.contype == ConType::ForeignKey
                && c.confrelid == Some(target)
                && !targets.contains(&c.conrelid)
        }) {
            return Err(DdlError::DependencyError(format!(
                "cannot truncate a table referenced in a foreign key constraint (Table \"{}\" \
                 references \"{}\".)",
                relname(interp, fk.conrelid),
                relname(interp, target)
            )));
        }
    }
    Ok(())
}

/// RangeVarCallbackMaintainsTable: tables and materialized views.
fn maintained_table(
    interp: &PgCatalog,
    rv: &typedpg_pg_query::protobuf::RangeVar,
) -> Result<PgClassOid, DdlError> {
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    if !matches!(
        relkind(interp, relid),
        Some(RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView)
    ) {
        return Err(DdlError::Parse(format!(
            "\"{}\" is not a table or materialized view",
            rv.relname
        )));
    }
    Ok(relid)
}

/// CLUSTER table [USING index]: the index must be clusterable; without
/// one, the table must have been clustered before.
pub fn cluster(interp: &mut PgCatalog, stmt: &ClusterStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let relid = maintained_table(interp, rv)?;
    if stmt.indexname.is_empty() {
        if !interp.clustered_indexes.contains_key(&relid) {
            return Err(DdlError::TypeNotFound(format!(
                "there is no previously clustered index for table \"{}\"",
                rv.relname
            )));
        }
        return Ok(());
    }
    let index = super::tables::check_clusterable_index(interp, relid, &stmt.indexname)?;
    interp.clustered_indexes.insert(relid, index);
    Ok(())
}

/// REINDEX INDEX / TABLE / SCHEMA.
pub fn reindex(interp: &PgCatalog, stmt: &ReindexStmt) -> Result<(), DdlError> {
    match ReindexObjectType::try_from(stmt.kind) {
        Ok(ReindexObjectType::ReindexObjectIndex) => {
            let Some(rv) = stmt.relation.as_ref() else {
                return Ok(());
            };
            let (_, relid) = super::util::lookup_relation(interp, rv)?;
            // RangeVarCallbackForReindexIndex.
            if !matches!(
                relkind(interp, relid),
                Some(RelKind::Index | RelKind::PartitionedIndex)
            ) {
                return Err(DdlError::Parse(format!(
                    "\"{}\" is not an index",
                    rv.relname
                )));
            }
        }
        Ok(ReindexObjectType::ReindexObjectTable) => {
            if let Some(rv) = stmt.relation.as_ref() {
                maintained_table(interp, rv)?;
            }
        }
        Ok(ReindexObjectType::ReindexObjectSchema)
            if interp.namespace_oid(&stmt.name).is_none() =>
        {
            return Err(DdlError::TableNotFound(format!(
                "schema \"{}\" does not exist",
                stmt.name
            )));
        }
        _ => {}
    }
    Ok(())
}

/// VACUUM / ANALYZE: the named relations and columns must exist
/// (vacuum_rel / do_analyze_rel); other relation kinds are skipped with a
/// WARNING.
pub fn vacuum(interp: &PgCatalog, stmt: &VacuumStmt) -> Result<(), DdlError> {
    for rel in &stmt.rels {
        let Some(node::Node::VacuumRelation(vr)) = rel.node.as_ref() else {
            continue;
        };
        let Some(rv) = vr.relation.as_ref() else {
            continue;
        };
        let (_, relid) = super::util::lookup_relation(interp, rv)?;
        if !matches!(
            relkind(interp, relid),
            Some(RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView)
        ) {
            continue;
        }
        for col in vr.va_cols.iter().filter_map(super::util::node_string) {
            if interp.attribute_by_name(relid, col).is_none() {
                return Err(DdlError::Parse(format!(
                    "column \"{col}\" of relation \"{}\" does not exist",
                    rv.relname
                )));
            }
        }
    }
    Ok(())
}

/// LOCK TABLE (RangeVarCallbackForLockTable): tables, partitioned tables,
/// views and foreign tables.
pub fn lock(interp: &PgCatalog, stmt: &LockStmt) -> Result<(), DdlError> {
    for rel in &stmt.relations {
        let Some(node::Node::RangeVar(rv)) = rel.node.as_ref() else {
            continue;
        };
        let (_, relid) = super::util::lookup_relation(interp, rv)?;
        let kinds = match relkind(interp, relid) {
            Some(RelKind::Table | RelKind::Partitioned | RelKind::View | RelKind::ForeignTable) => {
                continue;
            }
            Some(kind) => kind.plural(),
            None => "this relation",
        };
        return Err(DdlError::Parse(format!(
            "cannot lock relation \"{}\" (This operation is not supported for {kinds}.)",
            rv.relname
        )));
    }
    Ok(())
}

/// SECURITY LABEL: a plain PG server has no label provider loaded
/// (ExecSecLabelStmt).
pub fn security_label(stmt: &SecLabelStmt) -> Result<(), DdlError> {
    if stmt.provider.is_empty() {
        return Err(DdlError::UnsupportedDdl(
            "no security label providers have been loaded".into(),
        ));
    }
    Err(DdlError::UnsupportedDdl(format!(
        "security label provider \"{}\" is not loaded",
        stmt.provider
    )))
}

/// ALTER DEFAULT PRIVILEGES IN SCHEMA s ... : the schemas must exist.
pub fn alter_default_privileges(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterDefaultPrivilegesStmt,
) -> Result<(), DdlError> {
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if de.defname != "schemas" {
            continue;
        }
        let Some(node::Node::List(list)) = de.arg.as_deref().and_then(|a| a.node.as_ref()) else {
            continue;
        };
        for schema in list.items.iter().filter_map(super::util::node_string) {
            if interp.namespace_oid(schema).is_none() {
                return Err(DdlError::TableNotFound(format!(
                    "schema \"{schema}\" does not exist"
                )));
            }
        }
    }
    Ok(())
}

/// COPY (DoCopy / ProcessCopyOptions / BeginCopyTo / BeginCopyFrom): the
/// options must be known, the relation must suit the direction and the
/// column list name its columns once; COPY (query) TO analyzes the query.
/// The data itself (files, STDIN) isn't modeled.
pub fn copy(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::CopyStmt,
) -> Result<(), DdlError> {
    const OPTIONS: &[&str] = &[
        "format",
        "freeze",
        "delimiter",
        "null",
        "default",
        "header",
        "quote",
        "escape",
        "force_quote",
        "force_not_null",
        "force_null",
        "convert_selectively",
        "encoding",
        "on_error",
        "reject_limit",
        "log_verbosity",
    ];
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if !OPTIONS.contains(&de.defname.as_str()) {
            return Err(DdlError::Parse(format!(
                "option \"{}\" not recognized",
                de.defname
            )));
        }
        if de.defname == "format"
            && let Some(node::Node::String(s)) = de.arg.as_deref().and_then(|a| a.node.as_ref())
            && !matches!(
                s.sval.to_ascii_lowercase().as_str(),
                "text" | "csv" | "binary"
            )
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "COPY format \"{}\" not recognized",
                s.sval
            )));
        }
    }
    if let Some(query) = stmt.query.as_deref().and_then(|q| q.node.as_ref()) {
        return super::dml::check_statement(interp, query);
    }
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let kind = relkind(interp, relid);
    let refused = if stmt.is_from {
        match kind {
            Some(RelKind::View) => Some(("cannot copy to view", "")),
            Some(RelKind::MaterializedView) => Some(("cannot copy to materialized view", "")),
            Some(RelKind::Sequence) => Some(("cannot copy to sequence", "")),
            _ => None,
        }
    } else {
        match kind {
            Some(RelKind::View) => Some((
                "cannot copy from view",
                " (Try the COPY (SELECT ...) TO variant.)",
            )),
            Some(RelKind::Sequence) => Some(("cannot copy from sequence", "")),
            Some(RelKind::Partitioned) => Some((
                "cannot copy from partitioned table",
                " (Try the COPY (SELECT ...) TO variant.)",
            )),
            Some(RelKind::ForeignTable) => Some((
                "cannot copy from foreign table",
                " (Try the COPY (SELECT ...) TO variant.)",
            )),
            _ => None,
        }
    };
    if let Some((msg, hint)) = refused {
        return Err(DdlError::Parse(format!("{msg} \"{}\"{hint}", rv.relname)));
    }
    let mut seen: Vec<&str> = Vec::new();
    for col in stmt.attlist.iter().filter_map(super::util::node_string) {
        if interp.attribute_by_name(relid, col).is_none() {
            return Err(DdlError::Parse(format!(
                "column \"{col}\" of relation \"{}\" does not exist",
                rv.relname
            )));
        }
        if seen.contains(&col) {
            return Err(DdlError::DuplicateObject(format!(
                "column \"{col}\" specified more than once"
            )));
        }
        seen.push(col);
    }
    Ok(())
}
