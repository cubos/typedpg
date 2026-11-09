//! Configuration types for `typedpg`.
//!
//! The configuration is read from the `[package.metadata.typedpg]` section
//! of the consumer's `Cargo.toml`. A complete example:
//!
//! ```toml
//! [package]
//! name = "my-app"
//! version = "0.1.0"
//! edition = "2021"
//!
//! [package.metadata.typedpg.database]
//! migrations = "./migrations"    # path to SQL migration files (required)
//! extra_migrations = ["../other-crate/migrations"]  # extra migrations for compile-time only
//!
//! [package.metadata.typedpg.migrations]
//! table = "public._migrations"   # tracking table name (default: "public._migrations")
//! lock_id = 713705               # advisory lock ID (default: 713705)
//! use_transaction = true         # wrap each migration in a transaction (default: true)
//! fail_on_drift = true           # abort if an applied migration's statements changed (default: true)
//!
//! [package.metadata.typedpg.types]
//! user_preferences = "crate::domains::UserPreferences"  # JSONB domain
//! post_status = "crate::PostStatus"                     # enum
//! "public.address" = "crate::Address"                   # composite type
//! "public.citext" = "String"                            # custom scalar type
//! "extensions.ltree" = "String"                         # schema-qualified
//!
//! [package.metadata.typedpg.simplify]
//! not_in = false                 # turn one query simplification off (all on by default)
//!
//! [package.metadata.typedpg.lint]
//! fail_on = "warning"            # the severity `typedpg lint` fails on (default "error")
//! null_comparison = "off"        # one rule's level: "error", "warning", "info" or "off"
//! redundant_index = "info"
//! ignore = ["tests/**"]          # paths whose statement findings are not reported
//! ```
//!
//! The query rules are `null_comparison`, `unbounded_write`,
//! `not_in_nullable`, `redundant_null_check`, `limit_without_order`,
//! `redundant_limit`, `redundant_distinct` and `like_unescaped_param`; the
//! migration rules `create_index_blocking`, `constraint_validating`,
//! `set_not_null_scan`, `table_rewrite` and `missing_lock_timeout`; and the
//! schema rules `fk_without_index`, `redundant_index`, `fk_type_mismatch`,
//! `prefer_timestamptz`, `prefer_identity`, `prefer_text` and
//! `prefer_bigint_pk`, evaluated over the migrations' final schema.
//! `like_unescaped_param` and the `prefer_*` rules are off by default; set
//! them to a level to enable them. [`LINT_RULES`] is the one list of their
//! keys. The `sql!` rules `redundant_binding`, `renaming_binding`,
//! `fetch_may_return_many`, `fetch_all_single_row` and `unused_returning`
//! ([`RUST_LINT_RULES`]) run over the Rust source of the invocations, so only
//! `cargo typedpg lint` reports them, and only this configuration accepts
//! their keys — the TypeScript CLI has no bindings or Rust call site.
//! `ignore` lists glob patterns, relative to the manifest's directory. A
//! matching query file's findings are not reported; a matching migration
//! file keeps its schema findings (`fk_without_index`, `redundant_index`),
//! placed on the statements that created the objects, but skips the
//! statement rules. A file that does not compile still fails.
//!
//! `[types]` is a single unified map from a PostgreSQL type to the Rust type
//! it should surface as. Keys are looked up by `schema.name` (or just `name`,
//! defaulting to the `public` schema) in `pg_catalog` at compile time. The
//! `sql!` macro picks how to (de)serialize the value from the PG type's kind:
//!
//! | PostgreSQL type        | Bridging contract on the Rust type             |
//! |------------------------|------------------------------------------------|
//! | domain over `jsonb`    | `serde::{Serialize, Deserialize}`              |
//! | enum                   | `Display` + `FromStr`                          |
//! | composite              | rebuilt field-by-field (names must match)      |
//! | any other scalar       | `tokio_postgres::types::{ToSql, FromSql}`      |
//!
//! A transparent domain (over a non-`jsonb` base) is unwrapped to its base
//! type, recursively — so a domain-over-domain is handled like the type it
//! ultimately wraps. Array versions (`type[]`) are supported automatically as
//! `Vec<RustType>`.
//!
//! All fields have sensible defaults. The `[package.metadata.typedpg]` section
//! itself is required, but every field within it is optional. A key no field
//! reads (a typo, or the retired `domains` / `enums` sections) is an error.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::qualified_name::{ParseQualifiedNameError, QualifiedName};

/// Top-level configuration for `typedpg`, read from `[package.metadata.typedpg]` in `Cargo.toml`.
///
/// Load this from a `Cargo.toml` file with [`Config::from_cargo_toml`], or
/// parse a TOML string directly via [`str::parse`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// Database-related settings (migrations path).
    #[serde(default)]
    pub(crate) database: DatabaseConfig,
    /// Migration runner settings (tracking table, lock ID, transaction behavior).
    #[serde(default)]
    pub migrations: MigrationsConfig,
    /// Unified type mappings: `"schema.type_name"` or `"type_name"` to a Rust
    /// type path. The `sql!` macro infers the (de)serialization contract from
    /// the PostgreSQL type's kind — see the module docs.
    #[serde(default)]
    types: HashMap<String, String>,
    /// Named database configurations for multi-database support.
    /// Each key maps to a complete database configuration.
    /// Used with `sql!(db = name, ...)` syntax.
    #[serde(default)]
    pub(crate) databases: HashMap<String, DatabaseEntry>,
    /// Query simplifications: `simplify = false` turns them all off,
    /// `[simplify]` with `<rule> = false` one by one.
    #[serde(default)]
    simplify: SimplifySetting,
    /// The lint rules' levels and the severity `typedpg lint` fails on.
    #[serde(default)]
    lint: LintSettings,
    /// Keys no field claims, rejected by [`Config::check_unknown_keys`].
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

/// The query simplifications' configuration keys, one per rule: the
/// `[package.metadata.typedpg.simplify]` keys (and the TypeScript CLI's
/// `simplify` object's).
pub const SIMPLIFY_RULES: &[&str] = &["not_in", "count_exists", "distinct", "union_all", "fk_join"];

/// `simplify = <bool>`, or a `[simplify]` table of `<rule> = <bool>`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SimplifySetting {
    All(bool),
    Rules(BTreeMap<String, bool>),
}

impl Default for SimplifySetting {
    fn default() -> Self {
        SimplifySetting::All(true)
    }
}

/// Which query simplifications the `sql!` macro applies: the SQL it sends
/// is rewritten into an equivalent query PostgreSQL plans better, where the
/// analysis proves it returns the same rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimplifyConfig {
    /// `false`: none at all.
    pub enabled: bool,
    /// The rules turned off one by one (keys of [`SIMPLIFY_RULES`]).
    pub disabled: Vec<String>,
}

impl SimplifySetting {
    fn resolve(&self) -> SimplifyConfig {
        match self {
            SimplifySetting::All(enabled) => SimplifyConfig {
                enabled: *enabled,
                disabled: Vec::new(),
            },
            SimplifySetting::Rules(rules) => SimplifyConfig {
                enabled: true,
                disabled: rules
                    .iter()
                    .filter(|(_, on)| !**on)
                    .map(|(rule, _)| rule.clone())
                    .collect(),
            },
        }
    }
}

/// The lint rules' configuration keys, one per rule: the
/// `[package.metadata.typedpg.lint]` keys (and the TypeScript CLI's `lint`
/// object's, in camelCase). The query, migration and schema rules, in
/// `typedpg_lint::Rule::ALL`'s order; the Rust-only rules are in
/// [`RUST_LINT_RULES`].
pub const LINT_RULES: &[&str] = &[
    "null_comparison",
    "unbounded_write",
    "not_in_nullable",
    "create_index_blocking",
    "constraint_validating",
    "set_not_null_scan",
    "table_rewrite",
    "fk_without_index",
    "redundant_index",
    "redundant_null_check",
    "limit_without_order",
    "redundant_limit",
    "redundant_distinct",
    "like_unescaped_param",
    "fk_type_mismatch",
    "missing_lock_timeout",
    "prefer_timestamptz",
    "prefer_identity",
    "prefer_text",
    "prefer_bigint_pk",
];

/// The lint rules over the Rust source of `sql!` invocations: the binding
/// rules, and the call-site rules about the fetch method and discarded
/// `RETURNING` rows.
///
/// `cargo typedpg lint` reports them; the TypeScript CLI has no bindings or
/// Rust call site and keeps rejecting them, so they are accepted only in
/// `[package.metadata.typedpg.lint]`.
pub const RUST_LINT_RULES: &[&str] = &[
    "redundant_binding",
    "renaming_binding",
    "fetch_may_return_many",
    "fetch_all_single_row",
    "unused_returning",
];

/// The types whose default Rust types can't hold `infinity` / `-infinity`
/// (chrono's `NaiveDate`, `NaiveDateTime` and `DateTime<Utc>`, which
/// `ToSql` writes as days or microseconds from 2000 — their ±262 000 years
/// never reach PG's infinity sentinels): a parameter of one is finite, so
/// the analysis takes it to be (`PgCatalog::assume_finite_parameters`)
/// unless `[types]` maps the type. Not `interval`: an `Interval` with every
/// field at its maximum is PG's infinite one.
pub const FINITE_PARAMETER_TYPES: &[&str] = &["date", "timestamp", "timestamptz"];

/// A lint finding's severity, ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LintSeverity {
    Info,
    Warning,
    #[default]
    Error,
}

impl LintSeverity {
    /// The severity's configuration key.
    pub fn name(self) -> &'static str {
        match self {
            LintSeverity::Info => "info",
            LintSeverity::Warning => "warning",
            LintSeverity::Error => "error",
        }
    }

    /// The severity named `name` (see [`LintSeverity::name`]).
    pub fn from_name(name: &str) -> Option<LintSeverity> {
        [
            LintSeverity::Info,
            LintSeverity::Warning,
            LintSeverity::Error,
        ]
        .into_iter()
        .find(|s| s.name() == name)
    }
}

impl std::fmt::Display for LintSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A rule's configured level: a severity, or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LintLevel {
    /// The rule never runs.
    Off,
    /// The rule runs and its findings have this severity.
    #[default]
    Error,
    Warning,
    Info,
}

impl LintLevel {
    /// The level's configuration key.
    pub fn name(self) -> &'static str {
        match self {
            LintLevel::Off => "off",
            LintLevel::Error => "error",
            LintLevel::Warning => "warning",
            LintLevel::Info => "info",
        }
    }

    /// The level named `name` (see [`LintLevel::name`]).
    pub fn from_name(name: &str) -> Option<LintLevel> {
        [
            LintLevel::Off,
            LintLevel::Error,
            LintLevel::Warning,
            LintLevel::Info,
        ]
        .into_iter()
        .find(|l| l.name() == name)
    }

    /// The severity the rule's findings get, or `None` when it is off.
    pub fn severity(self) -> Option<LintSeverity> {
        match self {
            LintLevel::Off => None,
            LintLevel::Error => Some(LintSeverity::Error),
            LintLevel::Warning => Some(LintSeverity::Warning),
            LintLevel::Info => Some(LintSeverity::Info),
        }
    }
}

/// The `lint` section: the severity `typedpg lint` fails on, each rule's
/// level, and the paths whose statement findings are not reported. Rules
/// without an override use their default (in `typedpg_lint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintConfig {
    /// A finding at or above this severity fails the run.
    pub fail_on: LintSeverity,
    /// The rules whose level is overridden, keyed by [`LINT_RULES`].
    pub levels: BTreeMap<String, LintLevel>,
    /// Glob patterns, relative to the project root with forward slashes. A
    /// matching query file's findings are not reported; a matching migration
    /// file reports only its schema rules, placed on the creating
    /// statements. A failure in one still fails the run.
    pub ignore: Vec<String>,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            fail_on: LintSeverity::Error,
            levels: BTreeMap::new(),
            ignore: Vec::new(),
        }
    }
}

/// The `[package.metadata.typedpg.lint]` table as written.
#[derive(Debug, Clone, Default, Deserialize)]
struct LintSettings {
    #[serde(default)]
    fail_on: Option<String>,
    #[serde(default)]
    ignore: Option<Vec<String>>,
    /// Every rule key, as written.
    #[serde(flatten)]
    levels: BTreeMap<String, String>,
}

impl LintSettings {
    /// Validate and resolve the table.
    fn resolve(&self, section: &str) -> Result<LintConfig, ConfigError> {
        let fail_on = match self.fail_on.as_deref() {
            None => LintSeverity::Error,
            Some(value) => {
                LintSeverity::from_name(value).ok_or_else(|| ConfigError::InvalidLintValue {
                    section: section.to_owned(),
                    key: "fail_on".to_owned(),
                    value: value.to_owned(),
                    expected: "`error`, `warning`, `info`".to_owned(),
                })?
            }
        };
        let mut levels = BTreeMap::new();
        for (key, value) in &self.levels {
            let level =
                LintLevel::from_name(value).ok_or_else(|| ConfigError::InvalidLintValue {
                    section: section.to_owned(),
                    key: key.clone(),
                    value: value.clone(),
                    expected: "`error`, `warning`, `info`, `off`".to_owned(),
                })?;
            levels.insert(key.clone(), level);
        }
        Ok(LintConfig {
            fail_on,
            levels,
            ignore: self.ignore.clone().unwrap_or_default(),
        })
    }
}

/// A named database entry for multi-database support.
///
/// Configured under `[package.metadata.typedpg.databases.<name>]` in `Cargo.toml`.
/// Each entry has its own database settings, migrations, and type mappings.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseEntry {
    /// Database-related settings (migrations path).
    #[serde(default)]
    pub(crate) database: DatabaseConfig,
    /// Migration runner settings (tracking table, lock ID, transaction behavior).
    #[serde(default)]
    migrations: MigrationsConfig,
    /// Unified type mappings.
    #[serde(default)]
    types: HashMap<String, String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

/// Database-related configuration.
///
/// Specifies the path to the SQL migration files.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Path to the migrations directory, relative to the project root or absolute.
    /// Default: `"./migrations"`. If the directory does not exist, it is treated
    /// as having zero migrations.
    #[serde(default = "DatabaseConfig::default_migrations")]
    migrations: PathBuf,
    /// Additional migration directories from other crates to include at compile
    /// time. These are used only for static analysis and type checking — they are
    /// NOT executed by the runtime migration runner.
    /// Paths are relative to the project root or absolute.
    #[serde(default)]
    extra_migrations: Vec<PathBuf>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            migrations: Self::default_migrations(),
            extra_migrations: Vec::new(),
            unknown: BTreeMap::new(),
        }
    }
}

impl DatabaseConfig {
    fn default_migrations() -> PathBuf {
        PathBuf::from("./migrations")
    }

    /// Resolve the migrations path relative to a base directory.
    ///
    /// Absolute paths are returned unchanged; relative paths are joined onto `base`.
    pub(crate) fn migrations_dir(&self, base: &Path) -> PathBuf {
        resolve_against(&self.migrations, base)
    }

    /// Resolve the extra (compile-time-only) migration paths relative to a base directory.
    pub(crate) fn extra_migrations_dirs(&self, base: &Path) -> Vec<PathBuf> {
        self.extra_migrations
            .iter()
            .map(|p| resolve_against(p, base))
            .collect()
    }
}

/// Join `path` onto `base` unless `path` is already absolute.
fn resolve_against(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Configuration for the migration runner behavior.
///
/// Controls how migrations are tracked and executed. All fields have defaults,
/// so this entire section can be omitted from `Cargo.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationsConfig {
    /// Fully qualified table name for tracking applied migrations.
    /// Default: "public._migrations"
    #[serde(default = "MigrationsConfig::default_table")]
    pub table: String,

    /// Advisory lock ID to prevent concurrent migration execution.
    /// Default: 713705
    #[serde(default = "MigrationsConfig::default_lock_id")]
    pub lock_id: i64,

    /// Whether migrations run inside a transaction by default.
    /// Individual migrations can override this with `-- no-transaction` on the first line.
    /// Default: true
    #[serde(default = "MigrationsConfig::default_use_transaction")]
    pub use_transaction: bool,

    /// Whether drift (an applied migration's statements changing on disk
    /// after it ran) is treated as a fatal error. Comments, whitespace and
    /// the case of unquoted words do not count. With `true` the migration
    /// runner aborts before applying anything new; with `false` it only
    /// logs a warning and continues. `accept` records an intentional edit.
    /// Default: true.
    #[serde(default = "MigrationsConfig::default_fail_on_drift")]
    pub fail_on_drift: bool,
}

impl Default for MigrationsConfig {
    fn default() -> Self {
        Self {
            table: Self::default_table(),
            lock_id: Self::default_lock_id(),
            use_transaction: Self::default_use_transaction(),
            fail_on_drift: Self::default_fail_on_drift(),
        }
    }
}

impl MigrationsConfig {
    fn default_table() -> String {
        "public._migrations".to_string()
    }

    fn default_lock_id() -> i64 {
        713705
    }

    fn default_use_transaction() -> bool {
        true
    }

    fn default_fail_on_drift() -> bool {
        true
    }

    /// Validates the `table` field as a safe PostgreSQL qualified identifier.
    ///
    /// Accepts `name` or `schema.name`, where each part matches `[a-zA-Z_][a-zA-Z0-9_]*`.
    /// This prevents SQL injection since the table name is interpolated into queries.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_table_name(&self.table).map_err(|e| ConfigError::InvalidTable {
            table: e.table,
            reason: e.reason,
        })
    }
}

/// An invalid tracking-table name, as the runner reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTableName {
    /// The name as configured.
    pub table: String,
    /// Why it is not a valid qualified identifier.
    pub reason: String,
}

impl std::fmt::Display for InvalidTableName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid migration table name '{}': {}",
            self.table, self.reason
        )
    }
}

impl std::error::Error for InvalidTableName {}

/// Validate a tracking-table name as [`MigrationsConfig::validate`] does:
/// `name` or `schema.name`, each part unquoted or double-quoted. The name is
/// interpolated into every runner query, so this is the injection guard.
///
/// Shared with `typedpg_wasm`: the npm runner accepts exactly the names the
/// Rust runner does.
pub fn validate_table_name(table: &str) -> Result<(), InvalidTableName> {
    validate_qualified_name(table).map_err(|reason| InvalidTableName {
        table: table.to_owned(),
        reason,
    })
}

/// Checks if a string is a valid unquoted PG identifier: `[a-zA-Z_][a-zA-Z0-9_]*`.
fn is_pg_ident_unquoted(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checks if a string is a valid quoted PG identifier: `"..."` with no embedded
/// null bytes. Double quotes inside must be escaped as `""`.
fn is_pg_ident_quoted(s: &str) -> bool {
    if s.len() < 3 || !s.starts_with('"') || !s.ends_with('"') {
        return false;
    }
    let inner = &s[1..s.len() - 1];
    if inner.is_empty() || inner.contains('\0') {
        return false;
    }
    // Ensure no unescaped double quotes (consecutive `""` are fine)
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '"' && chars.next() != Some('"') {
            return false;
        }
    }
    true
}

fn is_pg_ident(s: &str) -> bool {
    is_pg_ident_unquoted(s) || is_pg_ident_quoted(s)
}

fn validate_qualified_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("table name cannot be empty".into());
    }

    // Split on '.' but respect quoted identifiers
    let parts = split_qualified_name(name);
    if parts.is_empty() || parts.len() > 2 {
        return Err("expected format: 'name' or 'schema.name'".into());
    }

    for part in &parts {
        if !is_pg_ident(part) {
            return Err(format!("'{}' is not a valid PostgreSQL identifier", part));
        }
    }

    Ok(())
}

/// Splits a qualified name like `schema.name` or `"my schema"."my table"` into parts.
/// Handles escaped double-quotes (`""`) inside quoted identifiers.
fn split_qualified_name(name: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let bytes = name.as_bytes();

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if in_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                // Escaped double-quote inside quoted identifier — skip both
                i += 2;
                continue;
            }
            in_quote = !in_quote;
        } else if bytes[i] == b'.' && !in_quote {
            parts.push(&name[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&name[start..]);
    parts
}

/// Wrapper to extract `[package.metadata.typedpg]` from a full Cargo.toml.
#[derive(Debug, Default, Deserialize)]
struct CargoToml {
    #[serde(default)]
    package: Package,
}

#[derive(Debug, Default, Deserialize)]
struct Package {
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    typedpg: Config,
}

/// Errors that can occur when loading or parsing the `typedpg` configuration.
///
/// Returned by [`Config::from_cargo_toml`] and [`MigrationsConfig::validate`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Cargo.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid migration table name '{table}': {reason}")]
    InvalidTable { table: String, reason: String },
    #[error("unknown database '{0}' — not found in [package.metadata.typedpg.databases]")]
    UnknownDatabase(String),
    #[error("unknown key `{key}` in [{section}]: {hint}")]
    UnknownKey {
        section: String,
        key: String,
        hint: String,
    },
    #[error("invalid value `{value}` for `{key}` in [{section}]: expected one of {expected}")]
    InvalidLintValue {
        section: String,
        key: String,
        value: String,
        expected: String,
    },
    #[error("invalid qualified name in [{section}] key '{key}': {source}")]
    InvalidQualifiedName {
        section: &'static str,
        key: String,
        #[source]
        source: ParseQualifiedNameError,
    },
}

impl std::str::FromStr for Config {
    type Err = ConfigError;

    fn from_str(content: &str) -> Result<Self, Self::Err> {
        let cargo: CargoToml = toml::from_str(content)?;
        let config = cargo.package.metadata.typedpg;
        config.check_unknown_keys()?;
        config.migrations.validate()?;
        Ok(config)
    }
}

/// Keys older releases read and this one ignores: accepted so old manifests
/// keep building, since they never changed what the macros generate.
const LEGACY_TOP_LEVEL_KEYS: &[&str] = &["analysis_mode"];
const LEGACY_DATABASE_KEYS: &[&str] = &["docker_image"];

/// Reject the keys of `unknown` that are not `legacy`. A misspelt or
/// retired key used to be dropped silently — `[…typedpg.domains]` mappings,
/// say, were just never applied.
fn reject_unknown(
    unknown: &BTreeMap<String, toml::Value>,
    legacy: &[&str],
    section: &str,
    expected: &str,
) -> Result<(), ConfigError> {
    let Some(key) = unknown.keys().find(|k| !legacy.contains(&k.as_str())) else {
        return Ok(());
    };
    let hint = match key.as_str() {
        "domains" | "enums" => format!(
            "the [{section}.{key}] section was replaced by [{section}.types], which maps \
             domains, enums, composites and scalars alike"
        ),
        _ => format!("expected one of {expected}"),
    };
    Err(ConfigError::UnknownKey {
        section: section.to_owned(),
        key: key.clone(),
        hint,
    })
}

impl Config {
    /// Reject keys no field reads, at every level of the configuration.
    fn check_unknown_keys(&self) -> Result<(), ConfigError> {
        const TOP: &str = "package.metadata.typedpg";
        const TOP_KEYS: &str = "`database`, `migrations`, `types`, `databases`, `simplify`, `lint`";
        const DATABASE_KEYS: &str = "`migrations`, `extra_migrations`";
        reject_unknown(&self.unknown, LEGACY_TOP_LEVEL_KEYS, TOP, TOP_KEYS)?;
        if let SimplifySetting::Rules(rules) = &self.simplify
            && let Some(rule) = rules.keys().find(|r| !SIMPLIFY_RULES.contains(&r.as_str()))
        {
            return Err(ConfigError::UnknownKey {
                section: format!("{TOP}.simplify"),
                key: rule.clone(),
                hint: format!(
                    "expected one of {}",
                    SIMPLIFY_RULES
                        .iter()
                        .map(|r| format!("`{r}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        let lint_section = format!("{TOP}.lint");
        if let Some(rule) =
            self.lint.levels.keys().find(|r| {
                !LINT_RULES.contains(&r.as_str()) && !RUST_LINT_RULES.contains(&r.as_str())
            })
        {
            let keys = std::iter::once("fail_on")
                .chain(std::iter::once("ignore"))
                .chain(LINT_RULES.iter().copied())
                .chain(RUST_LINT_RULES.iter().copied())
                .map(|r| format!("`{r}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ConfigError::UnknownKey {
                section: lint_section,
                key: rule.clone(),
                hint: format!("expected one of {keys}"),
            });
        }
        self.lint.resolve(&lint_section)?;
        reject_unknown(
            &self.database.unknown,
            LEGACY_DATABASE_KEYS,
            &format!("{TOP}.database"),
            DATABASE_KEYS,
        )?;
        let mut names: Vec<_> = self.databases.keys().collect();
        names.sort();
        for name in names {
            let entry = &self.databases[name];
            let section = format!("{TOP}.databases.{name}");
            reject_unknown(
                &entry.unknown,
                &[],
                &section,
                "`database`, `migrations`, `types`",
            )?;
            reject_unknown(
                &entry.database.unknown,
                LEGACY_DATABASE_KEYS,
                &format!("{section}.database"),
                DATABASE_KEYS,
            )?;
        }
        Ok(())
    }

    /// Load config from a `Cargo.toml` file.
    pub fn from_cargo_toml(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        content.parse()
    }

    /// Resolve the migrations path relative to a base directory.
    pub fn migrations_dir(&self, base: &Path) -> PathBuf {
        self.database.migrations_dir(base)
    }

    /// The named databases (`[package.metadata.typedpg.databases]`), sorted.
    /// The default database has no name: it is the configuration outside
    /// that table.
    pub fn database_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.databases.keys().cloned().collect();
        names.sort();
        names
    }

    /// Resolve configuration for a specific database.
    ///
    /// - `None` returns the default (top-level) config.
    /// - `Some("name")` looks up `[databases.name]` and returns an error if not found.
    pub fn resolve(&self, db_name: Option<&str>) -> Result<ResolvedConfig<'_>, ConfigError> {
        match db_name {
            None => Ok(ResolvedConfig {
                database: &self.database,
                migrations: &self.migrations,
                types: qualify_keys(&self.types, "types")?,
                simplify: self.simplify.resolve(),
                lint: self.lint.resolve("package.metadata.typedpg.lint")?,
            }),
            Some(name) => {
                let entry = self
                    .databases
                    .get(name)
                    .ok_or_else(|| ConfigError::UnknownDatabase(name.to_string()))?;
                Ok(ResolvedConfig {
                    database: &entry.database,
                    migrations: &entry.migrations,
                    types: qualify_keys(&entry.types, "types")?,
                    simplify: self.simplify.resolve(),
                    lint: self.lint.resolve("package.metadata.typedpg.lint")?,
                })
            }
        }
    }
}

/// A resolved view of a single database's configuration.
///
/// Returned by [`Config::resolve`]. Borrows database/migration settings from the
/// parent [`Config`], but owns the type-mapping HashMap because keys are normalized
/// into [`QualifiedName`]s. Bare names in Cargo.toml default to the `public` schema.
#[derive(Debug, Clone)]
pub struct ResolvedConfig<'a> {
    database: &'a DatabaseConfig,
    /// Per-database migration runner settings: the `sql!` macro reads
    /// `use_transaction` to interpret the migrations as the runner applies
    /// them.
    migrations: &'a MigrationsConfig,
    /// Unified type map keyed by qualified PG type name. The `sql!` macro
    /// infers the (de)serialization strategy from each type's kind.
    pub types: HashMap<QualifiedName, String>,
    /// The query simplifications (the top-level `simplify` setting, for
    /// every database).
    pub simplify: SimplifyConfig,
    /// The lint settings (the top-level `lint` table, for every database).
    pub lint: LintConfig,
}

/// Parse type-mapping keys into [`QualifiedName`]s.
///
/// Keys follow PostgreSQL identifier syntax: an optional `schema.` prefix and
/// quoting (`"My Schema"."My Type"`). A bare, single-part name lands in the
/// `public` schema — and because parsing is quote-aware, a quoted identifier
/// containing a `.` (e.g. `"odd.name"`) stays a single name rather than being
/// split on the dot. Unquoted identifiers are case-folded, as PG does.
fn qualify_keys(
    map: &HashMap<String, String>,
    section: &'static str,
) -> Result<HashMap<QualifiedName, String>, ConfigError> {
    map.iter()
        .map(|(k, v)| {
            let key = QualifiedName::parse_in_schema(k, "public").map_err(|source| {
                ConfigError::InvalidQualifiedName {
                    section,
                    key: k.clone(),
                    source,
                }
            })?;
            Ok((key, v.clone()))
        })
        .collect()
}

impl ResolvedConfig<'_> {
    /// Whether this database's migrations run each in a transaction
    /// (`[package.metadata.typedpg.databases.<name>.migrations]`, or the
    /// top-level section for the default database).
    pub fn migrations_use_transaction(&self) -> bool {
        self.migrations.use_transaction
    }

    /// Resolve the migrations path relative to a base directory.
    pub fn migrations_dir(&self, base: &Path) -> PathBuf {
        self.database.migrations_dir(base)
    }

    /// Resolve extra migration paths relative to a base directory.
    pub fn extra_migrations_dirs(&self, base: &Path) -> Vec<PathBuf> {
        self.database.extra_migrations_dirs(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const VALID_TOML: &str = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg]
[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.types]
user_preferences = "crate::domains::UserPreferences"
order_metadata = "crate::domains::OrderMetadata"
"#;

    const MINIMAL_TOML: &str = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations"
"#;

    #[test]
    fn parse_full_config() {
        let config = Config::from_str(VALID_TOML).unwrap();
        assert_eq!(config.database.migrations, PathBuf::from("./migrations"));
        assert_eq!(config.types.len(), 2);
        assert_eq!(
            config.types.get("user_preferences").unwrap(),
            "crate::domains::UserPreferences"
        );
        // Defaults for migrations config
        assert_eq!(config.migrations.table, "public._migrations");
        assert_eq!(config.migrations.lock_id, 713705);
        assert!(config.migrations.use_transaction);
    }

    #[test]
    fn parse_minimal_config() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        assert!(config.types.is_empty());
        assert_eq!(config.migrations.table, "public._migrations");
    }

    #[test]
    fn parse_bare_minimum_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg]
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.database.migrations, PathBuf::from("./migrations"));
        assert!(config.types.is_empty());
    }

    #[test]
    fn parse_custom_migrations_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.migrations]
table = "myschema._my_migrations"
lock_id = 999
use_transaction = false
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.migrations.table, "myschema._my_migrations");
        assert_eq!(config.migrations.lock_id, 999);
        assert!(!config.migrations.use_transaction);
    }

    #[test]
    fn missing_section_uses_defaults() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.database.migrations, PathBuf::from("./migrations"));
    }

    #[test]
    fn legacy_analysis_mode_ignored() {
        // Old Cargo.toml files with analysis_mode should still parse fine.
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg]
analysis_mode = "static"
"#;
        Config::from_str(toml).unwrap();
    }

    #[test]
    fn legacy_docker_image_ignored() {
        // Old Cargo.toml files with docker_image should still parse fine.
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
docker_image = "postgres:16"
migrations = "./migrations"
"#;
        Config::from_str(toml).unwrap();
    }

    fn parse_err(metadata: &str) -> String {
        let toml = format!("[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n{metadata}");
        Config::from_str(&toml).unwrap_err().to_string()
    }

    #[test]
    fn retired_domains_and_enums_sections_are_rejected() {
        // Both were folded into [types]; ignoring them silently left the
        // user's mappings unapplied.
        for section in ["domains", "enums"] {
            let err = parse_err(&format!(
                "[package.metadata.typedpg.{section}]\nuser_preferences = \"crate::Prefs\"\n"
            ));
            assert_eq!(
                err,
                format!(
                    "unknown key `{section}` in [package.metadata.typedpg]: the \
                     [package.metadata.typedpg.{section}] section was replaced by \
                     [package.metadata.typedpg.types], which maps domains, enums, \
                     composites and scalars alike"
                )
            );
        }
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        assert_eq!(
            parse_err("[package.metadata.typedpg.type]\nfoo = \"crate::Foo\"\n"),
            "unknown key `type` in [package.metadata.typedpg]: expected one of \
             `database`, `migrations`, `types`, `databases`, `simplify`, `lint`"
        );
        assert_eq!(
            parse_err("[package.metadata.typedpg.database]\nmigration = \"./m\"\n"),
            "unknown key `migration` in [package.metadata.typedpg.database]: expected one \
             of `migrations`, `extra_migrations`"
        );
        assert_eq!(
            parse_err("[package.metadata.typedpg.databases.analytics]\ntype = {}\n"),
            "unknown key `type` in [package.metadata.typedpg.databases.analytics]: expected \
             one of `database`, `migrations`, `types`"
        );
        assert_eq!(
            parse_err("[package.metadata.typedpg.databases.analytics.database]\npath = \"x\"\n"),
            "unknown key `path` in [package.metadata.typedpg.databases.analytics.database]: \
             expected one of `migrations`, `extra_migrations`"
        );
        let err = parse_err("[package.metadata.typedpg.migrations]\ntabel = \"x\"\n");
        assert!(
            err.contains("unknown field `tabel`, expected one of `table`"),
            "{err}"
        );
    }

    #[test]
    fn migrations_dir_relative() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        let dir = config.migrations_dir(Path::new("/home/user/project"));
        assert_eq!(dir, PathBuf::from("/home/user/project/./migrations"));
    }

    #[test]
    fn migrations_dir_absolute() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "/opt/migrations"
"#;
        let config = Config::from_str(toml).unwrap();
        let dir = config.migrations_dir(Path::new("/home/user/project"));
        assert_eq!(dir, PathBuf::from("/opt/migrations"));
    }

    #[test]
    fn validate_default_table() {
        let config = MigrationsConfig::default();
        config.validate().unwrap();
    }

    #[test]
    fn validate_unqualified_table() {
        let config = MigrationsConfig {
            table: "_migrations".into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_qualified_table() {
        let config = MigrationsConfig {
            table: "my_schema._migrations".into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_quoted_table() {
        let config = MigrationsConfig {
            table: r#""my-schema"."my-migrations""#.into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_mixed_quoted_unquoted() {
        let config = MigrationsConfig {
            table: r#"public."my-migrations""#.into(),
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_sql_injection() {
        let config = MigrationsConfig {
            table: "public; DROP TABLE users --".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty() {
        let config = MigrationsConfig {
            table: "".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_too_many_parts() {
        let config = MigrationsConfig {
            table: "a.b.c".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_quoted() {
        let config = MigrationsConfig {
            table: r#""""#.into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    /// The npm runner validates through this function: it must accept and
    /// reject exactly what `validate` does, with the same message.
    #[test]
    fn validate_table_name_is_the_runners_rule() {
        assert!(validate_table_name("public._migrations").is_ok());
        assert!(validate_table_name(r#""my schema".log"#).is_ok());
        for table in ["", "a.b.c", "1abc", r#""""#] {
            let config = MigrationsConfig {
                table: table.into(),
                ..Default::default()
            };
            assert_eq!(
                validate_table_name(table).unwrap_err().to_string(),
                config.validate().unwrap_err().to_string(),
                "table {table:?}"
            );
        }
    }

    #[test]
    fn parse_multi_db_config() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations/main"

[package.metadata.typedpg.databases.analytics]
[package.metadata.typedpg.databases.analytics.database]
migrations = "./migrations/analytics"

[package.metadata.typedpg.databases.analytics.migrations]
table = "public._analytics_migrations"

[package.metadata.typedpg.databases.analytics.types]
event_data = "crate::EventData"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(
            config.database.migrations,
            PathBuf::from("./migrations/main")
        );
        assert_eq!(config.databases.len(), 1);

        let analytics = config.databases.get("analytics").unwrap();
        assert_eq!(
            analytics.database.migrations,
            PathBuf::from("./migrations/analytics")
        );
        assert_eq!(analytics.migrations.table, "public._analytics_migrations");
        assert_eq!(
            analytics.types.get("event_data").unwrap(),
            "crate::EventData"
        );
    }

    #[test]
    fn resolve_default_db() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        let resolved = config.resolve(None).unwrap();
        assert_eq!(resolved.database.migrations, PathBuf::from("./migrations"));
    }

    #[test]
    fn resolve_named_db() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations/main"

[package.metadata.typedpg.databases.analytics.database]
migrations = "./migrations/analytics"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(Some("analytics")).unwrap();
        assert_eq!(
            resolved.database.migrations,
            PathBuf::from("./migrations/analytics")
        );
    }

    #[test]
    fn resolve_named_db_carries_its_runner_settings() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.migrations]
use_transaction = true

[package.metadata.typedpg.databases.analytics.migrations]
use_transaction = false
"#;
        let config = Config::from_str(toml).unwrap();
        assert!(config.resolve(None).unwrap().migrations_use_transaction());
        assert!(
            !config
                .resolve(Some("analytics"))
                .unwrap()
                .migrations_use_transaction()
        );
    }

    #[test]
    fn resolve_unknown_db_errors() {
        let config = Config::from_str(MINIMAL_TOML).unwrap();
        assert!(config.resolve(Some("nonexistent")).is_err());
    }

    #[test]
    fn database_names_lists_the_named_databases_sorted() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.databases.warehouse.database]
migrations = "./migrations/warehouse"

[package.metadata.typedpg.databases.analytics.database]
migrations = "./migrations/analytics"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.database_names(), ["analytics", "warehouse"]);
        assert!(
            Config::from_str(MINIMAL_TOML)
                .unwrap()
                .database_names()
                .is_empty()
        );
    }

    #[test]
    fn resolve_qualifies_unqualified_type_keys() {
        let config = Config::from_str(VALID_TOML).unwrap();
        let resolved = config.resolve(None).unwrap();
        // Unqualified "user_preferences" becomes "public.user_preferences"
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "user_preferences"))
                .unwrap(),
            "crate::domains::UserPreferences"
        );
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "order_metadata"))
                .unwrap(),
            "crate::domains::OrderMetadata"
        );
    }

    #[test]
    fn resolve_preserves_qualified_type_keys() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.types]
"whatsapp.health_data" = "crate::domains::HealthData"
"whatsapp.qr_data" = "crate::domains::QrData"
user_preferences = "crate::domains::UserPreferences"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(None).unwrap();
        // Schema-qualified keys are preserved as-is
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("whatsapp", "health_data"))
                .unwrap(),
            "crate::domains::HealthData"
        );
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("whatsapp", "qr_data"))
                .unwrap(),
            "crate::domains::QrData"
        );
        // Unqualified key gets "public." prefix
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "user_preferences"))
                .unwrap(),
            "crate::domains::UserPreferences"
        );
    }

    #[test]
    fn resolve_qualifies_type_keys() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.types]
user_role = "crate::UserRole"
"custom_schema.status" = "crate::Status"
point = "crate::Point"
"geo.polygon" = "crate::Polygon"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(None).unwrap();

        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "user_role"))
                .unwrap(),
            "crate::UserRole"
        );
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("custom_schema", "status"))
                .unwrap(),
            "crate::Status"
        );

        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "point"))
                .unwrap(),
            "crate::Point"
        );
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("geo", "polygon"))
                .unwrap(),
            "crate::Polygon"
        );
    }

    #[test]
    fn resolve_quoted_type_key_with_embedded_dot() {
        // A quoted identifier containing a `.` is one name in `public`, not a
        // schema-qualified pair — the key parses with PG identifier rules,
        // not a naïve split on the first dot.
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations"

[package.metadata.typedpg.types]
'"odd.name"' = "crate::Odd"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(None).unwrap();
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "odd.name"))
                .unwrap(),
            "crate::Odd"
        );
    }

    #[test]
    fn resolve_qualifies_named_db_keys() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations/main"

[package.metadata.typedpg.databases.analytics.database]
migrations = "./migrations/analytics"

[package.metadata.typedpg.databases.analytics.types]
event_data = "crate::EventData"
"stats.metric" = "crate::Metric"
"#;
        let config = Config::from_str(toml).unwrap();
        let resolved = config.resolve(Some("analytics")).unwrap();
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("public", "event_data"))
                .unwrap(),
            "crate::EventData"
        );
        assert_eq!(
            resolved
                .types
                .get(&QualifiedName::new("stats", "metric"))
                .unwrap(),
            "crate::Metric"
        );
    }

    #[test]
    fn simplify_is_on_by_default_and_can_be_turned_off() {
        let header = "[package]\nname = \"a\"\n[package.metadata.typedpg]\n";
        let simplify = |extra: &str| {
            Config::from_str(&format!("{header}{extra}"))
                .unwrap()
                .resolve(None)
                .unwrap()
                .simplify
        };
        assert_eq!(
            simplify(""),
            SimplifyConfig {
                enabled: true,
                disabled: vec![]
            }
        );
        assert!(!simplify("simplify = false\n").enabled);
        assert_eq!(
            simplify("[package.metadata.typedpg.simplify]\nnot_in = false\nfk_join = true\n"),
            SimplifyConfig {
                enabled: true,
                disabled: vec!["not_in".into()]
            }
        );
        let err = Config::from_str(&format!(
            "{header}[package.metadata.typedpg.simplify]\nnot_inn = false\n"
        ))
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("unknown key `not_inn` in [package.metadata.typedpg.simplify]"),
            "{err}"
        );
    }

    #[test]
    fn lint_defaults_and_overrides() {
        let header = "[package]\nname = \"a\"\n[package.metadata.typedpg]\n";
        let lint = |extra: &str| {
            Config::from_str(&format!("{header}{extra}"))
                .unwrap()
                .resolve(None)
                .unwrap()
                .lint
        };
        assert_eq!(lint(""), LintConfig::default());
        let config = lint(
            "[package.metadata.typedpg.lint]\nfail_on = \"warning\"\n\
             null_comparison = \"off\"\nunbounded_write = \"info\"\n\
             ignore = [\"tests/**\", \"**/fixtures/*.sql\"]\n",
        );
        assert_eq!(config.fail_on, LintSeverity::Warning);
        assert_eq!(config.levels["null_comparison"], LintLevel::Off);
        assert_eq!(config.levels["unbounded_write"], LintLevel::Info);
        assert_eq!(config.ignore, ["tests/**", "**/fixtures/*.sql"]);
        assert_eq!(LintLevel::Off.severity(), None);
        assert_eq!(LintLevel::Warning.severity(), Some(LintSeverity::Warning));
    }

    #[test]
    fn lint_ignore_is_not_a_rule_and_rejects_non_strings() {
        // `ignore` is a key of its own, never a rule name.
        let header = "[package]\nname = \"a\"\n[package.metadata.typedpg]\n";
        let config = Config::from_str(&format!(
            "{header}[package.metadata.typedpg.lint]\nignore = [\"tests/**\"]\n"
        ))
        .unwrap()
        .resolve(None)
        .unwrap()
        .lint;
        assert!(config.levels.is_empty());
        assert_eq!(config.ignore, ["tests/**"]);

        let err = Config::from_str(&format!(
            "{header}[package.metadata.typedpg.lint]\nignore = [1]\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("expected a string"), "{err}");
    }

    #[test]
    fn lint_unknown_rule_is_rejected() {
        let header = "[package]\nname = \"a\"\n[package.metadata.typedpg]\n";
        let err = Config::from_str(&format!(
            "{header}[package.metadata.typedpg.lint]\nnull_comparisions = \"off\"\n"
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown key `null_comparisions` in [package.metadata.typedpg.lint]: expected one \
             of `fail_on`, `ignore`, `null_comparison`, `unbounded_write`, `not_in_nullable`, \
             `create_index_blocking`, `constraint_validating`, `set_not_null_scan`, \
             `table_rewrite`, `fk_without_index`, `redundant_index`, `redundant_null_check`, \
             `limit_without_order`, `redundant_limit`, `redundant_distinct`, \
             `like_unescaped_param`, `fk_type_mismatch`, `missing_lock_timeout`, \
             `prefer_timestamptz`, `prefer_identity`, `prefer_text`, `prefer_bigint_pk`, \
             `redundant_binding`, `renaming_binding`, `fetch_may_return_many`, \
             `fetch_all_single_row`, `unused_returning`"
        );
    }

    #[test]
    fn lint_bad_level_and_fail_on_are_rejected() {
        let header = "[package]\nname = \"a\"\n[package.metadata.typedpg]\n";
        let err = Config::from_str(&format!(
            "{header}[package.metadata.typedpg.lint]\nnull_comparison = \"fatal\"\n"
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid value `fatal` for `null_comparison` in [package.metadata.typedpg.lint]: \
             expected one of `error`, `warning`, `info`, `off`"
        );
        let err = Config::from_str(&format!(
            "{header}[package.metadata.typedpg.lint]\nfail_on = \"off\"\n"
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid value `off` for `fail_on` in [package.metadata.typedpg.lint]: expected one \
             of `error`, `warning`, `info`"
        );
    }
}
