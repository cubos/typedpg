//! CREATE TABLE and ALTER TABLE DDL handlers.

use pg_query::protobuf::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, CreateStmt, DropBehavior, node,
};

use crate::oid::{PgClassOid, PgConstraintOid, PgTypeOid};
use crate::pg_catalog::{
    AttGenerated, AttIdentity, ConType, PgAttribute, PgClass, PgConstraint, PgIndex, PgInherits,
    PgType, RelKind, TypCategory, TypType,
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
type PendingConstraint = (String, ConType, Vec<i16>, Option<PgClassOid>, Vec<i16>);

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

    if interp.class_by_qname.contains_key(&(nsoid, name.clone())) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "relation \"{name}\" already exists"
        )));
    }

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

    // Key columns of table-level PRIMARY KEY / UNIQUE constraints must
    // exist (inherited and LIKE columns count); PRIMARY KEY marks them NOT
    // NULL (`transformIndexConstraint`).
    for elt in stmt.constraints.iter().chain(stmt.table_elts.iter()) {
        let Some(node::Node::Constraint(c)) = elt.node.as_ref() else {
            continue;
        };
        let is_primary = c.contype == ConstrType::ConstrPrimary as i32;
        if !is_primary && c.contype != ConstrType::ConstrUnique as i32 {
            continue;
        }
        for key in c.keys.iter().filter_map(super::util::node_string) {
            let Some(col) = columns.iter_mut().find(|col| col.name == key) else {
                return Err(DdlError::Parse(format!(
                    "column \"{key}\" named in key does not exist"
                )));
            };
            col.not_null |= is_primary;
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
    });

    for (i, &parent) in parents.iter().enumerate() {
        interp.pg_inherits.push(PgInherits {
            inhrelid: class_oid,
            inhparent: parent,
            inhseqno: (i + 1) as i32,
        });
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
}

// ─── ALTER TABLE ────────────────────────────────────────────────────────────

pub fn alter_table(interp: &mut PgCatalog, stmt: &AlterTableStmt) -> Result<(), DdlError> {
    let rv = stmt
        .relation
        .as_ref()
        .ok_or_else(|| DdlError::Parse("ALTER TABLE without relation".into()))?;

    let (schema, name) = range_var_names(rv, interp);
    let Some(nsoid) = interp.namespace_oid(&schema) else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TableNotFound(
            QualifiedName::new(schema, name).to_string(),
        ));
    };
    let class_oid = match interp.class_by_qname.get(&(nsoid, name.clone())).copied() {
        Some(oid) => oid,
        None => {
            if stmt.missing_ok {
                return Ok(());
            }
            return Err(DdlError::TableNotFound(
                QualifiedName::new(schema, name).to_string(),
            ));
        }
    };

    for cmd_node in &stmt.cmds {
        let Some(node::Node::AlterTableCmd(cmd)) = cmd_node.node.as_ref() else {
            continue;
        };
        apply_alter_cmd(interp, class_oid, cmd)?;
    }

    Ok(())
}

fn apply_alter_cmd(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    cmd: &AlterTableCmd,
) -> Result<(), DdlError> {
    let subtype = AlterTableType::try_from(cmd.subtype).unwrap_or(AlterTableType::Undefined);

    match subtype {
        AlterTableType::AtAddColumn | AlterTableType::AtAddColumnToView => {
            add_column(interp, relid, cmd)
        }
        AlterTableType::AtDropColumn => drop_column(interp, relid, cmd),
        AlterTableType::AtSetNotNull => set_not_null(interp, relid, &cmd.name, true),
        AlterTableType::AtDropNotNull => set_not_null(interp, relid, &cmd.name, false),
        AlterTableType::AtColumnDefault => set_default(interp, relid, cmd),
        AlterTableType::AtAlterColumnType => alter_column_type(interp, relid, cmd),
        AlterTableType::AtAddConstraint => add_constraint(interp, relid, cmd),
        AlterTableType::AtDropConstraint => drop_constraint(interp, relid, cmd),
        AlterTableType::AtAddIdentity => set_identity(interp, relid, cmd),
        AlterTableType::AtSetIdentity => set_identity(interp, relid, cmd),
        AlterTableType::AtDropIdentity => drop_identity(interp, relid, cmd),
        // Other subtypes are no-ops for schema analysis.
        _ => Ok(()),
    }
}

mod columns;
mod constraints;
mod merge;

use columns::*;
use constraints::*;
