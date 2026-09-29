//! CREATE TABLE and ALTER TABLE DDL handlers.

use typedpg_pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, CreateStmt, DropBehavior, node,
};

use crate::oid::{PgClassOid, PgConstraintOid, PgTypeOid};
use crate::pg_catalog::{
    AttGenerated, AttIdentity, ConType, PgAttribute, PgClass, PgConstraint, PgIndex, PgInherits,
    PgType, RelKind, TypCategory, TypStorage, TypType,
};

use super::DdlError;
use super::util::{
    ensure_range_var, format_type_for_message, lookup_type_name, range_var_names,
    register_composite_to_record_cast,
};
use super::views;
use crate::pg_catalog::PgCatalog;
use crate::qualified_name::QualifiedName;

/// Pending `pg_constraint` row built up while walking a `CreateStmt`:
/// `(conname, contype, conkey, confrelid, confkey)`. Materialized into
/// real catalog rows after all FK targets have been validated.
/// CHECK constraints also carry their definition and whether they are
/// ENFORCED; then come DEFERRABLE, the INCLUDE columns, and whether a key
/// is WITHOUT OVERLAPS.
type PendingConstraint = (
    ConName,
    ConType,
    Vec<i16>,
    Option<PgClassOid>,
    Vec<i16>,
    Option<(check_inherit::CheckDef, bool)>,
    bool,
    Vec<i16>,
    bool,
);

/// A constraint's name: the explicit one, or PG's generated
/// `<table>[_<addition>]_<label>` — unique among the schema's relations for
/// an index-backed constraint (ChooseRelationName), among its constraints
/// otherwise (ChooseConstraintName).
#[derive(Clone, Debug)]
enum ConName {
    Explicit(String),
    Relation {
        addition: String,
        label: &'static str,
    },
    Constraint {
        addition: String,
        label: &'static str,
    },
}

impl ConName {
    fn from_explicit(conname: &str, default: ConName) -> ConName {
        if conname.is_empty() {
            default
        } else {
            ConName::Explicit(conname.to_owned())
        }
    }

    fn is_explicit(&self) -> bool {
        matches!(self, ConName::Explicit(_))
    }

    fn resolve(&self, interp: &PgCatalog, relid: PgClassOid) -> String {
        match self {
            ConName::Explicit(name) => name.clone(),
            ConName::Relation { addition, label } => {
                let nsoid = interp.pg_class.get(&relid).map(|c| c.relnamespace);
                let relname = relname_of(interp, relid);
                match nsoid {
                    Some(ns) => {
                        super::util::choose_relation_name(interp, ns, &relname, addition, label)
                    }
                    None => super::util::make_object_name(&relname, addition, label),
                }
            }
            ConName::Constraint { addition, label } => {
                inherit::choose_constraint_name(interp, relid, addition, label)
            }
        }
    }
}

/// A CHECK constraint's `conkey`: the columns its expression reads, in
/// attnum order (StoreRelCheck / pull_varattnos). DROP COLUMN drops the
/// constraints whose key contains the column.
fn check_conkey(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&typedpg_pg_query::protobuf::Node>,
) -> Vec<i16> {
    let mut attnums: Vec<i16> = Vec::new();
    if let Some(inner) = expr.and_then(|e| e.node.as_ref()) {
        for (n, ..) in inner.nodes() {
            if let typedpg_pg_query::NodeRef::ColumnRef(cr) = n
                && let Some(name) = cr.fields.last().and_then(super::util::node_string)
                && let Some(attr) = interp.attribute_by_name(relid, name)
                && !attnums.contains(&attr.attnum)
            {
                attnums.push(attr.attnum);
            }
        }
    }
    attnums.sort_unstable();
    attnums
}

/// The name addition for a CHECK constraint (AddRelationNewConstraints):
/// the one column its expression reads, or nothing when it reads none or
/// several.
fn check_name_addition(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&typedpg_pg_query::protobuf::Node>,
) -> String {
    let mut columns: Vec<String> = Vec::new();
    if let Some(inner) = expr.and_then(|e| e.node.as_ref()) {
        for (n, ..) in inner.nodes() {
            if let typedpg_pg_query::NodeRef::ColumnRef(cr) = n
                && let Some(name) = cr.fields.last().and_then(super::util::node_string)
                && interp.attribute_by_name(relid, name).is_some()
                && !columns.iter().any(|c| c == name)
            {
                columns.push(name.to_owned());
            }
        }
    }
    match columns.as_slice() {
        [one] => one.clone(),
        _ => String::new(),
    }
}

/// Look up the `pg_class.relname` for `relid`. Used to produce PG-aligned
/// error messages of the form `column "X" of relation "T" does not exist` —
/// the analyzer's `pglite_sanity` cross-check requires this exact prefix.
fn relname_of(interp: &PgCatalog, relid: PgClassOid) -> String {
    interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_else(|| format!("oid={relid}"))
}

/// Build a PG-shaped `column "X" of relation "T" does not exist` message.
fn column_not_found_msg(interp: &PgCatalog, relid: PgClassOid, col: &str) -> String {
    let rel = relname_of(interp, relid);
    format!("column \"{col}\" of relation \"{rel}\" does not exist")
}

/// Build a PG-shaped `column "X" of relation "T" already exists` message.
fn column_exists_msg(interp: &PgCatalog, relid: PgClassOid, col: &str) -> String {
    let rel = relname_of(interp, relid);
    format!("column \"{col}\" of relation \"{rel}\" already exists")
}

// ─── CREATE TABLE ───────────────────────────────────────────────────────────

pub fn create_table(interp: &mut PgCatalog, stmt: &CreateStmt) -> Result<(), DdlError> {
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("CREATE TABLE without relation".into()))?;

    let (nsoid, name) = ensure_range_var(interp, rv)?;

    if interp.class_by_qname.contains_key(&(nsoid, name.clone())) && stmt.if_not_exists {
        return Ok(());
    }
    // transformCreateStmt.
    if stmt.partspec.is_some() && rv.relpersistence == "u" {
        return Err(DdlError::UnsupportedDdl(
            "partitioned tables cannot be unlogged".into(),
        ));
    }
    if stmt.partspec.is_some() && !stmt.inh_relations.is_empty() && stmt.partbound.is_none() {
        return Err(DdlError::Parse(
            "cannot create partitioned table as inheritance child".into(),
        ));
    }
    super::util::check_relation_name_free(interp, nsoid, &name)?;

    let mut pk_columns: Vec<String> = Vec::new();

    // First pass: extract table-level PRIMARY KEY constraint keys, and
    // validate any table-level CHECK constraint expressions for volatility.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        // PG does *not* enforce volatility on CHECK constraints at DDL
        // time — it only warns at runtime if the CHECK turns out to be
        // mutable. Indexes and GENERATED expressions are still checked
        // (further down). Skip the volatility walk for CHECK to stay
        // aligned with PG, otherwise the analyzer would reject DDL that
        // PG happily accepts.
        if let Some(node::Node::Constraint(c)) = elt.node.as_ref()
            && c.contype == ConstrType::ConstrPrimary as i32
        {
            for key_node in &c.keys {
                if let Some(node::Node::String(s)) = key_node.node.as_ref() {
                    pk_columns.push(s.sval.clone());
                }
            }
        }
    }

    // Assemble the column list: OF type, LIKE, INHERITS / PARTITION OF.
    let merge::AssembledColumns {
        mut columns,
        parents,
        likes,
    } = merge::assemble_columns(interp, stmt, &pk_columns)?;

    // transformIndexConstraints: one PRIMARY KEY at most.
    let column_pks = stmt
        .table_elts
        .iter()
        .filter_map(|e| match e.node.as_ref()? {
            node::Node::ColumnDef(cd) => Some(cd),
            _ => None,
        })
        .flat_map(|cd| cd.constraints.iter())
        .filter(|n| {
            matches!(n.node.as_ref(), Some(node::Node::Constraint(c))
                if c.contype == ConstrType::ConstrPrimary as i32)
        })
        .count();
    let table_pks = stmt
        .constraints
        .iter()
        .chain(stmt.table_elts.iter())
        .filter(|n| {
            matches!(n.node.as_ref(), Some(node::Node::Constraint(c))
                if c.contype == ConstrType::ConstrPrimary as i32)
        })
        .count();
    if column_pks + table_pks > 1 {
        return Err(DdlError::Parse(format!(
            "multiple primary keys for table \"{name}\" are not allowed"
        )));
    }

    // Key columns of table-level PRIMARY KEY / UNIQUE constraints must
    // exist (inherited and LIKE columns count); PRIMARY KEY marks them NOT
    // NULL (`transformIndexConstraint`).
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        let Some(node::Node::Constraint(c)) = elt.node.as_ref() else {
            continue;
        };
        let is_primary = c.contype == ConstrType::ConstrPrimary as i32;
        let is_not_null = c.contype == ConstrType::ConstrNotnull as i32;
        // transformTableConstraint.
        if is_not_null && c.is_no_inherit && stmt.partspec.is_some() {
            return Err(DdlError::UnsupportedDdl(
                "not-null constraints on partitioned tables cannot be NO INHERIT".into(),
            ));
        }
        if !is_primary && !is_not_null && c.contype != ConstrType::ConstrUnique as i32 {
            continue;
        }
        for key in c.keys.iter().filter_map(super::util::node_string) {
            let Some(col) = columns.iter_mut().find(|col| col.name == key) else {
                return Err(DdlError::Parse(if is_not_null {
                    format!("column \"{key}\" of relation \"{name}\" does not exist")
                } else {
                    format!("column \"{key}\" named in key does not exist")
                }));
            };
            // AddRelationNotNullConstraints: a column's not-null
            // specifications (column-level first, then table-level) merge
            // into one constraint; their names and NO INHERIT flags must
            // agree.
            if is_not_null && col.nn_local {
                if col.nn_no_inherit != c.is_no_inherit {
                    return Err(DdlError::Parse(format!(
                        "conflicting NO INHERIT declaration for not-null constraint on column \
                         \"{key}\""
                    )));
                }
                if let Some(existing) = col.nn_name.as_ref()
                    && !c.conname.is_empty()
                    && *existing != c.conname
                {
                    return Err(DdlError::Parse(format!(
                        "conflicting not-null constraint names \"{existing}\" and \"{}\"",
                        c.conname
                    )));
                }
            }
            if is_not_null {
                col.nn_no_inherit = c.is_no_inherit;
                if !c.conname.is_empty() {
                    col.nn_name = Some(c.conname.clone());
                }
            }
            if is_primary || is_not_null {
                col.not_null = true;
                col.nn_local = true;
            }
        }
    }
    // Two columns' not-null constraints may not share a given name.
    let mut given_names: Vec<&str> = Vec::new();
    for col in &columns {
        if let Some(n) = col.nn_name.as_deref() {
            if given_names.contains(&n) {
                return Err(DdlError::DuplicateObject(format!(
                    "constraint \"{n}\" for relation \"{name}\" already exists"
                )));
            }
            given_names.push(n);
        }
    }

    // transformIndexConstraint: so must their INCLUDE columns.
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        let Some(node::Node::Constraint(c)) = elt.node.as_ref() else {
            continue;
        };
        for key in c.including.iter().filter_map(super::util::node_string) {
            if !columns.iter().any(|col| col.name == key) {
                return Err(DdlError::Parse(format!(
                    "column \"{key}\" named in key does not exist"
                )));
            }
        }
    }

    // Allocate OIDs for the relation row, its composite type, and the array
    // type wrapping the composite.
    let class_oid = PgClassOid::from_nonzero(interp.alloc_oid()?);
    let composite_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);
    let array_oid = PgTypeOid::from_nonzero(interp.alloc_oid()?);

    interp.insert_pg_class(PgClass {
        oid: class_oid,
        relname: name.clone(),
        relnamespace: nsoid,
        relkind: if stmt.partspec.is_some() {
            RelKind::Partitioned
        } else {
            RelKind::Table
        },
        reltype: Some(composite_oid),
    });
    if let Some(tn) = stmt.of_typename.as_ref() {
        let of_type = lookup_type_name(tn, interp)?;
        typed::set_of_type(interp, class_oid, of_type);
    }
    if Some(nsoid) == interp.temp_namespace {
        interp.relpersistence.insert(class_oid, 't');
    } else if rv.relpersistence == "u" {
        interp.relpersistence.insert(class_oid, 'u');
    }
    // ON COMMIT applies to temporary tables only (transformCreateStmt).
    use typedpg_pg_query::protobuf::OnCommitAction;
    match OnCommitAction::try_from(stmt.oncommit) {
        Ok(OnCommitAction::OncommitNoop | OnCommitAction::Undefined) | Err(_) => {}
        Ok(action) => {
            if Some(nsoid) != interp.temp_namespace {
                return Err(DdlError::UnsupportedDdl(
                    "ON COMMIT can only be used on temporary tables".into(),
                ));
            }
            if action == OnCommitAction::OncommitDrop {
                interp.on_commit_drop.push(class_oid);
            }
        }
    }
    // CheckAttributeNamesTypes (heap_create_with_catalog).
    for col in &columns {
        if col.generated == Some(AttGenerated::Virtual) {
            generated::check_virtual_column_type(interp, &col.name, col.type_oid)?;
        }
    }
    for (i, col) in columns.iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: col.name.clone(),
            atttypid: col.type_oid,
            attnum: (i + 1) as i16,
            attnotnull: col.not_null,
            atthasdef: col.has_default,
            attgenerated: col.generated,
            atttypmod: col.typmod,
            attidentity: col.identity,
            attcollation: col.collation,
            attislocal: col.is_local,
            attinhcount: col.inhcount,
        });
    }
    let phys = crate::ddl::types::TypePhysical::COMPOSITE;
    interp.insert_pg_type(PgType {
        oid: composite_oid,
        typname: name.clone(),
        typnamespace: nsoid,
        typtype: TypType::Composite,
        typcategory: TypCategory::Composite,
        typispreferred: false,
        typrelid: Some(class_oid),
        typelem: None,
        typarray: Some(array_oid),
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });
    register_composite_to_record_cast(interp, composite_oid)?;

    // Array type for the composite (`_<name>` in the same schema).
    let phys = crate::ddl::types::TypePhysical::array(interp, composite_oid);
    interp.insert_pg_type(PgType {
        oid: array_oid,
        typname: format!("_{name}"),
        typnamespace: nsoid,
        typtype: TypType::Base,
        typcategory: TypCategory::Array,
        typispreferred: false,
        typrelid: None,
        typelem: Some(composite_oid),
        typarray: None,
        typbasetype: None,
        typnotnull: false,
        typtypmod: None,
        typcollation: None,
        typstorage: TypStorage::Extended,
        typlen: phys.typlen,
        typbyval: phys.typbyval,
        typalign: phys.typalign,
        typsubscript: phys.typsubscript,
        typisdefined: true,
    });

    for (i, col) in columns.iter().enumerate() {
        if col.not_null {
            inherit::record_not_null(interp, class_oid, (i + 1) as i16, col)?;
        }
    }
    // Default expressions carried over from parents / LIKE sources (a
    // local DEFAULT, recorded while validating below, overrides them), and
    // serial's `nextval(...)`, a bigint.
    for (i, col) in columns.iter().enumerate() {
        let attnum = (i + 1) as i16;
        if col.owned_sequence == Some(crate::pg_catalog::DepType::Auto) {
            interp
                .attr_default_types
                .insert((class_oid, attnum), crate::pg_catalog::oid::INT8);
            interp.attr_default_exprs.insert(
                (class_oid, attnum),
                columns::serial_default_text(class_oid, attnum),
            );
            continue;
        }
        // A generation expression written here is cooked below.
        if !col.has_default || (col.generated.is_some() && col.local_default) {
            continue;
        }
        let inherited = parents
            .iter()
            .chain(likes.iter().map(|l| &l.source))
            .find_map(|&src| {
                let src_attnum = interp.attribute_by_name(src, &col.name)?.attnum;
                let default_type = interp.attr_default_types.get(&(src, src_attnum)).copied()?;
                Some((src, src_attnum, default_type))
            });
        if let Some((src, src_attnum, default_type)) = inherited {
            interp
                .attr_default_types
                .insert((class_oid, attnum), default_type);
            if let Some(text) = interp.attr_default_exprs.get(&(src, src_attnum)).cloned() {
                interp.attr_default_exprs.insert((class_oid, attnum), text);
            }
            // An inherited generation expression reads the same-named
            // columns here (map_variable_attnos).
            if let Some(refs) = interp.generated_refs.get(&(src, src_attnum)).cloned() {
                let mapped: Vec<i16> = refs
                    .iter()
                    .filter_map(|&r| {
                        let name = interp
                            .attributes_of(src)
                            .iter()
                            .find(|a| a.attnum == r)?
                            .attname
                            .clone();
                        interp.attribute_by_name(class_oid, &name).map(|a| a.attnum)
                    })
                    .collect();
                interp.generated_refs.insert((class_oid, attnum), mapped);
            }
            // The copied default expression names the same sequences.
            let src_obj = crate::oid::PgGenericOid::from_nonzero(src.into_nonzero());
            let sequences: Vec<PgClassOid> = interp
                .iter_pg_depend()
                .filter(|d| {
                    d.classid == crate::pg_catalog::PG_CLASS_RELID
                        && d.objid == src_obj
                        && d.objsubid == src_attnum
                        && d.deptype == crate::pg_catalog::DepType::Normal
                })
                .filter_map(|d| PgClassOid::new(d.refobjid.get()))
                .collect();
            for seq in sequences {
                super::defaults::record_default_sequence(interp, class_oid, attnum, seq);
            }
        }
    }
    for (i, col) in columns.iter().enumerate() {
        if let Some(deptype) = col.owned_sequence {
            super::sequences::create_owned_sequence(
                interp,
                class_oid,
                (i + 1) as i16,
                deptype,
                &col.identity_options,
            )?;
        }
    }
    for (i, &parent) in parents.iter().enumerate() {
        interp.pg_inherits.push(PgInherits {
            inhrelid: class_oid,
            inhparent: parent,
            inhseqno: (i + 1) as i32,
        });
    }
    if let (Some(bound), Some(&parent)) = (stmt.partbound.as_ref(), parents.first()) {
        partbound::add_partition_bound(interp, parent, class_oid, bound)?;
    }

    // transformPartitionSpec / ComputePartitionAttrs: the key columns and
    // expressions, and their operator classes.
    if let Some(spec) = stmt.partspec.as_ref() {
        use typedpg_pg_query::protobuf::PartitionStrategy;
        let strategy = PartitionStrategy::try_from(spec.strategy).ok();
        if strategy == Some(PartitionStrategy::List) && spec.part_params.len() > 1 {
            return Err(DdlError::Parse(
                "cannot use \"list\" partition strategy with more than one column".into(),
            ));
        }
        let am = if strategy == Some(PartitionStrategy::Hash) {
            "hash"
        } else {
            "btree"
        };
        let mut key = Vec::new();
        // Every column the key reads (has_partition_attrs).
        let mut key_attrs: Vec<i16> = Vec::new();
        for elem in &spec.part_params {
            let Some(node::Node::PartitionElem(pe)) = elem.node.as_ref() else {
                continue;
            };
            let key_type = if pe.name.is_empty() {
                key.push(0);
                let Some(expr) = pe.expr.as_deref() else {
                    continue;
                };
                crate::ddl::expr_kind::check_expr_kind(
                    interp,
                    expr,
                    crate::ddl::expr_kind::ExprKind::PartitionExpression,
                )?;
                let typ = match crate::ddl::volatile::infer_over_relation(
                    interp, class_oid, expr, None,
                ) {
                    Some(Err(e)) => return Err(DdlError::UnsupportedDdl(e.to_string())),
                    Some(Ok(t)) => Some(t.type_oid),
                    None => None,
                };
                // ComputePartitionAttrs: the columns the expression reads —
                // all of them through a whole-row reference — may be neither
                // system nor generated columns.
                let mut read: Vec<i16> = Vec::new();
                if let Some(inner) = expr.node.as_ref() {
                    for (n, ..) in inner.nodes() {
                        let typedpg_pg_query::NodeRef::ColumnRef(cr) = n else {
                            continue;
                        };
                        let whole_row = match cr.fields.as_slice() {
                            [.., last]
                                if matches!(last.node.as_ref(), Some(node::Node::AStar(_))) =>
                            {
                                true
                            }
                            [only] => super::util::node_string(only).is_some_and(|f| {
                                f == name && interp.attribute_by_name(class_oid, f).is_none()
                            }),
                            _ => false,
                        };
                        if whole_row {
                            read.extend(interp.attributes_of(class_oid).iter().map(|a| a.attnum));
                            continue;
                        }
                        let Some(col) = cr.fields.last().and_then(super::util::node_string) else {
                            continue;
                        };
                        match interp.attribute_by_name(class_oid, col) {
                            Some(attr) => read.push(attr.attnum),
                            None if crate::pg_catalog::SYSTEM_COLUMNS
                                .iter()
                                .any(|(n, ..)| *n == col) =>
                            {
                                return Err(DdlError::Parse(
                                    "partition key expressions cannot contain system column \
                                     references"
                                        .into(),
                                ));
                            }
                            None => {}
                        }
                    }
                }
                read.sort_unstable();
                key_attrs.extend(read.iter().copied());
                if let Some(generated) = read.iter().find_map(|&attnum| {
                    interp
                        .attributes_of(class_oid)
                        .iter()
                        .find(|a| a.attnum == attnum && a.attgenerated.is_some())
                }) {
                    return Err(DdlError::Parse(format!(
                        "cannot use generated column in partition key (Column \"{}\" is a \
                         generated column.)",
                        generated.attname
                    )));
                }
                let reads_columns = expr.node.as_ref().is_some_and(|inner| {
                    inner
                        .nodes()
                        .into_iter()
                        .any(|(n, ..)| matches!(n, typedpg_pg_query::NodeRef::ColumnRef(_)))
                });
                if !reads_columns {
                    return Err(DdlError::Parse(
                        "cannot use constant expression as partition key".into(),
                    ));
                }
                crate::ddl::volatile::check_no_volatile(
                    expr,
                    crate::ddl::volatile::ExprLocation::PartitionKey,
                    interp,
                )?;
                crate::ddl::volatile::check_mutability(
                    interp,
                    class_oid,
                    expr,
                    crate::ddl::volatile::ExprLocation::PartitionKey,
                )?;
                typ
            } else {
                if crate::pg_catalog::SYSTEM_COLUMNS
                    .iter()
                    .any(|(n, ..)| *n == pe.name)
                {
                    return Err(DdlError::Parse(format!(
                        "cannot use system column \"{}\" in partition key",
                        pe.name
                    )));
                }
                let Some(attr) = interp.attribute_by_name(class_oid, &pe.name).cloned() else {
                    return Err(DdlError::Parse(format!(
                        "column \"{}\" named in partition key does not exist",
                        pe.name
                    )));
                };
                if attr.attgenerated.is_some() {
                    return Err(DdlError::Parse(format!(
                        "cannot use generated column in partition key (Column \"{}\" is a \
                         generated column.)",
                        pe.name
                    )));
                }
                key.push(attr.attnum);
                key_attrs.push(attr.attnum);
                Some(attr.atttypid)
            };
            if let Some(typ) = key_type {
                crate::ddl::opclass::resolve_index_opclass(interp, &pe.opclass, typ, am)?;
            }
        }
        interp.partition_keys.insert(class_oid, key);
        key_attrs.sort_unstable();
        key_attrs.dedup();
        interp.partition_key_attrs.insert(class_oid, key_attrs);
        partbound::record_partition_spec(interp, class_oid, spec);
    }

    // DefineRelation: the table access method.
    if !stmt.access_method.is_empty() {
        crate::ddl::opclass::check_table_am(interp, &stmt.access_method)?;
    }
    // heap_reloptions / partitioned_table_reloptions.
    crate::ddl::reloptions::check_reloptions(
        &stmt.options,
        if stmt.partspec.is_some() {
            crate::ddl::reloptions::RelOptKind::Partitioned
        } else {
            crate::ddl::reloptions::RelOptKind::Heap
        },
        false,
        true,
    )?;

    // Type-check CHECK and `GENERATED ... STORED` expressions against the
    // freshly-built table. CHECK must produce `bool`; the generated
    // expression must be assignable to the column's declared type.
    validate_constraint_expressions(interp, class_oid, &name, stmt)?;

    // CloneForeignKeyConstraints: a partition gets its parent's foreign
    // keys before its own constraints are added.
    if stmt.partbound.is_some()
        && let Some(&parent) = parents.first()
    {
        foreign_keys::clone_parent_fks(interp, parent, class_oid)?;
    }
    // Emit pg_constraint rows so ON CONFLICT, DROP CASCADE, and FK
    // dependency checks can consult them later. FK validation runs here.
    emit_constraints(interp, class_oid, &name, stmt)?;
    check_inherit::inherit_parent_checks(interp, class_oid)?;
    if stmt.partbound.is_some()
        && let Some(parent) = interp
            .pg_inherits
            .iter()
            .find(|i| i.inhrelid == class_oid)
            .map(|i| i.inhparent)
    {
        partidx::clone_parent_indexes(interp, parent, class_oid)?;
    }
    for like in &likes {
        copy_like_constraints(interp, class_oid, &name, like)?;
    }

    Ok(())
}

/// `CREATE FOREIGN TABLE name (...) SERVER s`: a relation like a table
/// (columns, NOT NULL, defaults, inheritance) with relkind 'f'. The rows live
/// elsewhere; the server and options don't affect typing.
pub fn create_foreign_table(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::CreateForeignTableStmt,
) -> Result<(), DdlError> {
    let Some(base) = stmt.base_stmt.as_ref() else {
        return Ok(());
    };
    let existed = base.relation.as_ref().and_then(|rv| {
        let (schema, name) = range_var_names(rv, interp);
        interp
            .namespace_oid(&schema)
            .and_then(|ns| interp.class_by_qname.get(&(ns, name)).copied())
    });
    create_table(interp, base)?;
    if existed.is_some() {
        return Ok(());
    }
    // CreateForeignTable: GetForeignServerByName.
    crate::ddl::fdw::check_server(interp, &stmt.servername)?;
    if let Some(rv) = base.relation.as_ref() {
        let (schema, name) = range_var_names(rv, interp);
        if let Some(oid) = interp
            .namespace_oid(&schema)
            .and_then(|ns| interp.class_by_qname.get(&(ns, name)).copied())
            && let Some(class) = interp.pg_class.get_mut(&oid)
        {
            class.relkind = RelKind::ForeignTable;
            interp
                .foreign_data
                .table_servers
                .insert(oid, stmt.servername.clone());
        }
    }
    Ok(())
}

/// Parsed column definition shared between `CREATE TABLE` and `ALTER TABLE`.
#[derive(Clone)]
struct ParsedColumn {
    name: String,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    not_null: bool,
    has_default: bool,
    /// `GENERATED ALWAYS AS (expr) {STORED | VIRTUAL}` (`attgenerated`).
    generated: Option<AttGenerated>,
    identity: Option<AttIdentity>,
    collation: Option<crate::oid::PgCollationOid>,
    /// Implicit sequence the column owns: `Auto` for serial columns,
    /// `Internal` for identity columns.
    owned_sequence: Option<crate::pg_catalog::DepType>,
    /// The identity's sequence options (`GENERATED ... AS IDENTITY (...)`).
    identity_options: Vec<typedpg_pg_query::protobuf::Node>,
    /// The column has a local NOT NULL (explicit, PRIMARY KEY, serial,
    /// identity) — PG 18 records it as a local not-null constraint.
    nn_local: bool,
    /// Explicit name of that local constraint (`CONSTRAINT x NOT NULL`).
    nn_name: Option<String>,
    /// That local constraint is `NO INHERIT` (`connoinherit`).
    nn_no_inherit: bool,
    /// Parents contributing a not-null constraint, and the first one's name
    /// (the name an inherited-only constraint keeps).
    nn_inhcount: i16,
    nn_inh_name: Option<String>,
    /// `attislocal` / `attinhcount`.
    is_local: bool,
    inhcount: i16,
    /// The definition writes a DEFAULT or generation expression of its own
    /// (`raw_default`), which overrides an inherited one.
    local_default: bool,
    /// The canonical text of the default / generation expression inherited
    /// from the parents, and whether two parents disagree on it
    /// (MergeAttributes' `bogus_marker`).
    inherited_default: Option<String>,
    bogus_default: bool,
}

// ─── ALTER TABLE ────────────────────────────────────────────────────────────

pub fn alter_table(interp: &mut PgCatalog, stmt: &AlterTableStmt) -> Result<(), DdlError> {
    let recurse = stmt.relation.as_ref().is_none_or(|rv| rv.inh);
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("ALTER TABLE without relation".into()))?;

    let class_oid = match super::util::lookup_relation(interp, rv) {
        Ok((_, oid)) => oid,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    // RangeVarCallbackForAlterRelation: ALTER TABLE doesn't reach a
    // composite type, and ALTER TYPE only reaches one.
    let relkind = interp.pg_class.get(&class_oid).map(|c| c.relkind);
    let via_alter_type = typedpg_pg_query::protobuf::ObjectType::try_from(stmt.objtype)
        == Ok(typedpg_pg_query::protobuf::ObjectType::ObjectType);
    if !via_alter_type && relkind == Some(RelKind::CompositeType) {
        return Err(DdlError::Parse(format!(
            "\"{}\" is a composite type",
            rv.relname
        )));
    }
    if via_alter_type && relkind != Some(RelKind::CompositeType) {
        return Err(DdlError::Parse(format!(
            "\"{}\" is not a composite type",
            rv.relname
        )));
    }

    for cmd_node in &stmt.cmds {
        let Some(node::Node::AlterTableCmd(cmd)) = cmd_node.node.as_ref() else {
            continue;
        };
        let rec = inherit::Recursion {
            recurse,
            recursing: false,
        };
        apply_alter_cmd(interp, class_oid, cmd, rec)?;
    }

    Ok(())
}

fn apply_alter_cmd(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
) -> Result<(), DdlError> {
    let subtype = AlterTableType::try_from(cmd.subtype).unwrap_or(AlterTableType::Undefined);
    check_alter_target(interp, relid, subtype)?;
    let mut typed_dependents = Vec::new();
    if !rec.recursing {
        typed::check_typed_table_cmd(interp, relid, subtype)?;
        // ATTypedTableRecursion: ALTER TYPE of a composite reaches its
        // typed tables only with CASCADE.
        if matches!(
            subtype,
            AlterTableType::AtAddColumn
                | AlterTableType::AtDropColumn
                | AlterTableType::AtAlterColumnType
        ) {
            let cascade = cmd.behavior == DropBehavior::DropCascade as i32;
            typed_dependents = typed::typed_table_dependents(interp, relid, cascade)?;
        }
    }
    apply_alter_subtype(interp, relid, cmd, rec, subtype)?;
    for table in typed_dependents {
        let rec = inherit::Recursion {
            recurse: true,
            recursing: true,
        };
        apply_alter_subtype(interp, table, cmd, rec, subtype)?;
    }
    Ok(())
}

fn apply_alter_subtype(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    rec: inherit::Recursion,
    subtype: AlterTableType,
) -> Result<(), DdlError> {
    match subtype {
        AlterTableType::AtAddColumn | AlterTableType::AtAddColumnToView => {
            add_column(interp, relid, cmd, rec)
        }
        AlterTableType::AtDropColumn => drop_column(interp, relid, cmd, rec),
        AlterTableType::AtSetNotNull => inherit::set_not_null(interp, relid, &cmd.name, rec),
        AlterTableType::AtDropNotNull => inherit::drop_not_null(interp, relid, &cmd.name, rec),
        AlterTableType::AtColumnDefault => set_default(interp, relid, cmd, rec),
        AlterTableType::AtAlterColumnType => alter_column_type(interp, relid, cmd, rec),
        AlterTableType::AtAddConstraint => add_constraint(interp, relid, cmd, rec),
        AlterTableType::AtDropConstraint => drop_constraint(interp, relid, cmd, rec),
        AlterTableType::AtAddIdentity => set_identity(interp, relid, cmd),
        AlterTableType::AtSetIdentity => set_identity(interp, relid, cmd),
        AlterTableType::AtDropIdentity => drop_identity(interp, relid, cmd),
        AlterTableType::AtDropExpression => drop_expression(interp, relid, cmd, rec),
        AlterTableType::AtSetExpression => set_expression(interp, relid, cmd, rec),
        AlterTableType::AtSetStatistics
        | AlterTableType::AtSetStorage
        | AlterTableType::AtSetCompression
        | AlterTableType::AtSetOptions
        | AlterTableType::AtResetOptions => {
            column_options::alter_column_setting(interp, relid, cmd, subtype)
        }
        AlterTableType::AtClusterOn => object_refs::cluster_on(interp, relid, cmd),
        AlterTableType::AtSetLogged | AlterTableType::AtSetUnLogged => {
            set_persistence(interp, relid, subtype == AlterTableType::AtSetLogged)
        }
        AlterTableType::AtSetAccessMethod if !cmd.name.is_empty() => {
            crate::ddl::opclass::check_table_am(interp, &cmd.name)
        }
        AlterTableType::AtSetRelOptions
        | AlterTableType::AtResetRelOptions
        | AlterTableType::AtReplaceRelOptions => set_reloptions(interp, relid, cmd, subtype),
        AlterTableType::AtDropCluster => {
            interp.clustered_indexes.remove(&relid);
            Ok(())
        }
        AlterTableType::AtAddOf => typed::add_of(interp, relid, cmd),
        AlterTableType::AtAddInherit => inherit_cmd::add_inherit(interp, relid, cmd),
        AlterTableType::AtDropInherit => inherit_cmd::drop_inherit(interp, relid, cmd),
        AlterTableType::AtAttachPartition => inherit_cmd::attach_partition(interp, relid, cmd),
        AlterTableType::AtDetachPartition => inherit_cmd::detach_partition(interp, relid, cmd),
        AlterTableType::AtDropOf => typed::drop_of(interp, relid),
        AlterTableType::AtReplicaIdentity => object_refs::replica_identity(interp, relid, cmd),
        AlterTableType::AtAlterConstraint => object_refs::alter_constraint(interp, relid, cmd, rec),
        AlterTableType::AtValidateConstraint => {
            object_refs::validate_constraint(interp, relid, cmd, rec)
        }
        AlterTableType::AtEnableRule
        | AlterTableType::AtEnableAlwaysRule
        | AlterTableType::AtEnableReplicaRule
        | AlterTableType::AtDisableRule => {
            crate::ddl::rules::check_rule_exists(interp, relid, &cmd.name)
        }
        AlterTableType::AtEnableTrig
        | AlterTableType::AtEnableAlwaysTrig
        | AlterTableType::AtEnableReplicaTrig
        | AlterTableType::AtDisableTrig => object_refs::enable_disable_trigger(interp, relid, cmd),
        // Other subtypes are no-ops for schema analysis.
        _ => Ok(()),
    }
}

/// `ALTER TABLE ... SET LOGGED / UNLOGGED` (ATPrepChangePersistence): a
/// logged table may not reference an unlogged one, and a table referenced
/// by a logged one can't become unlogged.
fn set_persistence(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    to_logged: bool,
) -> Result<(), DdlError> {
    let relname = relname_of(interp, relid);
    for con in interp.pg_constraint.values() {
        if con.contype != ConType::ForeignKey {
            continue;
        }
        let other = if to_logged {
            (con.conrelid == relid).then_some(con.confrelid).flatten()
        } else {
            (con.confrelid == Some(relid)).then_some(con.conrelid)
        };
        let Some(other) = other.filter(|o| *o != relid) else {
            continue;
        };
        let other_permanent = constraints::persistence(interp, other) == 'p';
        if to_logged && !other_permanent {
            return Err(DdlError::UnsupportedDdl(format!(
                "could not change table \"{relname}\" to logged because it references unlogged \
                 table \"{}\"",
                relname_of(interp, other)
            )));
        }
        if !to_logged && other_permanent {
            return Err(DdlError::UnsupportedDdl(format!(
                "could not change table \"{relname}\" to unlogged because it references logged \
                 table \"{}\"",
                relname_of(interp, other)
            )));
        }
    }
    if to_logged {
        interp.relpersistence.remove(&relid);
    } else {
        interp.relpersistence.insert(relid, 'u');
    }
    Ok(())
}

/// `ALTER ... SET / RESET (storage parameters)` (ATExecSetRelOptions): the
/// options of the relation's kind.
fn set_reloptions(
    interp: &PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
    subtype: AlterTableType,
) -> Result<(), DdlError> {
    use crate::ddl::reloptions::{IndexAm, RelOptKind};
    let Some(node::Node::List(list)) = cmd.def.as_deref().and_then(|d| d.node.as_ref()) else {
        return Ok(());
    };
    let kind = match interp.pg_class.get(&relid).map(|c| c.relkind) {
        Some(RelKind::Table | RelKind::MaterializedView) => RelOptKind::Heap,
        Some(RelKind::Partitioned) => RelOptKind::Partitioned,
        Some(RelKind::View) => RelOptKind::View,
        Some(RelKind::Index | RelKind::PartitionedIndex) => {
            let am = interp
                .index_access_methods
                .get(&relid)
                .map_or("btree", String::as_str);
            match IndexAm::from_name(am) {
                Some(am) => RelOptKind::Index(am),
                None => return Ok(()),
            }
        }
        _ => return Ok(()),
    };
    crate::ddl::reloptions::check_reloptions(
        &list.items,
        kind,
        subtype == AlterTableType::AtResetRelOptions,
        false,
    )?;
    if kind == RelOptKind::View
        && subtype != AlterTableType::AtResetRelOptions
        && crate::ddl::views::sets_check_option(&list.items)
        && let Some(query) = crate::ddl::views::view_query(interp, relid)
    {
        crate::ddl::views::check_option_allowed(interp, &query)?;
    }
    Ok(())
}

/// `ATSimplePermissions` (tablecmds.c): which relation kinds each ALTER
/// TABLE subcommand applies to.
fn check_alter_target(
    interp: &PgCatalog,
    relid: PgClassOid,
    subtype: AlterTableType,
) -> Result<(), DdlError> {
    use AlterTableType as At;
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let table_like = matches!(
        class.relkind,
        RelKind::Table | RelKind::Partitioned | RelKind::ForeignTable
    );
    let (allowed, action) = match subtype {
        At::AtAddColumn => (
            table_like || class.relkind == RelKind::CompositeType,
            "ADD COLUMN",
        ),
        At::AtDropColumn => (
            table_like || class.relkind == RelKind::CompositeType,
            "DROP COLUMN",
        ),
        At::AtAlterColumnType => (
            table_like || class.relkind == RelKind::CompositeType,
            "ALTER COLUMN ... SET DATA TYPE",
        ),
        At::AtColumnDefault => (
            table_like || class.relkind == RelKind::View,
            "ALTER COLUMN ... SET DEFAULT",
        ),
        At::AtSetNotNull => (table_like, "ALTER COLUMN ... SET NOT NULL"),
        At::AtDropNotNull => (table_like, "ALTER COLUMN ... DROP NOT NULL"),
        At::AtSetExpression => (table_like, "ALTER COLUMN ... SET EXPRESSION"),
        At::AtDropExpression => (table_like, "ALTER COLUMN ... DROP EXPRESSION"),
        At::AtSetStatistics => (
            table_like
                || matches!(
                    class.relkind,
                    RelKind::MaterializedView | RelKind::Index | RelKind::PartitionedIndex
                ),
            "ALTER COLUMN ... SET STATISTICS",
        ),
        At::AtSetOptions => (
            table_like || class.relkind == RelKind::MaterializedView,
            "ALTER COLUMN ... SET",
        ),
        At::AtResetOptions => (
            table_like || class.relkind == RelKind::MaterializedView,
            "ALTER COLUMN ... RESET",
        ),
        At::AtSetStorage => (
            table_like || class.relkind == RelKind::MaterializedView,
            "ALTER COLUMN ... SET STORAGE",
        ),
        At::AtSetCompression => (
            matches!(
                class.relkind,
                RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
            ),
            "ALTER COLUMN ... SET COMPRESSION",
        ),
        At::AtAddConstraint => (table_like, "ADD CONSTRAINT"),
        At::AtAddOf => (class.relkind == RelKind::Table, "OF"),
        At::AtSetAccessMethod => (
            matches!(
                class.relkind,
                RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
            ),
            "SET ACCESS METHOD",
        ),
        At::AtAddInherit => (table_like, "INHERIT"),
        At::AtAttachPartition => (class.relkind == RelKind::Partitioned, "ATTACH PARTITION"),
        At::AtDetachPartition => (class.relkind == RelKind::Partitioned, "DETACH PARTITION"),
        At::AtDropInherit => (table_like, "NO INHERIT"),
        At::AtDropOf => (class.relkind == RelKind::Table, "NOT OF"),
        At::AtDropConstraint => (table_like, "DROP CONSTRAINT"),
        At::AtSetLogged => (
            matches!(class.relkind, RelKind::Table | RelKind::Sequence),
            "SET LOGGED",
        ),
        At::AtSetUnLogged => (
            matches!(class.relkind, RelKind::Table | RelKind::Sequence),
            "SET UNLOGGED",
        ),
        _ => (true, ""),
    };
    if allowed {
        return Ok(());
    }
    let kinds = match class.relkind {
        RelKind::Table => "tables",
        RelKind::ForeignTable => "foreign tables",
        RelKind::View => "views",
        RelKind::MaterializedView => "materialized views",
        RelKind::Sequence => "sequences",
        RelKind::Index => "indexes",
        RelKind::PartitionedIndex => "partitioned indexes",
        RelKind::Partitioned => "partitioned tables",
        RelKind::CompositeType => "composite types",
        _ => "this relation",
    };
    Err(DdlError::Parse(format!(
        "ALTER action {action} cannot be performed on relation \"{}\" \
         (This operation is not supported for {kinds}.)",
        class.relname
    )))
}

pub(crate) mod check_inherit;
mod column_options;
mod columns;
mod constraints;
pub(crate) mod foreign_keys;
mod generated;
pub(crate) mod inherit;
mod inherit_cmd;
mod merge;
mod object_refs;
pub(crate) use object_refs::check_clusterable_index;
pub(crate) mod partbound;
pub(crate) mod partidx;
pub(crate) mod typed;

use columns::*;
pub(crate) use columns::{column_collation, type_collation};
pub(crate) use constraints::check_unique_covers_partition_key;
use constraints::*;
