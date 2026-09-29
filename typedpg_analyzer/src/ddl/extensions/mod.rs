//! CREATE EXTENSION / ALTER EXTENSION handlers with version tracking.
//!
//! Each extension is declared as a registry entry with a default version,
//! a base install script, and optional upgrade scripts. The interpreter
//! tracks which extensions are installed at which version, enabling
//! ALTER EXTENSION UPDATE to apply the correct upgrade chain.
//!
//! To add support for a new extension:
//! 1. Add `.sql` files to `typedpg_analyzer/src/extensions/`
//! 2. Add an `ExtensionDef` entry to the `REGISTRY` array below
//! 3. Run `./update_extensions.sh` to fetch from PG upstream

use typedpg_pg_query::protobuf::{AlterExtensionStmt, CreateExtensionStmt};

use super::DdlError;
use super::util::ensure_namespace;
use crate::oid::{PgCastOid, PgClassOid, PgExtensionOid, PgGenericOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{
    DepType, PG_CAST_RELID, PG_EXTENSION_RELID, PG_PROC_RELID, PG_TYPE_RELID, PgCatalog, PgDepend,
    PgExtension,
};

// ─── Extension version graph ────────────────────────────────────────────────

/// A single version of an extension: either a base install or an upgrade.
struct ExtensionVersion {
    /// The version this script installs (e.g. "1.4").
    version: &'static str,
    /// The version this upgrades FROM. `None` = base install.
    from: Option<&'static str>,
    /// The SQL to execute for this version.
    sql: &'static str,
}

/// An extension definition in the registry.
struct ExtensionDef {
    name: &'static str,
    /// The default version installed by `CREATE EXTENSION` with no VERSION clause.
    default_version: &'static str,
    /// All known versions (base installs + upgrades).
    versions: &'static [ExtensionVersion],
}

mod registry;
use registry::REGISTRY;

// ─── CREATE EXTENSION ───────────────────────────────────────────────────────

pub fn create_extension(
    interp: &mut PgCatalog,
    stmt: &CreateExtensionStmt,
) -> Result<(), DdlError> {
    let name = &stmt.extname;

    // Check if already installed.
    if interp.extension_by_name.contains_key(name.as_str()) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "extension \"{name}\" already exists"
        )));
    }

    let ext = REGISTRY
        .iter()
        .find(|e| e.name == name.as_str())
        .ok_or_else(|| {
            // PG (parse_extension_control_file): `extension "x" is not
            // available`. The analyzer only knows the extensions bundled
            // for static analysis, so say how to add one.
            DdlError::ExtensionError(format!(
                "extension \"{name}\" is not available (unknown to the analyzer: add its SQL \
                 script to typedpg_analyzer/src/extensions/ to register it for static analysis)"
            ))
        })?;

    let target_version = extract_option(&stmt.options, "new_version")
        .unwrap_or_else(|| ext.default_version.to_owned());
    // CreateExtensionInternal: the SCHEMA given, which must exist, or the
    // creation schema.
    let target_schema = match extract_option(&stmt.options, "schema") {
        Some(schema) => schema,
        None => super::util::creation_schema(interp)?,
    };
    let target_nsoid = super::util::existing_namespace(interp, &target_schema)?;

    // Find the install path: base version, then upgrades to target.
    let path = find_install_path(ext, &target_version)?;

    // get_required_extension: required extensions must be installed, or
    // are installed first with CASCADE.
    let cascade = stmt.options.iter().any(|o| {
        matches!(o.node.as_ref(), Some(typedpg_pg_query::protobuf::node::Node::DefElem(de))
            if de.defname == "cascade")
    });
    for required in requires(name) {
        if interp.extension_by_name.contains_key(*required) {
            continue;
        }
        if !cascade {
            return Err(DdlError::TypeNotFound(format!(
                "required extension \"{required}\" is not installed (Use CREATE EXTENSION ... \
                 CASCADE to install required extensions too.)"
            )));
        }
        let mut sub = stmt.clone();
        sub.extname = (*required).to_owned();
        sub.if_not_exists = true;
        sub.options.retain(|o| {
            !matches!(o.node.as_ref(), Some(typedpg_pg_query::protobuf::node::Node::DefElem(de))
                if de.defname == "new_version")
        });
        create_extension(interp, &sub)?;
    }

    // Allocate the pg_extension row up front so we can reference its OID
    // when tagging objects created during installation.
    let ext_oid = PgExtensionOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_extension(PgExtension {
        oid: ext_oid,
        extname: name.clone(),
        extnamespace: target_nsoid,
        extversion: target_version,
    });

    // Snapshot OIDs before install so the `pg_depend` tagging step can
    // identify the objects the extension created.
    let types_before: std::collections::HashSet<PgTypeOid> =
        interp.pg_type.keys().copied().collect();
    let procs_before: std::collections::HashSet<PgProcOid> =
        interp.pg_proc.keys().copied().collect();
    let casts_before: std::collections::HashSet<PgCastOid> =
        interp.pg_cast.keys().copied().collect();

    let installing = interp.installing_extension.replace(name.clone());
    let applied = apply_with_schema(interp, &target_schema, &path);
    interp.installing_extension = installing;
    applied?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);

    Ok(())
}

// ─── ALTER EXTENSION UPDATE ─────────────────────────────────────────────────

pub fn alter_extension(interp: &mut PgCatalog, stmt: &AlterExtensionStmt) -> Result<(), DdlError> {
    let name = &stmt.extname;

    let ext_oid = *interp
        .extension_by_name
        .get(name.as_str())
        .ok_or_else(|| DdlError::TypeNotFound(format!("extension \"{name}\" does not exist")))?;
    let installed_version = interp
        .pg_extension
        .get(&ext_oid)
        .map(|e| e.extversion.clone())
        .unwrap_or_default();
    let installed_nsname = interp
        .pg_extension
        .get(&ext_oid)
        .and_then(|e| interp.namespace_name(e.extnamespace).map(str::to_owned))
        .unwrap_or_else(|| "public".to_owned());

    let ext = REGISTRY
        .iter()
        .find(|e| e.name == name.as_str())
        .ok_or_else(|| DdlError::ExtensionError(format!("extension '{name}' not in registry")))?;

    let target_version = extract_option(&stmt.options, "new_version")
        .unwrap_or_else(|| ext.default_version.to_owned());

    if installed_version == target_version {
        return Ok(()); // Already at target version.
    }

    let path = find_upgrade_path(ext, &installed_version, &target_version)?;

    // Track new objects created by upgrade scripts via pg_depend tagging.
    let types_before: std::collections::HashSet<PgTypeOid> =
        interp.pg_type.keys().copied().collect();
    let procs_before: std::collections::HashSet<PgProcOid> =
        interp.pg_proc.keys().copied().collect();
    let casts_before: std::collections::HashSet<PgCastOid> =
        interp.pg_cast.keys().copied().collect();

    let installing = interp.installing_extension.replace(name.clone());
    let applied = apply_with_schema(interp, &installed_nsname, &path);
    interp.installing_extension = installing;
    applied?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);

    if let Some(entry) = interp.pg_extension.get_mut(&ext_oid) {
        entry.extversion = target_version;
    }

    Ok(())
}

/// Diff `pg_type`/`pg_proc`/`pg_cast` against the snapshot taken before the
/// extension scripts ran, and add `pg_depend` rows for every newly-created
/// object so that `DROP EXTENSION` can find them.
fn record_extension_membership(
    interp: &mut PgCatalog,
    ext_oid: PgExtensionOid,
    types_before: &std::collections::HashSet<PgTypeOid>,
    procs_before: &std::collections::HashSet<PgProcOid>,
    casts_before: &std::collections::HashSet<PgCastOid>,
) {
    let new_types: Vec<PgTypeOid> = interp
        .pg_type
        .keys()
        .filter(|k| !types_before.contains(k))
        .copied()
        .collect();
    let new_procs: Vec<PgProcOid> = interp
        .pg_proc
        .keys()
        .filter(|k| !procs_before.contains(k))
        .copied()
        .collect();
    let new_casts: Vec<PgCastOid> = interp
        .pg_cast
        .keys()
        .filter(|k| !casts_before.contains(k))
        .copied()
        .collect();

    let ref_oid = PgGenericOid::from_nonzero(ext_oid.into_nonzero());
    let ext_dep = |classid: PgClassOid, objid: PgGenericOid| PgDepend {
        classid,
        objid,
        objsubid: 0,
        refclassid: PG_EXTENSION_RELID,
        refobjid: ref_oid,
        refobjsubid: 0,
        deptype: DepType::Extension,
    };
    for type_oid in new_types {
        let g = PgGenericOid::from_nonzero(type_oid.into_nonzero());
        interp.add_dependency(ext_dep(PG_TYPE_RELID, g));
    }
    for proc_oid in new_procs {
        let g = PgGenericOid::from_nonzero(proc_oid.into_nonzero());
        interp.add_dependency(ext_dep(PG_PROC_RELID, g));
    }
    for cast_oid in new_casts {
        let g = PgGenericOid::from_nonzero(cast_oid.into_nonzero());
        interp.add_dependency(ext_dep(PG_CAST_RELID, g));
    }
}

/// The control files' `requires`: extensions that must be installed first.
fn requires(name: &str) -> &'static [&'static str] {
    match name {
        "earthdistance" => &["cube"],
        "hstore_plperl" => &["hstore", "plperl"],
        "hstore_plperlu" => &["hstore", "plperlu"],
        "bool_plperl" | "jsonb_plperl" => &["plperl"],
        "bool_plperlu" | "jsonb_plperlu" => &["plperlu"],
        "hstore_plpython3u" => &["hstore", "plpython3u"],
        "jsonb_plpython3u" => &["plpython3u"],
        "ltree_plpython3u" => &["ltree", "plpython3u"],
        _ => &[],
    }
}

// ─── Path resolution ────────────────────────────────────────────────────────

/// Find the script chain to install an extension at a target version.
/// Returns: base install script + any upgrades needed to reach target.
fn find_install_path<'a>(ext: &'a ExtensionDef, target: &str) -> Result<Vec<&'a str>, DdlError> {
    // Find the base version (from == None).
    let base = ext
        .versions
        .iter()
        .find(|v| v.from.is_none())
        .ok_or_else(|| {
            DdlError::ExtensionError(format!(
                "extension '{}' has no base install version",
                ext.name
            ))
        })?;

    let mut path = vec![base.sql];
    let mut current = base.version;

    if current == target {
        return Ok(path);
    }

    // Walk upgrade chain.
    for _ in 0..100 {
        if let Some(upgrade) = ext.versions.iter().find(|v| v.from == Some(current)) {
            path.push(upgrade.sql);
            current = upgrade.version;
            if current == target {
                return Ok(path);
            }
        } else {
            break;
        }
    }

    Err(DdlError::ExtensionError(format!(
        "extension \"{}\" has no installation script nor update path for version \"{target}\"",
        ext.name,
    )))
}

/// Find the upgrade path from one version to another.
fn find_upgrade_path<'a>(
    ext: &'a ExtensionDef,
    from: &str,
    target: &str,
) -> Result<Vec<&'a str>, DdlError> {
    let mut path = Vec::new();
    let mut current = from;

    for _ in 0..100 {
        if let Some(upgrade) = ext.versions.iter().find(|v| v.from == Some(current)) {
            path.push(upgrade.sql);
            current = upgrade.version;
            if current == target {
                return Ok(path);
            }
        } else {
            break;
        }
    }

    Err(DdlError::ExtensionError(format!(
        "extension \"{}\" has no update path from version \"{from}\" to version \"{target}\"",
        ext.name,
    )))
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Apply a list of SQL scripts with a temporary `search_path` prepend so
/// unqualified names in the extension SQL resolve into the target schema.
fn apply_with_schema(
    interp: &mut PgCatalog,
    schema: &str,
    scripts: &[&str],
) -> Result<(), DdlError> {
    ensure_namespace(interp, schema)?;
    let original = interp.push_search_path_front(schema);

    let mut result = Ok(());
    for sql in scripts {
        if !sql.is_empty() {
            // Bypass the public `apply_sql` so we don't double-mirror to
            // PGlite under `pglite_sanity` — the user-facing `CREATE
            // EXTENSION` already went there once and PGlite handles its
            // own internal scripts. Our embedded scripts also use
            // `MODULE_PATHNAME` placeholders that PGlite would reject.
            let sql = substitute_extschema(interp, schema, sql);
            result = super::apply_sql_to(interp, &sql);
            if result.is_err() {
                break;
            }
        }
    }

    interp.restore_search_path(original);
    result
}

/// `execute_extension_script` (extension.c) replaces `@extschema@` with the
/// extension's schema and `@extschema:name@` with the schema of the required
/// extension `name`, both as quoted identifiers.
fn substitute_extschema(interp: &PgCatalog, schema: &str, sql: &str) -> String {
    // `quote_identifier`: bare when a plain lowercase identifier.
    let quote = |s: &str| {
        let plain = s
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
        if plain {
            s.to_owned()
        } else {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
    };
    let mut out = sql.replace("@extschema@", &quote(schema));
    while let Some(start) = out.find("@extschema:") {
        let rest = &out[start + "@extschema:".len()..];
        let Some(end) = rest.find('@') else {
            break;
        };
        let ext = &rest[..end];
        let ext_schema = interp
            .extension_by_name
            .get(ext)
            .and_then(|oid| interp.pg_extension.get(oid))
            .and_then(|e| interp.namespace_name(e.extnamespace))
            .unwrap_or(schema)
            .to_owned();
        out.replace_range(
            start..start + "@extschema:".len() + end + 1,
            &quote(&ext_schema),
        );
    }
    out
}

/// Extract a string option from CREATE/ALTER EXTENSION options.
fn extract_option(options: &[typedpg_pg_query::protobuf::Node], name: &str) -> Option<String> {
    for opt in options {
        if let Some(typedpg_pg_query::protobuf::node::Node::DefElem(de)) = opt.node.as_ref()
            && de.defname == name
            && let Some(arg) = de.arg.as_deref()
            && let Some(typedpg_pg_query::protobuf::node::Node::String(s)) = arg.node.as_ref()
        {
            return Some(s.sval.clone());
        }
    }
    None
}

// ─── ALTER EXTENSION ADD / DROP, SET SCHEMA ─────────────────────────────────

fn extension_oid(interp: &PgCatalog, name: &str) -> Result<PgExtensionOid, DdlError> {
    interp
        .extension_by_name
        .get(name)
        .copied()
        .ok_or_else(|| DdlError::TypeNotFound(format!("extension \"{name}\" does not exist")))
}

/// The catalog and oid of the object an ALTER EXTENSION ADD / DROP names,
/// with PG's description of it (getObjectDescription).
fn member_object(
    interp: &PgCatalog,
    objtype: typedpg_pg_query::protobuf::ObjectType,
    object: &typedpg_pg_query::protobuf::node::Node,
) -> Result<Option<(PgClassOid, PgGenericOid, String)>, DdlError> {
    use typedpg_pg_query::protobuf::ObjectType;
    use typedpg_pg_query::protobuf::node::Node;
    super::comment::resolve_object(interp, objtype, object)?;
    let names = |n: &Node| -> Vec<String> {
        match n {
            Node::List(l) => l
                .items
                .iter()
                .filter_map(super::util::node_string)
                .map(str::to_owned)
                .collect(),
            Node::String(s) => vec![s.sval.clone()],
            _ => Vec::new(),
        }
    };
    Ok(match objtype {
        ObjectType::ObjectTable
        | ObjectType::ObjectView
        | ObjectType::ObjectMatview
        | ObjectType::ObjectSequence
        | ObjectType::ObjectForeignTable => {
            let parts = names(object);
            let (schema, name) = match parts.as_slice() {
                [name] => (None, name.as_str()),
                [schema, name] => (Some(schema.as_str()), name.as_str()),
                _ => return Ok(None),
            };
            let Some(class) = interp.resolve_table(schema, name) else {
                return Ok(None);
            };
            let kind = match class.relkind {
                crate::pg_catalog::RelKind::View => "view",
                crate::pg_catalog::RelKind::MaterializedView => "materialized view",
                crate::pg_catalog::RelKind::Sequence => "sequence",
                crate::pg_catalog::RelKind::ForeignTable => "foreign table",
                _ => "table",
            };
            Some((
                crate::pg_catalog::PG_CLASS_RELID,
                PgGenericOid::from_nonzero(class.oid.into_nonzero()),
                format!("{kind} {}", class.relname),
            ))
        }
        ObjectType::ObjectType | ObjectType::ObjectDomain => {
            let Node::TypeName(tn) = object else {
                return Ok(None);
            };
            let typ = super::util::lookup_type_name(tn, interp)?;
            Some((
                PG_TYPE_RELID,
                PgGenericOid::from_nonzero(typ.into_nonzero()),
                format!("type {}", super::util::format_type_for_message(interp, typ)),
            ))
        }
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectRoutine => {
            let boxed = Some(Box::new(typedpg_pg_query::protobuf::Node {
                node: Some(object.clone()),
            }));
            let Some((schema, name, args)) = super::alter::extract_func_target(&boxed, interp)
            else {
                return Ok(None);
            };
            let Some((_, oid)) = super::alter::find_proc(interp, schema.as_deref(), &name, &|p| {
                p.proargtypes == args
            }) else {
                return Ok(None);
            };
            let shown = args
                .iter()
                .map(|&t| super::util::format_type_for_message(interp, t))
                .collect::<Vec<_>>()
                .join(", ");
            Some((
                PG_PROC_RELID,
                PgGenericOid::from_nonzero(oid.into_nonzero()),
                format!("function {name}({shown})"),
            ))
        }
        _ => None,
    })
}

/// `ALTER EXTENSION name ADD | DROP object`
/// (ExecAlterExtensionContentsRecurse).
pub fn alter_extension_contents(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterExtensionContentsStmt,
) -> Result<(), DdlError> {
    // The extension's own scripts add and drop members while its
    // membership is still being recorded.
    if interp.installing_extension.as_deref() == Some(stmt.extname.as_str()) {
        return Ok(());
    }
    let ext = extension_oid(interp, &stmt.extname)?;
    let Some(object) = stmt.object.as_deref().and_then(|o| o.node.as_ref()) else {
        return Ok(());
    };
    let objtype = typedpg_pg_query::protobuf::ObjectType::try_from(stmt.objtype)
        .unwrap_or(typedpg_pg_query::protobuf::ObjectType::Undefined);
    let Some((classid, objid, description)) = member_object(interp, objtype, object)? else {
        return Ok(());
    };
    let membership = interp
        .iter_pg_depend()
        .find(|d| {
            d.classid == classid
                && d.objid == objid
                && d.deptype == DepType::Extension
                && d.refclassid == PG_EXTENSION_RELID
        })
        .map(|d| d.refobjid);
    let ext_generic = PgGenericOid::from_nonzero(ext.into_nonzero());
    let ext_name = |oid: PgGenericOid| {
        interp
            .pg_extension
            .values()
            .find(|e| PgGenericOid::from_nonzero(e.oid.into_nonzero()) == oid)
            .map(|e| e.extname.clone())
            .unwrap_or_default()
    };
    if stmt.action > 0 {
        if let Some(owner) = membership {
            return Err(DdlError::UnsupportedDdl(format!(
                "{description} is already a member of extension \"{}\"",
                ext_name(owner)
            )));
        }
        interp.add_dependency(PgDepend {
            classid,
            objid,
            objsubid: 0,
            refclassid: PG_EXTENSION_RELID,
            refobjid: ext_generic,
            refobjsubid: 0,
            deptype: DepType::Extension,
        });
    } else {
        if membership != Some(ext_generic) {
            return Err(DdlError::UnsupportedDdl(format!(
                "{description} is not a member of extension \"{}\"",
                stmt.extname
            )));
        }
        interp.pg_depend.retain(|d| {
            !(d.classid == classid
                && d.objid == objid
                && d.deptype == DepType::Extension
                && d.refobjid == ext_generic)
        });
    }
    Ok(())
}

/// `ALTER EXTENSION name SET SCHEMA s` (AlterExtensionNamespace): the
/// extension's types (with their array types) and functions move.
pub(crate) fn set_extension_schema(
    interp: &mut PgCatalog,
    name: &str,
    new_nsoid: crate::oid::PgNamespaceOid,
) -> Result<(), DdlError> {
    let ext = extension_oid(interp, name)?;
    let ext_generic = PgGenericOid::from_nonzero(ext.into_nonzero());
    let members: Vec<(PgClassOid, PgGenericOid)> = interp
        .iter_pg_depend()
        .filter(|d| d.refobjid == ext_generic && d.deptype == DepType::Extension)
        .map(|d| (d.classid, d.objid))
        .collect();
    for (classid, objid) in members {
        if classid == PG_TYPE_RELID {
            let typ = PgTypeOid::from_nonzero(objid.into_nonzero());
            let array = interp.array_type_of(typ);
            for t in std::iter::once(typ).chain(array) {
                if let Some(name) = interp.pg_type.get(&t).map(|r| r.typname.clone()) {
                    interp.rename_pg_type(t, name, new_nsoid);
                }
            }
        } else if classid == PG_PROC_RELID {
            let proc = PgProcOid::from_nonzero(objid.into_nonzero());
            if let Some(name) = interp.pg_proc.get(&proc).map(|p| p.proname.clone()) {
                interp.rename_pg_proc(proc, name, new_nsoid);
            }
        }
    }
    if let Some(row) = interp.pg_extension.get_mut(&ext) {
        row.extnamespace = new_nsoid;
    }
    Ok(())
}
