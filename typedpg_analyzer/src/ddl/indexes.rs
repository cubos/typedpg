//! `CREATE INDEX` / `DROP INDEX` handlers.
//!
//! Indexes don't change query result types — they're invisible to the
//! analyzer's type/nullability inference. We still mirror them in the
//! catalog because three things downstream consult `pg_index` (and a
//! matching `pg_class` row of `relkind = 'i'`):
//!
//! 1. **Volatility check** — PG forbids VOLATILE callees in expression
//!    indexes, since the index would otherwise never agree with itself.
//! 2. **`pg_constraint` for `ON CONFLICT`** — PG treats a non-partial
//!    `UNIQUE INDEX` on column names exactly like a `UNIQUE` constraint
//!    for the purposes of `ON CONFLICT (cols)` matching, so we mirror
//!    that here. Partial unique indexes (with a `WHERE`) are *not*
//!    valid `ON CONFLICT` targets and so we deliberately skip them.
//! 3. **DROP INDEX / DROP TABLE cascade** — index rows live as their own
//!    pg_class entries; dropping the underlying table tears down the
//!    indexes via `pg_index.indrelid`.

use pg_query::protobuf::{IndexStmt, node};
use prost::Message;

use super::DdlError;
use super::util::range_var_names;
use super::volatile::{ExprLocation, check_no_volatile};
use crate::oid::{PgClassOid, PgConstraintOid};
use crate::pg_catalog::{
    AstBinding, ConType, PgCatalog, PgClass, PgConstraint, PgIndex, RelKind, SerializedAst,
};

pub fn create_index(db: &mut PgCatalog, stmt: &IndexStmt) -> Result<(), DdlError> {
    // ── Volatility check on expression indexes ──
    for param in &stmt.index_params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
        if let Some(expr) = elem.expr.as_deref() {
            check_no_volatile(expr, ExprLocation::Index, db)?;
        }
    }

    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (schema, table_name) = range_var_names(rv, db);
    let Some(nsoid) = db.namespace_oid(&schema) else {
        return Ok(());
    };
    let Some(indrelid) = db.class_by_qname.get(&(nsoid, table_name.clone())).copied() else {
        return Ok(());
    };

    // ── Resolve indkey + indexprs (one ast per expression slot) ──
    //
    // Mirror PG: each index element is either a column reference (yields
    // an attnum, indexprs slot is empty) or an arbitrary expression
    // (indkey gets `0`, indexprs gets the next AST). The split keeps
    // expressions iterable per slot.
    let attnum_by_name: std::collections::HashMap<String, i16> = db
        .attributes_of(indrelid)
        .iter()
        .map(|a| (a.attname.clone(), a.attnum))
        .collect();
    let mut indkey: Vec<i16> = Vec::with_capacity(stmt.index_params.len());
    let mut indexprs: Vec<SerializedAst> = Vec::new();
    for param in &stmt.index_params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
        if !elem.name.is_empty() {
            let an = *attnum_by_name.get(&elem.name).ok_or_else(|| {
                DdlError::Parse(format!(
                    "column \"{}\" named in index does not exist",
                    elem.name
                ))
            })?;
            indkey.push(an);
        } else if let Some(expr) = elem.expr.as_deref() {
            indkey.push(0);
            indexprs.push(serialize_node(expr));
        }
    }
    let indpred = stmt.where_clause.as_deref().map(serialize_node);

    // ── Pick a name for the index ──
    //
    // ChooseIndexName (indexcmds.c): an unnamed index that isn't a
    // constraint's is `<table>_<columns>_idx` — unique or not — numbered
    // when taken; expression columns are named like FigureIndexColname.
    let conname = if stmt.idxname.is_empty() {
        let colnames: Vec<String> = stmt
            .index_params
            .iter()
            .filter_map(|param| match param.node.as_ref()? {
                node::Node::IndexElem(elem) if !elem.name.is_empty() => Some(elem.name.clone()),
                node::Node::IndexElem(elem) => Some(figure_index_colname(elem.expr.as_deref())),
                _ => None,
            })
            .collect();
        super::util::choose_relation_name(
            db,
            nsoid,
            &table_name,
            &super::util::index_name_addition(&colnames),
            if stmt.primary { "pkey" } else { "idx" },
        )
    } else {
        stmt.idxname.clone()
    };

    // ── Reject duplicate index names in the same schema ──
    //
    // PG: `relation "<idxname>" already exists`. Indexes share the
    // namespace with tables/views/etc. via `pg_class`.
    if db.class_by_qname.contains_key(&(nsoid, conname.clone())) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{conname}\" already exists"
        )));
    }

    // ── Allocate the index's pg_class oid + insert pg_index ──
    let indexrelid = PgClassOid::from_nonzero(db.alloc_oid()?);
    db.insert_pg_class(PgClass {
        oid: indexrelid,
        relname: conname.clone(),
        relnamespace: nsoid,
        relkind: RelKind::Index,
        reltype: None,
    });

    let indnatts = indkey.len() as i16;
    db.insert_pg_index(PgIndex {
        indexrelid,
        indrelid,
        indnatts,
        // PG distinguishes key cols from `INCLUDE (cols)`. We don't model
        // INCLUDE today — every column counts as a key column.
        indnkeyatts: indnatts,
        indisunique: stmt.unique,
        indisprimary: stmt.primary,
        indkey,
        indexprs,
        indpred,
    });

    // ── pg_constraint emission for non-partial UNIQUE INDEX ──
    //
    // PG only treats a unique index as a valid `ON CONFLICT` target when
    // it has no predicate (or a predicate that covers every row). A
    // partial unique index `WHERE deleted_at IS NULL` doesn't qualify
    // for the generic insert. By skipping rows with a `where_clause` we
    // make `ON CONFLICT (slug)` correctly fail to find a match for a
    // partial-unique-only schema.
    if stmt.unique && stmt.where_clause.is_none() {
        let idx = db.pg_index.get(&indexrelid).ok_or_else(|| {
            DdlError::Internal(format!(
                "create_index expects pg_index row for indexrelid={indexrelid} just inserted to be present"
            ))
        })?;
        // Expression-based UNIQUE INDEXes never line up with a
        // column-list ON CONFLICT, so skip those.
        if idx.indexprs.is_empty() && !idx.indkey.is_empty() {
            let conkey: Vec<i16> = idx.indkey.clone();
            let oid = PgConstraintOid::from_nonzero(db.alloc_oid()?);
            db.insert_pg_constraint(PgConstraint {
                oid,
                conname: conname.clone(),
                conrelid: indrelid,
                contype: ConType::Unique,
                conkey,
                confrelid: None,
                confkey: Vec::new(),
                conislocal: true,
                coninhcount: 0,
            });
        }
    }

    Ok(())
}

/// `FigureIndexColname`: a function call is named after the function, a
/// column reference after the column (through casts), anything else `expr`.
fn figure_index_colname(expr: Option<&pg_query::protobuf::Node>) -> String {
    match expr.and_then(|e| e.node.as_ref()) {
        Some(node::Node::FuncCall(fc)) => fc
            .funcname
            .last()
            .and_then(super::util::node_string)
            .unwrap_or("expr")
            .to_owned(),
        Some(node::Node::ColumnRef(cr)) => cr
            .fields
            .last()
            .and_then(super::util::node_string)
            .unwrap_or("expr")
            .to_owned(),
        Some(node::Node::TypeCast(tc)) => figure_index_colname(tc.arg.as_deref()),
        _ => "expr".to_owned(),
    }
}

/// Encode a `pg_query::Node` as a `SerializedAst` (protobuf bytes + an
/// empty bindings stream — index expressions don't yet flow through the
/// view-binding walker; that's a separate piece of work).
fn serialize_node(node: &pg_query::protobuf::Node) -> SerializedAst {
    let mut buf = Vec::with_capacity(64);
    node.encode(&mut buf).ok();
    SerializedAst {
        ast: buf,
        bindings: Vec::<AstBinding>::new(),
    }
}
