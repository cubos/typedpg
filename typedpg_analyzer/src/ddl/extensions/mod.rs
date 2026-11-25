//! CREATE EXTENSION / ALTER EXTENSION handlers with version tracking.
//!
//! Each extension is declared as a registry entry with a default version,
//! a base install script, and optional upgrade scripts. The interpreter
//! tracks which extensions are installed at which version, enabling
//! ALTER EXTENSION UPDATE to apply the correct upgrade chain.
//!
//! To add support for a new extension:
//! 1. Add `.sql` files to `typedpg_analyzer/src/extensions/`
//! 2. Add an `ExtensionDef` entry to the `REGISTRY` array (`registry.rs`)
//! 3. Run `./update_extensions.sh` to fetch from PG upstream
//!
//! The scripts are embedded compressed with zstd: update_extensions.sh
//! writes `<file>.sql.zst` next to each `<file>.sql` (the crate ships only
//! those), and [`Script::sql`] decompresses one when it is applied.

use typedpg_pg_query::ast::{self, AlterExtensionStmt, CreateExtensionStmt};

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
    sql: Script,
}

/// An extension script's SQL.
enum Script {
    /// A bundled script, compressed with zstd.
    Zstd(&'static [u8]),
    /// SQL written in the registry itself.
    Text(&'static str),
}

impl Script {
    /// The script's SQL, decompressed when it is a bundled one.
    fn sql(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Script::Text(sql) => std::borrow::Cow::Borrowed(sql),
            Script::Zstd(compressed) => {
                let mut sql = String::new();
                std::io::Read::read_to_string(
                    &mut ruzstd::decoding::StreamingDecoder::new(*compressed)
                        .expect("a bundled extension script is a zstd frame"),
                    &mut sql,
                )
                .expect("a bundled extension script decompresses to UTF-8");
                std::borrow::Cow::Owned(sql)
            }
        }
    }
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
    let cascade = stmt.options.iter().any(|o| {
        matches!(o.as_ref(), Some(typedpg_pg_query::ast::Node::DefElem(de))
            if de.defname == "cascade")
    });
    // CreateExtensionInternal: a SCHEMA given must exist, whatever the
    // control file says. A control file's fixed `schema` wins — giving
    // another one is an error, unless CASCADE (which passes its SCHEMA to
    // the extensions it installs) — else the SCHEMA given, or the creation
    // schema.
    if let Some(given) = extract_option(&stmt.options, "schema") {
        super::util::existing_namespace(interp, &given)?;
    }
    let target_schema = match fixed_schema(name) {
        Some(schema) => {
            if let Some(given) = extract_option(&stmt.options, "schema")
                && given != schema
                && !cascade
            {
                return Err(DdlError::ExtensionError(format!(
                    "extension \"{name}\" must be installed in schema \"{schema}\""
                )));
            }
            schema.to_owned()
        }
        None => match extract_option(&stmt.options, "schema") {
            Some(schema) => schema,
            None => super::util::creation_schema(interp)?,
        },
    };
    // Find the install path: base version, then upgrades to target.
    let path = find_install_path(ext, &target_version)?;

    // A control file's schema is created when missing (by the current
    // role, as a generated CREATE SCHEMA command).
    let target_nsoid = if fixed_schema(name).is_some() {
        ensure_namespace(interp, &target_schema)?
    } else {
        super::util::existing_namespace(interp, &target_schema)?
    };

    // get_required_extension: required extensions must be installed, or
    // are installed first with CASCADE.
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
            !matches!(o.as_ref(), Some(typedpg_pg_query::ast::Node::DefElem(de))
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
    let others_before = OtherMembers::snapshot(interp);

    let installing = interp.installing_extension.replace(name.clone());
    let applied = apply_with_schema(interp, name, &target_schema, &path);
    interp.installing_extension = installing;
    applied?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);
    others_before.record(interp, ext_oid);

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
    let others_before = OtherMembers::snapshot(interp);

    let installing = interp.installing_extension.replace(name.clone());
    let applied = apply_with_schema(interp, name, &installed_nsname, &path);
    interp.installing_extension = installing;
    applied?;

    record_extension_membership(interp, ext_oid, &types_before, &procs_before, &casts_before);
    others_before.record(interp, ext_oid);

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

/// What an extension's scripts may create besides types, functions and
/// casts, as it was before they ran: its languages, operators, relations,
/// operator families and classes, text search objects, access methods and
/// foreign-data wrappers become members too
/// (recordDependencyOnCurrentExtension).
struct OtherMembers {
    namespaces: std::collections::HashSet<crate::oid::PgNamespaceOid>,
    languages: std::collections::HashSet<crate::oid::PgLanguageOid>,
    operators: std::collections::HashSet<crate::oid::PgOperatorOid>,
    relations: std::collections::HashSet<PgClassOid>,
    /// Operator families and classes, text search objects, access methods,
    /// foreign-data wrappers.
    named: std::collections::HashSet<crate::ddl::depend::ObjectAddress>,
}

impl OtherMembers {
    fn snapshot(interp: &PgCatalog) -> Self {
        Self {
            namespaces: interp.pg_namespace.keys().copied().collect(),
            languages: interp.pg_language.keys().copied().collect(),
            operators: interp.pg_operator.keys().copied().collect(),
            relations: interp.pg_class.keys().copied().collect(),
            named: crate::ddl::depend::named_object_addresses(interp),
        }
    }

    fn record(self, interp: &mut PgCatalog, ext_oid: PgExtensionOid) {
        use crate::ddl::depend::ObjectAddress;
        let mut members: Vec<ObjectAddress> = interp
            .pg_operator
            .keys()
            .filter(|k| !self.operators.contains(k))
            .map(|&o| ObjectAddress::operator(o))
            .collect();
        members.extend(
            interp
                .pg_language
                .keys()
                .filter(|k| !self.languages.contains(k))
                .map(|&l| ObjectAddress::language(l)),
        );
        // A schema the script creates (pg_cron's `cron`); not the temporary
        // one, nor the control file's, created before the script runs.
        members.extend(
            interp
                .pg_namespace
                .keys()
                .filter(|k| !self.namespaces.contains(k) && interp.temp_namespace != Some(**k))
                .map(|&n| ObjectAddress::namespace(n)),
        );
        // A relation's indexes are its own, not members; a composite type's
        // relation neither (heap_create_with_catalog: its type is the
        // member).
        members.extend(
            interp
                .pg_class
                .iter()
                .filter(|(k, c)| {
                    !self.relations.contains(k)
                        && !matches!(
                            c.relkind,
                            crate::pg_catalog::RelKind::Index
                                | crate::pg_catalog::RelKind::PartitionedIndex
                                | crate::pg_catalog::RelKind::CompositeType
                        )
                })
                .map(|(&k, _)| ObjectAddress::relation(k)),
        );
        members.extend(crate::ddl::depend::extension_named_members(
            interp,
            &self.named,
        ));
        let ext = ObjectAddress {
            classid: PG_EXTENSION_RELID,
            objid: PgGenericOid::from_nonzero(ext_oid.into_nonzero()),
            objsubid: 0,
        };
        for member in members {
            interp.add_dependency(PgDepend {
                classid: member.classid,
                objid: member.objid,
                objsubid: 0,
                refclassid: ext.classid,
                refobjid: ext.objid,
                refobjsubid: 0,
                deptype: DepType::Extension,
            });
        }
    }
}

/// The `proj4text` of the `spatial_ref_sys` row PostGIS's installation
/// script inserts for `srid` (its rows are written one per line, `(srid,
/// 'auth', auth_srid, 'srtext', 'proj4text')`). Read from the script only
/// when asked.
pub(crate) fn postgis_proj4text(srid: i64) -> Option<String> {
    let script = REGISTRY
        .iter()
        .find(|e| e.name == "postgis")?
        .versions
        .iter()
        .find(|v| v.from.is_none())?
        .sql
        .sql();
    let prefix = format!("({srid}, '");
    let line = script.lines().find(|l| l.starts_with(&prefix))?;
    let start = line.rfind(", '")? + 3;
    line[start..]
        .rsplit_once('\'')
        .map(|(text, _)| text.to_owned())
}

/// Bundled extensions that aren't PostgreSQL's own (the registry's
/// third-party section).
#[cfg(any(test, feature = "pg_sanity"))]
const THIRD_PARTY: &[&str] = &[
    "pg_cron",
    "pg_jsonschema",
    "pgmq",
    "postgis",
    "postgis-3",
    "postgis_raster",
    "postgis_raster-3",
    "postgis_sfcgal",
    "postgis_sfcgal-3",
    "postgis_topology",
    "postgis_topology-3",
    "vector",
];

/// The procedural languages built apart from the server: the official
/// image has none of them (the extension oracle's `typedpg-pg-pl` image,
/// `scripts/pg-pl`, adds them).
#[cfg(any(test, feature = "pg_sanity"))]
const SEPARATE_LANGUAGES: &[&str] = &["plperl", "plperlu", "plpython3u"];

/// Whether `name` is one of [`SEPARATE_LANGUAGES`] or requires one.
#[cfg(any(test, feature = "pg_sanity"))]
fn needs_separate_language(name: &str) -> bool {
    SEPARATE_LANGUAGES.contains(&name)
        || requires(name)
            .iter()
            .any(|r| SEPARATE_LANGUAGES.contains(r))
}

/// Whether `name` is a bundled extension the `postgres` image doesn't ship:
/// a third-party one (`scripts/extensions-image` builds an image that
/// ships them), a procedural language built separately (plperl, plperlu,
/// plpython3u), or one requiring such a language. The `pg_sanity` mirror
/// stops comparing after the server rejects one of these as not available.
#[cfg(any(test, feature = "pg_sanity"))]
pub(crate) fn outside_postgres_image(name: &str) -> bool {
    let bundled = |n: &str| REGISTRY.iter().any(|e| e.name == n);
    bundled(name) && (THIRD_PARTY.contains(&name) || needs_separate_language(name))
}

/// The control files' `relocatable`: whether ALTER EXTENSION SET SCHEMA may
/// move the extension. Every bundled extension's control file says `true`
/// but these.
fn relocatable(name: &str) -> bool {
    !matches!(
        name,
        "plperl"
            | "plperlu"
            | "plpgsql"
            | "plpython3u"
            | "xml2"
            | "pg_cron"
            | "pg_jsonschema"
            | "pgmq"
            | "postgis"
            | "postgis-3"
            | "postgis_raster"
            | "postgis_raster-3"
            | "postgis_sfcgal-3"
            | "postgis_topology"
            | "postgis_topology-3"
    )
}

/// The control files' `trusted`: a non-superuser with CREATE on the
/// database may install these (every bundled extension needs a superuser
/// otherwise: `superuser = true`).
#[cfg(feature = "pg_sanity")]
fn trusted(name: &str) -> bool {
    matches!(
        name,
        "btree_gin"
            | "btree_gist"
            | "citext"
            | "cube"
            | "dict_int"
            | "fuzzystrmatch"
            | "hstore"
            | "intarray"
            | "isn"
            | "lo"
            | "ltree"
            | "pg_trgm"
            | "pgcrypto"
            | "bool_plperl"
            | "jsonb_plperl"
            | "plperl"
            | "plpgsql"
            | "seg"
            | "tablefunc"
            | "tcn"
            | "tsm_system_rows"
            | "tsm_system_time"
            | "unaccent"
            | "uuid-ossp"
    )
}

/// The control files' `schema`: extensions that must be installed in one
/// schema (the languages' control files: `schema = pg_catalog`).
fn fixed_schema(name: &str) -> Option<&'static str> {
    match name {
        "plperl" | "plperlu" | "plpgsql" | "plpython3u" | "pg_cron" => Some("pg_catalog"),
        "pgmq" => Some("pgmq"),
        "postgis_topology" => Some("topology"),
        _ => None,
    }
}

/// The control files' `requires`: extensions that must be installed first.
pub(crate) fn requires(name: &str) -> &'static [&'static str] {
    match name {
        "earthdistance" => &["cube"],
        "hstore_plperl" => &["hstore", "plperl"],
        "hstore_plperlu" => &["hstore", "plperlu"],
        "bool_plperl" | "jsonb_plperl" => &["plperl"],
        "bool_plperlu" | "jsonb_plperlu" => &["plperlu"],
        "hstore_plpython3u" => &["hstore", "plpython3u"],
        "jsonb_plpython3u" => &["plpython3u"],
        "ltree_plpython3u" => &["ltree", "plpython3u"],
        "postgis_raster" | "postgis_sfcgal" | "postgis_topology" => &["postgis"],
        _ => &[],
    }
}

// ─── Path resolution ────────────────────────────────────────────────────────

/// Find the script chain to install an extension at a target version.
/// Returns: base install script + any upgrades needed to reach target.
///
/// CreateExtensionInternal: the target version's own installation script
/// when there is one, else the shortest update path from a version that has
/// one (find_install_path).
fn find_install_path<'a>(ext: &'a ExtensionDef, target: &str) -> Result<Vec<&'a Script>, DdlError> {
    let bases = || ext.versions.iter().filter(|v| v.from.is_none());
    if let Some(direct) = bases().find(|v| v.version == target) {
        return Ok(vec![&direct.sql]);
    }
    let shortest = bases()
        .filter_map(|base| {
            let mut path = vec![&base.sql];
            path.extend(find_upgrade_path(ext, base.version, target).ok()?);
            Some(path)
        })
        .min_by_key(Vec::len);
    if let Some(path) = shortest {
        return Ok(path);
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
) -> Result<Vec<&'a Script>, DdlError> {
    let mut path = Vec::new();
    let mut current = from;

    for _ in 0..100 {
        if let Some(upgrade) = ext.versions.iter().find(|v| v.from == Some(current)) {
            path.push(&upgrade.sql);
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

/// Apply extension `name`'s scripts with the `search_path`
/// execute_extension_script gives them: the target schema, the schemas of
/// the extensions it requires (but `pg_catalog`), then `pg_temp`.
fn apply_with_schema(
    interp: &mut PgCatalog,
    name: &str,
    schema: &str,
    scripts: &[&Script],
) -> Result<(), DdlError> {
    ensure_namespace(interp, schema)?;
    search_path_element_check(schema)?;
    let mut path = vec![schema.to_owned()];
    for required in requires(name) {
        if let Some(ns) = interp
            .extension_by_name
            .get(*required)
            .and_then(|oid| interp.pg_extension.get(oid))
            .and_then(|e| interp.namespace_name(e.extnamespace))
        {
            search_path_element_check(ns)?;
            if ns != "pg_catalog" {
                path.push(ns.to_owned());
            }
        }
    }
    path.push("pg_temp".to_owned());
    let original = crate::ddl::permissions::suspended(|| interp.replace_search_path(path));

    let mut result = Ok(());
    for script in scripts {
        let sql = script.sql();
        if !sql.is_empty() {
            // Bypass the public `apply_sql` so we don't double-mirror to
            // PGlite under `pglite_sanity` — the user-facing `CREATE
            // EXTENSION` already went there once and PGlite handles its
            // own internal scripts. Our embedded scripts also use
            // `MODULE_PATHNAME` placeholders that PGlite would reject.
            let sql = match substitute_extschema(interp, name, schema, &sql) {
                Ok(sql) => sql,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            result = crate::ddl::permissions::suspended(|| super::apply_sql_to(interp, &sql));
            if result.is_err() {
                break;
            }
        }
    }

    interp.restore_scoped_search_path(original);
    result
}

/// extension_search_path_element_check (extension.c, PostgreSQL 18.7): a
/// schema the script's `search_path` lists may not be a name that path
/// treats specially — a temporary schema's, or `$user`.
fn search_path_element_check(name: &str) -> Result<(), DdlError> {
    if name.starts_with("pg_temp_")
        || name.starts_with("pg_toast_temp_")
        || name == "pg_temp"
        || name == "$user"
    {
        return Err(DdlError::Parse(format!(
            "unacceptable extension schema name \"{name}\""
        )));
    }
    Ok(())
}

/// check_substitution_name (extension.c, PostgreSQL 18.7): a schema name
/// substituted into extension `extname`'s script may not hold what could
/// break out of its quoting — quotes, `$`, a backslash, a newline, a
/// comment delimiter or another substitution token. (The owner's name,
/// `@extowner@`, is checked too; ownership isn't modeled.)
fn check_substitution_name(name: &str, extname: &str) -> Result<(), DdlError> {
    const QUOTING_RELEVANT_CHARS: &str = "\"$'\\";
    if name.chars().any(|c| QUOTING_RELEVANT_CHARS.contains(c)) {
        return Err(DdlError::Parse(format!(
            "invalid character in extension \"{extname}\" schema: must not contain any of \
             \"{QUOTING_RELEVANT_CHARS}\""
        )));
    }
    if name.contains(['\n', '\r'])
        || name.contains("--")
        || name.contains("/*")
        || name.contains("*/")
    {
        return Err(DdlError::Parse(format!(
            "invalid string in extension \"{extname}\" schema: must not contain newline, \
             carriage return, \"--\", \"/*\", or \"*/\""
        )));
    }
    if name.contains("@extowner@")
        || name.contains("@extschema")
        || name.contains("MODULE_PATHNAME")
    {
        return Err(DdlError::Parse(format!(
            "invalid string in extension \"{extname}\" schema: must not contain \
             \"@extowner@\", \"@extschema\", or \"MODULE_PATHNAME\""
        )));
    }
    Ok(())
}

/// `execute_extension_script` (extension.c) replaces `@extschema@` with the
/// extension's schema and `@extschema:name@` with the schema of the required
/// extension `name`, both as quoted identifiers — and checks each name it
/// substitutes (`@extschema@` only in a script of a non-relocatable
/// extension, the only ones that may use it).
fn substitute_extschema(
    interp: &PgCatalog,
    name: &str,
    schema: &str,
    sql: &str,
) -> Result<String, DdlError> {
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
    if !relocatable(name) && sql.contains("@extschema@") {
        check_substitution_name(schema, name)?;
    }
    // The owner: the migrations' role (ownership isn't modeled).
    let mut out = sql
        .replace("@extschema@", &quote(schema))
        .replace("@extowner@", "CURRENT_USER");
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
        check_substitution_name(&ext_schema, ext)?;
        out.replace_range(
            start..start + "@extschema:".len() + end + 1,
            &quote(&ext_schema),
        );
    }
    Ok(out)
}

/// Extract a string option from CREATE/ALTER EXTENSION options.
fn extract_option(options: &[Option<typedpg_pg_query::ast::Node>], name: &str) -> Option<String> {
    for opt in options {
        if let Some(typedpg_pg_query::ast::Node::DefElem(de)) = opt.as_ref()
            && de.defname == name
            && let Some(typedpg_pg_query::ast::Node::String(s)) = de.arg.as_ref()
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

/// `ALTER {TRIGGER | FUNCTION | PROCEDURE | ROUTINE | INDEX | MATERIALIZED
/// VIEW} … [NO] DEPENDS ON EXTENSION e` (ExecAlterObjectDependsStmt): the
/// object is looked up first (get_object_address_rv — a trigger's table,
/// then the trigger), then the extension; the object is then marked as
/// going along with a DROP EXTENSION, or no longer.
pub fn alter_object_depends(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterObjectDependsStmt,
) -> Result<(), DdlError> {
    use super::depend::ObjectAddress;
    use typedpg_pg_query::ast::{List, ObjectType};
    let objtype = stmt.object_type;
    let string = |s: &str| {
        Some(ast::Node::String(
            typedpg_pg_query::ast::String { sval: s.to_owned() }.into(),
        ))
    };
    // get_object_address_rv: the relation's name heads the object's.
    let object = stmt.object.clone();
    let object = match (stmt.relation.as_ref(), object) {
        (Some(rv), object) => {
            let mut items = Vec::new();
            if !rv.schemaname.is_empty() {
                items.push(string(&rv.schemaname));
            }
            items.push(string(&rv.relname));
            if let Some(ast::Node::List(l)) = object {
                items.extend(l.into_inner().items);
            }
            ast::Node::List(List { items }.into())
        }
        (None, Some(object)) => object,
        (None, None) => return Ok(()),
    };
    super::comment::resolve_object(interp, objtype, &object)?;
    let Some(extname) = stmt.extname.as_ref().map(|e| e.sval.as_str()) else {
        return Ok(());
    };
    let ext = extension_oid(interp, extname)?;

    let relation = |interp: &PgCatalog| {
        let rv = stmt.relation.as_ref()?;
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        interp.resolve_table(schema, &rv.relname).map(|c| c.oid)
    };
    let address = match (objtype, &object) {
        (ObjectType::ObjectTrigger, ast::Node::List(l)) => {
            let name = l
                .items
                .last()
                .and_then(|n| super::util::node_string(n.as_ref()));
            relation(interp).and_then(|relid| {
                let trigger = interp
                    .triggers
                    .get(&relid)?
                    .iter()
                    .find(|t| Some(t.name.as_str()) == name)?;
                Some(ObjectAddress::new(
                    super::depend::PG_TRIGGER_RELID,
                    trigger.oid.into_nonzero(),
                    0,
                ))
            })
        }
        (ObjectType::ObjectIndex | ObjectType::ObjectMatview, _) => {
            relation(interp).map(ObjectAddress::relation)
        }
        (_, ast::Node::ObjectWithArgs(owa)) => {
            super::functions::lookup_func_with_args(interp, objtype, owa, false)?
                .map(ObjectAddress::proc)
        }
        _ => None,
    };
    if let Some(address) = address {
        super::depend::set_auto_extension_dependency(interp, address, ext, stmt.remove);
    }
    Ok(())
}

/// The catalog and oid of the object an ALTER EXTENSION ADD / DROP names,
/// with PG's description of it (getObjectDescription).
fn member_object(
    interp: &PgCatalog,
    objtype: typedpg_pg_query::ast::ObjectType,
    object: &typedpg_pg_query::ast::Node,
) -> Result<Option<(PgClassOid, PgGenericOid, String)>, DdlError> {
    use typedpg_pg_query::ast::Node;
    use typedpg_pg_query::ast::ObjectType;
    super::comment::resolve_object(interp, objtype, object)?;
    let names = |n: &Node| -> Vec<String> {
        match n {
            Node::List(l) => l
                .items
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()))
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
            let Some((schema, name, args)) =
                super::alter::extract_func_target(Some(object), interp)
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
        ObjectType::ObjectOperator => {
            let Some(oid) = super::drop::lookup_operator(interp, Some(object))? else {
                return Ok(None);
            };
            let addr = super::depend::ObjectAddress::operator(oid);
            Some((
                crate::pg_catalog::PG_OPERATOR_RELID,
                addr.objid,
                super::depend::describe(interp, addr),
            ))
        }
        _ => None,
    })
}

/// `ALTER EXTENSION name ADD | DROP object`
/// (ExecAlterExtensionContentsRecurse).
pub fn alter_extension_contents(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterExtensionContentsStmt,
) -> Result<(), DdlError> {
    // The extension's own scripts add and drop members while its
    // membership is still being recorded.
    if interp.installing_extension.as_deref() == Some(stmt.extname.as_str()) {
        return Ok(());
    }
    let ext = extension_oid(interp, &stmt.extname)?;
    let Some(object) = stmt.object.as_ref() else {
        return Ok(());
    };
    let objtype = stmt.objtype;
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
/// extension's members move (AlterObjectNamespace_oid) — types (with their
/// array types), functions, operators, operator classes and families,
/// collations and relations.
pub(crate) fn set_extension_schema(
    interp: &mut PgCatalog,
    name: &str,
    new_schema: &str,
) -> Result<(), DdlError> {
    let ext = extension_oid(interp, name)?;
    let new_nsoid = super::util::lookup_creation_namespace(interp, new_schema)?;
    let Some(old_nsoid) = interp.pg_extension.get(&ext).map(|e| e.extnamespace) else {
        return Ok(());
    };
    // No change is no work; otherwise the control file must allow it.
    if old_nsoid == new_nsoid {
        return Ok(());
    }
    if !relocatable(name) {
        return Err(DdlError::ExtensionError(format!(
            "extension \"{name}\" does not support SET SCHEMA"
        )));
    }
    super::util::check_set_namespace(interp, old_nsoid, new_nsoid)?;
    let ext_generic = PgGenericOid::from_nonzero(ext.into_nonzero());
    let members: Vec<(PgClassOid, PgGenericOid)> = interp
        .iter_pg_depend()
        .filter(|d| d.refobjid == ext_generic && d.deptype == DepType::Extension)
        .map(|d| (d.classid, d.objid))
        .collect();
    // The stored expressions naming a member (bound to it by OID in PG)
    // follow it, as they follow the member's own SET SCHEMA.
    use super::stored_exprs::{Renamed, follow_rename};
    let schema = Some(new_schema);
    for (classid, objid) in members {
        if classid == PG_TYPE_RELID {
            let typ = PgTypeOid::from_nonzero(objid.into_nonzero());
            let array = interp.array_type_of(typ);
            for t in std::iter::once(typ).chain(array) {
                if let Some(name) = interp.pg_type.get(&t).map(|r| r.typname.clone()) {
                    follow_rename(
                        interp,
                        Renamed::Type {
                            oid: t,
                            schema,
                            name: &name,
                        },
                    );
                    super::policies::rewrite_type_refs(interp, t, schema, &name);
                    interp.rename_pg_type(t, name, new_nsoid);
                }
            }
        } else if classid == PG_PROC_RELID {
            let proc = PgProcOid::from_nonzero(objid.into_nonzero());
            if let Some(name) = interp.pg_proc.get(&proc).map(|p| p.proname.clone()) {
                super::policies::rewrite_function_refs(interp, proc, schema, &name);
                follow_rename(
                    interp,
                    Renamed::Function {
                        oid: proc,
                        schema,
                        name: &name,
                    },
                );
                interp.rename_pg_proc(proc, name, new_nsoid);
            }
        } else if classid == crate::pg_catalog::PG_OPERATOR_RELID {
            let oid = crate::oid::PgOperatorOid::from_nonzero(objid.into_nonzero());
            follow_rename(
                interp,
                Renamed::Operator {
                    oid,
                    schema: new_schema,
                },
            );
            super::policies::rewrite_operator_refs(interp, oid, new_schema);
            interp.move_pg_operator(oid, new_nsoid);
        } else if classid == super::depend::PG_OPCLASS_RELID
            || classid == super::depend::PG_OPFAMILY_RELID
        {
            super::opclass::move_am_object(
                interp,
                classid == super::depend::PG_OPCLASS_RELID,
                objid,
                new_nsoid,
            );
        } else if classid == crate::pg_catalog::PG_COLLATION_RELID {
            let coll = crate::oid::PgCollationOid::from_nonzero(objid.into_nonzero());
            if let Some(name) = interp.pg_collation.get(&coll).map(|c| c.collname.clone()) {
                follow_rename(
                    interp,
                    Renamed::Collation {
                        oid: coll,
                        schema,
                        name: &name,
                    },
                );
                super::policies::rewrite_collation_refs(interp, coll, schema, &name);
            }
            if let Some(mut row) = interp.remove_pg_collation(coll) {
                row.collnamespace = new_nsoid;
                interp.insert_pg_collation(row);
            }
        } else if classid == crate::pg_catalog::PG_CLASS_RELID {
            // A relation moves with its row type (and that type's array).
            let rel = PgClassOid::from_nonzero(objid.into_nonzero());
            if let Some(class) = interp.pg_class.get(&rel).cloned() {
                // Only a `regclass` constant names it.
                follow_rename(interp, Renamed::Other);
                super::policies::rewrite_relation_refs(interp, rel, schema, &class.relname);
                interp.rename_pg_class(rel, class.relname, new_nsoid);
                if let Some(rowtype) = class.reltype {
                    let array = interp.array_type_of(rowtype);
                    for t in std::iter::once(rowtype).chain(array) {
                        if let Some(name) = interp.pg_type.get(&t).map(|r| r.typname.clone()) {
                            follow_rename(
                                interp,
                                Renamed::Type {
                                    oid: t,
                                    schema,
                                    name: &name,
                                },
                            );
                            super::policies::rewrite_type_refs(interp, t, schema, &name);
                            interp.rename_pg_type(t, name, new_nsoid);
                        }
                    }
                }
            }
        }
    }
    if let Some(row) = interp.pg_extension.get_mut(&ext) {
        row.extnamespace = new_nsoid;
    }
    Ok(())
}

/// The operator classes that take options (an `options` support function,
/// `amoptsprocnum`) and their one integer option: `(method, opclass,
/// option, min, max, multiple-of)`. From PostgreSQL 18.6's
/// `add_local_int_reloption` calls: tsgistidx.c, brin_minmax_multi.c and
/// the pg_trgm / hstore / intarray / ltree GiST support
/// (`GISTMaxIndexKeySize` is 2024).
const OPCLASS_OPTIONS: &[(&str, &str, &str, i64, i64, i64)] = &[
    ("gist", "tsvector_ops", "siglen", 1, 2024, 1),
    ("gist", "gist_trgm_ops", "siglen", 1, 2024, 1),
    ("gist", "gist_hstore_ops", "siglen", 1, 2024, 1),
    ("gist", "gist__int_ops", "numranges", 1, 252, 1),
    ("gist", "gist__intbig_ops", "siglen", 1, 2024, 1),
    ("gist", "gist_ltree_ops", "siglen", 4, 2024, 4),
    ("gist", "gist__ltree_ops", "siglen", 1, 2024, 1),
];

/// An operator class option's type and range.
#[derive(Clone, Copy)]
enum OpclassOption {
    /// `add_local_int_reloption`: an integer in `min..=max`, a multiple of
    /// `multiple`.
    Int { min: i64, max: i64, multiple: i64 },
    /// `add_local_real_reloption`: a real in `min..=max`.
    Real { min: f64, max: f64 },
}

/// The BRIN `*_bloom_ops` classes' options (brin_bloom.c's
/// `brin_bloom_options`): `n_distinct_per_range` from -1 to INT_MAX, and
/// `false_positive_rate` between BLOOM_MIN_FALSE_POSITIVE_RATE and
/// BLOOM_MAX_FALSE_POSITIVE_RATE.
const BRIN_BLOOM_OPTIONS: &[(&str, OpclassOption)] = &[
    (
        "n_distinct_per_range",
        OpclassOption::Real {
            min: -1.0,
            max: i32::MAX as f64,
        },
    ),
    (
        "false_positive_rate",
        OpclassOption::Real {
            min: 0.0001,
            max: 0.25,
        },
    ),
];

/// `index_opclass_options` (indexam.c): options written after an index
/// column's operator class must be the class's own, in range.
pub(crate) fn check_opclass_options(
    interp: &PgCatalog,
    opclass: &[Option<typedpg_pg_query::ast::Node>],
    options: &[Option<typedpg_pg_query::ast::Node>],
    am: &str,
) -> Result<(), DdlError> {
    if options.is_empty() {
        return Ok(());
    }
    let parts: Vec<&str> = opclass
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    let (schema, name) = match parts.as_slice() {
        [s, n] => (Some(*s), *n),
        [n] => (None, *n),
        _ => return Ok(()),
    };
    let Some(found) = super::opclass::find_opclass(interp, schema, name, am) else {
        return Ok(());
    };
    let spec: Vec<(&str, OpclassOption)> = if let Some(&(_, _, option, min, max, multiple)) =
        OPCLASS_OPTIONS
            .iter()
            .find(|(m, c, ..)| *m == am && *c == found.opcname)
    {
        vec![(option, OpclassOption::Int { min, max, multiple })]
    } else if am == "brin" && found.opcname.ends_with("_minmax_multi_ops") {
        vec![(
            "values_per_range",
            OpclassOption::Int {
                min: 8,
                max: 256,
                multiple: 1,
            },
        )]
    } else if am == "brin" && found.opcname.ends_with("_bloom_ops") {
        BRIN_BLOOM_OPTIONS.to_vec()
    } else {
        return Err(DdlError::UnsupportedDdl(format!(
            "operator class {} has no options",
            found.opcname
        )));
    };
    for opt in options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        let Some(&(option, kind)) = spec.iter().find(|(o, _)| *o == de.defname) else {
            return Err(DdlError::UnsupportedDdl(format!(
                "unrecognized parameter \"{}\"",
                de.defname
            )));
        };
        // transformRelOptions: an option written without a value is `true`.
        let text = match de.arg.as_ref() {
            Some(ast::Node::Integer(i)) => i.ival.to_string(),
            Some(ast::Node::String(s)) => s.sval.clone(),
            Some(ast::Node::Float(f)) => f.fval.clone(),
            Some(ast::Node::TypeName(tn)) => super::util::type_name_to_string(tn),
            None => "true".to_owned(),
            _ => String::new(),
        };
        match kind {
            OpclassOption::Int { min, max, multiple } => {
                let Ok(value) = text.trim().parse::<i64>() else {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "invalid value for integer option \"{option}\": {text}"
                    )));
                };
                if value < min || value > max {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "value {value} out of bounds for option \"{option}\" (Valid values are \
                         between \"{min}\" and \"{max}\".)"
                    )));
                }
                if value % multiple != 0 {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "siglen value must be a multiple of {multiple}"
                    )));
                }
            }
            OpclassOption::Real { min, max } => {
                let Ok(value) = text.trim().parse::<f64>() else {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "invalid value for floating point option \"{option}\": {text}"
                    )));
                };
                // parse_one_reloption: the value as written, the bounds as
                // `%f`.
                if !(min..=max).contains(&value) {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "value {text} out of bounds for option \"{option}\" (Valid values are \
                         between \"{min:.6}\" and \"{max:.6}\".)"
                    )));
                }
            }
        }
    }
    Ok(())
}

// ─── The registry, for the extension oracle ────────────────────────────────

/// What the analyzer knows of a bundled extension: its control file's
/// facts and its version graph (for the extension oracle,
/// `tests/catalog_oracle`).
#[cfg(feature = "pg_sanity")]
#[derive(Clone, Debug)]
pub struct ExtensionInfo {
    pub name: &'static str,
    pub default_version: &'static str,
    /// Every script: `(version, from)` — `from` `None` for an install
    /// script.
    pub versions: Vec<(&'static str, Option<&'static str>)>,
    pub requires: &'static [&'static str],
    pub relocatable: bool,
    /// The control file's fixed `schema`.
    pub schema: Option<&'static str>,
    /// The control file's `superuser` and `trusted`.
    pub superuser: bool,
    pub trusted: bool,
    /// The Docker image whose server ships it.
    pub image: &'static str,
}

/// The Docker image whose PostgreSQL server ships extension `name`: the
/// official `postgres` image the release names for PostgreSQL's own, that
/// image with PL/Perl and PL/Python (`scripts/pg-pl`, which
/// `scripts/run-pg-sanity.sh` builds) for the separately built languages
/// and what requires them, the one `scripts/build-extensions-image.sh`
/// builds for the third-party ones.
#[cfg(feature = "pg_sanity")]
fn image(name: &str) -> &'static str {
    if THIRD_PARTY.contains(&name) {
        "typedpg-extensions:pg19"
    } else if needs_separate_language(name) {
        "typedpg-pg-pl:19.0"
    } else {
        "postgres:19.0"
    }
}

/// Every bundled extension, in registry order.
#[cfg(feature = "pg_sanity")]
pub fn extension_registry() -> Vec<ExtensionInfo> {
    REGISTRY
        .iter()
        .map(|e| ExtensionInfo {
            name: e.name,
            default_version: e.default_version,
            versions: e.versions.iter().map(|v| (v.version, v.from)).collect(),
            requires: requires(e.name),
            relocatable: relocatable(e.name),
            schema: fixed_schema(e.name),
            // The control files' `superuser`: every one says `true` but
            // pgmq's.
            superuser: e.name != "pgmq",
            trusted: trusted(e.name),
            image: image(e.name),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `src/extensions/<file>.sql` has a `<file>.sql.zst` that
    /// decompresses to it byte for byte, and every `.zst` has its `.sql`:
    /// run `./update_extensions.sh --compress` after changing a script.
    #[test]
    fn compressed_extension_scripts_match_the_sql_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/extensions");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            if let Some(sql_name) = name.strip_suffix(".zst") {
                assert!(
                    sql_name.ends_with(".sql") && dir.join(sql_name).is_file(),
                    "{name} has no {sql_name} next to it"
                );
            } else if name.ends_with(".sql") {
                let compressed = std::fs::read(dir.join(format!("{name}.zst")))
                    .unwrap_or_else(|e| panic!("{name}.zst: {e}"));
                let mut decompressed = Vec::new();
                std::io::Read::read_to_end(
                    &mut ruzstd::decoding::StreamingDecoder::new(compressed.as_slice()).unwrap(),
                    &mut decompressed,
                )
                .unwrap();
                assert!(
                    decompressed == std::fs::read(&path).unwrap(),
                    "{name}.zst does not decompress to {name}: run ./update_extensions.sh --compress"
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no .sql file in {}", dir.display());
    }

    #[test]
    fn bundled_extensions_outside_the_postgres_image() {
        let outside: Vec<&str> = REGISTRY
            .iter()
            .map(|e| e.name)
            .filter(|n| outside_postgres_image(n))
            .collect();
        assert_eq!(
            outside,
            [
                "bool_plperl",
                "bool_plperlu",
                "hstore_plperl",
                "hstore_plperlu",
                "hstore_plpython3u",
                "jsonb_plperl",
                "jsonb_plperlu",
                "jsonb_plpython3u",
                "ltree_plpython3u",
                "plperl",
                "plperlu",
                "plpython3u",
                "pg_cron",
                "pg_jsonschema",
                "pgmq",
                "postgis",
                "postgis-3",
                "postgis_raster",
                "postgis_raster-3",
                "postgis_sfcgal",
                "postgis_sfcgal-3",
                "postgis_topology",
                "postgis_topology-3",
                "vector",
            ]
        );
    }
}
