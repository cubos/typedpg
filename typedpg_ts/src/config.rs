//! `typedpg.config.json`: where the sources are, and per database its
//! migrations, its generated module and its type mappings.
//!
//! ```json
//! {
//!   "include": ["src"],
//!   "out": "src/db.ts",
//!   "migrations": "migrations",
//!   "types": { "user_prefs": "./src/domains#UserPrefs" }
//! }
//! ```
//!
//! `migrations` is the directory, or an object with it and the runner's
//! settings: `{ "dir", "table", "lockId", "useTransaction", "failOnDrift",
//! "embed" }`. Several databases go under `databases`, each with the
//! per-database keys (`out`, `migrations`, `extraMigrations`, `types`,
//! `int8`); the top level then carries none of them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use typedpg_core::QualifiedName;
use typedpg_core::config::MigrationsConfig;

pub const CONFIG_FILE: &str = "typedpg.config.json";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    include: Option<Vec<PathBuf>>,
    runtime: Option<String>,
    // The single database's keys, as in `RawDatabase` (serde's `flatten`
    // would lose `deny_unknown_fields`).
    out: Option<PathBuf>,
    migrations: Option<RawMigrations>,
    extra_migrations: Option<Vec<PathBuf>>,
    types: Option<BTreeMap<String, String>>,
    int8: Option<Int8>,
    databases: Option<BTreeMap<String, RawDatabase>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDatabase {
    out: Option<PathBuf>,
    migrations: Option<RawMigrations>,
    extra_migrations: Option<Vec<PathBuf>>,
    types: Option<BTreeMap<String, String>>,
    int8: Option<Int8>,
}

impl RawDatabase {
    fn is_empty(&self) -> bool {
        self.out.is_none()
            && self.migrations.is_none()
            && self.extra_migrations.is_none()
            && self.types.is_none()
            && self.int8.is_none()
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawMigrations {
    Dir(PathBuf),
    Full(RawMigrationsTable),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawMigrationsTable {
    dir: Option<PathBuf>,
    table: Option<String>,
    lock_id: Option<i64>,
    use_transaction: Option<bool>,
    fail_on_drift: Option<bool>,
    embed: Option<bool>,
}

/// How an `int8` (`bigint` column, `count(*)`) reaches TypeScript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Int8 {
    /// `bigint`: exact. The default.
    #[default]
    Bigint,
    /// `string`: exact, as `pg` returns it.
    String,
    /// `number`: a value beyond 2^53 is an error rather than rounded.
    Number,
}

/// The loaded configuration, every path absolute.
#[derive(Debug, Clone)]
pub struct Config {
    /// The directory of the config file; relative paths start from it.
    pub root: PathBuf,
    /// Directories (or files) whose TypeScript sources are scanned.
    pub include: Vec<PathBuf>,
    /// The module the generated files import the runtime from.
    pub runtime: String,
    pub databases: Vec<DatabaseConfig>,
}

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// `default` for the single-database form.
    pub name: String,
    /// The generated module.
    pub out: PathBuf,
    /// The runner's migrations directory.
    pub migrations_dir: PathBuf,
    /// More migrations the schema is built from but the runner doesn't
    /// apply (another project's tables).
    pub extra_migrations: Vec<PathBuf>,
    pub runner: MigrationsConfig,
    /// Whether the generated module embeds the migrations.
    pub embed_migrations: bool,
    /// PG type → the TypeScript type it maps to.
    pub types: HashMap<QualifiedName, TypeOverride>,
    pub int8: Int8,
}

impl DatabaseConfig {
    /// Every directory the schema is built from, the runner's first.
    pub fn migrations_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.migrations_dir.clone()];
        dirs.extend(self.extra_migrations.iter().cloned());
        dirs
    }
}

/// A TypeScript type a PG type maps to: `specifier#Export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeOverride {
    /// A path (absolute, resolved against the config's directory) or a
    /// package specifier.
    pub module: OverrideModule,
    /// The exported type's name.
    pub export: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideModule {
    Path(PathBuf),
    Package(String),
}

impl Config {
    /// Load `path`.
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
        let root = absolute.parent().unwrap_or(Path::new("/"));
        Self::parse(&text, root).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parse a config file's contents, its paths relative to `root`.
    pub fn parse(text: &str, root: &Path) -> Result<Config, String> {
        let raw: RawConfig = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let single = RawDatabase {
            out: raw.out,
            migrations: raw.migrations,
            extra_migrations: raw.extra_migrations,
            types: raw.types,
            int8: raw.int8,
        };
        let databases = match raw.databases {
            Some(dbs) => {
                if !single.is_empty() {
                    return Err(
                        "`out`, `migrations`, `extraMigrations`, `types` and `int8` go in \
                                each entry of `databases` when it is set"
                            .to_owned(),
                    );
                }
                if dbs.is_empty() {
                    return Err("`databases` is empty".to_owned());
                }
                dbs.into_iter()
                    .map(|(name, db)| {
                        database(root, name.clone(), db)
                            .map_err(|e| format!("databases.{name}: {e}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => vec![database(root, "default".to_owned(), single)?],
        };
        let mut outs: Vec<&Path> = databases.iter().map(|d| d.out.as_path()).collect();
        outs.sort();
        if outs.windows(2).any(|w| w[0] == w[1]) {
            return Err("two databases have the same `out`".to_owned());
        }
        let include = raw
            .include
            .unwrap_or_else(|| vec![PathBuf::from(".")])
            .iter()
            .map(|p| root.join(p))
            .collect();
        Ok(Config {
            root: root.to_path_buf(),
            include,
            runtime: raw.runtime.unwrap_or_else(|| "typedpg".to_owned()),
            databases,
        })
    }

    /// The database named `name` (the only one when `None` and there is
    /// one).
    pub fn database(&self, name: Option<&str>) -> Result<&DatabaseConfig, String> {
        let names = || {
            self.databases
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match name {
            Some(name) => self
                .databases
                .iter()
                .find(|d| d.name == name)
                .ok_or_else(|| format!("no database `{name}` in the config (it has: {})", names())),
            None if self.databases.len() == 1 => Ok(&self.databases[0]),
            None => Err(format!(
                "the config has several databases, pick one with --db ({})",
                names()
            )),
        }
    }
}

fn database(root: &Path, name: String, raw: RawDatabase) -> Result<DatabaseConfig, String> {
    let out = raw
        .out
        .ok_or("missing `out`: the path of the generated module")?;
    out_kind(&out)?;
    let table = match raw.migrations {
        None => RawMigrationsTable::default(),
        Some(RawMigrations::Dir(dir)) => RawMigrationsTable {
            dir: Some(dir),
            ..RawMigrationsTable::default()
        },
        Some(RawMigrations::Full(t)) => t,
    };
    let defaults = MigrationsConfig::default();
    let runner = MigrationsConfig {
        table: table.table.unwrap_or(defaults.table),
        lock_id: table.lock_id.unwrap_or(defaults.lock_id),
        use_transaction: table.use_transaction.unwrap_or(defaults.use_transaction),
        fail_on_drift: table.fail_on_drift.unwrap_or(defaults.fail_on_drift),
    };
    runner
        .validate()
        .map_err(|e| format!("migrations.table: {e}"))?;
    let types = raw
        .types
        .unwrap_or_default()
        .into_iter()
        .map(|(key, value)| {
            let qn = QualifiedName::parse_in_schema(&key, "public")
                .map_err(|e| format!("invalid type name `{key}` in `types`: {e}"))?;
            let (module, export) = value.rsplit_once('#').ok_or_else(|| {
                format!(
                    "`types.{key}` must name a module and an exported type, as \
                     `./src/types#MyType`"
                )
            })?;
            let module = if module.starts_with('.') {
                OverrideModule::Path(root.join(module))
            } else {
                OverrideModule::Package(module.to_owned())
            };
            Ok((
                qn,
                TypeOverride {
                    module,
                    export: export.to_owned(),
                },
            ))
        })
        .collect::<Result<_, String>>()?;
    Ok(DatabaseConfig {
        name,
        out: root.join(out),
        migrations_dir: root.join(table.dir.unwrap_or_else(|| "migrations".into())),
        extra_migrations: raw
            .extra_migrations
            .unwrap_or_default()
            .iter()
            .map(|p| root.join(p))
            .collect(),
        runner,
        embed_migrations: table.embed.unwrap_or(false),
        types,
        int8: raw.int8.unwrap_or_default(),
    })
}

/// What a generated module is, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutKind {
    /// `.ts` / `.mts` / `.cts`: one TypeScript module.
    TypeScript,
    /// `.js` / `.mjs`: an ES module and its `.d.ts`.
    EsModule,
    /// `.cjs`: a CommonJS module and its `.d.cts`.
    CommonJs,
}

pub fn out_kind(out: &Path) -> Result<OutKind, String> {
    match out.extension().and_then(|e| e.to_str()) {
        Some("ts" | "mts" | "cts") => Ok(OutKind::TypeScript),
        Some("js" | "mjs") => Ok(OutKind::EsModule),
        Some("cjs") => Ok(OutKind::CommonJs),
        _ => Err(format!(
            "`out` must be a .ts, .mts, .cts, .js, .mjs or .cjs file: {}",
            out.display()
        )),
    }
}

/// The declaration file a JavaScript `out` comes with.
pub fn declaration_path(out: &Path) -> Option<PathBuf> {
    let ext = match out.extension()?.to_str()? {
        "js" => "d.ts",
        "mjs" => "d.mts",
        "cjs" => "d.cts",
        _ => return None,
    };
    Some(out.with_extension(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_database() {
        let c = Config::parse(
            r#"{ "out": "src/db.ts", "types": { "prefs": "./src/domains#Prefs" } }"#,
            Path::new("/p"),
        )
        .unwrap();
        assert_eq!(c.include, vec![PathBuf::from("/p/.")]);
        let db = &c.databases[0];
        assert_eq!(db.name, "default");
        assert_eq!(db.out, PathBuf::from("/p/src/db.ts"));
        assert_eq!(db.migrations_dirs(), vec![PathBuf::from("/p/migrations")]);
        assert_eq!(db.runner.table, "public._migrations");
        assert_eq!(db.int8, Int8::Bigint);
        assert!(!db.embed_migrations);
        let o = &db.types[&QualifiedName::new("public", "prefs")];
        assert_eq!(o.module, OverrideModule::Path("/p/./src/domains".into()));
        assert_eq!(o.export, "Prefs");
    }

    #[test]
    fn migrations_object_and_int8() {
        let c = Config::parse(
            r#"{ "out": "db.js", "int8": "string", "extraMigrations": ["../shared"],
                 "migrations": { "dir": "db/m", "table": "app.schema_log", "lockId": 7,
                                 "useTransaction": false, "failOnDrift": false, "embed": true } }"#,
            Path::new("/p"),
        )
        .unwrap();
        let db = &c.databases[0];
        assert_eq!(
            db.migrations_dirs(),
            vec![PathBuf::from("/p/db/m"), PathBuf::from("/p/../shared")]
        );
        assert_eq!(db.runner.table, "app.schema_log");
        assert_eq!(db.runner.lock_id, 7);
        assert!(!db.runner.use_transaction && !db.runner.fail_on_drift);
        assert!(db.embed_migrations);
        assert_eq!(db.int8, Int8::String);
        assert_eq!(out_kind(&db.out), Ok(OutKind::EsModule));
        assert_eq!(declaration_path(&db.out), Some(PathBuf::from("/p/db.d.ts")));
    }

    #[test]
    fn several_databases() {
        let c = Config::parse(
            r#"{ "databases": { "main": { "out": "a.ts" }, "wh": { "out": "b.cjs" } } }"#,
            Path::new("/p"),
        )
        .unwrap();
        assert_eq!(
            c.database(Some("wh")).unwrap().out,
            PathBuf::from("/p/b.cjs")
        );
        assert!(c.database(None).unwrap_err().contains("--db (main, wh)"));
        assert!(
            c.database(Some("x"))
                .unwrap_err()
                .contains("no database `x`")
        );
    }

    #[test]
    fn errors() {
        let err = |text: &str| Config::parse(text, Path::new("/p")).unwrap_err();
        assert!(
            err(r#"{ "out": "a.ts", "databases": { "x": { "out": "x.ts" } } }"#)
                .contains("go in each entry")
        );
        assert!(err(r#"{ "out": "a.ts", "outt": 1 }"#).contains("outt"));
        assert!(err(r#"{ "out": "a.rs" }"#).contains("must be a .ts"));
        assert!(err(r#"{ "migrations": "m" }"#).contains("missing `out`"));
        assert!(
            err(r#"{ "out": "a.ts", "migrations": { "table": "x; DROP" } }"#)
                .contains("migrations.table")
        );
        assert!(err(r#"{ "out": "a.ts", "int8": "long" }"#).contains("long"));
        assert!(
            err(r#"{ "databases": { "a": { "out": "x.ts" }, "b": { "out": "x.ts" } } }"#)
                .contains("same `out`")
        );
        assert!(err(r#"{ "databases": { "a": { "out": "x.ts", "oops": 1 } } }"#).contains("oops"));
    }
}
