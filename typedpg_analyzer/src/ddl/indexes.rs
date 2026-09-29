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

use prost::Message;
use typedpg_pg_query::protobuf::{IndexStmt, node};

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
            RelKind::View
            | RelKind::Sequence
            | RelKind::CompositeType
            | RelKind::Index
            | RelKind::PartitionedIndex
            | RelKind::ForeignTable => Some(class.relkind.plural()),
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
    let mut key_columns: Vec<IndexKeyColumn> = Vec::with_capacity(stmt.index_params.len());
    let mut indexprs: Vec<SerializedAst> = Vec::new();
    for param in &stmt.index_params {
        let Some(node::Node::IndexElem(elem)) = param.node.as_ref() else {
            continue;
        };
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
        } else if let Some(expr) = elem.expr.as_deref() {
            indkey.push(0);
            indexprs.push(serialize_node(expr));
            match super::volatile::infer_over_relation(db, indrelid, expr, None) {
                Some(Ok(t)) => Some((t.type_oid, None)),
                _ => None,
            }
        } else {
            None
        };
        // ComputeIndexAttrs: the collation, ResolveOpClass, then the
        // ordering options (amcanorder).
        let (column, _) = compute_key_column(
            db,
            am,
            caps.as_ref(),
            Some(elem),
            column_type.map(|(t, _)| t),
            column_type.and_then(|(_, c)| c),
            None,
        )?;
        key_columns.push(column);
    }
    let indcollation: Vec<_> = key_columns.iter().map(|c| c.collation).collect();
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
        let an = attnum_by_name
            .get(&elem.name)
            .copied()
            .or_else(|| system_attnum(&elem.name))
            .ok_or_else(|| DdlError::Parse(format!("column \"{}\" does not exist", elem.name)))?;
        indkey.push(an);
    }
    let indpred = stmt.where_clause.as_deref().map(serialize_node);
    let usage = if stmt.primary {
        IndexUse::Primary
    } else if stmt.isconstraint {
        IndexUse::Constraint
    } else {
        IndexUse::Index
    };
    let key_exprs: Vec<&typedpg_pg_query::protobuf::Node> = stmt
        .index_params
        .iter()
        .filter_map(|p| match p.node.as_ref()? {
            node::Node::IndexElem(elem) => elem.expr.as_deref(),
            _ => None,
        })
        .chain(stmt.where_clause.as_deref())
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
            label,
        )?;
    }
    check_index_columns(db, indrelid, &indkey, &key_exprs, usage)?;

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
    let relkind = RelKind::index_on(db.pg_class.get(&indrelid).map(|c| c.relkind));
    db.insert_pg_class(PgClass {
        oid: indexrelid,
        relname: conname.clone(),
        relnamespace: nsoid,
        relkind,
        reltype: None,
    });

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

/// INDEX_MAX_KEYS (pg_config_manual.h): the most columns an index — key
/// and INCLUDE ones — a partition key or a foreign key may have.
pub(crate) const INDEX_MAX_KEYS: usize = 32;

/// What `pg_index` keeps of one key column beyond `indkey`.
#[derive(Clone, Debug, Default)]
pub(crate) struct IndexKeyColumn {
    /// The key's type, which `opclass` was resolved for (`opckeytype`
    /// aside).
    pub(crate) typ: Option<crate::oid::PgTypeOid>,
    /// `indclass`: the operator class. `None` when the column's type is
    /// unknown to the analyzer.
    pub(crate) opclass: Option<super::opclass::OpclassId>,
    /// `indcollation`.
    pub(crate) collation: Option<crate::oid::PgCollationOid>,
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

/// ComputeIndexAttrs (indexcmds.c) for one key column of type `typ`
/// (`None` when unknown): its collation — the COLLATE clause, which must
/// name an existing collation of a collatable type, else `column_collation`
/// —, its operator class (ResolveOpClass), its exclusion operator and its
/// ordering options.
#[allow(clippy::too_many_arguments)]
fn compute_key_column(
    db: &PgCatalog,
    am: &str,
    caps: Option<&super::opclass::AmCaps>,
    elem: Option<&typedpg_pg_query::protobuf::IndexElem>,
    typ: Option<crate::oid::PgTypeOid>,
    column_collation: Option<crate::oid::PgCollationOid>,
    exclusion_op: Option<&[typedpg_pg_query::protobuf::Node]>,
) -> Result<(IndexKeyColumn, Option<crate::oid::PgOperatorOid>), DdlError> {
    use typedpg_pg_query::protobuf::{SortByDir, SortByNulls};
    let mut collation = column_collation;
    if let Some(names) = elem
        .map(|e| e.collation.as_slice())
        .filter(|n| !n.is_empty())
    {
        // get_collation_oid(attribute->collation, false).
        collation = Some(
            super::tables::partbound::collation_clause(db, names).ok_or_else(|| {
                let written: Vec<&str> =
                    names.iter().filter_map(super::util::node_string).collect();
                DdlError::TypeNotFound(format!(
                    "collation \"{}\" for encoding \"UTF8\" does not exist",
                    written.join(".")
                ))
            })?,
        );
        let collatable = |t: crate::oid::PgTypeOid| {
            db.pg_type
                .get(&db.unwrap_domain(t))
                .is_some_and(|t| t.typcollation.is_some())
        };
        if let Some(t) = typ
            && t != crate::pg_catalog::oid::UNKNOWN
            && !collatable(t)
        {
            return Err(DdlError::Parse(format!(
                "collations are not supported by type {}",
                super::util::format_type_for_message(db, t)
            )));
        }
    }
    let opclass = match typ {
        Some(t) => super::opclass::resolve_index_opclass(
            db,
            elem.map_or(&[][..], |e| e.opclass.as_slice()),
            t,
            am,
        )?,
        None => None,
    };
    let exclusion_op: Option<Vec<String>> = exclusion_op.map(|names| {
        names
            .iter()
            .filter_map(super::util::node_string)
            .map(str::to_owned)
            .collect()
    });
    let exclusion_opr = match (exclusion_op.as_deref(), typ) {
        (Some(names), Some(t)) => check_exclusion_operator(db, names, t, opclass.as_ref())?,
        _ => None,
    };
    let (ordering, nulls) = elem.map_or(
        (
            SortByDir::SortbyDefault as i32,
            SortByNulls::SortbyNullsDefault as i32,
        ),
        |e| (e.ordering, e.nulls_ordering),
    );
    if let Some(caps) = caps
        && !caps.can_order
    {
        if ordering != SortByDir::SortbyDefault as i32 {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support ASC/DESC options"
            )));
        }
        if nulls != SortByNulls::SortbyNullsDefault as i32 {
            return Err(DdlError::UnsupportedDdl(format!(
                "access method \"{am}\" does not support NULLS FIRST/LAST options"
            )));
        }
    }
    // indoption: DESC, and NULLS FIRST (the default under DESC).
    let desc = ordering == SortByDir::SortbyDesc as i32;
    let nulls_first = if nulls == SortByNulls::SortbyNullsDefault as i32 {
        desc
    } else {
        nulls == SortByNulls::SortbyNullsFirst as i32
    };
    let column = IndexKeyColumn {
        typ,
        opclass,
        collation,
        default_order: !desc && !nulls_first,
        exclusion_op,
    };
    Ok((column, exclusion_opr))
}

/// ComputeIndexAttrs: an exclusion constraint's operator must take the
/// column's type on both sides without run-time coercion
/// (compatible_oper_opid), be its own commutator, and belong to the column
/// operator class's family. Returns the operator.
fn check_exclusion_operator(
    db: &PgCatalog,
    names: &[String],
    typ: crate::oid::PgTypeOid,
    opclass: Option<&super::opclass::OpclassId>,
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
    if let Some(class) = opclass.and_then(|c| c.get(db))
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
pub(crate) fn define_constraint_index(
    db: &PgCatalog,
    relid: PgClassOid,
    c: &typedpg_pg_query::protobuf::Constraint,
    conkey: &[i16],
    include: &[i16],
    period: bool,
) -> Result<(String, IndexKeys), DdlError> {
    use typedpg_pg_query::protobuf::ConstrType;
    let exclusion = c.contype == ConstrType::ConstrExclusion as i32;
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
    let attrs = db.attributes_of(relid);
    let mut columns = Vec::with_capacity(conkey.len());
    let mut exclusion_oprs = Vec::with_capacity(conkey.len());
    for (i, &attnum) in conkey.iter().enumerate() {
        let attr = attrs.iter().find(|a| attnum > 0 && a.attnum == attnum);
        let (elem, opnames) = if exclusion {
            match c.exclusions.get(i).and_then(|p| p.node.as_ref()) {
                Some(node::Node::List(l)) => {
                    let elem = match l.items.first().and_then(|n| n.node.as_ref()) {
                        Some(node::Node::IndexElem(e)) => Some(&**e),
                        _ => None,
                    };
                    let ops = match l.items.get(1).and_then(|n| n.node.as_ref()) {
                        Some(node::Node::List(ops)) => Some(ops.items.as_slice()),
                        _ => None,
                    };
                    (elem, ops)
                }
                _ => (None, None),
            }
        } else {
            (None, None)
        };
        let typ = match (attr, elem.and_then(|e| e.expr.as_deref())) {
            (Some(a), _) => Some(a.atttypid),
            (None, Some(expr)) => {
                super::expr_kind::check_expr_kind(
                    db,
                    expr,
                    super::expr_kind::ExprKind::IndexExpression,
                )?;
                match super::volatile::infer_over_relation(db, relid, expr, None) {
                    Some(Err(e)) => return Err(DdlError::UnsupportedDdl(e.to_string())),
                    Some(Ok(t)) => Some(t.type_oid),
                    None => None,
                }
            }
            (None, None) => None,
        };
        let (column, opr) = compute_key_column(
            db,
            &am,
            caps.as_ref(),
            elem,
            typ,
            attr.and_then(|a| a.attcollation),
            opnames,
        )?;
        columns.push(column);
        exclusion_oprs.push(opr);
    }
    if exclusion {
        check_exclusion_covers_partition_key(db, relid, conkey, &columns, &exclusion_oprs)?;
    }
    Ok((
        am,
        IndexKeys {
            columns,
            nulls_not_distinct: c.nulls_not_distinct,
        },
    ))
}

/// DefineIndex on a partitioned table, for an exclusion constraint: every
/// partition key column must be a key column, under the key's collation,
/// compared with the partition key's equality operator.
fn check_exclusion_covers_partition_key(
    db: &PgCatalog,
    relid: PgClassOid,
    conkey: &[i16],
    columns: &[IndexKeyColumn],
    oprs: &[Option<crate::oid::PgOperatorOid>],
) -> Result<(), DdlError> {
    let Some(part_key) = db.partition_keys.get(&relid) else {
        return Ok(());
    };
    if part_key.contains(&0) {
        return Err(DdlError::Parse(
            "unsupported EXCLUDE constraint with partition key definition".into(),
        ));
    }
    let collations = super::tables::partbound::partition_key_collations(db, relid);
    let (key_types, key_am) = super::tables::partbound::partition_key_types(db, relid);
    // BTEqualStrategyNumber / HTEqualStrategyNumber.
    let eq_strategy = if key_am == "hash" { 1 } else { 3 };
    let attname = |attnum: i16| {
        db.attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
            .unwrap_or_default()
    };
    for (i, pk) in part_key.iter().enumerate() {
        let ptkey_eqop = key_types.get(i).and_then(|&t| {
            let class = super::opclass::default_opclass_id(db, t, key_am)?;
            super::opclass::opfamily_member(db, class.get(db)?, eq_strategy)
        });
        let mut found = false;
        for (j, k) in conkey.iter().enumerate() {
            if k != pk || columns[j].collation != collations.get(i).copied().flatten() {
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
                "unique constraint on partitioned table must include all partitioning columns \
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

/// ATPostAlterTypeCleanup (tablecmds.c): after ALTER COLUMN TYPE, every
/// index over the column is rebuilt from its definition as
/// pg_get_indexdef prints it — an operator class only when it isn't the
/// old type's default, a COLLATE only when it isn't the column's — so its
/// operator classes (and exclusion operators) are resolved again for the
/// new type. `old_type` / `old_collation` are the column's before the
/// change, whose new type is already recorded.
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
    indexes.sort_by_key(|i| i.indexrelid);
    for index in indexes {
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
                let Some(expr) = exprs.next().and_then(|e| {
                    <typedpg_pg_query::protobuf::Node as Message>::decode(e.ast.as_slice()).ok()
                }) else {
                    continue;
                };
                let reads = expr.node.as_ref().is_some_and(|inner| {
                    inner.nodes().into_iter().any(|(n, ..)| {
                        matches!(n, typedpg_pg_query::NodeRef::ColumnRef(cr)
                            if cr.fields.last().and_then(super::util::node_string)
                                == Some(attr.attname.as_str()))
                    })
                });
                let Some(prior) = key.typ.filter(|_| reads) else {
                    continue;
                };
                match super::volatile::infer_over_relation(db, relid, &expr, None) {
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
            if key.collation.is_some() && key.collation != column_collation {
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
            } else if k == attnum {
                key.collation = attr.attcollation;
            }
            let explicit = key.opclass.clone().filter(|class| {
                super::opclass::default_opclass_id(db, prior_type, &am).as_ref() != Some(class)
            });
            let opclass = match explicit.as_ref().and_then(|c| c.get(db)) {
                Some(class) => {
                    super::opclass::check_opclass_accepts(db, class, new_type)?;
                    explicit
                }
                None => super::opclass::resolve_index_opclass(db, &[], new_type, &am)?,
            };
            if let Some(names) = key.exclusion_op.as_deref() {
                check_exclusion_operator(db, names, new_type, opclass.as_ref())?;
            }
            key.typ = Some(new_type);
            key.opclass = opclass;
            changed = true;
        }
        if changed {
            db.index_keys.insert(index.indexrelid, keys);
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
    stmt: &typedpg_pg_query::protobuf::DropStmt,
) -> Result<(), DdlError> {
    use typedpg_pg_query::protobuf::DropBehavior;
    if stmt.objects.len() != 1 {
        return Err(DdlError::UnsupportedDdl(
            "DROP INDEX CONCURRENTLY does not support dropping multiple objects".into(),
        ));
    }
    if stmt.behavior == DropBehavior::DropCascade as i32 {
        return Err(DdlError::UnsupportedDdl(
            "DROP INDEX CONCURRENTLY does not support CASCADE".into(),
        ));
    }
    let Some(node::Node::List(list)) = stmt.objects[0].node.as_ref() else {
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
    objtype: i32,
    relkind: Option<RelKind>,
    relname: &str,
) -> Result<(), DdlError> {
    use typedpg_pg_query::protobuf::ObjectType;
    let Some(relkind) = relkind else {
        return Ok(());
    };
    let expected = match ObjectType::try_from(objtype) {
        Ok(ObjectType::ObjectSequence) if relkind != RelKind::Sequence => "a sequence",
        Ok(ObjectType::ObjectView) if relkind != RelKind::View => "a view",
        Ok(ObjectType::ObjectMatview) if relkind != RelKind::MaterializedView => {
            "a materialized view"
        }
        Ok(ObjectType::ObjectForeignTable) if relkind != RelKind::ForeignTable => "a foreign table",
        Ok(ObjectType::ObjectIndex)
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
    exprs: &[&typedpg_pg_query::protobuf::Node],
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
        let Some(inner) = expr.node.as_ref() else {
            continue;
        };
        for (n, ..) in inner.nodes() {
            let typedpg_pg_query::NodeRef::ColumnRef(cr) = n else {
                continue;
            };
            let Some(name) = cr.fields.last().and_then(super::util::node_string) else {
                continue;
            };
            match db.attribute_by_name(relid, name) {
                Some(attr) => read.push(attr.attnum),
                None if crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, ..)| *n == name) =>
                {
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

/// `FigureIndexColname`: a function call is named after the function, a
/// column reference after the column (through casts), anything else `expr`.
pub(crate) fn figure_index_colname(expr: Option<&typedpg_pg_query::protobuf::Node>) -> String {
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

/// Encode a `typedpg_pg_query::Node` as a `SerializedAst` (protobuf bytes + an
/// empty bindings stream — index expressions don't yet flow through the
/// view-binding walker; that's a separate piece of work).
fn serialize_node(node: &typedpg_pg_query::protobuf::Node) -> SerializedAst {
    let mut buf = Vec::with_capacity(64);
    node.encode(&mut buf).ok();
    SerializedAst {
        ast: buf,
        bindings: Vec::<AstBinding>::new(),
    }
}
