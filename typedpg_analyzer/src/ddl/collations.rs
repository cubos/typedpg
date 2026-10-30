//! `CREATE COLLATION` handler.
//!
//! Two PG variants:
//! 1. `CREATE COLLATION name (LOCALE = '…' [, PROVIDER = 'libc'|'icu' …])` —
//!    a fresh collation built from locale parameters. We don't model the
//!    parameters; only the name + namespace round-trip.
//! 2. `CREATE COLLATION name FROM existing_collation` — clones an existing
//!    collation. We resolve the source name to confirm it exists; the new
//!    row inherits its encoding.
//!
//! Either way the analyzer just needs a valid `pg_collation` row so
//! subsequent `COLLATE "name"` references resolve.

use typedpg_pg_query::protobuf::{DefineStmt, node};

use super::DdlError;
use super::util::ensure_qualified_name;
use crate::oid::PgCollationOid;
use crate::pg_catalog::{PgCatalog, PgCollation};

pub fn define_collation(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    // PG: `collation "<name>" already exists`. Names are unique per schema.
    if interp
        .collation_by_qname
        .contains_key(&(nsoid, name.clone()))
    {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "collation \"{name}\" for encoding \"UTF8\" already exists"
        )));
    }

    // `CREATE COLLATION x FROM y` parses as a `DefElem { defname = "from",
    // arg = List([String("y")]) }`; we use it to resolve the source and
    // inherit its encoding. Option-style `(LOCALE = …, PROVIDER = …)` parses
    // as a list of DefElems we don't read today — collencoding stays `-1`.
    let mut collencoding: i32 = -1;
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if de.defname.eq_ignore_ascii_case("from")
            && let Some(arg) = de.arg.as_deref()
        {
            let parts: Vec<&str> = match arg.node.as_ref() {
                Some(node::Node::List(list)) => list
                    .items
                    .iter()
                    .filter_map(|n| match n.node.as_ref()? {
                        node::Node::String(s) => Some(s.sval.as_str()),
                        _ => None,
                    })
                    .collect(),
                Some(node::Node::String(s)) => vec![s.sval.as_str()],
                _ => Vec::new(),
            };
            let (schema, src_name) = match parts.as_slice() {
                [n] => (None, *n),
                [s, n] => (Some(*s), *n),
                _ => {
                    return Err(DdlError::Parse("malformed collation source name".into()));
                }
            };
            let source = interp.resolve_collation(schema, src_name).ok_or_else(|| {
                DdlError::DependencyError(format!(
                    "collation \"{src_name}\" for encoding \"UTF8\" does not exist"
                ))
            })?;
            collencoding = source.collencoding;
        }
    }

    let oid = PgCollationOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_collation(PgCollation {
        oid,
        collname: name,
        collnamespace: nsoid,
        collencoding,
    });
    Ok(())
}

/// DROP COLLATION name [CASCADE | RESTRICT] (`RemoveObjects` →
/// `performDeletion`).
pub(crate) fn drop_collation(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::List(list)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let names: Vec<&str> = list
        .items
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    let (schema, name) = match names.as_slice() {
        [n] => (None, *n),
        [s, n] => (Some(*s), *n),
        _ => return Ok(()),
    };
    // get_collation_oid: a missing schema is reported as such
    // (LookupExplicitNamespace) — skipped under IF EXISTS.
    if let Some(s) = schema
        && interp.namespace_oid(s).is_none()
    {
        if missing_ok {
            return Ok(());
        }
        return Err(DdlError::DependencyError(format!(
            "schema \"{s}\" does not exist"
        )));
    }
    let Some(oid) = interp.resolve_collation(schema, name).map(|c| c.oid) else {
        if missing_ok {
            return Ok(());
        }
        // NameListToString: the name as written, joined, unquoted.
        return Err(DdlError::DependencyError(format!(
            "collation \"{}\" for encoding \"UTF8\" does not exist",
            names.join(".")
        )));
    };
    drop_collation_oid(interp, oid, cascade)
}

/// Drop collation `oid`: what depends on it needs CASCADE, and goes with
/// it. A built-in collation is required by the database system.
pub(crate) fn drop_collation_oid(
    interp: &mut PgCatalog,
    oid: PgCollationOid,
    cascade: bool,
) -> Result<(), DdlError> {
    use super::depend::{self, ObjectAddress};
    if !interp.pg_collation.contains_key(&oid) {
        return Ok(());
    }
    let addr = ObjectAddress::collation(oid);
    let desc = depend::describe(interp, addr);
    depend::check_not_owned(interp, addr, &desc)?;
    let implied = implied_dependents(interp, oid);
    if !cascade && let Some(first) = implied.first() {
        let dependent = match *first {
            Implied::Column(relid, attnum) => {
                depend::describe(interp, ObjectAddress::column(relid, attnum))
            }
            Implied::Object(a) => depend::describe(interp, a),
        };
        return Err(DdlError::DependencyError(format!(
            "cannot drop {desc} because other objects depend on it ({dependent} depends on \
             {desc})"
        )));
    }
    // What records a dependency on it: an expression's COLLATE, a
    // `regcollation` constant, a range type's collation.
    depend::drop_dependents(interp, addr, &desc, cascade)?;
    // The rest goes along (CASCADE): whole objects first — a partitioned
    // table takes its partitions' columns with it — then the columns left.
    for dependent in &implied {
        if let Implied::Object(a) = *dependent
            && depend::object_exists(interp, a)
        {
            depend::cascade_delete(interp, a)?;
        }
    }
    for dependent in implied {
        let Implied::Column(relid, attnum) = dependent else {
            continue;
        };
        let Some(attname) = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum && a.attcollation == Some(oid))
            .map(|a| a.attname.clone())
        else {
            continue;
        };
        let cmd = typedpg_pg_query::protobuf::AlterTableCmd {
            subtype: typedpg_pg_query::protobuf::AlterTableType::AtDropColumn as i32,
            name: attname,
            behavior: typedpg_pg_query::protobuf::DropBehavior::DropCascade as i32,
            missing_ok: true,
            ..Default::default()
        };
        // Each inheritance child's copy depends on the collation too: it
        // goes as a dependent, not as an inherited column.
        super::tables::drop_column(
            interp,
            relid,
            &cmd,
            super::tables::inherit::Recursion {
                recurse: true,
                recursing: true,
            },
        )?;
    }
    interp.remove_pg_collation(oid);
    let obj = crate::oid::PgGenericOid::from_nonzero(oid.into_nonzero());
    interp.remove_dependencies_of(crate::pg_catalog::PG_COLLATION_RELID, obj);
    interp.remove_dependencies_on(crate::pg_catalog::PG_COLLATION_RELID, obj);
    Ok(())
}

/// A dependency on a collation PG records but the analyzer reads off the
/// catalog rows instead of `pg_depend`.
#[derive(Clone, Copy)]
enum Implied {
    /// A table's or composite type's column of the collation
    /// (AddNewAttributeTuples). Kept apart from `pg_depend`, where a
    /// column's address stands for its default expression.
    Column(crate::oid::PgClassOid, i16),
    /// A domain of the collation (GenerateTypeDependencies), an index
    /// with a key column of it (index_create), a partitioned table whose
    /// key uses it (StorePartitionKey).
    Object(super::depend::ObjectAddress),
}

/// What depends on collation `oid` beyond the `pg_depend` rows, in OID
/// order.
fn implied_dependents(interp: &PgCatalog, oid: PgCollationOid) -> Vec<Implied> {
    use super::depend::ObjectAddress;
    use crate::pg_catalog::{RelKind, TypType};
    let mut objects: Vec<ObjectAddress> = interp
        .pg_type
        .values()
        .filter(|t| t.typtype == TypType::Domain && t.typcollation == Some(oid))
        .map(|t| ObjectAddress::type_(t.oid))
        .chain(
            interp
                .pg_index
                .values()
                .filter(|i| i.indcollation.contains(&Some(oid)))
                .map(|i| ObjectAddress::relation(i.indexrelid)),
        )
        .chain(
            interp
                .pg_class
                .keys()
                .filter(|relid| {
                    super::tables::partbound::partition_key_collations(interp, **relid)
                        .contains(&Some(oid))
                })
                .map(|relid| ObjectAddress::relation(*relid)),
        )
        .collect();
    objects.sort();
    let mut columns: Vec<(crate::oid::PgClassOid, i16)> = interp
        .pg_attribute
        .iter()
        .filter(|(relid, _)| {
            interp.pg_class.get(relid).is_some_and(|c| {
                matches!(
                    c.relkind,
                    RelKind::Table
                        | RelKind::Partitioned
                        | RelKind::ForeignTable
                        | RelKind::CompositeType
                )
            })
        })
        .flat_map(|(relid, attrs)| {
            attrs
                .iter()
                .filter(|a| a.attnum > 0 && a.attcollation == Some(oid))
                .map(move |a| (*relid, a.attnum))
        })
        .collect();
    columns.sort();
    columns
        .into_iter()
        .map(|(relid, attnum)| Implied::Column(relid, attnum))
        .chain(objects.into_iter().map(Implied::Object))
        .collect()
}
