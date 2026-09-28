//! CREATE / ALTER SEQUENCE handlers.
//!
//! A sequence is registered as a `pg_class` row with `relkind = Sequence`.
//! The analyzer doesn't model the sequence's numeric state (START, INCREMENT,
//! …) — those options don't affect query typing, so they're accepted as
//! no-ops. DROP SEQUENCE is handled by `ddl/drop.rs` (sequences share the
//! relation-drop path with tables and views). RENAME / SET SCHEMA flow
//! through `ddl/alter.rs`.

use pg_query::protobuf::{AlterSeqStmt, CreateSeqStmt, node};

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

    let seq_oid = insert_sequence_relation(interp, nsoid, name)?;
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
    let name = choose_relation_name(interp, class.relnamespace, &class.relname, &colname, "seq");
    let seq_oid = insert_sequence_relation(interp, class.relnamespace, name)?;
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
    opt: &pg_query::protobuf::Node,
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
    if parts.len() == 1 && parts[0].eq_ignore_ascii_case("none") {
        interp.pg_depend.retain(|d| {
            !(d.classid == PG_CLASS_RELID && d.objid == seq_obj && d.deptype == DepType::Auto)
        });
        return Ok(());
    }
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
    let Some(table) = interp.resolve_table(schema, relname).map(|c| c.oid) else {
        return Err(DdlError::TableNotFound(format!(
            "relation \"{relname}\" does not exist"
        )));
    };
    let Some(attnum) = interp.attribute_by_name(table, col).map(|a| a.attnum) else {
        return Err(DdlError::Parse(format!(
            "column \"{col}\" of relation \"{relname}\" does not exist"
        )));
    };
    record_ownership(interp, seq_oid, table, attnum, DepType::Auto);
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
    for opt in &stmt.options {
        apply_owned_by(interp, seq_oid, opt)?;
    }
    Ok(())
}
