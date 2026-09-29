//! Column-list assembly for `CREATE TABLE`.
//!
//! Ports the two PG passes that turn a `CreateStmt` into the final column
//! list:
//!
//! - `transformCreateStmt` (`parse_utilcmd.c`): `OF type` contributes the
//!   type's attributes first (`transformOfType`), then the table elements
//!   in order, with each `LIKE source` expanded in place
//!   (`transformTableLikeClause`).
//! - `MergeAttributes` (`tablecmds.c`): typed-table column options merge
//!   into the type's columns, duplicate names are rejected, and the parents
//!   of `INHERITS (...)` / `PARTITION OF` contribute their columns *first*,
//!   with same-named columns merged (`MergeInheritedAttribute`,
//!   `MergeChildAttribute`).

use super::*;

/// `TableLikeOption` bits (`parsenodes.h`).
pub(crate) const LIKE_CONSTRAINTS: u32 = 1 << 2;
pub(crate) const LIKE_DEFAULTS: u32 = 1 << 3;
pub(crate) const LIKE_GENERATED: u32 = 1 << 4;
pub(crate) const LIKE_IDENTITY: u32 = 1 << 5;
pub(crate) const LIKE_INDEXES: u32 = 1 << 6;

/// A `LIKE source INCLUDING ...` clause whose constraints / indexes are
/// copied once the new relation exists.
pub(crate) struct LikeCopy {
    pub(crate) source: PgClassOid,
    pub(crate) options: u32,
}

/// Output of [`assemble_columns`].
pub(crate) struct AssembledColumns {
    pub(crate) columns: Vec<ParsedColumn>,
    /// Parents in `INHERITS` / `PARTITION OF` order.
    pub(crate) parents: Vec<PgClassOid>,
    pub(crate) likes: Vec<LikeCopy>,
}

/// One column-list entry before `MergeAttributes`.
struct Entry {
    col: ParsedColumn,
    /// `false` for typed-table / partition column options (`name WITH
    /// OPTIONS ...`), which carry no type of their own.
    has_type: bool,
    /// Column contributed by `OF type` that no option entry merged into yet.
    is_from_type: bool,
}

pub(crate) fn assemble_columns(
    interp: &PgCatalog,
    stmt: &CreateStmt,
    pk_columns: &[String],
) -> Result<AssembledColumns, DdlError> {
    let is_partition = stmt.partbound.is_some();
    let relname = stmt.relation.as_ref().map_or("", |rv| rv.relname.as_str());
    let cx = ColumnContext {
        of_type: stmt.of_typename.is_some(),
        partbound: is_partition,
        partitioned: stmt.partspec.is_some(),
    };
    let mut entries: Vec<Entry> = Vec::new();
    let mut likes: Vec<LikeCopy> = Vec::new();

    if let Some(tn) = stmt.of_typename.as_ref() {
        of_type_columns(interp, tn, &mut entries)?;
    }

    for elt in &stmt.table_elts {
        match elt.node.as_ref() {
            Some(node::Node::ColumnDef(cd)) => entries.push(Entry {
                col: parse_column_def(interp, relname, cd, pk_columns, cx)?,
                has_type: cd.type_name.is_some(),
                is_from_type: false,
            }),
            Some(node::Node::TableLikeClause(lc)) => {
                expand_like(interp, lc, &mut entries, &mut likes)?;
            }
            _ => {}
        }
    }

    // Typed-table options merge into the type's column; any other repeated
    // name is an error (`MergeAttributes`' first loop).
    let mut i = 0;
    while i < entries.len() {
        if !is_partition && !entries[i].has_type {
            return Err(DdlError::Parse(format!(
                "column \"{}\" does not exist",
                entries[i].col.name
            )));
        }
        let mut j = i + 1;
        while j < entries.len() {
            if entries[j].col.name != entries[i].col.name {
                j += 1;
                continue;
            }
            if !entries[i].is_from_type {
                return Err(DdlError::DuplicateObject(format!(
                    "column \"{}\" specified more than once",
                    entries[i].col.name
                )));
            }
            let opt = entries.remove(j).col;
            let target = &mut entries[i];
            target.col.not_null = opt.not_null;
            target.col.has_default = opt.has_default;
            target.col.generated = opt.generated;
            target.col.identity = opt.identity;
            target.col.owned_sequence = opt.owned_sequence;
            target.col.nn_local = opt.nn_local;
            target.col.nn_name = opt.nn_name;
            target.col.nn_no_inherit = opt.nn_no_inherit;
            target.is_from_type = false;
        }
        i += 1;
    }

    let (inherited, parents) = inherited_columns(interp, stmt, is_partition)?;
    if parents.is_empty() {
        return Ok(AssembledColumns {
            columns: entries.into_iter().map(|e| e.col).collect(),
            parents,
            likes,
        });
    }

    // Merge the local definitions into the inherited ones; what's left is
    // appended after every inherited column.
    let mut columns = inherited;
    let mut locals = Vec::new();
    for entry in entries {
        match columns.iter_mut().find(|c| c.name == entry.col.name) {
            Some(inh) if is_partition => merge_partition_column(inh, entry.col),
            Some(inh) => merge_child_column(inh, entry.col)?,
            None if is_partition => {
                return Err(DdlError::Parse(format!(
                    "column \"{}\" does not exist",
                    entry.col.name
                )));
            }
            None => locals.push(entry.col),
        }
    }
    columns.extend(locals);
    Ok(AssembledColumns {
        columns,
        parents,
        likes,
    })
}

/// `transformOfType`: the composite type's attributes, flagged so column
/// options can merge into them.
fn of_type_columns(
    interp: &PgCatalog,
    tn: &typedpg_pg_query::protobuf::TypeName,
    entries: &mut Vec<Entry>,
) -> Result<(), DdlError> {
    let type_oid = lookup_type_name(tn, interp)?;
    let relid = super::typed::check_of_type(interp, type_oid)?;
    for attr in interp.attributes_of(relid) {
        entries.push(Entry {
            col: ParsedColumn {
                name: attr.attname.clone(),
                type_oid: attr.atttypid,
                typmod: attr.atttypmod,
                not_null: false,
                has_default: false,
                generated: None,
                identity: None,
                collation: attr.attcollation,
                owned_sequence: None,
                identity_options: Vec::new(),
                nn_local: false,
                nn_name: None,
                nn_no_inherit: false,
                nn_inhcount: 0,
                nn_inh_name: None,
                is_local: true,
                inhcount: 0,
            },
            has_type: true,
            is_from_type: true,
        });
    }
    Ok(())
}

/// `transformTableLikeClause`: the source's columns, with defaults,
/// generation expressions and identity copied only when requested. NOT NULL
/// is always copied (PG 18 copies not-null constraints unconditionally);
/// views, matviews and composite types have no NOT NULL markings in PG's
/// catalog, so nothing is copied from those.
fn expand_like(
    interp: &PgCatalog,
    lc: &typedpg_pg_query::protobuf::TableLikeClause,
    entries: &mut Vec<Entry>,
    likes: &mut Vec<LikeCopy>,
) -> Result<(), DdlError> {
    let rv = lc
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("LIKE without relation".into()))?;
    let source = lookup_relation(interp, rv)?;
    let relkind = interp.pg_class.get(&source).map(|c| c.relkind);
    let copy_not_null = match relkind {
        Some(RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable) => true,
        Some(RelKind::View | RelKind::MaterializedView | RelKind::CompositeType) => false,
        _ => {
            return Err(DdlError::Parse(format!(
                "relation \"{}\" is invalid in LIKE clause",
                rv.relname
            )));
        }
    };
    let opts = lc.options;
    for attr in interp.attributes_of(source) {
        let generated = attr.attgenerated.filter(|_| opts & LIKE_GENERATED != 0);
        let identity = attr.attidentity.filter(|_| opts & LIKE_IDENTITY != 0);
        let plain_default = attr.atthasdef
            && attr.attgenerated.is_none()
            && attr.attidentity.is_none()
            && opts & LIKE_DEFAULTS != 0;
        entries.push(Entry {
            col: ParsedColumn {
                name: attr.attname.clone(),
                type_oid: attr.atttypid,
                typmod: attr.atttypmod,
                not_null: copy_not_null && attr.attnotnull,
                has_default: plain_default || generated.is_some() || identity.is_some(),
                generated,
                identity,
                collation: attr.attcollation,
                owned_sequence: identity.map(|_| crate::pg_catalog::DepType::Internal),
                // transformTableLikeClause: the source sequence's options.
                identity_options: identity
                    .and_then(|_| {
                        crate::ddl::sequences::identity_sequences(interp, source, attr.attnum)
                            .first()
                            .and_then(|seq| interp.sequence_params.get(seq))
                            .map(crate::ddl::seqparams::SeqParams::as_options)
                    })
                    .unwrap_or_default(),
                nn_local: copy_not_null && attr.attnotnull,
                nn_name: None,
                nn_no_inherit: false,
                nn_inhcount: 0,
                nn_inh_name: None,
                is_local: true,
                inhcount: 0,
            },
            has_type: true,
            is_from_type: false,
        });
    }
    likes.push(LikeCopy {
        source,
        options: opts,
    });
    Ok(())
}

/// Resolve an existing relation named by a `RangeVar`, with PG's
/// `relation "x" does not exist` when it doesn't.
fn lookup_relation(
    interp: &PgCatalog,
    rv: &typedpg_pg_query::protobuf::RangeVar,
) -> Result<PgClassOid, DdlError> {
    let (schema, name) = range_var_names(rv, interp);
    interp
        .namespace_oid(&schema)
        .and_then(|ns| interp.class_by_qname.get(&(ns, name.clone())).copied())
        .ok_or_else(|| {
            let shown = if rv.schemaname.is_empty() {
                name.clone()
            } else {
                QualifiedName::new(&schema, &name).to_string()
            };
            DdlError::TableNotFound(format!("relation \"{shown}\" does not exist"))
        })
}

/// The parents' columns in order, same-named columns merged
/// (`MergeInheritedAttribute`). A regular inheritance child doesn't inherit
/// identity; a partition does.
fn inherited_columns(
    interp: &PgCatalog,
    stmt: &CreateStmt,
    is_partition: bool,
) -> Result<(Vec<ParsedColumn>, Vec<PgClassOid>), DdlError> {
    let mut columns: Vec<ParsedColumn> = Vec::new();
    let mut parents: Vec<PgClassOid> = Vec::new();
    for parent_node in &stmt.inh_relations {
        let Some(node::Node::RangeVar(rv)) = parent_node.node.as_ref() else {
            continue;
        };
        let parent = lookup_relation(interp, rv)?;
        let relkind = interp.pg_class.get(&parent).map(|c| c.relkind);
        if is_partition {
            if relkind != Some(RelKind::Partitioned) {
                return Err(DdlError::Parse(format!(
                    "\"{}\" is not partitioned",
                    rv.relname
                )));
            }
        } else {
            match relkind {
                Some(RelKind::Table | RelKind::ForeignTable) => {}
                Some(RelKind::Partitioned) => {
                    return Err(DdlError::Parse(format!(
                        "cannot inherit from partitioned table \"{}\"",
                        rv.relname
                    )));
                }
                _ => {
                    return Err(DdlError::Parse(format!(
                        "inherited relation \"{}\" is not a table or foreign table",
                        rv.relname
                    )));
                }
            }
        }
        if parents.contains(&parent) {
            return Err(DdlError::DuplicateObject(format!(
                "relation \"{}\" would be inherited from more than once",
                rv.relname
            )));
        }
        parents.push(parent);

        for attr in interp.attributes_of(parent) {
            let identity = if is_partition { attr.attidentity } else { None };
            let parent_nn = attr
                .attnotnull
                .then(|| super::inherit::not_null_name(interp, parent, attr.attnum));
            let has_default = attr.atthasdef && (attr.attidentity.is_none() || identity.is_some());
            if let Some(existing) = columns.iter_mut().find(|c| c.name == attr.attname) {
                if existing.type_oid != attr.atttypid || existing.typmod != attr.atttypmod {
                    return Err(DdlError::Parse(format!(
                        "inherited column \"{}\" has a type conflict",
                        attr.attname
                    )));
                }
                if existing.collation != attr.attcollation {
                    return Err(DdlError::Parse(format!(
                        "inherited column \"{}\" has a collation conflict",
                        attr.attname
                    )));
                }
                existing.not_null |= attr.attnotnull;
                existing.has_default |= has_default;
                existing.generated = existing.generated.or(attr.attgenerated);
                existing.inhcount += 1;
                if let Some(nn) = parent_nn {
                    existing.nn_inhcount += 1;
                    existing.nn_inh_name.get_or_insert(nn);
                }
                continue;
            }
            columns.push(ParsedColumn {
                name: attr.attname.clone(),
                type_oid: attr.atttypid,
                typmod: attr.atttypmod,
                not_null: attr.attnotnull,
                has_default,
                generated: attr.attgenerated,
                identity,
                collation: attr.attcollation,
                owned_sequence: None,
                identity_options: Vec::new(),
                nn_local: false,
                nn_name: None,
                nn_no_inherit: false,
                nn_inhcount: i16::from(parent_nn.is_some()),
                nn_inh_name: parent_nn,
                is_local: false,
                inhcount: 1,
            });
        }
    }
    Ok((columns, parents))
}

/// `MergeChildAttribute`: a local column definition that repeats an
/// inherited column must agree on type and collation; NOT NULL is OR-ed and
/// a local default / identity wins.
fn merge_child_column(inh: &mut ParsedColumn, local: ParsedColumn) -> Result<(), DdlError> {
    if inh.type_oid != local.type_oid || inh.typmod != local.typmod {
        return Err(DdlError::Parse(format!(
            "column \"{}\" has a type conflict",
            local.name
        )));
    }
    if local.collation.is_some() && local.collation != inh.collation {
        return Err(DdlError::Parse(format!(
            "column \"{}\" has a collation conflict",
            local.name
        )));
    }
    inh.not_null |= local.not_null;
    inh.nn_local |= local.nn_local;
    if local.nn_name.is_some() {
        inh.nn_name = local.nn_name;
    }
    inh.is_local = true;
    if local.has_default {
        inh.has_default = true;
    }
    if local.identity.is_some() {
        inh.identity = local.identity;
        inh.owned_sequence = local.owned_sequence;
    }
    Ok(())
}

/// Partition column options (`PARTITION OF parent (col WITH OPTIONS ...)`):
/// they carry no type, only NOT NULL / DEFAULT to layer over the parent's
/// column.
fn merge_partition_column(inh: &mut ParsedColumn, local: ParsedColumn) {
    inh.not_null |= local.not_null;
    inh.nn_local |= local.nn_local;
    if local.nn_name.is_some() {
        inh.nn_name = local.nn_name;
    }
    if local.has_default {
        inh.has_default = true;
    }
}
