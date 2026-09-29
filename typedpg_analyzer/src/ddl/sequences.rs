//! CREATE / ALTER SEQUENCE handlers.
//!
//! A sequence is registered as a `pg_class` row with `relkind = Sequence`.
//! The analyzer doesn't model the sequence's numeric state (START, INCREMENT,
//! …) — those options don't affect query typing, so they're accepted as
//! no-ops. DROP SEQUENCE is handled by `ddl/drop.rs` (sequences share the
//! relation-drop path with tables and views). RENAME / SET SCHEMA flow
//! through `ddl/alter.rs`.

use typedpg_pg_query::protobuf::{AlterSeqStmt, CreateSeqStmt, node};

use super::DdlError;
use super::util::{choose_relation_name, ensure_range_var, node_string, range_var_names};
use crate::oid::{PgClassOid, PgGenericOid, PgNamespaceOid};
use crate::pg_catalog::{
    DepType, PG_CLASS_RELID, PgAttribute, PgCatalog, PgClass, PgDepend, RelKind, oid,
};

/// `CREATE SEQUENCE [IF NOT EXISTS] name [options]`.
pub fn create_sequence(interp: &mut PgCatalog, stmt: &CreateSeqStmt) -> Result<(), DdlError> {
    let rv = stmt
        .sequence
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE SEQUENCE without relation".into()))?;

    let (nsoid, name) = ensure_range_var(interp, rv)?;

    if interp.class_by_qname.contains_key(&(nsoid, name.clone())) && stmt.if_not_exists {
        return Ok(());
    }
    super::util::check_relation_name_free(interp, nsoid, &name)?;
    let params = super::seqparams::init_params(interp, &stmt.options, None)?;

    let seq_oid = insert_sequence_relation(interp, nsoid, name)?;
    interp.sequence_params.insert(seq_oid, params);
    for opt in &stmt.options {
        apply_owned_by(interp, seq_oid, opt)?;
    }
    Ok(())
}

/// Register a sequence relation: a `pg_class` row (relkind 'S') and the
/// three columns every PG sequence exposes to `SELECT * FROM seq`
/// (`last_value bigint`, `log_cnt bigint`, `is_called boolean`, all NOT
/// NULL — `DefineSequence` in `sequence.c`).
pub(crate) fn insert_sequence_relation(
    interp: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    name: String,
) -> Result<PgClassOid, DdlError> {
    let class_oid = PgClassOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name,
        relnamespace: nsoid,
        relkind: RelKind::Sequence,
        reltype: None,
    });
    let columns = [
        ("last_value", oid::INT8),
        ("log_cnt", oid::INT8),
        ("is_called", oid::BOOL),
    ];
    for (i, (attname, atttypid)) in columns.into_iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: attname.to_owned(),
            atttypid,
            attnum: (i + 1) as i16,
            attnotnull: true,
            atthasdef: false,
            attgenerated: None,
            atttypmod: None,
            attidentity: None,
            attcollation: None,
            attislocal: true,
            attinhcount: 0,
        });
    }
    Ok(class_oid)
}

/// The implicit sequence behind a `serial` column (`deptype = Auto`, as
/// with `OWNED BY`) or an identity column (`deptype = Internal`), named
/// `<table>_<column>_seq` like `transformColumnDefinition` /
/// `generateSerialExtraStmts` do (`ChooseRelationName`).
pub(crate) fn create_owned_sequence(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    deptype: DepType,
    options: &[typedpg_pg_query::protobuf::Node],
) -> Result<(), DdlError> {
    let Some(class) = interp.pg_class.get(&relid).cloned() else {
        return Err(DdlError::Internal(format!("relation oid {relid} missing")));
    };
    let colname = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .map(|a| a.attname.clone())
        .unwrap_or_default();
    // An identity's `SEQUENCE NAME` option names the sequence.
    let explicit_name = options.iter().find_map(|o| match o.node.as_ref() {
        Some(node::Node::DefElem(de)) if de.defname == "sequence_name" => {
            match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
                Some(node::Node::List(l)) => {
                    let parts: Vec<&str> = l.items.iter().filter_map(node_string).collect();
                    match parts.as_slice() {
                        [schema, name] => Some((Some((*schema).to_owned()), (*name).to_owned())),
                        [.., name] => Some((None, (*name).to_owned())),
                        [] => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    });
    // A serial / identity sequence has its column's integer type; its
    // options are checked like CREATE SEQUENCE's.
    let column_type = interp
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .map_or(oid::INT8, |a| a.atttypid);
    let sequence_options: Vec<typedpg_pg_query::protobuf::Node> = options
        .iter()
        .filter(|o| {
            !matches!(o.node.as_ref(), Some(node::Node::DefElem(de))
                if matches!(de.defname.as_str(), "sequence_name" | "generated" | "logged" | "unlogged"))
        })
        .cloned()
        .collect();
    let params = super::seqparams::init_column_params(interp, &sequence_options, column_type)?;
    let (nsoid, name) = match explicit_name {
        Some((schema, name)) => {
            let nsoid = match schema {
                Some(s) => super::util::existing_namespace(interp, &s)?,
                None => class.relnamespace,
            };
            super::util::check_relation_name_free(interp, nsoid, &name)?;
            (nsoid, name)
        }
        None => (
            class.relnamespace,
            choose_relation_name(interp, class.relnamespace, &class.relname, &colname, "seq"),
        ),
    };
    let seq_oid = insert_sequence_relation(interp, nsoid, name)?;
    interp.sequence_params.insert(seq_oid, params);
    record_ownership(interp, seq_oid, relid, attnum, deptype);
    if deptype == DepType::Auto {
        // serial's `DEFAULT nextval('<seq>')` depends on the sequence.
        crate::ddl::defaults::record_default_sequence(interp, relid, attnum, seq_oid);
    }
    Ok(())
}

fn record_ownership(
    interp: &mut PgCatalog,
    seq_oid: PgClassOid,
    relid: PgClassOid,
    attnum: i16,
    deptype: DepType,
) {
    let seq_obj = PgGenericOid::from_nonzero(seq_oid.into_nonzero());
    interp.pg_depend.retain(|d| {
        !(d.classid == PG_CLASS_RELID
            && d.objid == seq_obj
            && matches!(d.deptype, DepType::Auto | DepType::Internal))
    });
    interp.add_dependency(PgDepend {
        classid: PG_CLASS_RELID,
        objid: seq_obj,
        objsubid: 0,
        refclassid: PG_CLASS_RELID,
        refobjid: PgGenericOid::from_nonzero(relid.into_nonzero()),
        refobjsubid: attnum,
        deptype,
    });
}

/// Sequences owned by `relid` (or by its column `attnum`): the ones a DROP
/// TABLE / DROP COLUMN takes along (`OWNED BY`, serial, identity).
pub(crate) fn owned_sequences(
    interp: &PgCatalog,
    relid: PgClassOid,
    attnum: Option<i16>,
) -> Vec<PgClassOid> {
    owned_sequences_by(interp, relid, attnum, &[DepType::Auto, DepType::Internal])
}

/// The identity sequence of column `attnum` (`deptype = Internal`).
pub(crate) fn identity_sequences(
    interp: &PgCatalog,
    relid: PgClassOid,
    attnum: i16,
) -> Vec<PgClassOid> {
    owned_sequences_by(interp, relid, Some(attnum), &[DepType::Internal])
}

fn owned_sequences_by(
    interp: &PgCatalog,
    relid: PgClassOid,
    attnum: Option<i16>,
    deptypes: &[DepType],
) -> Vec<PgClassOid> {
    let rel_obj = PgGenericOid::from_nonzero(relid.into_nonzero());
    let mut out: Vec<PgClassOid> = interp
        .iter_pg_depend()
        .filter(|d| {
            d.classid == PG_CLASS_RELID
                && d.refclassid == PG_CLASS_RELID
                && d.refobjid == rel_obj
                && attnum.is_none_or(|an| d.refobjsubid == an)
                && deptypes.contains(&d.deptype)
        })
        .filter_map(|d| PgClassOid::new(d.objid.get()))
        .filter(|oid| {
            interp
                .pg_class
                .get(oid)
                .is_some_and(|c| c.relkind == RelKind::Sequence)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `OWNED BY table.column` / `OWNED BY NONE` (CREATE / ALTER SEQUENCE).
fn apply_owned_by(
    interp: &mut PgCatalog,
    seq_oid: PgClassOid,
    opt: &typedpg_pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
        return Ok(());
    };
    if de.defname != "owned_by" {
        return Ok(());
    }
    let parts: Vec<String> = match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
        Some(node::Node::List(l)) => l
            .items
            .iter()
            .filter_map(node_string)
            .map(str::to_owned)
            .collect(),
        _ => return Ok(()),
    };
    let seq_obj = PgGenericOid::from_nonzero(seq_oid.into_nonzero());
    let owner = if parts.len() == 1 {
        if !parts[0].eq_ignore_ascii_case("none") {
            return Err(DdlError::Parse(
                "invalid OWNED BY option (Specify OWNED BY table.column or OWNED BY NONE.)".into(),
            ));
        }
        None
    } else {
        // `process_owned_by` (sequence.c): the last name is the column, the
        // rest the (optionally qualified) table.
        let Some((col, rel)) = parts.split_last() else {
            return Ok(());
        };
        let (schema, relname) = match rel {
            [relname] => (None, relname.as_str()),
            [schema, relname] => (Some(schema.as_str()), relname.as_str()),
            _ => {
                return Err(DdlError::Parse("invalid OWNED BY option".to_owned()));
            }
        };
        let Some(table) = interp.resolve_table(schema, relname).cloned() else {
            return Err(DdlError::TableNotFound(format!(
                "relation \"{relname}\" does not exist"
            )));
        };
        // A sequence belongs to a table, foreign table or view, in its own
        // schema.
        if !matches!(
            table.relkind,
            RelKind::Table | RelKind::ForeignTable | RelKind::View | RelKind::Partitioned
        ) {
            return Err(DdlError::UnsupportedDdl(format!(
                "sequence cannot be owned by relation \"{}\" (This operation is not supported \
                 for {}.)",
                table.relname,
                table.relkind.plural()
            )));
        }
        if interp.pg_class.get(&seq_oid).map(|c| c.relnamespace) != Some(table.relnamespace) {
            return Err(DdlError::UnsupportedDdl(
                "sequence must be in same schema as table it is linked to".into(),
            ));
        }
        let Some(attnum) = interp.attribute_by_name(table.oid, col).map(|a| a.attnum) else {
            return Err(DdlError::Parse(format!(
                "column \"{col}\" of relation \"{relname}\" does not exist"
            )));
        };
        Some((table.oid, attnum))
    };
    // An identity column's sequence keeps its owner.
    check_not_identity_sequence(interp, seq_oid)?;
    match owner {
        Some((table, attnum)) => record_ownership(interp, seq_oid, table, attnum, DepType::Auto),
        None => interp.pg_depend.retain(|d| {
            !(d.classid == PG_CLASS_RELID && d.objid == seq_obj && d.deptype == DepType::Auto)
        }),
    }
    Ok(())
}

/// The table a sequence belongs to (`sequenceIsOwned` with AUTO or
/// INTERNAL): `(table, deptype)`.
pub(crate) fn sequence_owner(
    interp: &PgCatalog,
    seq_oid: PgClassOid,
) -> Option<(PgClassOid, DepType)> {
    let seq_obj = PgGenericOid::from_nonzero(seq_oid.into_nonzero());
    interp
        .iter_pg_depend()
        .find(|d| {
            d.classid == PG_CLASS_RELID
                && d.objid == seq_obj
                && d.refclassid == PG_CLASS_RELID
                && matches!(d.deptype, DepType::Auto | DepType::Internal)
        })
        .and_then(|d| Some((PgClassOid::new(d.refobjid.get())?, d.deptype)))
}

/// `Sequence "s" is linked to table "t".`
fn linked_detail(interp: &PgCatalog, seq_oid: PgClassOid, table: PgClassOid) -> String {
    let name = |oid: PgClassOid| {
        interp
            .pg_class
            .get(&oid)
            .map(|c| c.relname.clone())
            .unwrap_or_default()
    };
    format!(
        "Sequence \"{}\" is linked to table \"{}\".",
        name(seq_oid),
        name(table)
    )
}

/// process_owned_by: `cannot change ownership of identity sequence`.
fn check_not_identity_sequence(interp: &PgCatalog, seq_oid: PgClassOid) -> Result<(), DdlError> {
    if let Some((table, DepType::Internal)) = sequence_owner(interp, seq_oid) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot change ownership of identity sequence ({})",
            linked_detail(interp, seq_oid, table)
        )));
    }
    Ok(())
}

/// `ALTER SEQUENCE [IF EXISTS] name [options]`.
///
/// Every option (`RESTART`, `INCREMENT BY`, `MINVALUE`, …) is a no-op for
/// static type analysis — this handler only validates that the sequence
/// exists. RENAME TO and SET SCHEMA arrive as `RenameStmt` /
/// `AlterObjectSchemaStmt` instead and are handled in `ddl/alter.rs`.
pub fn alter_sequence(interp: &mut PgCatalog, stmt: &AlterSeqStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.sequence.as_ref() else {
        return Ok(());
    };
    let (schema, name) = range_var_names(rv, interp);

    let resolved = interp
        .namespace_oid(&schema)
        .and_then(|nsoid| interp.class_by_qname.get(&(nsoid, name.clone())).copied());

    let Some(seq_oid) = resolved else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(format!(
            "relation \"{name}\" does not exist"
        )));
    };
    // validate_relation_kind (sequence.c).
    let relkind = interp.pg_class.get(&seq_oid).map(|c| c.relkind);
    if relkind != Some(RelKind::Sequence) {
        let kinds = relkind.map_or("this relation", RelKind::plural);
        return Err(DdlError::Parse(format!(
            "cannot open relation \"{name}\" (This operation is not supported for {kinds}.)"
        )));
    }
    let current = interp
        .sequence_params
        .get(&seq_oid)
        .copied()
        .unwrap_or(super::seqparams::SeqParams::defaults(oid::INT8));
    let params = super::seqparams::init_params(interp, &stmt.options, Some(current))?;
    interp.sequence_params.insert(seq_oid, params);
    for opt in &stmt.options {
        apply_owned_by(interp, seq_oid, opt)?;
    }
    Ok(())
}
