//! Extended statistics (`CREATE / ALTER / DROP STATISTICS`). They don't
//! affect typing, but PG validates their definition (`CreateStatistics`)
//! and keeps their names unique per schema, and a statistics object goes
//! away with its table or any column it reads.

use typedpg_pg_query::protobuf::{AlterStatsStmt, CreateStatsStmt, node};

use super::DdlError;
use super::util::node_string;
use crate::oid::{PgClassOid, PgNamespaceOid};
use crate::pg_catalog::{PgCatalog, RelKind, SYSTEM_COLUMNS};

/// `pg_statistic_ext`: a statistics object on one relation.
#[derive(Clone, Debug)]
pub(crate) struct StatisticsObject {
    pub(crate) name: String,
    pub(crate) namespace: PgNamespaceOid,
    pub(crate) relid: PgClassOid,
    /// The columns it reads (`stxkeys` and the expressions' columns).
    pub(crate) attnums: Vec<i16>,
}

/// STATS_MAX_DIMENSIONS.
const MAX_DIMENSIONS: usize = 8;

fn split(names: &[typedpg_pg_query::protobuf::Node]) -> (Option<String>, String) {
    let parts: Vec<&str> = names.iter().filter_map(node_string).collect();
    match parts.as_slice() {
        [schema, name] => (Some((*schema).to_owned()), (*name).to_owned()),
        [.., name] => (None, (*name).to_owned()),
        [] => (None, String::new()),
    }
}

/// get_statistics_object_oid: an unqualified name is looked up along the
/// search path.
fn find(interp: &PgCatalog, names: &[typedpg_pg_query::protobuf::Node]) -> Option<usize> {
    let (schema, name) = split(names);
    interp
        .schemas_for_lookup(schema.as_deref())
        .into_iter()
        .find_map(|ns| {
            interp
                .statistics
                .iter()
                .position(|s| s.namespace == ns && s.name == name)
        })
}

fn not_found(names: &[typedpg_pg_query::protobuf::Node]) -> DdlError {
    let parts: Vec<&str> = names.iter().filter_map(node_string).collect();
    DdlError::TypeNotFound(format!(
        "statistics object \"{}\" does not exist",
        parts.join(".")
    ))
}

pub fn create_statistics(interp: &mut PgCatalog, stmt: &CreateStatsStmt) -> Result<(), DdlError> {
    // ProcessUtilitySlow.
    if stmt.relations.len() != 1 {
        return Err(DdlError::UnsupportedDdl(
            "only a single relation is allowed in CREATE STATISTICS".into(),
        ));
    }
    let Some(node::Node::RangeVar(rv)) = stmt.relations[0].node.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let class = interp.pg_class.get(&relid).cloned();
    let kinds = match class.as_ref().map(|c| c.relkind) {
        Some(
            RelKind::Table
            | RelKind::Partitioned
            | RelKind::ForeignTable
            | RelKind::MaterializedView,
        ) => None,
        Some(RelKind::View) => Some("views"),
        Some(RelKind::Sequence) => Some("sequences"),
        Some(RelKind::Index | RelKind::PartitionedIndex) => Some("indexes"),
        Some(RelKind::CompositeType) => Some("composite types"),
        _ => Some("this relation"),
    };
    if let Some(kinds) = kinds {
        return Err(DdlError::Parse(format!(
            "cannot define statistics for relation \"{}\" (This operation is not supported \
             for {kinds}.)",
            rv.relname
        )));
    }
    let Some(class) = class else {
        return Ok(());
    };

    // The name: explicit, or ChooseExtendedStatisticName in the table's
    // schema.
    let explicit = (!stmt.defnames.is_empty()).then(|| split(&stmt.defnames));
    let namespace = match explicit.as_ref().and_then(|(s, _)| s.as_deref()) {
        Some(schema) => interp.namespace_oid(schema).ok_or_else(|| {
            DdlError::TableNotFound(format!("schema \"{schema}\" does not exist"))
        })?,
        None if explicit.is_some() => {
            let schema = super::util::creation_schema(interp)?;
            interp.namespace_oid(&schema).unwrap_or(class.relnamespace)
        }
        None => class.relnamespace,
    };
    if let Some((_, name)) = explicit.as_ref()
        && interp
            .statistics
            .iter()
            .any(|s| s.namespace == namespace && &s.name == name)
    {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "statistics object \"{name}\" already exists"
        )));
    }

    // The columns and expressions.
    let virtual_column = || {
        DdlError::UnsupportedDdl(
            "statistics creation on virtual generated columns is not supported".into(),
        )
    };
    let mut columns: Vec<i16> = Vec::new();
    let mut exprs: Vec<super::tables::check_inherit::StoredExpr> = Vec::new();
    let mut attnums: Vec<i16> = Vec::new();
    let mut name_parts: Vec<String> = Vec::new();
    for elem in &stmt.exprs {
        let Some(node::Node::StatsElem(se)) = elem.node.as_ref() else {
            continue;
        };
        if !se.name.is_empty() {
            if SYSTEM_COLUMNS.iter().any(|(n, ..)| *n == se.name) {
                return Err(DdlError::UnsupportedDdl(
                    "statistics creation on system columns is not supported".into(),
                ));
            }
            let Some(attr) = interp.attribute_by_name(relid, &se.name).cloned() else {
                return Err(DdlError::Parse(format!(
                    "column \"{}\" does not exist",
                    se.name
                )));
            };
            if attr.attgenerated == Some(crate::pg_catalog::AttGenerated::Virtual) {
                return Err(virtual_column());
            }
            if !super::opclass::has_default_btree_opclass(interp, attr.atttypid) {
                return Err(DdlError::UnsupportedDdl(format!(
                    "column \"{}\" cannot be used in statistics because its type {} has no \
                     default btree operator class",
                    se.name,
                    super::util::format_type_for_message(interp, attr.atttypid)
                )));
            }
            columns.push(attr.attnum);
            attnums.push(attr.attnum);
            name_parts.push(se.name.clone());
        } else if let Some(expr) = se.expr.as_deref() {
            if let Some(Err(e)) = super::volatile::infer_over_relation(interp, relid, expr, None) {
                return Err(DdlError::UnsupportedDdl(e.to_string()));
            }
            if let Some(inner) = expr.node.as_ref() {
                for (n, ..) in inner.nodes() {
                    if let typedpg_pg_query::NodeRef::ColumnRef(cr) = n
                        && let Some(col) = cr.fields.last().and_then(node_string)
                        && let Some(a) = interp.attribute_by_name(relid, col)
                    {
                        // pull_varattnos: no virtual generated column.
                        if a.attgenerated == Some(crate::pg_catalog::AttGenerated::Virtual) {
                            return Err(virtual_column());
                        }
                        attnums.push(a.attnum);
                    }
                }
            }
            exprs.push(super::tables::check_inherit::StoredExpr::written(expr));
            name_parts.push("expr".into());
        }
    }
    if columns.len() + exprs.len() > MAX_DIMENSIONS {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot have more than {MAX_DIMENSIONS} columns in statistics"
        )));
    }
    // Statistic kinds.
    for kind in stmt.stat_types.iter().filter_map(node_string) {
        if !matches!(kind, "ndistinct" | "dependencies" | "mcv") {
            return Err(DdlError::Parse(format!(
                "unrecognized statistics kind \"{kind}\""
            )));
        }
    }
    let single_expression = columns.is_empty() && exprs.len() == 1;
    if single_expression && !stmt.stat_types.is_empty() {
        return Err(DdlError::UnsupportedDdl(
            "when building statistics on a single expression, statistics kinds may not be \
             specified"
                .into(),
        ));
    }
    if !single_expression && columns.len() + exprs.len() < 2 {
        return Err(DdlError::Parse(
            "extended statistics require at least 2 columns".into(),
        ));
    }
    let mut sorted = columns.clone();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w[0] == w[1]) {
        return Err(DdlError::DuplicateObject(
            "duplicate column name in statistics definition".into(),
        ));
    }
    if exprs
        .iter()
        .enumerate()
        .any(|(i, e)| exprs[..i].contains(e))
    {
        return Err(DdlError::DuplicateObject(
            "duplicate expression in statistics definition".into(),
        ));
    }

    let name = match explicit {
        Some((_, name)) => name,
        None => choose_name(interp, namespace, &class.relname, &name_parts),
    };
    attnums.sort_unstable();
    attnums.dedup();
    interp.statistics.push(StatisticsObject {
        name,
        namespace,
        relid,
        attnums,
    });
    Ok(())
}

/// ChooseExtendedStatisticName: `<table>_<columns>_stat`, numbered when
/// taken in the schema.
fn choose_name(
    interp: &PgCatalog,
    namespace: PgNamespaceOid,
    relname: &str,
    parts: &[String],
) -> String {
    // ChooseExtendedStatisticNameAddition: the names joined, not made
    // unique, cut at NAMEDATALEN.
    let mut addition = String::new();
    for part in parts {
        if !addition.is_empty() {
            addition.push('_');
        }
        addition.push_str(part);
        if addition.len() >= 64 {
            break;
        }
    }
    let base = super::util::make_object_name(relname, &addition, "stat");
    let mut candidate = base.clone();
    let mut n = 0;
    while interp
        .statistics
        .iter()
        .any(|s| s.namespace == namespace && s.name == candidate)
    {
        n += 1;
        let label = format!("stat{n}");
        candidate = super::util::make_object_name(relname, &addition, &label);
    }
    candidate
}

/// `ALTER STATISTICS [IF EXISTS] name SET STATISTICS n` (AlterStatistics).
pub fn alter_statistics(interp: &PgCatalog, stmt: &AlterStatsStmt) -> Result<(), DdlError> {
    if find(interp, &stmt.defnames).is_none() && !stmt.missing_ok {
        return Err(not_found(&stmt.defnames));
    }
    Ok(())
}

/// `DROP STATISTICS [IF EXISTS] name`.
pub(crate) fn drop_statistics(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::List(l)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    match find(interp, &l.items) {
        Some(i) => {
            interp.statistics.remove(i);
            Ok(())
        }
        None if missing_ok => Ok(()),
        None => Err(not_found(&l.items)),
    }
}

/// `ALTER STATISTICS name RENAME TO new` (AlterObjectRename_internal).
pub(crate) fn rename_statistics(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(node::Node::List(l)) = stmt.object.as_deref().and_then(|o| o.node.as_ref()) else {
        return Ok(());
    };
    let Some(i) = find(interp, &l.items) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(not_found(&l.items));
    };
    let namespace = interp.statistics[i].namespace;
    if interp
        .statistics
        .iter()
        .any(|s| s.namespace == namespace && s.name == stmt.newname)
    {
        let schema = interp.namespace_name(namespace).unwrap_or("?").to_owned();
        return Err(DdlError::DuplicateObject(format!(
            "statistics object \"{}\" already exists in schema \"{schema}\"",
            stmt.newname
        )));
    }
    interp.statistics[i].name = stmt.newname.clone();
    Ok(())
}

/// Whether a statistics object `names` exists (get_object_address).
pub(crate) fn statistics_exist(
    interp: &PgCatalog,
    names: &[typedpg_pg_query::protobuf::Node],
) -> bool {
    find(interp, names).is_some()
}

/// A dropped column takes the statistics objects reading it along.
pub(crate) fn drop_column_statistics(interp: &mut PgCatalog, relid: PgClassOid, attnum: i16) {
    interp
        .statistics
        .retain(|s| !(s.relid == relid && s.attnums.contains(&attnum)));
}
