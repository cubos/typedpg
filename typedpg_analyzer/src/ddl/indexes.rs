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

use typedpg_pg_query::ast::{self, IndexStmt};

use super::DdlError;
use super::volatile::{ExprLocation, check_no_volatile};
use crate::oid::{PgClassOid, PgCollationOid, PgOpclassOid};
use crate::pg_catalog::{AstBinding, PgCatalog, PgClass, PgIndex, RelKind, SerializedAst};

pub fn create_index(db: &mut PgCatalog, stmt: &IndexStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    // DefineIndex: the table must exist and be indexable.
    let (nsoid, indrelid) = super::util::lookup_relation(db, rv)?;
    // RangeVarCallbackOwnsRelation, as the table is looked up: no system
    // catalog. Then table_open: an index or a composite type can't be
    // opened as a table.
    super::util::check_not_system_class(db, indrelid)?;
    // ProcessUtilitySlow, before DefineIndex: a unique index on a
    // partitioned table (without ONLY) can't reach a foreign table
    // anywhere below it.
    if (stmt.unique || stmt.primary)
        && rv.inh
        && db.pg_class.get(&indrelid).map(|c| c.relkind) == Some(RelKind::Partitioned)
        && super::tables::inherit::all_inheritors(db, indrelid)
            .iter()
            .any(|&p| db.pg_class.get(&p).map(|c| c.relkind) == Some(RelKind::ForeignTable))
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot create unique index on partitioned table \"{0}\" (Table \"{0}\" contains \
             partitions that are foreign tables.)",
            rv.relname
        )));
    }
    super::util::table_open_check(db, indrelid, &rv.relname)?;
    let table_name = rv.relname.clone();
    if let Some(class) = db.pg_class.get(&indrelid) {
        let kinds = match class.relkind {
            RelKind::View | RelKind::Sequence | RelKind::ForeignTable => {
                Some(class.relkind.plural())
            }
            _ => None,
        };
        if let Some(kinds) = kinds {
            return Err(DdlError::Parse(format!(
                "cannot create index on relation \"{}\" (This operation is not supported \
                 for {kinds}.)",
                class.relname
            )));
        }
        if stmt.concurrent && class.relkind == RelKind::Partitioned {
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot create index on partitioned table \"{}\" concurrently",
                class.relname
            )));
        }
    }
    check_index_max_keys(stmt.index_params.len() + stmt.index_including_params.len())?;

    // DefineIndex: not in pg_global.
    super::tables::check_tablespace_placement(&stmt.table_space)?;
    // DefineIndex: the access method, what it supports, and its options.
    let am = if stmt.access_method.is_empty() {
        "btree"
    } else {
        stmt.access_method.as_str()
    };
    let caps = check_index_am(
        db,
        am,
        stmt.unique,
        !stmt.index_including_params.is_empty(),
        stmt.index_params.len(),
        false,
    )?;
    if let Some(index_am) = super::reloptions::IndexAm::from_name(am) {
        super::reloptions::check_reloptions(
            &stmt.options,
            super::reloptions::RelOptKind::Index(index_am),
            false,
            false,
        )?;
    }

    if stmt.where_clause.is_some() {
        check_index_predicate(db, indrelid, stmt.where_clause.as_ref())?;
    }

    // ── Mutability check on expression indexes (ComputeIndexAttrs) ──
    for param in &stmt.index_params {
        let Some(ast::Node::IndexElem(elem)) = param.as_ref() else {
            continue;
        };
        if elem.expr.is_some() {
            check_index_expression(db, indrelid, elem.expr.as_ref())?;
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
    let mut key_columns: Vec<IndexKeyColumn> = Vec::with_capacity(stmt.index_params.len());
    let mut indclass = Vec::with_capacity(stmt.index_params.len());
    let mut indcollation = Vec::with_capacity(stmt.index_params.len());
    let mut indexprs: Vec<SerializedAst> = Vec::new();
    // (key column, type) of each expression column.
    let mut expr_types: Vec<(usize, crate::oid::PgTypeOid)> = Vec::new();
    for param in &stmt.index_params {
        let Some(ast::Node::IndexElem(elem)) = param.as_ref() else {
            continue;
        };
        let expr_column = expression_column(db, indrelid, elem.expr.as_ref());
        let column_type = if !elem.name.is_empty() {
            // ComputeIndexAttrs: `column "x" does not exist`; a system
            // column resolves (and DefineIndex refuses it).
            let an = attnum_by_name
                .get(&elem.name)
                .copied()
                .or_else(|| system_attnum(&elem.name))
                .ok_or_else(|| {
                    DdlError::Parse(format!("column \"{}\" does not exist", elem.name))
                })?;
            indkey.push(an);
            let attr = db.attribute_by_name(indrelid, &elem.name);
            attr.map(|a| (a.atttypid, a.attcollation))
        } else if let Some(an) = expr_column {
            indkey.push(an);
            // A COLLATE clause stripped off the column gives the key its
            // collation.
            let collated = matches!(elem.expr, Some(ast::Node::CollateClause(_)));
            db.attributes_of(indrelid)
                .iter()
                .find(|a| a.attnum == an)
                .map(|a| {
                    let collation = if collated {
                        match super::volatile::infer_over_relation(
                            db,
                            indrelid,
                            elem.expr.as_ref(),
                            None,
                        ) {
                            Some(Ok(t)) => expression_collation(db, &t),
                            _ => a.attcollation,
                        }
                    } else {
                        a.attcollation
                    };
                    (a.atttypid, collation)
                })
        } else if elem.expr.is_some() {
            let expr = &elem.expr;
            indkey.push(0);
            indexprs.push(serialize_node(expr.as_ref()));
            match super::volatile::infer_over_relation(db, indrelid, expr.as_ref(), None) {
                Some(Ok(t)) if known_key_type(expr.as_ref(), &t) => {
                    expr_types.push((key_columns.len(), t.type_oid));
                    Some((t.type_oid, expression_collation(db, &t)))
                }
                _ => None,
            }
        } else {
            None
        };
        // ComputeIndexAttrs: the collation, ResolveOpClass, then the
        // ordering options (amcanorder).
        let key = compute_key_column(
            db,
            am,
            caps.as_ref(),
            Some(elem),
            column_type.map(|(t, _)| t),
            column_type.and_then(|(_, c)| c),
            None,
        )?;
        if elem.name.is_empty() && expr_column.is_none() {
            check_expression_key_collation(db, column_type.map(|(t, _)| t), key.collation)?;
        }
        key_columns.push(key.column);
        indclass.push(key.opclass);
        indcollation.push(key.collation);
        super::extensions::check_opclass_options(db, &elem.opclass, &elem.opclassopts, am)?;
    }
    // ComputeIndexAttrs: the INCLUDE columns follow the key columns in
    // indkey; they must be plain columns.
    let indnkeyatts = indkey.len() as i16;
    for param in &stmt.index_including_params {
        let Some(ast::Node::IndexElem(elem)) = param.as_ref() else {
            continue;
        };
        if elem.name.is_empty() {
            return Err(DdlError::UnsupportedDdl(
                "expressions are not supported in included columns".into(),
            ));
        }
        let an = attnum_by_name
            .get(&elem.name)
            .copied()
            .or_else(|| system_attnum(&elem.name))
            .ok_or_else(|| DdlError::Parse(format!("column \"{}\" does not exist", elem.name)))?;
        indkey.push(an);
    }
    // DefineIndex's make_ands_implicit: a predicate that is the constant
    // true is none.
    let indpred = stmt
        .where_clause
        .as_ref()
        .filter(|w| !is_true_constant(w))
        .map(|w| serialize_node(Some(w)));
    let usage = if stmt.primary {
        IndexUse::Primary
    } else if stmt.isconstraint {
        IndexUse::Constraint
    } else {
        IndexUse::Index
    };
    let key_exprs: Vec<Option<&typedpg_pg_query::ast::Node>> = stmt
        .index_params
        .iter()
        .filter_map(|p| match p.as_ref()? {
            ast::Node::IndexElem(elem) => elem.expr.as_ref().map(Some),
            _ => None,
        })
        .chain(stmt.where_clause.as_ref().map(Some))
        .collect();
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
            &indcollation,
            &indclass,
            label,
        )?;
    }
    check_index_columns(db, indrelid, &indkey, &key_exprs, usage)?;

    // ChooseIndexColumnNames over all the index's columns, INCLUDE ones
    // too; expression columns are named like FigureIndexColname.
    let colnames: Vec<String> = stmt
        .index_params
        .iter()
        .chain(&stmt.index_including_params)
        .filter_map(|param| match param.as_ref()? {
            ast::Node::IndexElem(elem) if !elem.name.is_empty() => Some(elem.name.clone()),
            ast::Node::IndexElem(elem) => Some(figure_index_colname(elem.expr.as_ref())),
            _ => None,
        })
        .collect();
    check_index_expression_types(db, &colnames, &expr_types)?;

    // ── Pick a name for the index ──
    //
    // ChooseIndexName (indexcmds.c): an unnamed index that isn't a
    // constraint's is `<table>_<columns>_idx` — unique or not — numbered
    // when taken.
    let conname = if stmt.idxname.is_empty() {
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
    let relkind = RelKind::index_on(db.pg_class.get(&indrelid).map(|c| c.relkind));
    db.insert_pg_class(PgClass {
        oid: indexrelid,
        relname: conname.clone(),
        relnamespace: nsoid,
        relkind,
        reltype: None,
    });
    super::acl::index_created(db, indexrelid, indrelid);

    db.index_keys.insert(
        indexrelid,
        IndexKeys {
            columns: key_columns,
            nulls_not_distinct: stmt.nulls_not_distinct,
        },
    );
    let indnatts = indkey.len() as i16;
    db.insert_pg_index(PgIndex {
        indexrelid,
        indrelid,
        indnatts,
        indnkeyatts,
        indisunique: stmt.unique,
        indisprimary: stmt.primary,
        indisexclusion: false,
        indkey,
        indexprs,
        indpred,
        indcollation,
        indclass,
    });
    // DefineIndex on a partitioned table recurses — not under ONLY, where
    // the index stays invalid while the table has partitions without one
    // attached (ALTER INDEX ... ATTACH PARTITION validates it).
    if rv.inh {
        super::tables::partidx::propagate_new_index(db, indrelid, indexrelid)?;
    } else if db.pg_class.get(&indrelid).map(|c| c.relkind) == Some(RelKind::Partitioned)
        && !super::tables::inherit::children_of(db, indrelid).is_empty()
    {
        db.invalid_indexes.insert(indexrelid);
    }

    Ok(())
}

/// transformIndexStmt / CheckPredicate: an index's WHERE is a boolean over
/// the table's row, of IMMUTABLE functions, in EXPR_KIND_INDEX_PREDICATE.
fn check_index_predicate(
    db: &PgCatalog,
    relid: PgClassOid,
    pred: Option<&typedpg_pg_query::ast::Node>,
) -> Result<(), DdlError> {
    super::expr_kind::check_expr_kind(db, pred, super::expr_kind::ExprKind::IndexPredicate)?;
    // transformWhereClause: a boolean over the table's row.
    match super::volatile::infer_over_relation(db, relid, pred, None) {
        Some(Err(e)) => return Err(DdlError::UnsupportedDdl(e.to_string())),
        Some(Ok(t)) if t.unknown_value => {
            t.check_unknown_value_coercion(
                crate::pg_catalog::oid::BOOL,
                crate::coerce::CoercionContext::Assignment,
                db,
                pred,
            )
            .map_err(|e| DdlError::UnsupportedDdl(e.to_string()))?;
        }
        Some(Ok(t)) if !crate::coerce::coerces_to_boolean(t.type_oid, db) => {
            return Err(DdlError::UnsupportedDdl(format!(
                "argument of WHERE must be type boolean, not type {}",
                super::util::format_type_for_message(db, t.type_oid)
            )));
        }
        _ => {}
    }
    // CheckPredicate: every function must be IMMUTABLE.
    check_no_volatile(pred, ExprLocation::IndexPredicate, db)?;
    super::volatile::check_mutability(db, relid, pred, ExprLocation::IndexPredicate)
}

/// transformIndexStmt / ComputeIndexAttrs: an index expression is analyzed
/// over the table's row in EXPR_KIND_INDEX_EXPRESSION, and may call only
/// IMMUTABLE functions.
fn check_index_expression(
    db: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&typedpg_pg_query::ast::Node>,
) -> Result<(), DdlError> {
    super::expr_kind::check_expr_kind(db, expr, super::expr_kind::ExprKind::IndexExpression)?;
    // transformIndexStmt: the expression is analyzed over the row.
    if let Some(Err(e)) = super::volatile::infer_over_relation(db, relid, expr, None) {
        return Err(DdlError::UnsupportedDdl(e.to_string()));
    }
    check_no_volatile(expr, ExprLocation::Index, db)?;
    super::volatile::check_mutability(db, relid, expr, ExprLocation::Index)
}

/// Whether an expression of inferred type `t` is certainly of type
/// `unknown` for PG too: an untyped string literal or NULL, or a value the
/// analyzer knows to be `unknown` (a cast to it, `unknownin(…)`, a field
/// of a record over a literal — see [`crate::expr::ExprType::unknown_value`]).
/// The analyzer also infers `unknown` where it can't tell a type.
pub(crate) fn certainly_unknown(
    expr: Option<&typedpg_pg_query::ast::Node>,
    t: &crate::expr::ExprType,
) -> bool {
    use typedpg_pg_query::ast;
    t.type_oid == crate::pg_catalog::oid::UNKNOWN
        && (t.unknown_value
            || matches!(
                expr,
                Some(ast::Node::AConst(c))
                    if c.isnull || matches!(c.val, Some(ast::AConstValue::String(_)))
            ))
}

/// Whether an index expression's inferred type is its type for PG: any
/// but `unknown`, which only [`certainly_unknown`] keys are — PG resolves
/// their operator class (ComputeIndexAttrs) and fails, `unknown` having
/// none. Other keys are left unchecked.
fn known_key_type(expr: Option<&typedpg_pg_query::ast::Node>, t: &crate::expr::ExprType) -> bool {
    t.type_oid != crate::pg_catalog::oid::UNKNOWN || certainly_unknown(expr, t)
}

/// index_create's ConstructTupleDescriptor: an expression column's type must
/// be one a table column could have (CheckAttributeType) — not `record`,
/// for one, though it has operator classes. `colnames` are the index's
/// columns' names before ChooseIndexColumnNames' deduplication, `expr_types`
/// the (column, type) of each expression column.
fn check_index_expression_types(
    db: &PgCatalog,
    colnames: &[String],
    expr_types: &[(usize, crate::oid::PgTypeOid)],
) -> Result<(), DdlError> {
    let names = super::util::choose_index_column_names(colnames);
    for &(column, typ) in expr_types {
        if let Some(name) = names.get(column) {
            super::tables::check_attribute_type(db, name, typ, None, false)?;
        }
    }
    Ok(())
}

/// The `attname`s of index `index`'s columns, key and INCLUDE ones: the
/// names it was given ([`PgCatalog::index_attnames`]) or, for most
/// indexes, ChooseIndexColumnNames (indexcmds.c) over the table's columns —
/// each named after its table column, or "expr", with a numeric suffix
/// appended until it is unique among the earlier ones.
pub(crate) fn index_attnames(db: &PgCatalog, index: PgClassOid) -> Vec<String> {
    if let Some(names) = db.index_attnames.get(&index) {
        return names.clone();
    }
    let Some(index) = db.pg_index.get(&index) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    let mut exprs = index.indexprs.iter();
    for &key in &index.indkey {
        // A column is named after the table's; an expression as
        // FigureIndexColname names it (a function's name, a cast's
        // argument, ...; `expr` for an operator and the like).
        let origname = if key == 0 {
            figure_index_colname(exprs.next().and_then(|e| e.ast.as_ref()))
        } else {
            db.attributes_of(index.indrelid)
                .iter()
                .find(|a| a.attnum == key)
                .map_or_else(|| "expr".to_owned(), |a| a.attname.clone())
        };
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

/// The column names of index `index` as its relcache entry has them, which
/// generateClonedIndexStmt copies: those it was created with, or had when
/// given a new relfilenode, for an index created or given one in the
/// current transaction ([`PgCatalog::relcache_index_attnames`]); the
/// catalog's otherwise.
///
/// Not modelled: an open cursor whose plan scans an index holds that
/// index's relcache entry open, so a rename doesn't reach a clone made
/// while it is open either; whether a cursor's plan uses an index scan is
/// the planner's choice, which the analyzer can't know. Nor are tablespace
/// moves (ALTER INDEX ... SET TABLESPACE), which the analyzer doesn't track.
pub(crate) fn relcache_index_attnames(db: &PgCatalog, index: PgClassOid) -> Vec<String> {
    db.relcache_index_attnames
        .get(&index)
        .cloned()
        .unwrap_or_else(|| index_attnames(db, index))
}

/// Before an index column is renamed: an index whose relcache entry stays
/// for the rest of the transaction (created or given a new relfilenode in
/// it) keeps the names it has now there.
pub(crate) fn pin_relcache_index_attnames(db: &mut PgCatalog, index: PgClassOid) {
    let pinned =
        index.get() >= db.transaction_oid_floor || db.indexes_with_new_relfilenode.contains(&index);
    if pinned && !db.relcache_index_attnames.contains_key(&index) {
        let names = index_attnames(db, index);
        db.relcache_index_attnames.insert(index, names);
    }
}

/// Index `index` gets a new relfilenode (TRUNCATE, REINDEX, CLUSTER): its
/// relcache entry stays as it is for the rest of the transaction. A
/// partitioned index has no storage, and no relfilenode to change.
pub(crate) fn new_relfilenode(db: &mut PgCatalog, index: PgClassOid) {
    if db.pg_class.get(&index).map(|c| c.relkind) == Some(RelKind::Index) {
        db.indexes_with_new_relfilenode.insert(index);
    }
}

/// Table `relid`'s storage is rewritten — TRUNCATE, CLUSTER, REINDEX
/// TABLE, REFRESH MATERIALIZED VIEW, and an ALTER TABLE that rewrites the
/// rows (ATRewriteTables: a column type change that isn't a relabeling, a
/// volatile or constrained-domain default, a stored generated column, SET
/// LOGGED / UNLOGGED): finish_heap_swap rebuilds every index of it, each
/// in a new relfilenode.
pub(crate) fn table_rewritten(db: &mut PgCatalog, relid: PgClassOid) {
    db.tables_with_new_relfilenode.insert(relid);
    let indexes: Vec<PgClassOid> = db
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid)
        .map(|i| i.indexrelid)
        .collect();
    for index in indexes {
        new_relfilenode(db, index);
    }
}

/// Before a column of table `relid` is renamed, its indexes keep the names
/// their columns have now: PG never renames an index column after its
/// table's.
pub(crate) fn freeze_index_attnames(db: &mut PgCatalog, relid: PgClassOid) {
    let indexes: Vec<PgClassOid> = db
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid)
        .map(|i| i.indexrelid)
        .collect();
    for index in indexes {
        if !db.index_attnames.contains_key(&index) {
            let names = index_attnames(db, index);
            db.index_attnames.insert(index, names);
        }
    }
}

/// INDEX_MAX_KEYS (pg_config_manual.h): the most columns an index — key
/// and INCLUDE ones — a partition key or a foreign key may have.
pub(crate) const INDEX_MAX_KEYS: usize = 32;

/// What the analyzer keeps of one key column beyond `pg_index`'s
/// `indkey` / `indclass` / `indcollation`.
#[derive(Clone, Debug, Default)]
pub(crate) struct IndexKeyColumn {
    /// The key's type, which `indclass` was resolved for (`opckeytype`
    /// aside).
    pub(crate) typ: Option<crate::oid::PgTypeOid>,
    /// `indoption == 0`: ASC NULLS LAST.
    pub(crate) default_order: bool,
    /// The exclusion constraint's operator for this column (`conexclop`),
    /// as written.
    pub(crate) exclusion_op: Option<Vec<String>>,
}

/// The key columns of an index as [`IndexKeyColumn`]s, and
/// `indnullsnotdistinct`.
#[derive(Clone, Debug, Default)]
pub(crate) struct IndexKeys {
    pub(crate) columns: Vec<IndexKeyColumn>,
    pub(crate) nulls_not_distinct: bool,
}

/// One key column as ComputeIndexAttrs resolves it.
struct ComputedKey {
    column: IndexKeyColumn,
    /// `indclass`.
    opclass: Option<PgOpclassOid>,
    /// `indcollation`.
    collation: Option<PgCollationOid>,
    /// The exclusion operator (`indexInfo->ii_ExclusionOps`).
    exclusion_opr: Option<crate::oid::PgOperatorOid>,
}

/// An index-backed constraint's index as DefineIndex resolves it.
pub(crate) struct ConstraintIndex {
    pub(crate) am: String,
    /// An exclusion constraint's expression keys (`indexprs`), in key order.
    pub(crate) indexprs: Vec<SerializedAst>,
    /// An exclusion constraint's WHERE (`indpred`).
    pub(crate) indpred: Option<SerializedAst>,
    pub(crate) keys: IndexKeys,
    pub(crate) indclass: Vec<Option<PgOpclassOid>>,
    pub(crate) indcollation: Vec<Option<PgCollationOid>>,
}

/// DefineIndex: an index has at most INDEX_MAX_KEYS columns, key and
/// INCLUDE ones together (54011).
fn check_index_max_keys(natts: usize) -> Result<(), DdlError> {
    if natts > INDEX_MAX_KEYS {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot use more than {INDEX_MAX_KEYS} columns in an index"
        )));
    }
    Ok(())
}

/// DefineIndex: the access method must exist and be an index one (a table
/// AM's handler returns no IndexAmRoutine), and support what the index
/// asks of it. Returns the built-in AM's capabilities.
fn check_index_am(
    db: &PgCatalog,
    am: &str,
    unique: bool,
    include: bool,
    nkeys: usize,
    exclusion: bool,
) -> Result<Option<super::opclass::AmCaps>, DdlError> {
    let Some(row) = db.pg_am.iter().find(|a| a.amname == am) else {
        return Err(DdlError::TypeNotFound(format!(
            "access method \"{am}\" does not exist"
        )));
    };
    if row.amtype == "t" {
        // GetIndexAmRoutine (amapi.c), an elog.
        return Err(DdlError::UnsupportedDdl(format!(
            "index access method handler function {} did not return an IndexAmRoutine struct",
            row.amhandler.map_or(0, |h| h.get())
        )));
    }
    let caps = super::opclass::am_caps(am);
    if let Some(caps) = caps.as_ref() {
        if unique && !caps.can_unique {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support unique indexes"
            )));
        }
        if include && !caps.can_include {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support included columns"
            )));
        }
        if nkeys > 1 && !caps.can_multicol {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support multicolumn indexes"
            )));
        }
        if exclusion && !caps.has_gettuple {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support exclusion constraints"
            )));
        }
    }
    Ok(caps)
}

/// An index expression's collation (`exprCollation`): its own, else its
/// type's default; `None` when the expression has none (or its type is
/// not collatable).
fn expression_collation(
    db: &PgCatalog,
    t: &crate::expr::ExprType,
) -> Option<crate::oid::PgCollationOid> {
    if t.no_collation.is_some() {
        return None;
    }
    t.collation
        .or_else(|| db.get_type(t.type_oid).and_then(|ty| ty.typcollation))
}

/// ComputeIndexAttrs: a collatable expression key needs a collation — its
/// own, or the COLLATE clause's.
fn check_expression_key_collation(
    db: &PgCatalog,
    typ: Option<crate::oid::PgTypeOid>,
    collation: Option<crate::oid::PgCollationOid>,
) -> Result<(), DdlError> {
    let collatable = typ
        .and_then(|t| db.get_type(t))
        .is_some_and(|t| t.typcollation.is_some());
    if collatable && collation.is_none() {
        return Err(DdlError::Parse(
            "could not determine which collation to use for index expression".into(),
        ));
    }
    Ok(())
}

/// ComputeIndexAttrs (indexcmds.c) for one key column of type `typ`
/// (`None` when the analyzer can't tell it; `unknown` only for a
/// [`certainly_unknown`] expression): its collation — the COLLATE clause,
/// which must name an existing collation of a collatable type, else
/// `column_collation` —, its operator class (ResolveOpClass), its exclusion
/// operator and its ordering options.
#[allow(clippy::too_many_arguments)]
fn compute_key_column(
    db: &PgCatalog,
    am: &str,
    caps: Option<&super::opclass::AmCaps>,
    elem: Option<&typedpg_pg_query::ast::IndexElem>,
    typ: Option<crate::oid::PgTypeOid>,
    column_collation: Option<crate::oid::PgCollationOid>,
    exclusion_op: Option<&[Option<typedpg_pg_query::ast::Node>]>,
) -> Result<ComputedKey, DdlError> {
    use typedpg_pg_query::ast::{SortByDir, SortByNulls};
    let mut collation = column_collation;
    if let Some(names) = elem
        .map(|e| e.collation.as_slice())
        .filter(|n| !n.is_empty())
    {
        // get_collation_oid(attribute->collation, false).
        collation = Some(super::collations::collation_of_clause(db, names)?);
        let collatable = |t: crate::oid::PgTypeOid| {
            db.pg_type
                .get(&db.unwrap_domain(t))
                .is_some_and(|t| t.typcollation.is_some())
        };
        if let Some(t) = typ
            && !collatable(t)
        {
            return Err(DdlError::Parse(format!(
                "collations are not supported by type {}",
                super::util::format_type_for_message(db, t)
            )));
        }
    }
    let opclass = match typ {
        Some(t) => Some(super::opclass::resolve_opclass(
            db,
            elem.map_or(&[][..], |e| e.opclass.as_slice()),
            t,
            am,
        )?),
        None => None,
    };
    // The btree pattern operator classes compare bytes, which a
    // nondeterministic collation can't (PostgreSQL 19's ComputeIndexAttrs).
    if let Some(opc) = opclass.and_then(|o| db.pg_opclass.iter().find(|c| c.oid == o))
        && opc.opcmethod == "btree"
        && db.namespace_oid("pg_catalog") == Some(opc.opcnamespace)
        && matches!(
            opc.opcname.as_str(),
            "text_pattern_ops" | "varchar_pattern_ops" | "bpchar_pattern_ops"
        )
        && collation
            .and_then(|c| db.pg_collation.get(&c))
            .is_some_and(|c| !c.collisdeterministic)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "nondeterministic collations are not supported for operator class \"{}\"",
            opc.opcname
        )));
    }
    let exclusion_op: Option<Vec<String>> = exclusion_op.map(|names| {
        names
            .iter()
            .filter_map(|n| super::util::node_string(n.as_ref()))
            .map(str::to_owned)
            .collect()
    });
    let exclusion_opr = match (exclusion_op.as_deref(), typ) {
        (Some(names), Some(t)) => check_exclusion_operator(db, names, t, opclass)?,
        _ => None,
    };
    let (ordering, nulls) = elem.map_or(
        (SortByDir::SortbyDefault, SortByNulls::SortbyNullsDefault),
        |e| (e.ordering, e.nulls_ordering),
    );
    if let Some(caps) = caps
        && !caps.can_order
    {
        if ordering != SortByDir::SortbyDefault {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support ASC/DESC options"
            )));
        }
        if nulls != SortByNulls::SortbyNullsDefault {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support NULLS FIRST/LAST options"
            )));
        }
    }
    // indoption: DESC, and NULLS FIRST (the default under DESC).
    let desc = ordering == SortByDir::SortbyDesc;
    let nulls_first = if nulls == SortByNulls::SortbyNullsDefault {
        desc
    } else {
        nulls == SortByNulls::SortbyNullsFirst
    };
    Ok(ComputedKey {
        column: IndexKeyColumn {
            typ,
            default_order: !desc && !nulls_first,
            exclusion_op,
        },
        opclass,
        collation,
        exclusion_opr,
    })
}

/// ComputeIndexAttrs: an exclusion constraint's operator must take the
/// column's type on both sides without run-time coercion
/// (compatible_oper_opid), be its own commutator, and belong to the column
/// operator class's family. Returns the operator.
fn check_exclusion_operator(
    db: &PgCatalog,
    names: &[String],
    typ: crate::oid::PgTypeOid,
    opclass: Option<PgOpclassOid>,
) -> Result<Option<crate::oid::PgOperatorOid>, DdlError> {
    use crate::lookup::OperatorMatch;
    let name = names.join(".");
    let opname = names.last().map(String::as_str).unwrap_or_default();
    let typname = || super::util::format_type_for_message(db, typ);
    let found = match db.find_operator_detailed(&name, Some(typ), typ) {
        OperatorMatch::Found(op) => op,
        OperatorMatch::NotFound => {
            return Err(DdlError::TypeNotFound(format!(
                "operator does not exist: {} {opname} {} (No operator matches the given name \
                 and argument types. You might need to add explicit type casts.)",
                typname(),
                typname()
            )));
        }
        OperatorMatch::Ambiguous => {
            return Err(DdlError::UnsupportedDdl(format!(
                "operator is not unique: {} {opname} {}",
                typname(),
                typname()
            )));
        }
        OperatorMatch::Error(e) => return Err(DdlError::UnsupportedDdl(e.to_string())),
    };
    let opr = found.oid;
    let declared_ok = |declared: Option<crate::oid::PgTypeOid>| {
        declared.is_none_or(|d| {
            d == typ
                || db.is_binary_coercible(typ, d)
                || db.is_binary_coercible(db.unwrap_domain(typ), d)
                || db
                    .pg_type
                    .get(&d)
                    .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Pseudo)
        })
    };
    // compatible_oper: the operand types must be binary-compatible.
    let op_row = db.pg_operator.get(&opr);
    if !declared_ok(op_row.and_then(|o| o.oprleft)) || !declared_ok(op_row.map(|o| o.oprright)) {
        return Err(DdlError::UnsupportedDdl(format!(
            "operator requires run-time type coercion: {}",
            super::opclass::format_operator(db, opr)
        )));
    }
    if op_row.and_then(|o| o.oprcom) != Some(opr) {
        return Err(DdlError::UnsupportedDdl(format!(
            "operator {} is not commutative (Only commutative operators can be used in \
             exclusion constraints.)",
            super::opclass::format_operator(db, opr)
        )));
    }
    // get_op_opfamily_strategy — skipped for a family none of whose
    // operators the analyzer could record.
    if let Some(class) = opclass.and_then(|c| super::opclass::opclass_by_oid(db, c))
        && db.pg_amop.iter().any(|o| {
            o.amopfamily == class.opcfamily
                && o.amopfamilynamespace == class.opcfamilynamespace
                && o.amopmethod == class.opcmethod
        })
        && !super::opclass::opfamily_has_operator(db, class, opr)
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "operator {} is not a member of operator family \"{}\" (The exclusion operator \
             must be related to the index operator class for the constraint.)",
            super::opclass::format_operator(db, opr),
            super::opclass::format_opfamily(db, class)
        )));
    }
    Ok(Some(opr))
}

/// DefineIndex (and transformIndexConstraint's IndexStmt) for the index of
/// a PRIMARY KEY, UNIQUE or EXCLUDE constraint `c` with key columns
/// `conkey` (`0` for an EXCLUDE expression) and INCLUDE columns `include`:
/// the column limit, the access method (btree; GiST for WITHOUT OVERLAPS;
/// the EXCLUDE's `USING`), its WITH options, then each key column's
/// collation, operator class, exclusion operator and ordering — and, on a
/// partitioned table, that an EXCLUDE constraint compares every partition
/// key column with its equality operator.
/// ComputeIndexAttrs: an index expression whose transformed form is a
/// plain column — `(a)`, `((t).a)`, `(a::text)` of a text column, any of
/// them under top-level COLLATE clauses (which it strips: the collation is
/// the key's) — is that column ("User wrote "(column)" ... Treat it like
/// simple attribute anyway"): its attnum.
pub(crate) fn expression_column(
    db: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<i16> {
    let mut expr = expr?;
    while let ast::Node::CollateClause(c) = expr {
        expr = c.arg.as_ref()?;
    }
    // A cast to the column's own type, without a typmod, coerces nothing
    // (coerce_type returns the Var itself).
    if let ast::Node::TypeCast(tc) = expr {
        let tn = tc.type_name.as_ref()?;
        let attnum = expression_column(db, relid, tc.arg.as_ref())?;
        let attr = db
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)?;
        let target = super::functions::typename_type_id(db, tn).ok()?;
        return (tn.typmods.is_empty()
            && tn.array_bounds.is_empty()
            && attr.atttypmod.is_none()
            && target == attr.atttypid)
            .then_some(attnum);
    }
    let relname = db.pg_class.get(&relid).map(|c| c.relname.as_str());
    let name = match expr {
        ast::Node::ColumnRef(cr) => {
            let names: Vec<&str> = cr
                .fields
                .iter()
                .map(|f| super::util::node_string(f.as_ref()))
                .collect::<Option<_>>()?;
            match names.as_slice() {
                [name] => *name,
                [rel, name] if Some(*rel) == relname => *name,
                _ => return None,
            }
        }
        ast::Node::AIndirection(ind) if ind.indirection.len() == 1 => {
            let selections = super::util::relation_row_selections(db, relid, Some(expr));
            let &(_, field) = selections.first()?;
            match ind.arg.as_ref() {
                Some(ast::Node::ColumnRef(cr)) if std::ptr::eq(&**cr, selections[0].0) => field,
                _ => return None,
            }
        }
        _ => return None,
    };
    db.attribute_by_name(relid, name)
        .map(|a| a.attnum)
        .or_else(|| system_attnum(name))
}

/// A boolean constant true as transformExpr leaves it: `true`, or a typed
/// literal of boolean (`'t'::boolean`, `boolean 'yes'`).
fn is_true_constant(node: &ast::Node) -> bool {
    match node {
        ast::Node::AConst(c) => matches!(&c.val, Some(ast::AConstValue::Boolean(b)) if b.boolval),
        ast::Node::TypeCast(tc) => {
            let is_bool = tc.type_name.as_ref().is_some_and(|tn| {
                tn.typmods.is_empty()
                    && tn.array_bounds.is_empty()
                    && matches!(
                        tn.names
                            .iter()
                            .filter_map(|n| super::util::node_string(n.as_ref()))
                            .collect::<Vec<_>>()
                            .as_slice(),
                        ["bool"] | ["pg_catalog", "bool"] | ["boolean"]
                    )
            });
            is_bool
                && match tc.arg.as_ref() {
                    Some(ast::Node::AConst(c)) => match &c.val {
                        Some(ast::AConstValue::Boolean(b)) => b.boolval,
                        Some(ast::AConstValue::String(s)) => {
                            super::reloptions::parse_bool(&s.sval) == Some(true)
                        }
                        _ => false,
                    },
                    _ => false,
                }
        }
        _ => false,
    }
}

pub(crate) fn define_constraint_index(
    db: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::ast::Constraint,
    conkey: &[i16],
    include: &[i16],
    period: bool,
) -> Result<ConstraintIndex, DdlError> {
    use typedpg_pg_query::ast::ConstrType;
    let exclusion = c.contype == ConstrType::ConstrExclusion;
    check_index_max_keys(conkey.len() + include.len())?;
    let am = if exclusion && !c.access_method.is_empty() {
        c.access_method.clone()
    } else if period {
        "gist".to_owned()
    } else {
        "btree".to_owned()
    };
    let caps = check_index_am(
        db,
        &am,
        !exclusion && !period,
        !include.is_empty(),
        conkey.len(),
        exclusion,
    )?;
    if let Some(index_am) = super::reloptions::IndexAm::from_name(&am) {
        super::reloptions::check_reloptions(
            &c.options,
            super::reloptions::RelOptKind::Index(index_am),
            false,
            false,
        )?;
    }
    // transformIndexConstraint: an EXCLUDE constraint's WHERE is the
    // index's predicate.
    if c.where_clause.is_some() {
        check_index_predicate(db, relid, c.where_clause.as_ref())?;
    }
    let attrs = db.attributes_of(relid);
    let mut columns = Vec::with_capacity(conkey.len());
    let mut indclass = Vec::with_capacity(conkey.len());
    let mut indcollation = Vec::with_capacity(conkey.len());
    let mut exclusion_oprs = Vec::with_capacity(conkey.len());
    let mut colnames = Vec::with_capacity(conkey.len());
    let mut expr_types = Vec::new();
    for (i, &attnum) in conkey.iter().enumerate() {
        let attr = attrs.iter().find(|a| attnum > 0 && a.attnum == attnum);
        let (elem, opnames) = if exclusion {
            match c.exclusions.get(i).and_then(|p| p.as_ref()) {
                Some(ast::Node::List(l)) => {
                    let elem = match l.items.first().and_then(|n| n.as_ref()) {
                        Some(ast::Node::IndexElem(e)) => Some(&**e),
                        _ => None,
                    };
                    let ops = match l.items.get(1).and_then(|n| n.as_ref()) {
                        Some(ast::Node::List(ops)) => Some(ops.items.as_slice()),
                        _ => None,
                    };
                    (elem, ops)
                }
                _ => (None, None),
            }
        } else {
            (None, None)
        };
        let (typ, collation) = match (attr, elem.map(|e| &e.expr).filter(|e| e.is_some())) {
            (Some(a), _) => {
                colnames.push(a.attname.clone());
                (Some(a.atttypid), a.attcollation)
            }
            (None, Some(expr)) => {
                colnames.push(figure_index_colname(expr.as_ref()));
                check_index_expression(db, relid, expr.as_ref())?;
                match super::volatile::infer_over_relation(db, relid, expr.as_ref(), None) {
                    Some(Err(e)) => return Err(DdlError::UnsupportedDdl(e.to_string())),
                    Some(Ok(t)) if known_key_type(expr.as_ref(), &t) => {
                        expr_types.push((i, t.type_oid));
                        (Some(t.type_oid), expression_collation(db, &t))
                    }
                    Some(Ok(_)) | None => (None, None),
                }
            }
            (None, None) => {
                colnames.push(String::new());
                (None, None)
            }
        };
        let key = compute_key_column(db, &am, caps.as_ref(), elem, typ, collation, opnames)?;
        if attr.is_none() {
            check_expression_key_collation(db, typ, key.collation)?;
        }
        columns.push(key.column);
        indclass.push(key.opclass);
        indcollation.push(key.collation);
        exclusion_oprs.push(key.exclusion_opr);
    }
    if exclusion {
        check_exclusion_covers_partition_key(db, relid, conkey, &indcollation, &exclusion_oprs)?;
    }
    check_index_expression_types(db, &colnames, &expr_types)?;
    // The expression keys (an expression that is a plain column was given
    // its attnum) and the WHERE: the index's indexprs and indpred.
    let indexprs = if exclusion {
        conkey
            .iter()
            .enumerate()
            .filter(|&(_, &attnum)| attnum == 0)
            .filter_map(
                |(i, _)| match c.exclusions.get(i).and_then(|p| p.as_ref()) {
                    Some(ast::Node::List(l)) => match l.items.first().and_then(|n| n.as_ref()) {
                        Some(ast::Node::IndexElem(e)) => Some(serialize_node(e.expr.as_ref())),
                        _ => None,
                    },
                    _ => None,
                },
            )
            .collect()
    } else {
        Vec::new()
    };
    let indpred = c
        .where_clause
        .as_ref()
        .filter(|w| !is_true_constant(w))
        .map(|w| serialize_node(Some(w)));
    Ok(ConstraintIndex {
        am,
        indexprs,
        indpred,
        keys: IndexKeys {
            columns,
            nulls_not_distinct: c.nulls_not_distinct,
        },
        indclass,
        indcollation,
    })
}

/// DefineIndex on a partitioned table, for an exclusion constraint: every
/// partition key column must be a key column, under the key's collation,
/// compared with the partition key's equality operator.
fn check_exclusion_covers_partition_key(
    db: &PgCatalog,
    relid: PgClassOid,
    conkey: &[i16],
    key_collations: &[Option<PgCollationOid>],
    oprs: &[Option<crate::oid::PgOperatorOid>],
) -> Result<(), DdlError> {
    let Some(part_key) = db.partition_keys.get(&relid) else {
        return Ok(());
    };
    let collations = super::tables::partbound::partition_key_collations(db, relid);
    let eq_ops = super::tables::partbound::partition_key_eqops(db, relid);
    let attname = |attnum: i16| {
        db.attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
            .unwrap_or_default()
    };
    for (i, pk) in part_key.iter().enumerate() {
        // DefineIndex walks the key in order.
        if *pk == 0 {
            return Err(DdlError::Parse(
                "unsupported EXCLUDE constraint with partition key definition (EXCLUDE \
                 constraints cannot be used when partition keys include expressions.)"
                    .into(),
            ));
        }
        let ptkey_eqop = eq_ops.get(i).copied().flatten();
        let mut found = false;
        for (j, k) in conkey.iter().enumerate() {
            if k != pk || key_collations[j] != collations.get(i).copied().flatten() {
                continue;
            }
            let (Some(opr), Some(eq)) = (oprs[j], ptkey_eqop) else {
                // Types or operators the analyzer doesn't know: assume it
                // matches.
                found = true;
                break;
            };
            if opr == eq {
                found = true;
                break;
            }
            let opname = db
                .pg_operator
                .get(&opr)
                .map(|o| o.oprname.clone())
                .unwrap_or_default();
            return Err(DdlError::UnsupportedDdl(format!(
                "cannot match partition key to index on column \"{}\" using non-equal operator \
                 \"{opname}\"",
                attname(*pk)
            )));
        }
        if !found {
            return Err(DdlError::UnsupportedDdl(format!(
                "EXCLUDE constraint on partitioned table must include all partitioning columns \
                 (EXCLUDE constraint on table \"{}\" lacks column \"{}\" which is part of the \
                 partition key.)",
                db.pg_class
                    .get(&relid)
                    .map(|c| c.relname.as_str())
                    .unwrap_or_default(),
                attname(*pk)
            )));
        }
    }
    Ok(())
}

/// DefineIndex's recursion, for the copy of `parent` (an index of a
/// partitioned table) it creates on partition `part` with key columns
/// `indkey`: when the partition is itself partitioned, a unique or
/// exclusion index must cover its partition key too.
pub(crate) fn check_partition_copy_covers_partition_key(
    db: &PgCatalog,
    parent: &PgIndex,
    part: PgClassOid,
    indkey: &[i16],
) -> Result<(), DdlError> {
    let nkey = usize::try_from(parent.indnkeyatts)
        .unwrap_or(0)
        .min(indkey.len());
    let key = &indkey[..nkey];
    if parent.indisexclusion {
        // The exclusion operators, resolved as for the parent's columns.
        let oprs: Vec<_> = db
            .index_keys
            .get(&parent.indexrelid)
            .map(|keys| {
                keys.columns
                    .iter()
                    .zip(&parent.indclass)
                    .map(|(c, &class)| match (c.exclusion_op.as_deref(), c.typ) {
                        (Some(names), Some(t)) => {
                            check_exclusion_operator(db, names, t, class).ok().flatten()
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let oprs: Vec<_> = (0..key.len())
            .map(|j| oprs.get(j).copied().flatten())
            .collect();
        let collations: Vec<_> = (0..key.len())
            .map(|j| parent.indcollation.get(j).copied().flatten())
            .collect();
        check_exclusion_covers_partition_key(db, part, key, &collations, &oprs)
    } else if parent.indisunique {
        let label = if parent.indisprimary {
            "PRIMARY KEY"
        } else {
            "UNIQUE"
        };
        super::tables::check_unique_covers_partition_key(
            db,
            part,
            key,
            &parent.indcollation,
            &parent.indclass,
            label,
        )
    } else {
        Ok(())
    }
}

/// get_opfamily_member_for_cmptype(COMPARE_EQ): the equality operator of
/// a btree or hash operator class's family over its input type; `None`
/// for other access methods or an unknown class.
pub(crate) fn index_eq_operator(
    db: &PgCatalog,
    class: PgOpclassOid,
) -> Option<crate::oid::PgOperatorOid> {
    let class = super::opclass::opclass_by_oid(db, class)?;
    // BTEqualStrategyNumber / HTEqualStrategyNumber.
    let strategy = match class.opcmethod.as_str() {
        "btree" => 3,
        "hash" => 1,
        _ => return None,
    };
    super::opclass::opfamily_member(db, class, strategy)
}

/// ATPostAlterTypeCleanup (tablecmds.c): after ALTER COLUMN TYPE, every
/// index over the column is rebuilt from its definition as
/// pg_get_indexdef prints it — an operator class only when it isn't the
/// old type's default, a COLLATE only when it isn't the column's — so its
/// operator classes (and exclusion operators) are resolved again for the
/// new type. `old_type` / `old_collation` are the column's before the
/// change, whose new type is already recorded.
/// Where index `index` stands in PG's OID order: its OID, or the one PG
/// gave it when ALTER COLUMN TYPE rebuilt it.
pub(crate) fn index_order(db: &PgCatalog, index: PgClassOid) -> u32 {
    db.rebuilt_index_oids
        .get(&index)
        .copied()
        .unwrap_or_else(|| index.get())
}

pub(crate) fn rebuild_indexes_for_column_type(
    db: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    old_type: crate::oid::PgTypeOid,
    old_collation: Option<crate::oid::PgCollationOid>,
) -> Result<(), DdlError> {
    let Some(attr) = db
        .attributes_of(relid)
        .iter()
        .find(|a| a.attnum == attnum)
        .cloned()
    else {
        return Ok(());
    };
    let mut indexes: Vec<PgIndex> = db
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid)
        .cloned()
        .collect();
    indexes.sort_by_key(|i| index_order(db, i.indexrelid));
    let mut rebuilt: Vec<PgClassOid> = Vec::new();
    // Every index reading the column — as a key or INCLUDE column, in an
    // expression or in its predicate — is rebuilt from pg_get_indexdef,
    // which names the table's columns: its columns take their names anew.
    // The rebuilt index is a new object, which no DEPENDS ON EXTENSION
    // marks.
    for index in &indexes {
        let reads_column = |ast: &SerializedAst| {
            ast.ast.as_ref().is_some_and(|inner| {
                inner.nodes().any(|(n, ..)| {
                    matches!(n, typedpg_pg_query::NodeRef::ColumnRef(cr)
                        if cr.fields.last().and_then(|n| super::util::node_string(n.as_ref()))
                            == Some(attr.attname.as_str()))
                })
            })
        };
        if index.indkey.contains(&attnum)
            || index.indexprs.iter().any(reads_column)
            || index.indpred.as_ref().is_some_and(reads_column)
        {
            db.index_attnames.remove(&index.indexrelid);
            // A new index, whose relcache entry is built anew.
            db.relcache_index_attnames.remove(&index.indexrelid);
            // (A new object's entry, partitioned index or not, stays as
            // created for the rest of the transaction.)
            db.indexes_with_new_relfilenode.insert(index.indexrelid);
            rebuilt.push(index.indexrelid);
            super::depend::forget_auto_extension_marks(
                db,
                super::depend::ObjectAddress::relation(index.indexrelid),
            );
        }
    }
    // ATPostAlterTypeCleanup re-adds the constraints first, then the plain
    // indexes, each in the order it found them: their new OIDs follow.
    let backs_constraint = |db: &PgCatalog, index: PgClassOid| {
        let name = db.pg_class.get(&index).map(|c| c.relname.clone());
        db.pg_constraint.values().any(|c| {
            c.conrelid == relid
                && matches!(
                    c.contype,
                    crate::pg_catalog::ConType::PrimaryKey
                        | crate::pg_catalog::ConType::Unique
                        | crate::pg_catalog::ConType::Exclusion
                )
                && Some(&c.conname) == name.as_ref()
        })
    };
    // A partition's index attached to its parent's is not rebuilt from its
    // own definition: RememberIndexForRebuilding leaves it to the parent's,
    // whose DefineIndex creates it anew — named as a new clone is (from the
    // parent index's column names), the old one gone.
    for &index in &rebuilt {
        let Some(parent) = db.index_parents.get(&index).copied() else {
            continue;
        };
        let Some(class) = db.pg_class.get(&index).cloned() else {
            continue;
        };
        let Some(table) = db.pg_class.get(&relid).cloned() else {
            continue;
        };
        // The parent index is rebuilt too, from pg_get_indexdef: its columns
        // are named after the table's columns now (the parent may come after
        // its partitions here).
        db.index_attnames.remove(&parent);
        db.relcache_index_attnames.remove(&parent);
        let colnames = relcache_index_attnames(db, parent);
        let parent_con = super::tables::partidx::backing_constraint(db, parent);
        let addition = super::util::index_name_addition(&colnames);
        // The old name is free again.
        db.rename_pg_class(index, String::new(), class.relnamespace);
        let name = match parent_con.map(|c| c.contype) {
            Some(crate::pg_catalog::ConType::PrimaryKey) => {
                super::util::choose_constraint_index_name(
                    db,
                    table.relnamespace,
                    &table.relname,
                    "",
                    "pkey",
                )
            }
            Some(contype) => super::util::choose_constraint_index_name(
                db,
                table.relnamespace,
                &table.relname,
                &addition,
                if contype == crate::pg_catalog::ConType::Unique {
                    "key"
                } else {
                    "excl"
                },
            ),
            None => super::util::choose_relation_name(
                db,
                table.relnamespace,
                &table.relname,
                &addition,
                "idx",
            ),
        };
        db.rename_pg_class(index, name.clone(), class.relnamespace);
        // Its constraint, if any, is the clone's too, under the same name.
        if let Some(con) = db
            .pg_constraint
            .values_mut()
            .find(|c| c.conrelid == relid && c.conname == class.relname)
        {
            con.conname = name;
        }
    }
    let (constraints, plain): (Vec<PgClassOid>, Vec<PgClassOid>) =
        rebuilt.into_iter().partition(|&i| backs_constraint(db, i));
    for index in constraints.into_iter().chain(plain) {
        let oid = db.alloc_oid()?.get();
        db.rebuilt_index_oids.insert(index, oid);
    }
    for mut index in indexes {
        let Some(mut keys) = db.index_keys.get(&index.indexrelid).cloned() else {
            continue;
        };
        let am = db
            .index_access_methods
            .get(&index.indexrelid)
            .cloned()
            .unwrap_or_else(|| "btree".to_owned());
        let mut exprs = index.indexprs.iter();
        let mut changed = false;
        for (i, key) in keys.columns.iter_mut().enumerate() {
            let Some(&k) = index.indkey.get(i) else {
                break;
            };
            let (new_type, prior_type) = if k == attnum {
                (attr.atttypid, old_type)
            } else if k == 0 {
                // An expression: re-analyzed when it reads the column.
                let Some(expr) = exprs.next() else {
                    continue;
                };
                let reads = expr.ast.as_ref().is_some_and(|inner| {
                    inner.nodes().any(|(n, ..)| {
                        matches!(n, typedpg_pg_query::NodeRef::ColumnRef(cr)
                            if cr.fields.last().and_then(|n| super::util::node_string(n.as_ref()))
                                == Some(attr.attname.as_str()))
                    })
                });
                let Some(prior) = key.typ.filter(|_| reads) else {
                    continue;
                };
                match super::volatile::infer_over_relation(db, relid, expr.ast.as_ref(), None) {
                    Some(Ok(t)) => (t.type_oid, prior),
                    Some(Err(e)) => return Err(DdlError::UnsupportedDdl(e.to_string())),
                    None => continue,
                }
            } else {
                continue;
            };
            // pg_get_indexdef's COLLATE: an explicit one must suit the new
            // type.
            let column_collation = if k == attnum { old_collation } else { None };
            let collation = index.indcollation.get(i).copied().flatten();
            if collation.is_some() && collation != column_collation {
                let collatable = db
                    .pg_type
                    .get(&db.unwrap_domain(new_type))
                    .is_some_and(|t| t.typcollation.is_some());
                if !collatable {
                    return Err(DdlError::Parse(format!(
                        "collations are not supported by type {}",
                        super::util::format_type_for_message(db, new_type)
                    )));
                }
            } else if k == attnum
                && let Some(slot) = index.indcollation.get_mut(i)
            {
                *slot = attr.attcollation;
            }
            let explicit = index.indclass.get(i).copied().flatten().filter(|&class| {
                super::opclass::default_opclass_id(db, prior_type, &am) != Some(class)
            });
            let opclass = match explicit.and_then(|c| super::opclass::opclass_by_oid(db, c)) {
                Some(class) => {
                    super::opclass::check_opclass_accepts(db, class, new_type)?;
                    explicit
                }
                None => super::opclass::resolve_index_opclass(db, &[], new_type, &am)?,
            };
            if let Some(names) = key.exclusion_op.as_deref() {
                check_exclusion_operator(db, names, new_type, opclass)?;
            }
            key.typ = Some(new_type);
            if let Some(slot) = index.indclass.get_mut(i) {
                *slot = opclass;
            }
            changed = true;
        }
        if changed {
            db.index_keys.insert(index.indexrelid, keys);
            if let Some(row) = db.pg_index.get_mut(&index.indexrelid) {
                row.indclass = index.indclass;
                row.indcollation = index.indcollation;
            }
            // The rebuilt index depends on its new classes, and on what its
            // expressions refer to now.
            crate::ddl::depend::record_index_opclasses(db, index.indexrelid);
            crate::ddl::depend::record_index_expressions(db, index.indexrelid);
        }
    }
    Ok(())
}

/// check_index_is_clusterable (cluster.c): the index's access method must
/// support clustering (`amclusterable`: btree and GiST).
pub(crate) fn check_am_clusterable(
    db: &PgCatalog,
    index: PgClassOid,
    name: &str,
) -> Result<(), DdlError> {
    let am = db
        .index_access_methods
        .get(&index)
        .map_or("btree", String::as_str);
    if super::opclass::am_caps(am).is_some_and(|caps| !caps.can_cluster) {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot cluster on index \"{name}\" because access method does not support \
             clustering"
        )));
    }
    Ok(())
}

/// RemoveRelations (tablecmds.c): DROP INDEX CONCURRENTLY drops a single
/// index, without CASCADE — and not a partitioned one, unless it is
/// temporary (a temporary relation is never dropped concurrently).
pub(crate) fn check_drop_concurrently(
    db: &PgCatalog,
    stmt: &typedpg_pg_query::ast::DropStmt,
) -> Result<(), DdlError> {
    use typedpg_pg_query::ast::DropBehavior;
    if stmt.objects.len() != 1 {
        return Err(DdlError::UnsupportedDdl(
            "DROP INDEX CONCURRENTLY does not support dropping multiple objects".into(),
        ));
    }
    if stmt.behavior == DropBehavior::DropCascade {
        return Err(DdlError::UnsupportedDdl(
            "DROP INDEX CONCURRENTLY does not support CASCADE".into(),
        ));
    }
    let Some(ast::Node::List(list)) = stmt.objects[0].as_ref() else {
        return Ok(());
    };
    let (schema, name) = super::util::extract_names(&list.items, db);
    let index = db
        .namespace_oid(&schema)
        .and_then(|ns| db.class_by_qname.get(&(ns, name.clone())).copied());
    if let Some(index) = index
        && db.pg_class.get(&index).map(|c| c.relkind) == Some(RelKind::PartitionedIndex)
        && db
            .pg_index
            .get(&index)
            .and_then(|i| db.relpersistence.get(&i.indrelid))
            != Some(&'t')
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "cannot drop partitioned index \"{name}\" concurrently"
        )));
    }
    Ok(())
}

/// RangeVarCallbackForAlterRelation (tablecmds.c): ALTER TABLE reaches most
/// relations, but ALTER SEQUENCE / VIEW / MATERIALIZED VIEW / FOREIGN TABLE
/// / INDEX must name a relation of that kind (42809).
pub(crate) fn check_alter_relation_kind(
    objtype: typedpg_pg_query::ast::ObjectType,
    relkind: Option<RelKind>,
    relname: &str,
) -> Result<(), DdlError> {
    use typedpg_pg_query::ast::ObjectType;
    let Some(relkind) = relkind else {
        return Ok(());
    };
    let expected = match objtype {
        ObjectType::ObjectSequence if relkind != RelKind::Sequence => "a sequence",
        ObjectType::ObjectView if relkind != RelKind::View => "a view",
        ObjectType::ObjectMatview if relkind != RelKind::MaterializedView => "a materialized view",
        ObjectType::ObjectForeignTable if relkind != RelKind::ForeignTable => "a foreign table",
        ObjectType::ObjectIndex
            if !matches!(relkind, RelKind::Index | RelKind::PartitionedIndex) =>
        {
            "an index"
        }
        _ => return Ok(()),
    };
    Err(DdlError::Parse(format!("\"{relname}\" is not {expected}")))
}

/// The (negative) attnum of system column `name`.
fn system_attnum(name: &str) -> Option<i16> {
    crate::pg_catalog::SYSTEM_COLUMNS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(.., attnum)| *attnum)
}

/// What an index is built for, as DefineIndex words its column errors.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexUse {
    Index,
    /// A UNIQUE or EXCLUDE constraint's index.
    Constraint,
    Primary,
}

/// DefineIndex (indexcmds.c): no index on a system column, nor on a virtual
/// generated column — as a key or INCLUDE column (`attnums`, `0` for an
/// expression slot) or read by a key expression or the predicate (`exprs`).
pub(crate) fn check_index_columns(
    db: &PgCatalog,
    relid: PgClassOid,
    attnums: &[i16],
    exprs: &[Option<&typedpg_pg_query::ast::Node>],
    usage: IndexUse,
) -> Result<(), DdlError> {
    use crate::pg_catalog::AttGenerated;
    let is_virtual = |attnum: i16| {
        db.attributes_of(relid)
            .iter()
            .any(|a| a.attnum == attnum && a.attgenerated == Some(AttGenerated::Virtual))
    };
    let system =
        || DdlError::UnsupportedDdl("index creation on system columns is not supported".into());
    let constraint_msg = |usage: IndexUse| match usage {
        IndexUse::Constraint | IndexUse::Primary => {
            "unique constraints on virtual generated columns are not supported"
        }
        IndexUse::Index => "indexes on virtual generated columns are not supported",
    };
    for &attnum in attnums {
        if attnum < 0 {
            return Err(system());
        }
        if attnum > 0 && is_virtual(attnum) {
            return Err(DdlError::UnsupportedDdl(
                match usage {
                    IndexUse::Primary => {
                        "primary keys on virtual generated columns are not supported"
                    }
                    other => constraint_msg(other),
                }
                .into(),
            ));
        }
    }
    let mut read: Vec<i16> = Vec::new();
    for expr in exprs {
        let Some(inner) = expr.as_ref() else {
            continue;
        };
        // A column by name, or selected from the whole row (`(t).a`).
        let names = inner
            .nodes()
            .filter_map(|(n, ..)| match n {
                typedpg_pg_query::NodeRef::ColumnRef(cr) => cr
                    .fields
                    .last()
                    .and_then(|n| super::util::node_string(n.as_ref())),
                _ => None,
            })
            .chain(
                super::util::relation_row_selections(db, relid, *expr)
                    .into_iter()
                    .map(|(_, field)| field),
            );
        for name in names {
            match db.attribute_by_name(relid, name) {
                Some(attr) => read.push(attr.attnum),
                None if super::util::is_system_column(name) => {
                    return Err(system());
                }
                None => {}
            }
        }
    }
    read.sort_unstable();
    if read.into_iter().any(is_virtual) {
        return Err(DdlError::UnsupportedDdl(constraint_msg(usage).into()));
    }
    Ok(())
}

/// `FigureIndexColname`: the name `FigureColnameInternal` gives the
/// expression as a select-list item — a column, a function, a cast's
/// type, `case`, ... — or `expr` when it has none.
pub(crate) fn figure_index_colname(expr: Option<&typedpg_pg_query::ast::Node>) -> String {
    expr.and_then(|e| crate::resolve::infer_column_name(Some(e)))
        .unwrap_or_else(|| "expr".to_owned())
}

/// Store a `typedpg_pg_query::ast::Node` as a `SerializedAst` (an empty bindings
/// stream — index expressions don't yet flow through the view-binding walker;
/// that's a separate piece of work).
pub(crate) fn serialize_node(node: Option<&typedpg_pg_query::ast::Node>) -> SerializedAst {
    SerializedAst {
        ast: node.cloned(),
        bindings: Vec::<AstBinding>::new(),
    }
}

/// find_all_inheritors over an index: `index` and the partition indexes
/// attached to it, at every level.
pub(crate) fn all_index_inheritors(interp: &PgCatalog, index: PgClassOid) -> Vec<PgClassOid> {
    let mut tree = vec![index];
    let mut i = 0;
    while i < tree.len() {
        let mut children: Vec<PgClassOid> = interp
            .index_parents
            .iter()
            .filter(|&(_, &parent)| parent == tree[i])
            .map(|(&child, _)| child)
            .collect();
        children.sort();
        for child in children {
            if !tree.contains(&child) {
                tree.push(child);
            }
        }
        i += 1;
    }
    tree
}
