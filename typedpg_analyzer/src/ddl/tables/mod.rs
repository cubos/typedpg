//! CREATE TABLE and ALTER TABLE DDL handlers.

use pg_query::protobuf::{
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
type PendingConstraint = (ConName, ConType, Vec<i16>, Option<PgClassOid>, Vec<i16>);

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

/// The name addition for a CHECK constraint (AddRelationNewConstraints):
/// the one column its expression reads, or nothing when it reads none or
/// several.
fn check_name_addition(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&pg_query::protobuf::Node>,
) -> String {
    let mut columns: Vec<String> = Vec::new();
    if let Some(inner) = expr.and_then(|e| e.node.as_ref()) {
        for (n, ..) in inner.nodes() {
            if let pg_query::NodeRef::ColumnRef(cr) = n
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
            if is_primary || is_not_null {
                col.not_null = true;
                col.nn_local = true;
            }
            if is_not_null && !c.conname.is_empty() {
                col.nn_name = Some(c.conname.clone());
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
    for (i, col) in columns.iter().enumerate() {
        interp.insert_pg_attribute(PgAttribute {
            attrelid: class_oid,
            attname: col.name.clone(),
            atttypid: col.type_oid,
            attnum: (i + 1) as i16,
            attnotnull: col.not_null,
            atthasdef: col.has_default,
            attgenerated: col.is_generated.then_some(AttGenerated::Stored),
            atttypmod: col.typmod,
            attidentity: col.identity,
            attcollation: col.collation,
            attislocal: col.is_local,
            attinhcount: col.inhcount,
        });
    }
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
    });
    register_composite_to_record_cast(interp, composite_oid)?;

    // Array type for the composite (`_<name>` in the same schema).
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
            continue;
        }
        if !col.has_default || col.is_generated {
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
            super::sequences::create_owned_sequence(interp, class_oid, (i + 1) as i16, deptype)?;
        }
    }
    for (i, &parent) in parents.iter().enumerate() {
        interp.pg_inherits.push(PgInherits {
            inhrelid: class_oid,
            inhparent: parent,
            inhseqno: (i + 1) as i32,
        });
    }

    // ComputePartitionAttrs: a partition key column must exist.
    if let Some(spec) = stmt.partspec.as_ref() {
        let mut key = Vec::new();
        for elem in &spec.part_params {
            let Some(node::Node::PartitionElem(pe)) = elem.node.as_ref() else {
                continue;
            };
            if pe.name.is_empty() {
                key.push(0);
                continue;
            }
            let Some(attnum) = interp
                .attribute_by_name(class_oid, &pe.name)
                .map(|a| a.attnum)
            else {
                return Err(DdlError::Parse(format!(
                    "column \"{}\" named in partition key does not exist",
                    pe.name
                )));
            };
            key.push(attnum);
        }
        interp.partition_keys.insert(class_oid, key);
    }

    // Type-check CHECK and `GENERATED ... STORED` expressions against the
    // freshly-built table. CHECK must produce `bool`; the generated
    // expression must be assignable to the column's declared type.
    validate_constraint_expressions(interp, class_oid, &name, stmt)?;

    // Emit pg_constraint rows so ON CONFLICT, DROP CASCADE, and FK
    // dependency checks can consult them later. FK validation runs here.
    emit_constraints(interp, class_oid, &name, stmt)?;
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
    stmt: &pg_query::protobuf::CreateForeignTableStmt,
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
    if let Some(rv) = base.relation.as_ref() {
        let (schema, name) = range_var_names(rv, interp);
        if let Some(oid) = interp
            .namespace_oid(&schema)
            .and_then(|ns| interp.class_by_qname.get(&(ns, name)).copied())
            && let Some(class) = interp.pg_class.get_mut(&oid)
        {
            class.relkind = RelKind::ForeignTable;
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
    is_generated: bool,
    identity: Option<AttIdentity>,
    collation: Option<crate::oid::PgCollationOid>,
    /// Implicit sequence the column owns: `Auto` for serial columns,
    /// `Internal` for identity columns.
    owned_sequence: Option<crate::pg_catalog::DepType>,
    /// The column has a local NOT NULL (explicit, PRIMARY KEY, serial,
    /// identity) — PG 18 records it as a local not-null constraint.
    nn_local: bool,
    /// Explicit name of that local constraint (`CONSTRAINT x NOT NULL`).
    nn_name: Option<String>,
    /// Parents contributing a not-null constraint, and the first one's name
    /// (the name an inherited-only constraint keeps).
    nn_inhcount: i16,
    nn_inh_name: Option<String>,
    /// `attislocal` / `attinhcount`.
    is_local: bool,
    inhcount: i16,
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
    let via_alter_type = pg_query::protobuf::ObjectType::try_from(stmt.objtype)
        == Ok(pg_query::protobuf::ObjectType::ObjectType);
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

    match subtype {
        AlterTableType::AtAddColumn | AlterTableType::AtAddColumnToView => {
            add_column(interp, relid, cmd, rec)
        }
        AlterTableType::AtDropColumn => drop_column(interp, relid, cmd, rec),
        AlterTableType::AtSetNotNull => inherit::set_not_null(interp, relid, &cmd.name, None, rec),
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
        AlterTableType::AtReplicaIdentity => object_refs::replica_identity(interp, relid, cmd),
        AlterTableType::AtAlterConstraint => object_refs::alter_constraint(interp, relid, cmd),
        AlterTableType::AtValidateConstraint => {
            object_refs::validate_constraint(interp, relid, cmd)
        }
        AlterTableType::AtEnableTrig
        | AlterTableType::AtEnableAlwaysTrig
        | AlterTableType::AtEnableReplicaTrig
        | AlterTableType::AtDisableTrig => object_refs::enable_disable_trigger(interp, relid, cmd),
        // Other subtypes are no-ops for schema analysis.
        _ => Ok(()),
    }
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
        At::AtDropConstraint => (table_like, "DROP CONSTRAINT"),
        _ => (true, ""),
    };
    if allowed {
        return Ok(());
    }
    let kinds = match class.relkind {
        RelKind::View => "views",
        RelKind::MaterializedView => "materialized views",
        RelKind::Sequence => "sequences",
        RelKind::Index | RelKind::PartitionedIndex => "indexes",
        RelKind::CompositeType => "composite types",
        _ => "this relation",
    };
    Err(DdlError::Parse(format!(
        "ALTER action {action} cannot be performed on relation \"{}\" \
         (This operation is not supported for {kinds}.)",
        class.relname
    )))
}

mod column_options;
mod columns;
mod constraints;
pub(crate) mod inherit;
mod merge;
mod object_refs;

use columns::*;
pub(crate) use columns::{column_collation, type_collation};
pub(crate) use constraints::check_unique_covers_partition_key;
use constraints::*;
