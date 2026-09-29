//! `CREATE INDEX` / `DROP INDEX` handlers.
//!
//! Indexes don't change query result types — they're invisible to the
//! analyzer's type/nullability inference. We still mirror them in the
//! catalog because three things downstream consult `pg_index` (and a
//! matching `pg_class` row of `relkind = 'i'`):
//!
//! 1. **Volatility check** — PG forbids VOLATILE callees in expression
//!    indexes, since the index would otherwise never agree with itself.
//! 2. **`ON CONFLICT` / FOREIGN KEY targets** — a unique index (with its
//!    key columns, expressions and partial-index predicate) is what arbiter
//!    inference and FK target checks match against. Like PG, a plain
//!    `CREATE UNIQUE INDEX` creates no `pg_constraint` row.
//! 3. **DROP INDEX / DROP TABLE cascade** — index rows live as their own
//!    pg_class entries; dropping the underlying table tears down the
//!    indexes via `pg_index.indrelid`.

use pg_query::protobuf::{IndexStmt, node};
use prost::Message;

use super::DdlError;
use super::volatile::{ExprLocation, check_no_volatile};
use crate::oid::PgClassOid;
use crate::pg_catalog::{AstBinding, PgCatalog, PgClass, PgIndex, RelKind, SerializedAst};

pub fn create_index(db: &mut PgCatalog, stmt: &IndexStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    // DefineIndex: the table must exist and be indexable.
    let (nsoid, indrelid) = super::util::lookup_relation(db, rv)?;
    let table_name = rv.relname.clone();
    if let Some(class) = db.pg_class.get(&indrelid) {
        let kinds = match class.relkind {
            RelKind::View => Some("views"),
            RelKind::Sequence => Some("sequences"),
            RelKind::CompositeType => Some("composite types"),
            RelKind::Index | RelKind::PartitionedIndex => Some("indexes"),
            RelKind::ForeignTable => Some("foreign tables"),
            _ => None,
        };
        if let Some(kinds) = kinds {
            return Err(DdlError::Parse(format!(
                "cannot create index on relation \"{}\" (This operation is not supported \
                 for {kinds}.)",
                class.relname
            )));
        }
    }

    // DefineIndex: the access method and what it supports.
    let am = if stmt.access_method.is_empty() {
        "btree"
    } else {
        stmt.access_method.as_str()
    };
    if !super::opclass::am_exists(db, am) {
        return Err(DdlError::TypeNotFound(format!(
            "access method \"{am}\" does not exist"
        )));
    }
    let caps = super::opclass::am_caps(am);
    if let Some(index_am) = super::reloptions::IndexAm::from_name(am) {
        super::reloptions::check_reloptions(
            &stmt.options,
            super::reloptions::RelOptKind::Index(index_am),
            false,
            false,
        )?;
    }
    if let Some(caps) = caps.as_ref() {
        if stmt.unique && !caps.can_unique {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support unique indexes"
            )));
        }
        if !stmt.index_including_params.is_empty() && !caps.can_include {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support included columns"
            )));
        }
        if stmt.index_params.len() > 1 && !caps.can_multicol {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support multicolumn indexes"
            )));
        }
    }

    if let Some(pred) = stmt.where_clause.as_deref() {
        super::expr_kind::check_expr_kind(db, pred, super::expr_kind::ExprKind::IndexPredicate)?;
        // transformWhereClause: a boolean over the table's row.
        match super::volatile::infer_over_relation(db, indrelid, pred, None) {
            Some(Err(e)) => return Err(DdlError::UnsupportedDdl(e.to_string())),
            Some(Ok(t))
                if t.type_oid != crate::pg_catalog::oid::BOOL
                    && t.type_oid != crate::pg_catalog::oid::UNKNOWN =>
            {
                return Err(DdlError::UnsupportedDdl(format!(
                    "argument of WHERE must be type boolean, not type {}",
                    super::util::format_type_for_message(db, t.type_oid)
                )));
            }
            _ => {}
        }
        // CheckPredicate: every function must be IMMUTABLE.
        check_no_volatile(pred, ExprLocation::IndexPredicate, db)?;
        super::volatile::check_mutability(db, indrelid, pred, ExprLocation::IndexPredicate)?;
    }

    // ── Mutability check on expression indexes (ComputeIndexAttrs) ──
    for param in &stmt.index_params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
        if let Some(expr) = elem.expr.as_deref() {
            super::expr_kind::check_expr_kind(
                db,
                expr,
                super::expr_kind::ExprKind::IndexExpression,
            )?;
            // transformIndexStmt: the expression is analyzed over the row.
            if let Some(Err(e)) = super::volatile::infer_over_relation(db, indrelid, expr, None) {
                return Err(DdlError::UnsupportedDdl(e.to_string()));
            }
            check_no_volatile(expr, ExprLocation::Index, db)?;
            super::volatile::check_mutability(db, indrelid, expr, ExprLocation::Index)?;
        }
    }

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
        let column_type = if !elem.name.is_empty() {
            // ComputeIndexAttrs: `column "x" does not exist`.
            let an = *attnum_by_name.get(&elem.name).ok_or_else(|| {
                DdlError::Parse(format!("column \"{}\" does not exist", elem.name))
            })?;
            indkey.push(an);
            db.attribute_by_name(indrelid, &elem.name)
                .map(|a| a.atttypid)
        } else if let Some(expr) = elem.expr.as_deref() {
            indkey.push(0);
            indexprs.push(serialize_node(expr));
            match super::volatile::infer_over_relation(db, indrelid, expr, None) {
                Some(Ok(t)) => Some(t.type_oid),
                _ => None,
            }
        } else {
            None
        };
        // ResolveOpClass, then the ordering options (amcanorder).
        if let Some(typ) = column_type {
            super::opclass::resolve_index_opclass(db, &elem.opclass, typ, am)?;
        }
        if let Some(caps) = caps.as_ref()
            && !caps.can_order
        {
            use pg_query::protobuf::{SortByDir, SortByNulls};
            if elem.ordering != SortByDir::SortbyDefault as i32 {
                return Err(DdlError::UnsupportedDdl(format!(
                    "access method \"{am}\" does not support ASC/DESC options"
                )));
            }
            if elem.nulls_ordering != SortByNulls::SortbyNullsDefault as i32 {
                return Err(DdlError::UnsupportedDdl(format!(
                    "access method \"{am}\" does not support NULLS FIRST/LAST options"
                )));
            }
        }
    }
    // ComputeIndexAttrs: the INCLUDE columns follow the key columns in
    // indkey; they must be plain columns.
    let indnkeyatts = indkey.len() as i16;
    for param in &stmt.index_including_params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
        if elem.name.is_empty() {
            return Err(DdlError::UnsupportedDdl(
                "expressions are not supported in included columns".into(),
            ));
        }
        let an = *attnum_by_name
            .get(&elem.name)
            .ok_or_else(|| DdlError::Parse(format!("column \"{}\" does not exist", elem.name)))?;
        indkey.push(an);
    }
    let indpred = stmt.where_clause.as_deref().map(serialize_node);
    if stmt.unique || stmt.primary {
        let label = if stmt.primary {
            "PRIMARY KEY"
        } else {
            "UNIQUE"
        };
        super::tables::check_unique_covers_partition_key(
            db,
            indrelid,
            &indkey[..indnkeyatts as usize],
            label,
        )?;
    }

    // ── Pick a name for the index ──
    //
    // ChooseIndexName (indexcmds.c): an unnamed index that isn't a
    // constraint's is `<table>_<columns>_idx` — unique or not — numbered
    // when taken; expression columns are named like FigureIndexColname.
    let conname = if stmt.idxname.is_empty() {
        // ChooseIndexColumnNames over all the index's columns, INCLUDE ones
        // too.
        let colnames: Vec<String> = stmt
            .index_params
            .iter()
            .chain(&stmt.index_including_params)
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
    db.index_access_methods.insert(indexrelid, am.to_owned());
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
        indnkeyatts,
        indisunique: stmt.unique,
        indisprimary: stmt.primary,
        indkey,
        indexprs,
        indpred,
    });
    // DefineIndex on a partitioned table recurses (not under ONLY).
    if rv.inh {
        super::tables::partidx::propagate_new_index(db, indrelid, indexrelid)?;
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
