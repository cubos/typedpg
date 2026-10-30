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
//! subsequent `COLLATE "name"` references resolve. `ALTER COLLATION`
//! renames and moves it, `DROP COLLATION` drops it.

use typedpg_pg_query::protobuf::{DefineStmt, node};

use super::DdlError;
use super::util::ensure_qualified_name;
use crate::oid::PgCollationOid;
use crate::pg_catalog::{PgCatalog, PgCollation};

/// `pg_collation.collencoding` of a collation for the database encoding
/// (UTF8, `PG_UTF8`); `-1` is one for any encoding.
const DATABASE_ENCODING: i32 = 6;

pub fn define_collation(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    // `CREATE COLLATION x FROM y` parses as a `DefElem { defname = "from",
    // arg = List([String("y")]) }`; we use it to resolve the source and
    // inherit its encoding. Option-style `(LOCALE = …, PROVIDER = …)`:
    // a libc collation is for the database encoding, an ICU or builtin
    // one for any (DefineCollation).
    let mut collencoding: i32 = DATABASE_ENCODING;
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if de.defname.eq_ignore_ascii_case("provider") {
            // defGetString: a bare word arrives as a type name.
            let provider = match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
                Some(node::Node::String(s)) => Some(s.sval.as_str()),
                Some(node::Node::TypeName(tn)) => {
                    tn.names.last().and_then(super::util::node_string)
                }
                _ => None,
            };
            if provider.is_some_and(|p| !p.eq_ignore_ascii_case("libc")) {
                collencoding = -1;
            }
        }
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

    // CollationCreate: names are unique per schema — under the database
    // encoding, a name for any encoding clashes with one for this one.
    if let Some(existing) = interp
        .collation_by_qname
        .get(&(nsoid, name.clone()))
        .and_then(|oid| interp.pg_collation.get(oid))
    {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(
            if collencoding == DATABASE_ENCODING && existing.collencoding == DATABASE_ENCODING {
                format!("collation \"{name}\" for encoding \"UTF8\" already exists")
            } else {
                format!("collation \"{name}\" already exists")
            },
        ));
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

/// The collation a statement names (get_collation_oid): a missing schema
/// is reported as such (LookupExplicitNamespace), then a missing
/// collation. `Ok(None)` when it's missing and `missing_ok`.
pub(crate) fn lookup_collation(
    interp: &PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<Option<PgCollationOid>, DdlError> {
    let Some(node::Node::List(list)) = obj_node.node.as_ref() else {
        return Ok(None);
    };
    let names: Vec<&str> = list
        .items
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    let (schema, name) = match names.as_slice() {
        [n] => (None, *n),
        [s, n] => (Some(*s), *n),
        _ => return Ok(None),
    };
    if let Some(s) = schema
        && interp.namespace_oid(s).is_none()
    {
        if missing_ok {
            return Ok(None);
        }
        return Err(DdlError::TableNotFound(format!(
            "schema \"{s}\" does not exist"
        )));
    }
    match interp.resolve_collation(schema, name) {
        Some(c) => Ok(Some(c.oid)),
        None if missing_ok => Ok(None),
        // NameListToString: the name as written, joined, unquoted.
        None => Err(DdlError::TypeNotFound(format!(
            "collation \"{}\" for encoding \"UTF8\" does not exist",
            names.join(".")
        ))),
    }
}

/// IsThereCollationInNamespace: no collation `name` usable under the
/// database encoding (one of it, or one for any encoding) may be in schema
/// `nsoid` already.
fn check_collation_name_free(
    interp: &PgCatalog,
    name: &str,
    nsoid: crate::oid::PgNamespaceOid,
) -> Result<(), DdlError> {
    let Some(existing) = interp
        .collation_by_qname
        .get(&(nsoid, name.to_owned()))
        .and_then(|oid| interp.pg_collation.get(oid))
    else {
        return Ok(());
    };
    let schema = interp.namespace_name(nsoid).unwrap_or_default();
    Err(DdlError::DuplicateObject(
        if existing.collencoding == DATABASE_ENCODING {
            format!(
                "collation \"{name}\" for encoding \"UTF8\" already exists in schema \"{schema}\""
            )
        } else {
            format!("collation \"{name}\" already exists in schema \"{schema}\"")
        },
    ))
}

/// `ALTER COLLATION name RENAME TO new` (AlterObjectRename_internal).
pub(crate) fn rename_collation(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(obj) = stmt.object.as_deref() else {
        return Ok(());
    };
    let Some(oid) = lookup_collation(interp, obj, stmt.missing_ok)? else {
        return Ok(());
    };
    let Some(mut row) = interp.pg_collation.get(&oid).cloned() else {
        return Ok(());
    };
    check_collation_name_free(interp, &stmt.newname, row.collnamespace)?;
    interp.remove_pg_collation(oid);
    row.collname.clone_from(&stmt.newname);
    interp.insert_pg_collation(row);
    Ok(())
}

/// `ALTER COLLATION name SET SCHEMA s` (AlterObjectNamespace_internal):
/// the collation is looked up before the schema; moving it where it
/// already is does nothing.
pub(crate) fn set_collation_schema(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterObjectSchemaStmt,
) -> Result<(), DdlError> {
    let Some(obj) = stmt.object.as_deref() else {
        return Ok(());
    };
    let Some(oid) = lookup_collation(interp, obj, stmt.missing_ok)? else {
        return Ok(());
    };
    let Some(mut row) = interp.pg_collation.get(&oid).cloned() else {
        return Ok(());
    };
    // LookupCreationNamespace: `pg_temp` is the session's temporary
    // schema, which CheckSetNamespace refuses.
    if stmt.newschema == "pg_temp" {
        return Err(DdlError::Parse(
            "cannot move objects into or out of temporary schemas".into(),
        ));
    }
    let new_nsoid = super::util::existing_namespace(interp, &stmt.newschema)?;
    if new_nsoid == row.collnamespace {
        return Ok(());
    }
    // CheckSetNamespace.
    if interp.temp_namespace == Some(new_nsoid) || interp.temp_namespace == Some(row.collnamespace)
    {
        return Err(DdlError::Parse(
            "cannot move objects into or out of temporary schemas".into(),
        ));
    }
    let toast = interp.namespace_oid("pg_toast");
    if toast.is_some() && (toast == Some(new_nsoid) || toast == Some(row.collnamespace)) {
        return Err(DdlError::Parse(
            "cannot move objects into or out of TOAST schema".into(),
        ));
    }
    check_collation_name_free(interp, &row.collname, new_nsoid)?;
    interp.remove_pg_collation(oid);
    row.collnamespace = new_nsoid;
    interp.insert_pg_collation(row);
    Ok(())
}

/// `ALTER COLLATION name REFRESH VERSION` (AlterCollation): the version
/// isn't modelled; the collation must exist.
pub(crate) fn refresh_version(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterCollationStmt,
) -> Result<(), DdlError> {
    let list = typedpg_pg_query::protobuf::Node {
        node: Some(node::Node::List(typedpg_pg_query::protobuf::List {
            items: stmt.collname.clone(),
        })),
    };
    lookup_collation(interp, &list, false).map(|_| ())
}

/// DROP COLLATION name [CASCADE | RESTRICT] (`RemoveObjects` →
/// `performDeletion`).
pub(crate) fn drop_collation(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    match lookup_collation(interp, obj_node, missing_ok)? {
        Some(oid) => drop_collation_oid(interp, oid, cascade),
        None => Ok(()),
    }
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
