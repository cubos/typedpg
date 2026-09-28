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

use pg_query::protobuf::{AlterExtensionStmt, CreateExtensionStmt};

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
            DdlError::ExtensionError(format!(
                "unknown extension '{name}': add a SQL file to typedpg_analyzer/src/extensions/ \
                 to register it for static analysis"
            ))
        })?;

    let target_version = extract_option(&stmt.options, "new_version")
        .unwrap_or_else(|| ext.default_version.to_owned());
    let target_schema =
        extract_option(&stmt.options, "schema").unwrap_or_else(|| "public".to_owned());

    // Find the install path: base version, then upgrades to target.
    let path = find_install_path(ext, &target_version)?;

    // Allocate the pg_extension row up front so we can reference its OID
    // when tagging objects created during installation.
    let target_nsoid = ensure_namespace(interp, &target_schema)?;
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

    apply_with_schema(interp, &target_schema, &path)?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);

    Ok(())
}

// ─── ALTER EXTENSION UPDATE ─────────────────────────────────────────────────

pub fn alter_extension(interp: &mut PgCatalog, stmt: &AlterExtensionStmt) -> Result<(), DdlError> {
    let name = &stmt.extname;

    let ext_oid = *interp
        .extension_by_name
        .get(name.as_str())
        .ok_or_else(|| DdlError::ExtensionError(format!("extension '{name}' is not installed")))?;
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

    apply_with_schema(interp, &installed_nsname, &path)?;

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
        "no install path for extension '{}' to version '{target}' (reached '{current}')",
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
        "no upgrade path for extension '{}' from '{from}' to '{target}' (reached '{current}')",
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
            result = super::apply_sql_to(interp, sql);
            if result.is_err() {
                break;
            }
        }
    }

    interp.restore_search_path(original);
    result
}

/// Extract a string option from CREATE/ALTER EXTENSION options.
fn extract_option(options: &[pg_query::protobuf::Node], name: &str) -> Option<String> {
    for opt in options {
        if let Some(pg_query::protobuf::node::Node::DefElem(de)) = opt.node.as_ref()
            && de.defname == name
            && let Some(arg) = de.arg.as_deref()
            && let Some(pg_query::protobuf::node::Node::String(s)) = arg.node.as_ref()
        {
            return Some(s.sval.clone());
        }
    }
    None
}
