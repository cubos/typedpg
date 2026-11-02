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
pub fn truncate(interp: &mut PgCatalog, stmt: &TruncateStmt) -> Result<(), DdlError> {
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
        // truncate_check_rel: a system catalog (IsSystemClass) is off
        // limits unless allow_system_table_mods is on.
        crate::ddl::util::check_not_system_class(interp, relid)?;
        // ExecuteTruncate: ONLY on a partitioned table truncates nothing.
        if !rv.inh && relkind(interp, relid) == Some(RelKind::Partitioned) {
            return Err(DdlError::Parse(
                "cannot truncate only a partitioned table (Do not specify the ONLY keyword, or \
                 use TRUNCATE ONLY on the partitions directly.)"
                    .into(),
            ));
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
        // ExecuteTruncate: CASCADE truncates the referencing tables too.
        let mut i = 0;
        while i < targets.len() {
            let referencing: Vec<PgClassOid> = interp
                .pg_constraint
                .values()
                .filter(|c| {
                    c.contype == ConType::ForeignKey
                        && c.conenforced
                        && c.confrelid == Some(targets[i])
                })
                .map(|c| c.conrelid)
                .collect();
            for table in referencing {
                if !targets.contains(&table) {
                    targets.push(table);
                }
            }
            i += 1;
        }
        new_relfilenodes(interp, &targets);
        return Ok(());
    }
    for &target in &targets {
        // heap_truncate_find_FKs: NOT ENFORCED foreign keys don't count.
        if let Some(fk) = interp.pg_constraint.values().find(|c| {
            c.contype == ConType::ForeignKey
                && c.conenforced
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
    new_relfilenodes(interp, &targets);
    Ok(())
}

/// CLUSTER / VACUUM FULL of `relid`: its heap is rewritten (make_new_heap),
/// its indexes rebuilt with it and its TOAST table made anew — for a
/// partitioned table, its leaf partitions'; with `inheritors` (VACUUM),
/// its inheritance children's too. cluster_rel / vacuum_rel skip a
/// materialized view that was never populated.
fn heap_rewrites(interp: &mut PgCatalog, relid: PgClassOid, inheritors: bool) {
    let tables = if inheritors || relkind(interp, relid) == Some(RelKind::Partitioned) {
        super::tables::inherit::all_inheritors(interp, relid)
    } else {
        vec![relid]
    };
    for table in tables {
        if matches!(
            relkind(interp, table),
            Some(RelKind::Table | RelKind::MaterializedView | RelKind::ToastTable)
        ) && !interp.unpopulated_matviews.contains(&table)
        {
            super::indexes::table_rewritten(interp, table);
            super::tables::toast::heap_rewritten(interp, table);
        }
    }
}

/// The indexes of `tables` get new relfilenodes (TRUNCATE, CLUSTER,
/// REINDEX TABLE).
fn new_relfilenodes(interp: &mut PgCatalog, tables: &[PgClassOid]) {
    for &table in tables {
        super::indexes::table_rewritten(interp, table);
    }
}

/// RangeVarCallbackMaintainsTable: tables, TOAST tables and materialized
/// views (the message names only the first and last).
fn maintained_table(
    interp: &PgCatalog,
    rv: &typedpg_pg_query::protobuf::RangeVar,
) -> Result<PgClassOid, DdlError> {
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    if !matches!(
        relkind(interp, relid),
        Some(
            RelKind::Table | RelKind::ToastTable | RelKind::Partitioned | RelKind::MaterializedView
        )
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
        // Bare CLUSTER (get_tables_to_cluster): every table with an index
        // marked clustered is clustered again.
        let tables: Vec<PgClassOid> = interp.clustered_indexes.keys().copied().collect();
        for table in tables {
            heap_rewrites(interp, table, false);
        }
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
        heap_rewrites(interp, relid, false);
        return Ok(());
    }
    // cluster_rel refuses a shared catalog once the index is found by name
    // (get_relname_relid, in the table's schema), before checking it.
    let named_index_exists = interp.pg_class.get(&relid).is_some_and(|c| {
        interp
            .class_by_qname
            .contains_key(&(c.relnamespace, stmt.indexname.clone()))
    });
    if named_index_exists && interp.is_shared_relation(relid) {
        return Err(DdlError::UnsupportedDdl(
            "cannot cluster a shared catalog".into(),
        ));
    }
    let index = super::tables::check_clusterable_index(interp, relid, &stmt.indexname)?;
    // cluster(): a partitioned table's partitions are clustered each in a
    // transaction of its own.
    if relkind(interp, relid) == Some(RelKind::Partitioned) {
        prevent_in_transaction_block(interp, "CLUSTER")?;
    }
    interp.clustered_indexes.insert(relid, index);
    heap_rewrites(interp, relid, false);
    Ok(())
}

/// PreventInTransactionBlock: `what` can't run inside a transaction block.
fn prevent_in_transaction_block(interp: &PgCatalog, what: &str) -> Result<(), DdlError> {
    if interp.in_transaction_block {
        return Err(DdlError::UnsupportedDdl(format!(
            "{what} cannot run inside a transaction block"
        )));
    }
    Ok(())
}

/// REINDEX INDEX / TABLE / SCHEMA.
pub fn reindex(interp: &mut PgCatalog, stmt: &ReindexStmt) -> Result<(), DdlError> {
    let concurrently = super::txblock::reindex_options(interp, &stmt.params)?;
    // ReindexRelationConcurrently / ReindexMultipleTables: a system catalog
    // (IsCatalogRelationOid — its indexes and TOAST table too) can't be
    // rebuilt concurrently.
    let no_catalog = |relid: PgClassOid| {
        if concurrently && relid.get() < FIRST_UNPINNED_OBJECT_ID {
            return Err(DdlError::UnsupportedDdl(
                "cannot reindex system catalogs concurrently".into(),
            ));
        }
        Ok(())
    };
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
            // ReindexPartitions: each partition in a transaction of its own.
            if relkind(interp, relid) == Some(RelKind::PartitionedIndex) {
                prevent_in_transaction_block(interp, "REINDEX INDEX")?;
            }
            no_catalog(relid)?;
            super::indexes::new_relfilenode(interp, relid);
        }
        Ok(ReindexObjectType::ReindexObjectTable) => {
            if let Some(rv) = stmt.relation.as_ref() {
                let relid = maintained_table(interp, rv)?;
                if relkind(interp, relid) == Some(RelKind::Partitioned) {
                    prevent_in_transaction_block(interp, "REINDEX TABLE")?;
                }
                no_catalog(relid)?;
                new_relfilenodes(interp, &[relid]);
            }
        }
        // get_namespace_oid: the name as written — `pg_temp` is no schema's.
        Ok(ReindexObjectType::ReindexObjectSchema)
            if stmt.name == "pg_temp" || interp.namespace_oid(&stmt.name).is_none() =>
        {
            return Err(DdlError::TableNotFound(format!(
                "schema \"{}\" does not exist",
                stmt.name
            )));
        }
        Ok(ReindexObjectType::ReindexObjectSystem) if concurrently => {
            return Err(DdlError::UnsupportedDdl(
                "cannot reindex system catalogs concurrently".into(),
            ));
        }
        _ => {}
    }
    Ok(())
}

/// FirstUnpinnedObjectId (transam.h): the system catalogs, their indexes
/// and TOAST tables have smaller OIDs (IsCatalogRelationOid).
const FIRST_UNPINNED_OBJECT_ID: u32 = 12000;

/// VACUUM / ANALYZE: the named relations and columns must exist
/// (vacuum_rel / do_analyze_rel); other relation kinds are skipped with a
/// WARNING.
pub fn vacuum(interp: &mut PgCatalog, stmt: &VacuumStmt) -> Result<(), DdlError> {
    // VACUUM FULL rewrites what it processes — every table, without a list.
    let full = vacuum_options(stmt)?;
    let mut rewritten: Vec<PgClassOid> = Vec::new();
    if full && stmt.rels.is_empty() {
        rewritten = interp
            .pg_class
            .values()
            .filter(|c| matches!(c.relkind, RelKind::Table | RelKind::MaterializedView))
            .map(|c| c.oid)
            .collect();
    }
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
        if full {
            rewritten.push(relid);
        }
    }
    for relid in rewritten {
        heap_rewrites(interp, relid, true);
    }
    Ok(())
}

/// ExecVacuum's options, as it parses and checks them before anything
/// else: whether the VACUUM rewrites the tables (FULL, the last one given,
/// with PROCESS_MAIN).
fn vacuum_options(stmt: &VacuumStmt) -> Result<bool, DdlError> {
    use super::session::def_get_boolean;
    let syntax = |msg: String| Err(DdlError::Parse(msg));
    let (mut verbose, mut analyze, mut freeze, mut full) = (false, false, false, false);
    let (mut skip_locked, mut disable_page_skipping) = (false, false);
    let (mut process_main, mut process_toast) = (true, true);
    let (mut skip_database_stats, mut only_database_stats) = (false, false);
    let (mut index_cleanup, mut truncate) = (false, false);
    let mut nworkers: i64 = -1;
    let mut buffer_usage_limit = false;
    // A value as defGetString renders it.
    let string = |de: &typedpg_pg_query::protobuf::DefElem| -> Option<String> {
        match de.arg.as_deref().and_then(|a| a.node.as_ref())? {
            node::Node::String(s) => Some(s.sval.clone()),
            node::Node::Integer(i) => Some(i.ival.to_string()),
            node::Node::Float(f) => Some(f.fval.clone()),
            node::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.into()),
            _ => None,
        }
    };
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "verbose" => verbose = def_get_boolean(de)?,
            "skip_locked" => skip_locked = def_get_boolean(de)?,
            "buffer_usage_limit" => {
                let Some(value) = string(de) else {
                    return syntax("buffer_usage_limit requires a parameter".into());
                };
                if buffer_usage_limit_kb(&value).is_none() {
                    return Err(DdlError::Parse(
                        "BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB"
                            .into(),
                    ));
                }
                buffer_usage_limit = true;
            }
            name if !stmt.is_vacuumcmd => {
                return syntax(format!("unrecognized ANALYZE option \"{name}\""));
            }
            "analyze" => analyze = def_get_boolean(de)?,
            "freeze" => freeze = def_get_boolean(de)?,
            "full" => full = def_get_boolean(de)?,
            "disable_page_skipping" => disable_page_skipping = def_get_boolean(de)?,
            "index_cleanup" => {
                index_cleanup = match string(de) {
                    None => false,
                    Some(v) if v.eq_ignore_ascii_case("auto") => false,
                    Some(_) => {
                        vacopt_from_boolean(de)?;
                        true
                    }
                };
            }
            "process_main" => process_main = def_get_boolean(de)?,
            "process_toast" => process_toast = def_get_boolean(de)?,
            "truncate" => {
                vacopt_from_boolean(de)?;
                truncate = true;
            }
            "parallel" => {
                let Some(value) = string(de) else {
                    return Err(DdlError::Parse(
                        "parallel option requires a value between 0 and 1024".into(),
                    ));
                };
                let Ok(n) = value.parse::<i64>() else {
                    return syntax("parallel requires an integer value".into());
                };
                if !(0..=1024).contains(&n) {
                    return Err(DdlError::Parse(
                        "parallel workers for vacuum must be between 0 and 1024".into(),
                    ));
                }
                nworkers = n;
            }
            "skip_database_stats" => skip_database_stats = def_get_boolean(de)?,
            "only_database_stats" => only_database_stats = def_get_boolean(de)?,
            name => return syntax(format!("unrecognized VACUUM option \"{name}\"")),
        }
    }
    let _ = (verbose, skip_locked, freeze, skip_database_stats);
    if !stmt.is_vacuumcmd {
        return Ok(false);
    }
    if full && nworkers > 0 {
        return Err(DdlError::UnsupportedDdl(
            "VACUUM FULL cannot be performed in parallel".into(),
        ));
    }
    if buffer_usage_limit && full && !analyze {
        return Err(DdlError::Parse(
            "BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL".into(),
        ));
    }
    if !analyze
        && stmt.rels.iter().any(|r| {
            matches!(r.node.as_ref(), Some(node::Node::VacuumRelation(vr)) if !vr.va_cols.is_empty())
        })
    {
        return Err(DdlError::Parse(
            "ANALYZE option must be specified when a column list is provided".into(),
        ));
    }
    if full && disable_page_skipping {
        return Err(DdlError::UnsupportedDdl(
            "VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL".into(),
        ));
    }
    if full && !process_toast {
        return Err(DdlError::UnsupportedDdl(
            "PROCESS_TOAST required with VACUUM FULL".into(),
        ));
    }
    if only_database_stats {
        if !stmt.rels.is_empty() {
            return Err(DdlError::Parse(
                "ONLY_DATABASE_STATS cannot be specified with a list of tables".into(),
            ));
        }
        if analyze
            || freeze
            || full
            || disable_page_skipping
            || skip_locked
            || skip_database_stats
            || index_cleanup
            || truncate
            || nworkers >= 0
            || buffer_usage_limit
        {
            return Err(DdlError::Parse(
                "ONLY_DATABASE_STATS cannot be specified with other VACUUM options".into(),
            ));
        }
    }
    Ok(full && process_main)
}

/// get_vacoptval_from_boolean: defGetBoolean's values.
fn vacopt_from_boolean(de: &typedpg_pg_query::protobuf::DefElem) -> Result<bool, DdlError> {
    super::session::def_get_boolean(de)
}

/// BUFFER_USAGE_LIMIT's value in kB (parse_int with GUC_UNIT_KB): 0, or
/// between 128 kB and 16 GB.
fn buffer_usage_limit_kb(value: &str) -> Option<i64> {
    let v = value.trim();
    let digits = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let number: i64 = v[..digits].parse().ok()?;
    let kb = match v[digits..].trim() {
        "" | "kB" => number,
        "MB" => number * 1024,
        "GB" => number * 1024 * 1024,
        "TB" => number * 1024 * 1024 * 1024,
        _ => return None,
    };
    (kb == 0 || (128..=16_777_216).contains(&kb)).then_some(kb)
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
    // `COPY (query) TO` runs the query.
    if let Some(query) = stmt.query.as_deref().and_then(|q| q.node.as_ref()) {
        return super::dml::check_executed(interp, query);
    }
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let kind = relkind(interp, relid);
    let refused = if stmt.is_from {
        match kind {
            // CopyFrom: a view takes rows through an INSTEAD OF INSERT
            // trigger.
            Some(RelKind::View)
                if !interp.triggers.get(&relid).is_some_and(|ts| {
                    ts.iter()
                        .any(|t| t.is_instead_row_for(crate::ddl::triggers::TRIGGER_TYPE_INSERT))
                }) =>
            {
                Some((
                    "cannot copy to view",
                    " (To enable copying to a view, provide an INSTEAD OF INSERT trigger.)",
                ))
            }
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
        // CopyGetAttnums: generated columns take no input.
        if interp
            .attribute_by_name(relid, col)
            .is_some_and(|a| a.attgenerated.is_some())
        {
            return Err(DdlError::Parse(format!(
                "column \"{col}\" is a generated column (Generated columns cannot be used in \
                 COPY.)"
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
