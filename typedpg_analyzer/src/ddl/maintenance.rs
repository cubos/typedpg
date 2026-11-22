//! Maintenance and utility statements that don't change the schema the
//! analyzer reads — TRUNCATE, REPACK / CLUSTER, REINDEX, VACUUM / ANALYZE, LOCK,
//! SECURITY LABEL, ALTER DEFAULT PRIVILEGES — but that PG runs against
//! named objects which must exist and suit the command.

use typedpg_pg_query::ast::{
    self, DropBehavior, LockStmt, ReindexObjectType, ReindexStmt, RepackCommand, RepackStmt,
    SecLabelStmt, TruncateStmt, VacuumStmt,
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
        let Some(ast::Node::RangeVar(rv)) = rel.as_ref() else {
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
    if stmt.behavior == DropBehavior::DropCascade {
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

/// REPACK / CLUSTER / VACUUM FULL of `relid`: its heap is rewritten (make_new_heap),
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
    rv: &typedpg_pg_query::ast::RangeVar,
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

/// RepackCommandAsString.
fn repack_command_name(command: RepackCommand) -> &'static str {
    match command {
        RepackCommand::Repack => "REPACK",
        RepackCommand::Vacuumfull => "VACUUM",
        RepackCommand::Cluster => "CLUSTER",
    }
}

/// REPACK and CLUSTER (ExecRepack, PostgreSQL 19): rewrite a table, in the
/// order of an index when one is named (or, with USING INDEX alone, the one
/// marked clustered), a partitioned table's leaf partitions, or — without a
/// table — every table (with USING INDEX or CLUSTER, those with a clustered
/// index).
pub fn repack(interp: &mut PgCatalog, stmt: &RepackStmt) -> Result<(), DdlError> {
    use super::session::def_get_boolean;
    let command = repack_command_name(stmt.command);
    let (mut analyze, mut concurrently) = (false, false);
    for opt in &stmt.params {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "verbose" => {
                def_get_boolean(de)?;
            }
            "analyze" | "analyse" | "concurrently" if stmt.command != RepackCommand::Repack => {
                let option = if de.defname == "concurrently" {
                    "CONCURRENTLY"
                } else {
                    "ANALYZE"
                };
                return Err(DdlError::UnsupportedDdl(format!(
                    "{option} option not supported for {command}"
                )));
            }
            "analyze" | "analyse" => analyze = def_get_boolean(de)?,
            "concurrently" => concurrently = def_get_boolean(de)?,
            name => {
                return Err(DdlError::Parse(format!(
                    "unrecognized {command} option \"{name}\""
                )));
            }
        }
    }
    if concurrently {
        prevent_in_transaction_block(interp, "REPACK (CONCURRENTLY)")?;
    } else if analyze {
        prevent_in_transaction_block(interp, "REPACK (ANALYZE)")?;
    }
    let mut partitioned = None;
    if let Some(vr) = stmt.relation.as_ref() {
        let Some(rv) = vr.relation.as_ref() else {
            return Ok(());
        };
        // process_single_relation.
        if !analyze && !vr.va_cols.is_empty() {
            return Err(DdlError::UnsupportedDdl(
                "ANALYZE option must be specified when a column list is provided".into(),
            ));
        }
        let relid = maintained_table(interp, rv)?;
        if relkind(interp, relid) == Some(RelKind::Partitioned) {
            partitioned = Some((relid, rv.relname.clone()));
        } else {
            let index = clustered_index(interp, relid, stmt)?;
            if let Some(index) = index {
                super::tables::check_clusterable_index_oid(interp, relid, index)?;
            }
            repack_rel(interp, stmt.command, relid, index, concurrently)?;
            // analyze_rel.
            if analyze {
                analyze_columns(interp, relid, &vr.va_cols)?;
            }
            return Ok(());
        }
    }
    // Several tables.
    if analyze {
        return Err(DdlError::UnsupportedDdl(
            "cannot execute REPACK (ANALYZE) on multiple tables".into(),
        ));
    }
    if concurrently {
        return Err(DdlError::UnsupportedDdl(if partitioned.is_some() {
            "REPACK (CONCURRENTLY) is not supported for partitioned tables (Consider running the \
             command on individual partitions.)"
                .into()
        } else {
            "REPACK (CONCURRENTLY) requires an explicit table name".into()
        }));
    }
    prevent_in_transaction_block(interp, command)?;
    let targets: Vec<(PgClassOid, Option<PgClassOid>)> = match partitioned {
        // get_tables_to_repack.
        None if stmt.usingindex => interp
            .clustered_indexes
            .iter()
            .map(|(&table, &index)| (table, Some(index)))
            .collect(),
        None => interp
            .pg_class
            .values()
            .filter(|c| matches!(c.relkind, RelKind::Table | RelKind::MaterializedView))
            .map(|c| (c.oid, None))
            .collect(),
        // get_tables_to_repack_partitioned: the leaf partitions, with the
        // leaf indexes of the partitioned index named.
        Some((relid, relname)) if stmt.usingindex => {
            if stmt.indexname.is_empty() {
                return Err(DdlError::Parse(if stmt.command == RepackCommand::Cluster {
                    format!("there is no previously clustered index for table \"{relname}\"")
                } else {
                    format!(
                        "cannot execute {command} on partitioned table \"{relname}\" USING \
                         INDEX with no index name"
                    )
                }));
            }
            let index = super::tables::check_clusterable_index(interp, relid, &stmt.indexname)?;
            super::indexes::all_index_inheritors(interp, index)
                .into_iter()
                .filter(|&i| relkind(interp, i) == Some(RelKind::Index))
                .filter_map(|i| interp.pg_index.get(&i).map(|ix| (ix.indrelid, Some(i))))
                .collect()
        }
        Some((relid, _)) => super::tables::inherit::all_inheritors(interp, relid)
            .into_iter()
            .filter(|&t| relkind(interp, t) == Some(RelKind::Table))
            .map(|t| (t, None))
            .collect(),
    };
    for (table, index) in targets {
        // cluster_rel_recheck, then cluster_rel.
        if matches!(
            relkind(interp, table),
            Some(RelKind::Table | RelKind::MaterializedView)
        ) {
            repack_rel(interp, stmt.command, table, index, false)?;
        }
    }
    Ok(())
}

/// determine_clustered_index: the index named, or with USING INDEX alone
/// the one marked clustered.
fn clustered_index(
    interp: &PgCatalog,
    relid: PgClassOid,
    stmt: &RepackStmt,
) -> Result<Option<PgClassOid>, DdlError> {
    if !stmt.indexname.is_empty() {
        let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
        let found = nsoid.and_then(|ns| {
            interp
                .class_by_qname
                .get(&(ns, stmt.indexname.clone()))
                .copied()
        });
        return match found {
            Some(index) => Ok(Some(index)),
            None => Err(DdlError::TypeNotFound(format!(
                "index \"{}\" for table \"{}\" does not exist",
                stmt.indexname,
                relname(interp, relid)
            ))),
        };
    }
    if !stmt.usingindex {
        return Ok(None);
    }
    match interp.clustered_indexes.get(&relid) {
        Some(&index) => Ok(Some(index)),
        None => Err(DdlError::TypeNotFound(format!(
            "there is no previously clustered index for table \"{}\"",
            relname(interp, relid)
        ))),
    }
}

/// cluster_rel: rewrite `relid` (in the order of `index`, which it marks
/// clustered).
fn repack_rel(
    interp: &mut PgCatalog,
    command: RepackCommand,
    relid: PgClassOid,
    index: Option<PgClassOid>,
    concurrently: bool,
) -> Result<(), DdlError> {
    let name = repack_command_name(command);
    if concurrently {
        check_concurrent_repack(interp, relid)?;
    }
    // check_index_requirements: REPACK refuses a table with an invalid
    // index.
    if command == RepackCommand::Repack {
        let mut invalid: Vec<PgClassOid> = interp
            .pg_index
            .values()
            .filter(|i| i.indrelid == relid && interp.invalid_indexes.contains(&i.indexrelid))
            .map(|i| i.indexrelid)
            .collect();
        invalid.sort();
        if !invalid.is_empty() {
            let names: Vec<String> = invalid
                .iter()
                .map(|&i| format!("\"{}\"", relname(interp, i)))
                .collect();
            let detail = if names.len() == 1 {
                "An invalid index cannot be processed correctly"
            } else {
                "Some invalid indexes cannot be processed correctly"
            };
            return Err(DdlError::Parse(format!(
                "cannot execute {name} on relation \"{}\" ({detail}: {}.)",
                relname(interp, relid),
                names.join(", ")
            )));
        }
    }
    if index.is_some() && interp.is_shared_relation(relid) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot execute {} on a shared catalog (Marking an index as clustered on a shared \
             catalog would only take effect in the current database.)",
            if command == RepackCommand::Repack {
                "REPACK ... USING INDEX"
            } else {
                "CLUSTER"
            }
        )));
    }
    if let Some(index) = index {
        // REPACK may follow a system catalog's index only if it is the one
        // already clustered (CLUSTER historically may).
        if command != RepackCommand::Cluster
            && !interp.allow_system_table_mods.get()
            && relid.get() < FIRST_UNPINNED_OBJECT_ID
            && interp.clustered_indexes.get(&relid) != Some(&index)
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "permission denied: \"{}\" is a system catalog",
                relname(interp, relid)
            )));
        }
    }
    if interp.unpopulated_matviews.contains(&relid) {
        return Ok(());
    }
    if let Some(index) = index {
        // rebuild_relation: mark_index_clustered.
        interp.clustered_indexes.insert(relid, index);
    }
    heap_rewrites(interp, relid, false);
    Ok(())
}

/// check_concurrent_repack_requirements (with the default `wal_level`,
/// `replica`, and every table on heap).
fn check_concurrent_repack(interp: &mut PgCatalog, relid: PgClassOid) -> Result<(), DdlError> {
    let name = relname(interp, relid);
    let refuse = |detail: &str| {
        Err(DdlError::UnsupportedDdl(format!(
            "cannot execute REPACK (CONCURRENTLY) on relation \"{name}\" ({detail})"
        )))
    };
    if relid.get() < FIRST_UNPINNED_OBJECT_ID {
        return refuse("This operation is not supported for system catalogs.");
    }
    if interp.user_catalog_tables.contains(&relid) {
        return refuse("This operation is not supported for user catalog tables.");
    }
    super::tables::toast::note_toast_tables(interp);
    if relkind(interp, relid) == Some(RelKind::ToastTable) {
        return refuse("This operation is not supported for TOAST tables.");
    }
    if interp.relpersistence.contains_key(&relid) {
        return refuse("This operation is only supported for permanent relations.");
    }
    if relkind(interp, relid) == Some(RelKind::MaterializedView) {
        return refuse("This operation is not supported for materialized views.");
    }
    match interp.replica_identity_kinds.get(&relid) {
        Some('n') => {
            return refuse("This operation does not support tables with REPLICA IDENTITY NOTHING.");
        }
        Some('f') => {
            return refuse("This operation does not support tables with REPLICA IDENTITY FULL.");
        }
        _ => {}
    }
    // RelationGetReplicaIndex: the REPLICA IDENTITY USING INDEX one, or the
    // primary key unless it is deferrable.
    if interp.replica_identity_indexes.contains_key(&relid) {
        return Ok(());
    }
    let pkey = interp
        .pg_index
        .values()
        .find(|i| i.indisprimary && i.indrelid == relid)
        .map(|i| i.indexrelid);
    match pkey {
        Some(index) if interp.nonimmediate_indexes.contains(&index) => {
            refuse("This operation does not support deferrable primary keys.")
        }
        Some(_) => Ok(()),
        None => refuse(&format!("Relation \"{name}\" has no identity index.")),
    }
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
    let concurrently = super::txblock::reindex_options(&stmt.params)?;
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
    // The tablespace the indexes move to (any name is taken to exist, see
    // check_tablespace_placement).
    let tablespace = super::txblock::reindex_tablespace(&stmt.params);
    match stmt.kind {
        ReindexObjectType::ReindexObjectIndex => {
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
            if tablespace.is_some() {
                reindex_move(interp, &[relid], &[], concurrently, &tablespace)?;
            }
            super::indexes::new_relfilenode(interp, relid);
        }
        ReindexObjectType::ReindexObjectTable => {
            if let Some(rv) = stmt.relation.as_ref() {
                let relid = maintained_table(interp, rv)?;
                if relkind(interp, relid) == Some(RelKind::Partitioned) {
                    prevent_in_transaction_block(interp, "REINDEX TABLE")?;
                }
                no_catalog(relid)?;
                if tablespace.is_some() {
                    let tables = if relkind(interp, relid) == Some(RelKind::Partitioned) {
                        super::tables::inherit::all_inheritors(interp, relid)
                    } else {
                        vec![relid]
                    };
                    let indexes = indexes_of(interp, &tables);
                    reindex_move(interp, &indexes, &tables, concurrently, &tablespace)?;
                }
                new_relfilenodes(interp, &[relid]);
            }
        }
        // get_namespace_oid: the name as written — `pg_temp` is no schema's.
        ReindexObjectType::ReindexObjectSchema
            if stmt.name == "pg_temp" || interp.namespace_oid(&stmt.name).is_none() =>
        {
            return Err(DdlError::TableNotFound(format!(
                "schema \"{}\" does not exist",
                stmt.name
            )));
        }
        ReindexObjectType::ReindexObjectSystem if concurrently => {
            return Err(DdlError::UnsupportedDdl(
                "cannot reindex system catalogs concurrently".into(),
            ));
        }
        // ReindexMultipleTables, with a tablespace: the system relations
        // are skipped (with a WARNING); every other table's indexes move.
        // REINDEX SYSTEM moves nothing. (REINDEX DATABASE / SYSTEM name the
        // database the session is connected to, which the analyzer can't
        // know: any name is taken as it.)
        kind @ (ReindexObjectType::ReindexObjectSchema
        | ReindexObjectType::ReindexObjectDatabase)
            if tablespace.is_some() =>
        {
            let schema = (kind == ReindexObjectType::ReindexObjectSchema)
                .then(|| interp.namespace_oid(&stmt.name))
                .flatten();
            let tables: Vec<PgClassOid> = interp
                .pg_class
                .values()
                .filter(|c| matches!(c.relkind, RelKind::Table | RelKind::MaterializedView))
                .filter(|c| schema.is_none_or(|ns| c.relnamespace == ns))
                .filter(|c| !interp.is_system_class(c.oid))
                .map(|c| c.oid)
                .collect();
            let indexes = indexes_of(interp, &tables);
            reindex_move(interp, &indexes, &tables, concurrently, &tablespace)?;
        }
        _ => {}
    }
    Ok(())
}

/// The indexes on `tables`, in OID order.
fn indexes_of(interp: &PgCatalog, tables: &[PgClassOid]) -> Vec<PgClassOid> {
    let mut indexes: Vec<PgClassOid> = interp
        .pg_index
        .values()
        .filter(|i| tables.contains(&i.indrelid))
        .map(|i| i.indexrelid)
        .collect();
    indexes.sort();
    indexes
}

/// REINDEX (TABLESPACE ...) moving `indexes` (those of `tables`, for a
/// TABLE / SCHEMA / DATABASE target): a system relation's indexes don't
/// move (`cannot move system relation "i"`, without CONCURRENTLY, which
/// refuses catalogs before), and pg_global takes only shared relations —
/// rebuilt in place, an index of its own (`only shared relations can be
/// placed in pg_global tablespace`); concurrently, a table's TOAST index
/// too (`cannot move non-shared relation to tablespace "pg_global"`).
fn reindex_move(
    interp: &mut PgCatalog,
    indexes: &[PgClassOid],
    tables: &[PgClassOid],
    concurrently: bool,
    tablespace: &Option<String>,
) -> Result<(), DdlError> {
    if !concurrently && let Some(&index) = indexes.iter().find(|&&i| interp.is_system_class(i)) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot move system relation \"{}\"",
            relname(interp, index)
        )));
    }
    if tablespace.as_deref() != Some("pg_global") {
        return Ok(());
    }
    if concurrently {
        super::tables::toast::note_toast_tables(interp);
        if !indexes.is_empty() || tables.iter().any(|t| interp.toast_tables.contains(t)) {
            return Err(DdlError::UnsupportedDdl(
                "cannot move non-shared relation to tablespace \"pg_global\"".into(),
            ));
        }
    } else if !indexes.is_empty() {
        return Err(DdlError::Parse(
            "only shared relations can be placed in pg_global tablespace".into(),
        ));
    }
    Ok(())
}

/// pg_statistic, which analyze_rel never analyzes (update_attstats).
const STATISTIC_RELATION_ID: PgClassOid = PgClassOid::from_raw(2619);

/// What validate_va_cols_list refuses in an ANALYZE column list.
pub(crate) enum AnalyzeColumnError {
    /// `undefined_column`.
    Undefined(String),
    /// `duplicate_column`.
    Duplicate(String),
}

impl From<AnalyzeColumnError> for DdlError {
    fn from(e: AnalyzeColumnError) -> Self {
        match e {
            AnalyzeColumnError::Undefined(msg) => DdlError::Parse(msg),
            AnalyzeColumnError::Duplicate(msg) => DdlError::DuplicateObject(msg),
        }
    }
}

/// analyze_rel → validate_va_cols_list (analyze.c, PostgreSQL 19), for
/// ANALYZE, VACUUM (ANALYZE) and REPACK (ANALYZE): every column listed
/// exists (no system column) and is listed once — whatever the relation's
/// kind, which is checked after; pg_statistic is skipped before.
pub(crate) fn analyze_columns(
    interp: &PgCatalog,
    relid: PgClassOid,
    va_cols: &[Option<ast::Node>],
) -> Result<(), AnalyzeColumnError> {
    if relid == STATISTIC_RELATION_ID {
        return Ok(());
    }
    let mut seen: Vec<i16> = Vec::new();
    for col in va_cols
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
    {
        let Some(attnum) = interp
            .attribute_by_name(relid, col)
            .filter(|a| a.attnum > 0)
            .map(|a| a.attnum)
        else {
            return Err(AnalyzeColumnError::Undefined(format!(
                "column \"{col}\" of relation \"{}\" does not exist",
                relname(interp, relid)
            )));
        };
        if seen.contains(&attnum) {
            return Err(AnalyzeColumnError::Duplicate(format!(
                "column \"{col}\" of relation \"{}\" appears more than once",
                relname(interp, relid)
            )));
        }
        seen.push(attnum);
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
        let Some(ast::Node::VacuumRelation(vr)) = rel.as_ref() else {
            continue;
        };
        let Some(rv) = vr.relation.as_ref() else {
            continue;
        };
        let (_, relid) = super::util::lookup_relation(interp, rv)?;
        // vacuum(): VACUUM skips (with a WARNING) what vacuum_rel can't
        // vacuum, its ANALYZE included; a plain ANALYZE goes straight to
        // analyze_rel, which checks the column list whatever the
        // relation's kind (a column list implies ANALYZE).
        let vacuumable = matches!(
            relkind(interp, relid),
            Some(
                RelKind::Table
                    | RelKind::Partitioned
                    | RelKind::MaterializedView
                    | RelKind::ToastTable
            )
        );
        if stmt.is_vacuumcmd && !vacuumable {
            continue;
        }
        analyze_columns(interp, relid, &vr.va_cols)?;
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
    let string = |de: &typedpg_pg_query::ast::DefElem| -> Option<String> {
        match de.arg.as_ref()? {
            ast::Node::String(s) => Some(s.sval.clone()),
            ast::Node::Integer(i) => Some(i.ival.to_string()),
            ast::Node::Float(f) => Some(f.fval.clone()),
            ast::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.into()),
            _ => None,
        }
    };
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
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
            // defGetInt32 (PostgreSQL 19): an integer literal only.
            "parallel" => {
                let Some(ast::Node::Integer(i)) = de.arg.as_ref() else {
                    return syntax("parallel requires an integer value".into());
                };
                let n = i64::from(i.ival);
                if !(0..=1024).contains(&n) {
                    return syntax("PARALLEL option must be between 0 and 1024".into());
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
        && stmt.rels.iter().any(
            |r| matches!(r.as_ref(), Some(ast::Node::VacuumRelation(vr)) if !vr.va_cols.is_empty()),
        )
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
fn vacopt_from_boolean(de: &typedpg_pg_query::ast::DefElem) -> Result<bool, DdlError> {
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
        let Some(ast::Node::RangeVar(rv)) = rel.as_ref() else {
            continue;
        };
        let (_, relid) = super::util::lookup_relation(interp, rv)?;
        let kinds = match relkind(interp, relid) {
            // RangeVarCallbackForLockTable: plain and partitioned tables
            // and views only.
            Some(RelKind::Table | RelKind::Partitioned | RelKind::View) => {
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

/// COPY, in PG 19's order: DoCopy opens the relation, analyzes COPY FROM's
/// WHERE and reads the column list (CopyGetAttnums); BeginCopyTo refuses
/// a relation it can't copy from, then reads the options
/// (ProcessCopyOptions) and the FORCE_QUOTE columns; BeginCopyFrom reads
/// the options and the columns they name, and CopyFrom then refuses a
/// relation it can't copy to and checks FREEZE. COPY (query) TO reads the
/// options, then analyzes the query. The data itself (files, STDIN) isn't
/// modeled.
pub fn copy(interp: &PgCatalog, stmt: &typedpg_pg_query::ast::CopyStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        let opts = process_copy_options(stmt.is_from, &stmt.options)?;
        let Some(query) = stmt.query.as_ref() else {
            return Ok(());
        };
        // BeginCopyTo: the query runs, and FORCE_QUOTE names its columns
        // (CopyGetAttnums without a relation), every one of which is
        // copied.
        let Some(columns) =
            super::dml::verdict(crate::resolve::analyze_executed(interp, query, || {
                crate::resolve::analyze_raw_node(interp, query, &[])
            }))?
        else {
            return Ok(());
        };
        copy_query_shape(interp, query)?;
        if let CopyColumns::Names(names) = &opts.force_quote {
            let columns: Vec<CopyColumn> = columns
                .iter()
                .zip(1..)
                .map(|(c, attnum)| CopyColumn {
                    name: &c.name,
                    attnum,
                    generated: false,
                })
                .collect();
            copy_get_attnums(&columns, None, names)?;
        }
        return Ok(());
    };
    let (_, relid) = super::util::lookup_table(interp, rv)?;
    let kind = relkind(interp, relid);
    // DoCopy: COPY FROM's WHERE (the grammar takes none for COPY TO).
    if let Some(w) = stmt.where_clause.as_ref() {
        copy_where(interp, relid, w)?;
    }
    let attrs = interp.attributes_of(relid);
    let columns: Vec<CopyColumn> = attrs
        .iter()
        .map(|a| CopyColumn {
            name: &a.attname,
            attnum: a.attnum,
            generated: a.attgenerated.is_some(),
        })
        .collect();
    let attlist: Vec<String> = stmt
        .attlist
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .map(str::to_owned)
        .collect();
    let relname = rv.relname.as_str();
    let copied = copy_get_attnums(&columns, Some(relname), &attlist)?;
    let refuse = |msg: &str, hint: &str| Err(DdlError::Parse(format!("{msg} \"{relname}\"{hint}")));

    if !stmt.is_from {
        // BeginCopyTo: a table, a populated materialized view, or a
        // partitioned table — copied from its partitions, none of which
        // may be a foreign table.
        match kind {
            Some(RelKind::Table) => {}
            Some(RelKind::View) => {
                return refuse(
                    "cannot copy from view",
                    " (Try the COPY (SELECT ...) TO variant.)",
                );
            }
            Some(RelKind::MaterializedView) => {
                if interp.unpopulated_matviews.contains(&relid) {
                    return refuse(
                        "cannot copy from unpopulated materialized view",
                        " (Use the REFRESH MATERIALIZED VIEW command.)",
                    );
                }
            }
            Some(RelKind::ForeignTable) => {
                return refuse(
                    "cannot copy from foreign table",
                    " (Try the COPY (SELECT ...) TO variant.)",
                );
            }
            Some(RelKind::Sequence) => return refuse("cannot copy from sequence", ""),
            Some(RelKind::Partitioned) => {
                let mut parts = super::tables::inherit::all_inheritors(interp, relid);
                parts.sort();
                if let Some(foreign) = parts
                    .into_iter()
                    .find(|&p| relkind(interp, p) == Some(RelKind::ForeignTable))
                {
                    let name = self::relname(interp, foreign);
                    return Err(DdlError::Parse(format!(
                        "cannot copy from foreign table \"{name}\" (Partition \"{name}\" is a \
                         foreign table in partitioned table \"{relname}\". Try the COPY \
                         (SELECT ...) TO variant.)"
                    )));
                }
            }
            _ => return refuse("cannot copy from non-table relation", ""),
        }
        let opts = process_copy_options(false, &stmt.options)?;
        return check_copied(&columns, relname, &opts.force_quote, "FORCE_QUOTE", &copied);
    }

    // BeginCopyFrom: the options, then the columns they name.
    let opts = process_copy_options(true, &stmt.options)?;
    check_copied(
        &columns,
        relname,
        &opts.force_notnull,
        "FORCE_NOT_NULL",
        &copied,
    )?;
    check_copied(&columns, relname, &opts.force_null, "FORCE_NULL", &copied)?;
    if let Some(names) = &opts.convert_select {
        for attnum in copy_get_attnums(&columns, Some(relname), names)? {
            if !copied.contains(&attnum) {
                return Err(DdlError::Parse(format!(
                    "selected column \"{}\" not referenced by COPY",
                    column_name(&columns, attnum)
                )));
            }
        }
    }
    // CopyFrom: a table, a foreign or partitioned table, or a relation with
    // an INSTEAD OF INSERT row trigger (only a view can have one).
    let instead_of_insert = interp.triggers.get(&relid).is_some_and(|ts| {
        ts.iter()
            .any(|t| t.is_instead_row_for(crate::ddl::triggers::TRIGGER_TYPE_INSERT))
    });
    match kind {
        Some(RelKind::Table | RelKind::ForeignTable | RelKind::Partitioned) => {}
        _ if instead_of_insert => {}
        Some(RelKind::View) => {
            return refuse(
                "cannot copy to view",
                " (To enable copying to a view, provide an INSTEAD OF INSERT trigger.)",
            );
        }
        Some(RelKind::MaterializedView) => return refuse("cannot copy to materialized view", ""),
        Some(RelKind::Sequence) => return refuse("cannot copy to sequence", ""),
        _ => return refuse("cannot copy to non-table relation", ""),
    }
    if opts.freeze {
        copy_freeze(interp, relid, kind)?;
    }
    Ok(())
}

/// BeginCopyTo's checks of an analyzed COPY query: no SELECT INTO, and a
/// data-modifying statement must have a RETURNING clause. A statement on
/// a relation with rules is left alone: the rewriter may turn it into
/// none or several queries, which BeginCopyTo refuses first.
fn copy_query_shape(interp: &PgCatalog, query: &ast::Node) -> Result<(), DdlError> {
    let (relation, returning) = match query {
        ast::Node::SelectStmt(select) => {
            // The grammar leaves INTO on the leftmost SELECT of a set
            // operation.
            let mut leftmost = select.as_ref();
            while let Some(larg) = leftmost.larg.as_deref() {
                leftmost = larg;
            }
            if leftmost.into_clause.is_some() {
                return Err(DdlError::UnsupportedDdl(
                    "COPY (SELECT INTO) is not supported".into(),
                ));
            }
            return Ok(());
        }
        ast::Node::InsertStmt(s) => (s.relation.as_deref(), s.returning_clause.as_deref()),
        ast::Node::UpdateStmt(s) => (s.relation.as_deref(), s.returning_clause.as_deref()),
        ast::Node::DeleteStmt(s) => (s.relation.as_deref(), s.returning_clause.as_deref()),
        ast::Node::MergeStmt(s) => (s.relation.as_deref(), s.returning_clause.as_deref()),
        _ => return Ok(()),
    };
    if returning.is_some_and(|r| !r.exprs.is_empty()) {
        return Ok(());
    }
    let has_rules = relation
        .and_then(|rv| super::util::lookup_relation(interp, rv).ok())
        .is_none_or(|(_, relid)| interp.rules.get(&relid).is_some_and(|r| !r.is_empty()));
    if has_rules {
        return Ok(());
    }
    Err(DdlError::UnsupportedDdl(
        "COPY query must have a RETURNING clause".into(),
    ))
}

/// A column CopyGetAttnums can name: a relation's, or a COPY query's.
struct CopyColumn<'a> {
    name: &'a str,
    attnum: i16,
    generated: bool,
}

/// The name of column `attnum` of `columns`.
fn column_name<'a>(columns: &[CopyColumn<'a>], attnum: i16) -> &'a str {
    columns
        .iter()
        .find(|c| c.attnum == attnum)
        .map_or("", |c| c.name)
}

/// CopyGetAttnums: the columns `names` names — each one existing, not
/// generated, and named once — or, for no names, every column but the
/// generated ones. `relname` is the relation's name, `None` for a query's
/// columns.
fn copy_get_attnums(
    columns: &[CopyColumn],
    relname: Option<&str>,
    names: &[String],
) -> Result<Vec<i16>, DdlError> {
    if names.is_empty() {
        return Ok(columns
            .iter()
            .filter(|c| !c.generated)
            .map(|c| c.attnum)
            .collect());
    }
    let mut attnums = Vec::with_capacity(names.len());
    for name in names {
        let Some(column) = columns.iter().find(|c| c.name == name) else {
            return Err(DdlError::Parse(match relname {
                Some(rel) => format!("column \"{name}\" of relation \"{rel}\" does not exist"),
                None => format!("column \"{name}\" does not exist"),
            }));
        };
        if column.generated {
            return Err(DdlError::Parse(format!(
                "column \"{name}\" is a generated column (Generated columns cannot be used in \
                 COPY.)"
            )));
        }
        if attnums.contains(&column.attnum) {
            return Err(DdlError::DuplicateObject(format!(
                "column \"{name}\" specified more than once"
            )));
        }
        attnums.push(column.attnum);
    }
    Ok(attnums)
}

/// BeginCopyTo / BeginCopyFrom: the columns a FORCE_* `option` names
/// (CopyGetAttnums) must each be one the COPY copies.
fn check_copied(
    columns: &[CopyColumn],
    relname: &str,
    columns_option: &CopyColumns,
    option: &str,
    copied: &[i16],
) -> Result<(), DdlError> {
    let CopyColumns::Names(names) = columns_option else {
        return Ok(());
    };
    for attnum in copy_get_attnums(columns, Some(relname), names)? {
        if !copied.contains(&attnum) {
            return Err(DdlError::Parse(format!(
                "{option} column \"{}\" not referenced by COPY",
                column_name(columns, attnum)
            )));
        }
    }
    Ok(())
}

/// DoCopy's WHERE of COPY FROM: transformed over the table's row in
/// EXPR_KIND_COPY_WHERE and coerced to boolean, then every column it reads
/// (pull_varattnos, a whole-row reference reading all of them), in attnum
/// order, must be neither a system column nor a generated one.
fn copy_where(interp: &PgCatalog, relid: PgClassOid, expr: &ast::Node) -> Result<(), DdlError> {
    use crate::error::AnalyzeError;
    super::expr_kind::check_expr_kind(interp, Some(expr), super::expr_kind::ExprKind::CopyWhere)?;
    // DoCopy's ParseState has no parameters.
    if let Some(number) = expr.nodes().find_map(|(n, _)| match n {
        typedpg_pg_query::NodeRef::ParamRef(p) => Some(p.number),
        _ => None,
    }) {
        return Err(DdlError::Parse(format!("there is no parameter ${number}")));
    }
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or_default();
    let attrs = interp.attributes_of(relid);
    // The relation, as addNSItemToQuery puts it in the namespace, and
    // coerce_to_boolean's "argument of WHERE".
    let mut scope = crate::scope::Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname.to_owned(), class.relname.clone()),
        attrs,
    );
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let ctx = crate::expr::Ctx::new(&scope, &null_ctx, interp);
    match crate::clause::coerce_clause_expr(
        Some(expr),
        ctx,
        &mut params,
        crate::clause::ClauseKind::Where,
    ) {
        Ok(_)
        // The analyzer's own gaps leave the expression unchecked.
        | Err(
            AnalyzeError::Unsupported(_)
            | AnalyzeError::UnsupportedJoinType(_)
            | AnalyzeError::Internal(_)
            | AnalyzeError::Lex(_)
            | AnalyzeError::Serde(_)
            | AnalyzeError::Io(_),
        ) => {}
        Err(e) => return Err(DdlError::UnsupportedDdl(e.to_string())),
    }
    let mut read: std::collections::BTreeSet<i16> = std::collections::BTreeSet::new();
    for (n, _) in expr.nodes() {
        let typedpg_pg_query::NodeRef::ColumnRef(cr) = n else {
            continue;
        };
        // transformColumnRef: the fields naming the relation, then the
        // column (`*` for the whole row; `None` here).
        let fields: Vec<Option<&str>> = cr
            .fields
            .iter()
            .map(|f| super::util::node_string(f.as_ref()))
            .collect();
        let is = |i: usize, name: &str| fields.get(i).copied().flatten() == Some(name);
        let qualifiers = match fields.len() {
            1 => 0,
            2 if is(0, &class.relname) => 1,
            3 if is(0, nspname) && is(1, &class.relname) => 2,
            3 if is(0, &class.relname) => 1,
            n if n >= 4 && is(n - 2, &class.relname) => n - 1,
            _ => 0,
        };
        let whole_row = || attrs.iter().map(|a| a.attnum).collect::<Vec<_>>();
        match fields.get(qualifiers).copied().flatten() {
            None => read.extend(whole_row()),
            Some(name) => {
                if let Some(a) = attrs.iter().find(|a| a.attname == name) {
                    read.insert(a.attnum);
                } else if let Some((_, _, attnum)) = crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .find(|(s, ..)| *s == name)
                {
                    read.insert(*attnum);
                } else if fields.len() == 1 && name == class.relname {
                    read.extend(whole_row());
                }
            }
        }
    }
    for attnum in read {
        if attnum < 0 {
            let name = crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .find(|(.., n)| *n == attnum)
                .map_or("", |(s, ..)| *s);
            return Err(DdlError::Parse(format!(
                "system columns are not supported in COPY FROM WHERE conditions (Column \
                 \"{name}\" is a system column.)"
            )));
        }
        if let Some(a) = attrs
            .iter()
            .find(|a| a.attnum == attnum && a.attgenerated.is_some())
        {
            return Err(DdlError::Parse(format!(
                "generated columns are not supported in COPY FROM WHERE conditions (Column \
                 \"{}\" is a generated column.)",
                a.attname
            )));
        }
    }
    Ok(())
}

/// CopyFrom's FREEZE checks, in order: not a partitioned table, not a
/// foreign table; no snapshot or open cursor of the transaction's but
/// its first (ThereAreNoPriorRegisteredSnapshots, ThereAreNoReadyPortals:
/// "prior transaction activity"); then a relation created or given new
/// storage in the current subtransaction. The last two are reported only
/// where certain: not inside a running body (a DO block, a procedure),
/// whose executor holds snapshots of its own, nor while user code may have
/// opened or closed cursors; and as savepoints aren't followed, only a
/// relation that predates the transaction and got no new storage in it
/// (TRUNCATE, a rewrite — any ALTER TABLE counts) certainly fails.
fn copy_freeze(
    interp: &PgCatalog,
    relid: PgClassOid,
    kind: Option<RelKind>,
) -> Result<(), DdlError> {
    match kind {
        Some(RelKind::Partitioned) => {
            return Err(DdlError::UnsupportedDdl(
                "cannot perform COPY FREEZE on a partitioned table".into(),
            ));
        }
        Some(RelKind::ForeignTable) => {
            return Err(DdlError::UnsupportedDdl(
                "cannot perform COPY FREEZE on a foreign table".into(),
            ));
        }
        _ => {}
    }
    if super::do_block::running_body() {
        return Ok(());
    }
    match interp.cursors.any_open() {
        Some(true) => {
            return Err(DdlError::UnsupportedDdl(
                "cannot perform COPY FREEZE because of prior transaction activity".into(),
            ));
        }
        Some(false) => {}
        None => return Ok(()),
    }
    if relid.get() < interp.transaction_oid_floor
        && !interp.tables_with_new_relfilenode.contains(&relid)
    {
        return Err(DdlError::UnsupportedDdl(
            "cannot perform COPY FREEZE because the table was not created or truncated in the \
             current subtransaction"
                .into(),
        ));
    }
    Ok(())
}

/// A utility option's value as defGetString renders it.
fn def_string(de: &typedpg_pg_query::ast::DefElem) -> Option<String> {
    match de.arg.as_ref()? {
        ast::Node::String(s) => Some(s.sval.clone()),
        ast::Node::Integer(i) => Some(i.ival.to_string()),
        ast::Node::Float(f) => Some(f.fval.clone()),
        ast::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.into()),
        _ => None,
    }
}

/// CHECKPOINT [( options )] (ExecCheckpoint, PostgreSQL 19): MODE FAST or
/// SPREAD and FLUSH_UNLOGGED, then the privileges of `pg_checkpoint`.
pub fn checkpoint(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::CheckPointStmt,
) -> Result<(), DdlError> {
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "mode" => {
                let Some(mode) = def_string(de) else {
                    return Err(DdlError::Parse("mode requires a parameter".into()));
                };
                if mode != "fast" && mode != "spread" {
                    return Err(DdlError::Parse(format!(
                        "unrecognized value for CHECKPOINT option \"mode\": \"{mode}\""
                    )));
                }
            }
            "flush_unlogged" => {
                super::session::def_get_boolean(de)?;
            }
            name => {
                return Err(DdlError::Parse(format!(
                    "unrecognized CHECKPOINT option \"{name}\""
                )));
            }
        }
    }
    let current = super::roles::current_role(interp);
    let allowed = interp
        .roles
        .id("pg_checkpoint")
        .is_none_or(|role| interp.roles.has_privs_of_role(current, role));
    if !allowed && !interp.roles.is_superuser(current) {
        return Err(DdlError::UnsupportedDdl(
            "permission denied to execute CHECKPOINT command (Only roles with privileges of \
             the \"pg_checkpoint\" role may execute this command.)"
                .into(),
        ));
    }
    Ok(())
}

/// WAIT FOR LSN 'lsn' [WITH ( options )] (ExecWaitStmt, PostgreSQL 19), as
/// a migration runs it — on a primary: the LSN and the options are checked;
/// a standby mode (the default) finds no recovery in progress, which is an
/// error unless NO_THROW; PRIMARY_FLUSH waits for the WAL to be flushed.
pub fn wait_for_lsn(stmt: &typedpg_pg_query::ast::WaitStmt) -> Result<(), DdlError> {
    crate::literal_input::validate_pg_lsn(&stmt.lsn_literal).map_err(DdlError::Parse)?;
    let conflicting = || DdlError::Parse("conflicting or redundant options".into());
    let (mut mode, mut timeout, mut no_throw) = (None::<String>, false, None::<bool>);
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "mode" => {
                if mode.is_some() {
                    return Err(conflicting());
                }
                let Some(value) = def_string(de) else {
                    return Err(DdlError::Parse("mode requires a parameter".into()));
                };
                let lowered = value.to_ascii_lowercase();
                if !matches!(
                    lowered.as_str(),
                    "standby_replay" | "standby_write" | "standby_flush" | "primary_flush"
                ) {
                    return Err(DdlError::Parse(format!(
                        "unrecognized value for WAIT option \"mode\": \"{value}\""
                    )));
                }
                mode = Some(lowered);
            }
            "timeout" => {
                if timeout {
                    return Err(conflicting());
                }
                timeout = true;
                let Some(value) = def_string(de) else {
                    return Err(DdlError::Parse("timeout requires a parameter".into()));
                };
                let Some(ms) = super::guc::parse_with_unit(&value, "ms") else {
                    return Err(DdlError::Parse(format!(
                        "invalid timeout value: \"{value}\""
                    )));
                };
                if ms.round() > f64::from(i32::MAX) || ms.round() < f64::from(i32::MIN) {
                    return Err(DdlError::Parse(format!(
                        "invalid timeout value: \"{value}\""
                    )));
                }
                if ms.round() < 0.0 {
                    return Err(DdlError::Parse("timeout cannot be negative".into()));
                }
            }
            "no_throw" => {
                if no_throw.is_some() {
                    return Err(conflicting());
                }
                no_throw = Some(super::session::def_get_boolean(de)?);
            }
            name => {
                return Err(DdlError::Parse(format!("option \"{name}\" not recognized")));
            }
        }
    }
    if mode.as_deref() != Some("primary_flush") && no_throw != Some(true) {
        return Err(DdlError::UnsupportedDdl(
            "recovery is not in progress".into(),
        ));
    }
    Ok(())
}

/// defGetString: an option's value as text, `%s requires a parameter`
/// without one.
fn copy_string(de: &typedpg_pg_query::ast::DefElem) -> Result<String, DdlError> {
    match de.arg.as_ref() {
        None => Err(DdlError::Parse(format!(
            "{} requires a parameter",
            de.defname
        ))),
        Some(ast::Node::AStar(_)) => Ok("*".into()),
        Some(ast::Node::List(l)) => Ok(l
            .items
            .iter()
            .filter_map(|n| super::util::node_string(n.as_ref()))
            .collect::<Vec<_>>()
            .join(".")),
        Some(_) => Ok(def_string(de).unwrap_or_default()),
    }
}

/// COPY's ON_ERROR (CopyOnErrorChoice).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyOnError {
    Stop,
    Ignore,
    SetNull,
}

/// The columns a FORCE_QUOTE / FORCE_NOT_NULL / FORCE_NULL option names
/// (`force_*_all` / the `force_*` list).
#[derive(Default)]
enum CopyColumns {
    #[default]
    Unset,
    All,
    Names(Vec<String>),
}

/// What the checks after ProcessCopyOptions read of the options.
#[derive(Default)]
struct CopyOptions {
    freeze: bool,
    force_quote: CopyColumns,
    force_notnull: CopyColumns,
    force_null: CopyColumns,
    /// CONVERT_SELECTIVELY's list (`convert_select`), when given.
    convert_select: Option<Vec<String>>,
}

/// The column names of a FORCE_* / CONVERT_SELECTIVELY list.
fn copy_column_names(arg: Option<&ast::Node>) -> Vec<String> {
    match arg {
        Some(ast::Node::List(l)) => l
            .items
            .iter()
            .filter_map(|n| super::util::node_string(n.as_ref()))
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// ProcessCopyOptions (copy.c, PostgreSQL 19), in its order: each option
/// once and with a value it takes, then the options against each other,
/// the format and the direction.
fn process_copy_options(
    is_from: bool,
    options: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<CopyOptions, DdlError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Format {
        Text,
        Csv,
        Binary,
        Json,
    }
    let conflicting = || DdlError::Parse("conflicting or redundant options".into());
    let column_list = |de: &typedpg_pg_query::ast::DefElem| {
        DdlError::Parse(format!(
            "argument to option \"{}\" must be a list of column names",
            de.defname
        ))
    };
    let mut format = Format::Text;
    let (mut format_specified, mut freeze_specified, mut header_specified) = (false, false, false);
    let (mut on_error_specified, mut log_verbosity_specified) = (false, false);
    let (mut reject_limit_specified, mut force_array_specified) = (false, false);
    let (mut freeze, mut header, mut force_array) = (false, false, false);
    let mut delim: Option<String> = None;
    let mut null_print: Option<String> = None;
    let mut default_print: Option<String> = None;
    let mut quote: Option<String> = None;
    let mut escape: Option<String> = None;
    let (mut force_quote, mut force_notnull, mut force_null) = (false, false, false);
    let (mut convert_selectively, mut encoding) = (false, false);
    let mut on_error = CopyOnError::Stop;
    let mut reject_limit = 0;
    let mut parsed = CopyOptions::default();
    for opt in options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "format" => {
                let fmt = copy_string(de)?;
                if format_specified {
                    return Err(conflicting());
                }
                format_specified = true;
                format = match fmt.as_str() {
                    "text" => Format::Text,
                    "csv" => Format::Csv,
                    "binary" => Format::Binary,
                    "json" => Format::Json,
                    _ => {
                        return Err(DdlError::UnsupportedDdl(format!(
                            "COPY format \"{fmt}\" not recognized"
                        )));
                    }
                };
            }
            "freeze" => {
                if freeze_specified {
                    return Err(conflicting());
                }
                freeze_specified = true;
                freeze = super::session::def_get_boolean(de)?;
            }
            "delimiter" | "null" | "default" | "quote" | "escape" => {
                let slot = match de.defname.as_str() {
                    "delimiter" => &mut delim,
                    "null" => &mut null_print,
                    "default" => &mut default_print,
                    "quote" => &mut quote,
                    _ => &mut escape,
                };
                if slot.is_some() {
                    return Err(conflicting());
                }
                *slot = Some(copy_string(de)?);
            }
            "header" => {
                if header_specified {
                    return Err(conflicting());
                }
                header_specified = true;
                header = copy_header(de, is_from)?;
            }
            "force_quote" | "force_not_null" | "force_null" => {
                let slot = match de.defname.as_str() {
                    "force_quote" => &mut force_quote,
                    "force_not_null" => &mut force_notnull,
                    _ => &mut force_null,
                };
                if *slot {
                    return Err(conflicting());
                }
                let columns = match de.arg {
                    Some(ast::Node::AStar(_)) => CopyColumns::All,
                    Some(ast::Node::List(_)) => {
                        CopyColumns::Names(copy_column_names(de.arg.as_ref()))
                    }
                    _ => return Err(column_list(de)),
                };
                *slot = true;
                match de.defname.as_str() {
                    "force_quote" => parsed.force_quote = columns,
                    "force_not_null" => parsed.force_notnull = columns,
                    _ => parsed.force_null = columns,
                }
            }
            "convert_selectively" => {
                if convert_selectively {
                    return Err(conflicting());
                }
                convert_selectively = true;
                if !matches!(de.arg, None | Some(ast::Node::List(_))) {
                    return Err(column_list(de));
                }
                parsed.convert_select = Some(copy_column_names(de.arg.as_ref()));
            }
            "encoding" => {
                if encoding {
                    return Err(conflicting());
                }
                let name = copy_string(de)?;
                if super::conversions::encoding(&name).is_none() {
                    return Err(DdlError::Parse(format!(
                        "argument to option \"{}\" must be a valid encoding name",
                        de.defname
                    )));
                }
                encoding = true;
            }
            "force_array" => {
                if force_array_specified {
                    return Err(conflicting());
                }
                force_array_specified = true;
                force_array = super::session::def_get_boolean(de)?;
            }
            "on_error" => {
                if on_error_specified {
                    return Err(conflicting());
                }
                on_error_specified = true;
                // defGetCopyOnErrorChoice.
                let sval = copy_string(de)?;
                if !is_from {
                    return Err(DdlError::Parse(
                        "COPY ON_ERROR cannot be used with COPY TO".into(),
                    ));
                }
                on_error = match sval.to_ascii_lowercase().as_str() {
                    "stop" => CopyOnError::Stop,
                    "ignore" => CopyOnError::Ignore,
                    "set_null" => CopyOnError::SetNull,
                    _ => {
                        return Err(DdlError::Parse(format!(
                            "COPY ON_ERROR \"{sval}\" not recognized"
                        )));
                    }
                };
            }
            "log_verbosity" => {
                if log_verbosity_specified {
                    return Err(conflicting());
                }
                log_verbosity_specified = true;
                // defGetCopyLogVerbosityChoice.
                let sval = copy_string(de)?;
                if !matches!(
                    sval.to_ascii_lowercase().as_str(),
                    "silent" | "default" | "verbose"
                ) {
                    return Err(DdlError::Parse(format!(
                        "COPY LOG_VERBOSITY \"{sval}\" not recognized"
                    )));
                }
            }
            "reject_limit" => {
                if reject_limit_specified {
                    return Err(conflicting());
                }
                reject_limit_specified = true;
                // defGetCopyRejectLimitOption: a number, or a string
                // pg_strtoint64 reads.
                let Some(limit) = super::seqparams::numeric_arg(de)? else {
                    return Err(DdlError::Parse(format!(
                        "{} requires a numeric value",
                        de.defname
                    )));
                };
                if limit <= 0 {
                    return Err(DdlError::Parse(format!(
                        "REJECT_LIMIT ({limit}) must be greater than zero"
                    )));
                }
                reject_limit = limit;
            }
            other => {
                return Err(DdlError::Parse(format!(
                    "option \"{other}\" not recognized"
                )));
            }
        }
    }

    // Options BINARY and JSON don't take (checked before the defaults).
    let mode = match format {
        Format::Binary => Some("BINARY"),
        Format::Json => Some("JSON"),
        _ => None,
    };
    if let Some(mode) = mode {
        for (given, option) in [
            (delim.is_some(), "DELIMITER"),
            (null_print.is_some(), "NULL"),
            (default_print.is_some(), "DEFAULT"),
        ] {
            if given {
                return Err(DdlError::Parse(format!(
                    "cannot specify {option} in {mode} mode"
                )));
            }
        }
    }
    let csv = format == Format::Csv;
    let delim = delim.unwrap_or_else(|| if csv { "," } else { "\t" }.into());
    let null_print = null_print.unwrap_or_else(|| if csv { "" } else { "\\N" }.into());
    let (quote_given, escape_given) = (quote.is_some(), escape.is_some());
    let quote = quote.unwrap_or_else(|| "\"".into());
    let escape = escape.unwrap_or_else(|| quote.clone());
    let has_newline = |s: &str| s.contains(['\r', '\n']);

    if delim.len() != 1 {
        return Err(DdlError::UnsupportedDdl(
            "COPY delimiter must be a single one-byte character".into(),
        ));
    }
    let delim_byte = delim.as_bytes()[0];
    if has_newline(&delim) {
        return Err(DdlError::Parse(
            "COPY delimiter cannot be newline or carriage return".into(),
        ));
    }
    if has_newline(&null_print) {
        return Err(DdlError::Parse(
            "COPY null representation cannot use newline or carriage return".into(),
        ));
    }
    if default_print.as_deref().is_some_and(has_newline) {
        return Err(DdlError::Parse(
            "COPY default representation cannot use newline or carriage return".into(),
        ));
    }
    if !csv && b"\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(&delim_byte) {
        return Err(DdlError::Parse(format!(
            "COPY delimiter cannot be \"{delim}\""
        )));
    }
    if header && let Some(mode) = mode {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot specify HEADER in {mode} mode"
        )));
    }
    if !csv && quote_given {
        return Err(DdlError::UnsupportedDdl(
            "COPY QUOTE requires CSV mode".into(),
        ));
    }
    if csv && quote.len() != 1 {
        return Err(DdlError::UnsupportedDdl(
            "COPY quote must be a single one-byte character".into(),
        ));
    }
    if csv && delim_byte == quote.as_bytes()[0] {
        return Err(DdlError::Parse(
            "COPY delimiter and quote must be different".into(),
        ));
    }
    if !csv && escape_given {
        return Err(DdlError::UnsupportedDdl(
            "COPY ESCAPE requires CSV mode".into(),
        ));
    }
    if csv && escape.len() != 1 {
        return Err(DdlError::UnsupportedDdl(
            "COPY escape must be a single one-byte character".into(),
        ));
    }
    for (given, option, refused_from) in [
        (force_quote, "FORCE_QUOTE", true),
        (force_notnull, "FORCE_NOT_NULL", false),
        (force_null, "FORCE_NULL", false),
    ] {
        if !given {
            continue;
        }
        if !csv {
            return Err(DdlError::UnsupportedDdl(format!(
                "COPY {option} requires CSV mode"
            )));
        }
        if is_from == refused_from {
            let direction = if is_from { "COPY FROM" } else { "COPY TO" };
            return Err(DdlError::Parse(format!(
                "COPY {option} cannot be used with {direction}"
            )));
        }
    }
    if null_print.as_bytes().contains(&delim_byte) {
        return Err(DdlError::Parse(
            "COPY delimiter character must not appear in the NULL specification".into(),
        ));
    }
    if csv && null_print.as_bytes().contains(&quote.as_bytes()[0]) {
        return Err(DdlError::Parse(
            "CSV quote character must not appear in the NULL specification".into(),
        ));
    }
    if freeze && !is_from {
        return Err(DdlError::Parse(
            "COPY FREEZE cannot be used with COPY TO".into(),
        ));
    }
    if format == Format::Json && is_from {
        return Err(DdlError::Parse(
            "COPY FORMAT JSON is not supported for COPY FROM".into(),
        ));
    }
    if format != Format::Json && force_array {
        return Err(DdlError::Parse(
            "COPY FORCE_ARRAY can only be used with JSON mode".into(),
        ));
    }
    if let Some(default_print) = default_print {
        if !is_from {
            return Err(DdlError::UnsupportedDdl(
                "COPY DEFAULT cannot be used with COPY TO".into(),
            ));
        }
        if default_print.as_bytes().contains(&delim_byte) {
            return Err(DdlError::UnsupportedDdl(
                "COPY delimiter character must not appear in the DEFAULT specification".into(),
            ));
        }
        if csv && default_print.as_bytes().contains(&quote.as_bytes()[0]) {
            return Err(DdlError::UnsupportedDdl(
                "CSV quote character must not appear in the DEFAULT specification".into(),
            ));
        }
        if null_print == default_print {
            return Err(DdlError::UnsupportedDdl(
                "NULL specification and DEFAULT specification cannot be the same".into(),
            ));
        }
    }
    if format == Format::Binary && on_error != CopyOnError::Stop {
        return Err(DdlError::Parse(
            "only ON_ERROR STOP is allowed in BINARY mode".into(),
        ));
    }
    if reject_limit != 0 && on_error != CopyOnError::Ignore {
        return Err(DdlError::Parse(
            "COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE".into(),
        ));
    }
    parsed.freeze = freeze;
    Ok(parsed)
}

/// defGetCopyHeaderOption (PostgreSQL 19): whether COPY's HEADER option is
/// on — a Boolean, "match" (COPY FROM only), or a number of lines to skip
/// (zero or more; one at most for COPY TO).
fn copy_header(de: &typedpg_pg_query::ast::DefElem, is_from: bool) -> Result<bool, DdlError> {
    let lines: i64 = match de.arg.as_ref() {
        None => 1,
        Some(ast::Node::Integer(i)) => i64::from(i.ival),
        Some(ast::Node::Boolean(b)) => i64::from(b.boolval),
        Some(other) => {
            let Some(s) = super::session::string_arg(other) else {
                return Ok(true);
            };
            let s = s.as_str();
            if let Some(v) = super::reloptions::parse_bool(s) {
                i64::from(v)
            } else if s.eq_ignore_ascii_case("match") {
                if !is_from {
                    return Err(DdlError::Parse(
                        "cannot use \"match\" with HEADER in COPY TO".into(),
                    ));
                }
                return Ok(true);
            } else {
                match s.trim().parse::<i32>() {
                    Ok(n) => i64::from(n),
                    Err(_) => {
                        return Err(DdlError::Parse(format!(
                            "{} requires a Boolean value, an integer value greater than or \
                             equal to zero, or the string \"match\"",
                            de.defname
                        )));
                    }
                }
            }
        }
    };
    if lines < 0 {
        return Err(DdlError::Parse(format!(
            "a negative integer value cannot be specified for {}",
            de.defname
        )));
    }
    if !is_from && lines > 1 {
        return Err(DdlError::UnsupportedDdl(
            "cannot use multi-line header in COPY TO".into(),
        ));
    }
    Ok(lines != 0)
}
