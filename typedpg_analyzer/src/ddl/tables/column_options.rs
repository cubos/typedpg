//! `ALTER COLUMN ... SET STATISTICS / SET STORAGE / SET COMPRESSION /
//! SET (...) / RESET (...)`. None of them changes a column's type, but PG
//! validates the target column and the new setting (`ATExecSetStatistics`,
//! `ATExecSetStorage`, `ATExecSetCompression`, `ATExecSetOptions`).

use super::*;
use crate::pg_catalog::{SYSTEM_COLUMNS, TypStorage};

/// `MAX_STATISTICS_TARGET`: larger targets are lowered with a WARNING.
const MIN_STATISTICS_TARGET: i32 = -1;

pub(super) fn alter_column_setting(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    subtype: AlterTableType,
) -> Result<(), DdlError> {
    let relname = relname_of(interp, relid);
    let is_index = interp
        .pg_class
        .get(&relid)
        .is_some_and(|c| matches!(c.relkind, RelKind::Index | RelKind::PartitionedIndex));

    if subtype == AlterTableType::AtSetStatistics {
        if cmd.name.is_empty() && !is_index {
            return Err(DdlError::UnsupportedDdl(
                "cannot refer to non-index column by number".into(),
            ));
        }
        let target = match cmd.def.as_deref().and_then(|d| d.node.as_ref()) {
            Some(node::Node::Integer(i)) => i.ival,
            _ => -1,
        };
        if target < MIN_STATISTICS_TARGET {
            return Err(DdlError::UnsupportedDdl(format!(
                "statistics target {target} is too low"
            )));
        }
        if is_index {
            return check_index_statistics_column(interp, relid, &relname, cmd);
        }
    }

    // The column itself.
    if SYSTEM_COLUMNS.iter().any(|(n, ..)| *n == cmd.name) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter system column \"{}\"",
            cmd.name
        )));
    }
    let Some(attr) = interp.attribute_by_name(relid, &cmd.name) else {
        return Err(DdlError::Parse(column_not_found_msg(
            interp, relid, &cmd.name,
        )));
    };
    // ATExecSetStatistics: ANALYZE skips virtual generated columns.
    if subtype == AlterTableType::AtSetStatistics
        && attr.attgenerated == Some(crate::pg_catalog::AttGenerated::Virtual)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter statistics on virtual generated column \"{}\"",
            cmd.name
        )));
    }
    let def_name = || match cmd.def.as_deref().and_then(|d| d.node.as_ref()) {
        Some(node::Node::String(s)) => Some(s.sval.clone()),
        _ => None,
    };

    match subtype {
        AlterTableType::AtSetStorage => {
            let storage =
                get_attribute_storage(interp, attr.atttypid, &def_name().unwrap_or_default())?;
            record_attr_storage(interp, relid, attr.attnum, storage);
        }
        AlterTableType::AtSetCompression => {
            let method = def_name().unwrap_or_default();
            if !method.is_empty() {
                get_attribute_compression(interp, attr.atttypid, &method)?;
            }
        }
        AlterTableType::AtSetOptions => check_attribute_options(cmd.def.as_deref())?,
        _ => {}
    }
    Ok(())
}

/// GetAttributeStorage (tablecmds.c): the storage mode `mode` for a column
/// of type `typid` — only PLAIN for a type that can't be toasted.
pub(crate) fn get_attribute_storage(
    interp: &PgCatalog,
    typid: PgTypeOid,
    mode: &str,
) -> Result<TypStorage, DdlError> {
    let typstorage = interp
        .pg_type
        .get(&typid)
        .map_or(TypStorage::Plain, |t| t.typstorage);
    let storage = match mode.to_ascii_lowercase().as_str() {
        "plain" => TypStorage::Plain,
        "external" => TypStorage::External,
        "extended" => TypStorage::Extended,
        "main" => TypStorage::Main,
        "default" => typstorage,
        _ => {
            return Err(DdlError::UnsupportedDdl(format!(
                "invalid storage type \"{mode}\""
            )));
        }
    };
    if storage != TypStorage::Plain && typstorage == TypStorage::Plain {
        return Err(DdlError::UnsupportedDdl(format!(
            "column data type {} can only have storage PLAIN",
            format_type_for_message(interp, typid)
        )));
    }
    Ok(storage)
}

/// GetAttributeCompression (tablecmds.c): the compression method `method`
/// for a column of type `typid` — none for a type that can't be toasted.
pub(crate) fn get_attribute_compression(
    interp: &PgCatalog,
    typid: PgTypeOid,
    method: &str,
) -> Result<(), DdlError> {
    let typstorage = interp
        .pg_type
        .get(&typid)
        .map_or(TypStorage::Plain, |t| t.typstorage);
    if typstorage == TypStorage::Plain {
        if method == "default" {
            return Ok(());
        }
        return Err(DdlError::UnsupportedDdl(format!(
            "column data type {} does not support compression",
            format_type_for_message(interp, typid)
        )));
    }
    if !matches!(method, "default" | "pglz" | "lz4") {
        return Err(DdlError::UnsupportedDdl(format!(
            "invalid compression method \"{method}\""
        )));
    }
    Ok(())
}

/// BuildDescForRelation: a column definition's STORAGE and COMPRESSION
/// clauses, for its type. Returns the STORAGE clause's mode.
pub(crate) fn check_column_def_options(
    interp: &PgCatalog,
    cd: &typedpg_pg_query::protobuf::ColumnDef,
    typid: PgTypeOid,
) -> Result<Option<TypStorage>, DdlError> {
    let storage = if cd.storage_name.is_empty() {
        None
    } else {
        Some(get_attribute_storage(interp, typid, &cd.storage_name)?)
    };
    if !cd.compression.is_empty() {
        get_attribute_compression(interp, typid, &cd.compression)?;
    }
    Ok(storage)
}

/// Remember column `relid.attnum`'s `attstorage` when it isn't its type's.
pub(crate) fn record_attr_storage(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    storage: TypStorage,
) {
    let typstorage = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .and_then(|a| interp.pg_type.get(&a.atttypid))
        .map(|t| t.typstorage);
    if typstorage == Some(storage) {
        interp.attr_storage.remove(&(relid, attnum));
    } else {
        interp.attr_storage.insert((relid, attnum), storage);
    }
}

/// ChooseIndexColumnNames (indexcmds.c): an index column is named after
/// its table column, or "expr", with a numeric suffix appended until it is
/// unique among the earlier ones.
fn index_column_names(interp: &PgCatalog, index: &crate::pg_catalog::PgIndex) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for &key in &index.indkey {
        let origname = interp
            .attributes_of(index.indrelid)
            .iter()
            .find(|a| key != 0 && a.attnum == key)
            .map_or_else(|| "expr".to_owned(), |a| a.attname.clone());
        let mut name = origname.clone();
        let mut i = 1;
        while names.contains(&name) {
            let suffix = i.to_string();
            let mut base = origname.clone();
            // NAMEDATALEN - 1 bytes in all.
            while base.len() + suffix.len() > 63 {
                base.pop();
            }
            name = base + &suffix;
            i += 1;
        }
        names.push(name);
    }
    names
}

/// `ALTER INDEX ... ALTER COLUMN {n | name} SET STATISTICS`
/// (ATExecSetStatistics): only expression key columns of the index carry
/// their own statistics.
fn check_index_statistics_column(
    interp: &PgCatalog,
    relid: PgClassOid,
    relname: &str,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let Some(index) = interp.pg_index.get(&relid) else {
        return Ok(());
    };
    let names = index_column_names(interp, index);
    let position = if cmd.name.is_empty() {
        let num = cmd.num;
        usize::try_from(num - 1)
            .ok()
            .filter(|&i| i < names.len())
            .ok_or_else(|| {
                DdlError::Parse(format!(
                    "column number {num} of relation \"{relname}\" does not exist"
                ))
            })?
    } else {
        names.iter().position(|n| *n == cmd.name).ok_or_else(|| {
            DdlError::Parse(format!(
                "column \"{}\" of relation \"{relname}\" does not exist",
                cmd.name
            ))
        })?
    };
    let colname = &names[position];
    if position >= usize::try_from(index.indnkeyatts).unwrap_or(0) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter statistics on included column \"{colname}\" of index \"{relname}\""
        )));
    }
    if index.indkey[position] != 0 {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot alter statistics on non-expression column \"{colname}\" of index \
             \"{relname}\" (Alter statistics on table column instead.)"
        )));
    }
    Ok(())
}

/// `attribute_reloptions`: the only per-column options are the float
/// `n_distinct` and `n_distinct_inherited`, each at least -1.
fn check_attribute_options(def: Option<&typedpg_pg_query::protobuf::Node>) -> Result<(), DdlError> {
    let Some(node::Node::List(list)) = def.and_then(|d| d.node.as_ref()) else {
        return Ok(());
    };
    let mut seen: Vec<&str> = Vec::new();
    for item in &list.items {
        let Some(node::Node::DefElem(de)) = item.node.as_ref() else {
            continue;
        };
        // transformRelOptions: no namespaces are valid for columns.
        if !de.defnamespace.is_empty() {
            return Err(DdlError::UnsupportedDdl(format!(
                "unrecognized parameter namespace \"{}\"",
                de.defnamespace
            )));
        }
        let name = de.defname.as_str();
        if name != "n_distinct" && name != "n_distinct_inherited" {
            return Err(DdlError::UnsupportedDdl(format!(
                "unrecognized parameter \"{name}\""
            )));
        }
        if seen.contains(&name) {
            return Err(DdlError::UnsupportedDdl(format!(
                "parameter \"{name}\" specified more than once"
            )));
        }
        seen.push(name);
        // defGetString; a bare option name means "true".
        let raw = match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
            None => "true".to_owned(),
            Some(node::Node::Integer(i)) => i.ival.to_string(),
            Some(node::Node::Float(f)) => f.fval.clone(),
            Some(node::Node::String(s)) => s.sval.clone(),
            Some(node::Node::Boolean(b)) => b.boolval.to_string(),
            _ => continue,
        };
        // parse_real: strtod, rejecting out-of-range values.
        let value = raw
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| {
                DdlError::UnsupportedDdl(format!(
                    "invalid value for floating point option \"{name}\": {raw}"
                ))
            })?;
        if value < -1.0 {
            return Err(DdlError::UnsupportedDdl(format!(
                "value {raw} out of bounds for option \"{name}\""
            )));
        }
    }
    Ok(())
}
