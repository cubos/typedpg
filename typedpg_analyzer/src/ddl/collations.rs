//! `CREATE COLLATION` handler.
//!
//! Two PG variants:
//! 1. `CREATE COLLATION name (LOCALE = '…' [, PROVIDER = 'libc'|'icu' …])` —
//!    a fresh collation built from locale parameters. The parameters are
//!    checked as DefineCollation checks them, but for the locales a libc or
//!    ICU provider would look up on the server.
//! 2. `CREATE COLLATION name FROM existing_collation` — clones an existing
//!    collation. We resolve the source name to confirm it exists; the new
//!    row inherits its encoding.
//!
//! Either way the analyzer just needs a valid `pg_collation` row so
//! subsequent `COLLATE "name"` references resolve. `ALTER COLLATION`
//! renames and moves it, `DROP COLLATION` drops it.

use typedpg_pg_query::ast::{self, DefineStmt};

use super::DdlError;
use super::util::ensure_qualified_name;
use crate::oid::PgCollationOid;
use crate::pg_catalog::{PgCatalog, PgCollation};

/// `pg_collation.collencoding` of a collation for the database encoding
/// (UTF8, `PG_UTF8`); `-1` is one for any encoding.
const DATABASE_ENCODING: i32 = 6;

pub fn define_collation(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    let (collencoding, deterministic) = collation_parameters(interp, &stmt.definition)?;

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
        collisdeterministic: deterministic,
    });
    Ok(())
}

/// DefineCollation's reading of the attributes: the new collation's
/// `(collencoding, collisdeterministic)`. `FROM y` copies the source's; a
/// libc collation is for the database encoding, an ICU one for any, a
/// builtin one for any (`C`) or UTF8 (`builtin_locale_encoding`). Locale
/// names a libc or ICU provider would check against the server's locales
/// are taken to exist.
fn collation_parameters(
    interp: &PgCatalog,
    definition: &[Option<ast::Node>],
) -> Result<(i32, bool), DdlError> {
    const KNOWN: [&str; 8] = [
        "from",
        "locale",
        "lc_collate",
        "lc_ctype",
        "provider",
        "deterministic",
        "rules",
        "version",
    ];
    // Each attribute once; LOCALE apart from LC_COLLATE / LC_CTYPE; FROM
    // alone.
    let mut given: Vec<&ast::DefElem> = Vec::new();
    for opt in definition {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        if !KNOWN.contains(&de.defname.as_str()) {
            return Err(DdlError::Parse(format!(
                "collation attribute \"{}\" not recognized",
                de.defname
            )));
        }
        if given.iter().any(|g| g.defname == de.defname) {
            return Err(DdlError::Parse("conflicting or redundant options".into()));
        }
        given.push(de);
    }
    let get = |name: &str| given.iter().copied().find(|de| de.defname == name);
    if get("locale").is_some() && (get("lc_collate").is_some() || get("lc_ctype").is_some()) {
        return Err(DdlError::Parse(
            "conflicting or redundant options (LOCALE cannot be specified together with \
             LC_COLLATE or LC_CTYPE.)"
                .into(),
        ));
    }
    if get("from").is_some() && given.len() != 1 {
        return Err(DdlError::Parse(
            "conflicting or redundant options (FROM cannot be specified together with any \
             other options.)"
                .into(),
        ));
    }
    if let Some(de) = get("from") {
        // `CREATE COLLATION x FROM y` parses as a `DefElem { defname =
        // "from", arg = List([String("y")]) }`.
        let parts: Vec<&str> = match de.arg.as_ref() {
            Some(ast::Node::List(list)) => list
                .items
                .iter()
                .filter_map(|n| match n.as_ref()? {
                    ast::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                })
                .collect(),
            Some(ast::Node::String(s)) => vec![s.sval.as_str()],
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
        // The database's default collation (provider `d`) has nothing to
        // copy.
        if source.oid.get() == DEFAULT_COLLATION_OID {
            return Err(DdlError::Parse(
                "collation \"default\" cannot be copied".into(),
            ));
        }
        return Ok((source.collencoding, source.collisdeterministic));
    }
    // defGetString: a value is required (a bare word arrives as a type
    // name).
    let string = |de: &ast::DefElem| -> Result<String, DdlError> {
        match de.arg.as_ref() {
            Some(ast::Node::String(s)) => Ok(s.sval.clone()),
            Some(ast::Node::Integer(i)) => Ok(i.ival.to_string()),
            Some(ast::Node::Float(f)) => Ok(f.fval.clone()),
            Some(ast::Node::Boolean(b)) => Ok(b.boolval.to_string()),
            Some(ast::Node::TypeName(tn)) => Ok(tn
                .names
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()))
                .collect::<Vec<_>>()
                .join(".")),
            _ => Err(DdlError::Parse(format!(
                "{} requires a parameter",
                de.defname
            ))),
        }
    };
    let provider = get("provider").map(string).transpose()?;
    let deterministic = get("deterministic")
        .map(super::session::def_get_boolean)
        .transpose()?
        .unwrap_or(true);
    let rules = get("rules").map(string).transpose()?;
    if let Some(de) = get("version") {
        string(de)?;
    }
    #[derive(PartialEq)]
    enum Provider {
        Builtin,
        Icu,
        Libc,
    }
    let provider = match provider.as_deref() {
        None => Provider::Libc,
        Some(p) if p.eq_ignore_ascii_case("builtin") => Provider::Builtin,
        Some(p) if p.eq_ignore_ascii_case("icu") => Provider::Icu,
        Some(p) if p.eq_ignore_ascii_case("libc") => Provider::Libc,
        Some(p) => {
            return Err(DdlError::Parse(format!(
                "unrecognized collation provider: {p}"
            )));
        }
    };
    let locale = get("locale").map(string).transpose()?;
    let lc_collate = get("lc_collate").map(string).transpose()?;
    let lc_ctype = get("lc_ctype").map(string).transpose()?;
    let missing = |name: &str| DdlError::Parse(format!("parameter \"{name}\" must be specified"));
    let encoding = match provider {
        Provider::Builtin => {
            // builtin_validate_locale, then builtin_locale_encoding.
            let locale = locale.ok_or_else(|| missing("locale"))?;
            match locale.as_str() {
                "C" => -1,
                "C.UTF-8" | "C.UTF8" | "PG_UNICODE_FAST" => DATABASE_ENCODING,
                _ => {
                    return Err(DdlError::Parse(format!(
                        "invalid locale name \"{locale}\" for builtin provider"
                    )));
                }
            }
        }
        Provider::Libc => {
            if locale.is_none() {
                lc_collate.ok_or_else(|| missing("lc_collate"))?;
                lc_ctype.ok_or_else(|| missing("lc_ctype"))?;
            }
            DATABASE_ENCODING
        }
        Provider::Icu => {
            locale.ok_or_else(|| missing("locale"))?;
            -1
        }
    };
    if !deterministic && provider != Provider::Icu {
        return Err(DdlError::UnsupportedDdl(
            "nondeterministic collations not supported with this provider".into(),
        ));
    }
    if rules.is_some() && provider != Provider::Icu {
        return Err(DdlError::UnsupportedDdl(
            "ICU rules cannot be specified unless locale provider is ICU".into(),
        ));
    }
    Ok((encoding, deterministic))
}

/// A `COLLATE` clause's collation (get_collation_oid with missing_ok =
/// false): `[catalog.][schema.]name` — the catalog taken to be the current
/// database —, a missing schema reported as such (LookupExplicitNamespace),
/// then a missing collation named as written (NameListToString).
pub(crate) fn collation_of_clause(
    interp: &PgCatalog,
    names: &[Option<ast::Node>],
) -> Result<PgCollationOid, DdlError> {
    let parts: Vec<&str> = names
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    collation_of_parts(interp, &parts)
}

/// [`collation_of_clause`] for a name already split into its parts.
pub(crate) fn collation_of_parts(
    interp: &PgCatalog,
    parts: &[&str],
) -> Result<PgCollationOid, DdlError> {
    let (schema, name) = match parts {
        [n] => (None, *n),
        [s, n] | [_, s, n] => (Some(*s), *n),
        _ => {
            return Err(DdlError::Parse(format!(
                "improper qualified name (too many dotted names): {}",
                parts.join(".")
            )));
        }
    };
    if let Some(s) = schema
        && interp.namespace_oid(s).is_none()
    {
        return Err(DdlError::TableNotFound(format!(
            "schema \"{s}\" does not exist"
        )));
    }
    interp
        .resolve_collation(schema, name)
        .map(|c| c.oid)
        .ok_or_else(|| {
            DdlError::TypeNotFound(format!(
                "collation \"{}\" for encoding \"UTF8\" does not exist",
                parts.join(".")
            ))
        })
}

/// The database's default collation, `pg_collation.oid` 100.
const DEFAULT_COLLATION_OID: u32 = 100;

/// The collation a statement names (get_collation_oid): a missing schema
/// is reported as such (LookupExplicitNamespace), then a missing
/// collation. `Ok(None)` when it's missing and `missing_ok`.
pub(crate) fn lookup_collation(
    interp: &PgCatalog,
    obj_node: Option<&typedpg_pg_query::ast::Node>,
    missing_ok: bool,
) -> Result<Option<PgCollationOid>, DdlError> {
    let Some(ast::Node::List(list)) = obj_node else {
        return Ok(None);
    };
    let names: Vec<&str> = list
        .items
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
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
    stmt: &typedpg_pg_query::ast::RenameStmt,
) -> Result<(), DdlError> {
    let Some(oid) = lookup_collation(interp, stmt.object.as_ref(), stmt.missing_ok)? else {
        return Ok(());
    };
    let Some(mut row) = interp.pg_collation.get(&oid).cloned() else {
        return Ok(());
    };
    check_collation_name_free(interp, &stmt.newname, row.collnamespace)?;
    super::stored_exprs::follow_rename(
        interp,
        super::stored_exprs::Renamed::Collation {
            oid,
            schema: None,
            name: &stmt.newname,
        },
    );
    interp.remove_pg_collation(oid);
    row.collname.clone_from(&stmt.newname);
    interp.insert_pg_collation(row);
    super::policies::rewrite_collation_refs(interp, oid, None, &stmt.newname);
    Ok(())
}

/// `ALTER COLLATION name SET SCHEMA s` (AlterObjectNamespace_internal):
/// the collation is looked up before the schema; moving it where it
/// already is does nothing.
pub(crate) fn set_collation_schema(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterObjectSchemaStmt,
) -> Result<(), DdlError> {
    let Some(oid) = lookup_collation(interp, stmt.object.as_ref(), stmt.missing_ok)? else {
        return Ok(());
    };
    let Some(mut row) = interp.pg_collation.get(&oid).cloned() else {
        return Ok(());
    };
    let Some(new_nsoid) =
        super::util::set_schema_target(interp, row.collnamespace, &stmt.newschema)?
    else {
        return Ok(());
    };
    check_collation_name_free(interp, &row.collname, new_nsoid)?;
    let name = row.collname.clone();
    super::stored_exprs::follow_rename(
        interp,
        super::stored_exprs::Renamed::Collation {
            oid,
            schema: Some(&stmt.newschema),
            name: &name,
        },
    );
    interp.remove_pg_collation(oid);
    row.collnamespace = new_nsoid;
    interp.insert_pg_collation(row);
    super::policies::rewrite_collation_refs(interp, oid, Some(&stmt.newschema), &name);
    Ok(())
}

/// `ALTER COLLATION name REFRESH VERSION` (AlterCollation): the version
/// isn't modelled; the collation must exist.
pub(crate) fn refresh_version(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterCollationStmt,
) -> Result<(), DdlError> {
    let list = Some(typedpg_pg_query::ast::Node::List(
        typedpg_pg_query::ast::List {
            items: stmt.collname.clone(),
        }
        .into(),
    ));
    lookup_collation(interp, list.as_ref(), false).map(|_| ())
}

/// DROP COLLATION name [CASCADE | RESTRICT] (`RemoveObjects` →
/// `performDeletion`).
pub(crate) fn drop_collation(
    interp: &mut PgCatalog,
    obj_node: Option<&typedpg_pg_query::ast::Node>,
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
        let cmd = typedpg_pg_query::ast::AlterTableCmd {
            subtype: typedpg_pg_query::ast::AlterTableType::AtDropColumn,
            name: attname,
            behavior: typedpg_pg_query::ast::DropBehavior::DropCascade,
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
