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
//! per-database keys (`out`, `sql`, `migrations`, `extraMigrations`,
//! `types`, `int8`); the top level then carries none of them.
//!
//! `sql` takes the queries of `.sql` files, each a `/* @name x */` block
//! (see [`crate::sqlfile`]):
//!
//! ```json
//! "sql": {
//!   "include": ["src/**/queries.sql"],
//!   "emit": "ts",
//!   "out": "{dir}/{name}.ts",
//!   "typeNames": { "params": "{Name}Params", "row": "{Name}Row" }
//! }
//! ```
//!
//! `emit` is what `typedpg gen` writes for each file: `ts`, a module beside
//! it (at `out`) that the code imports; `dts`, only its types, as
//! `<name>.d.sql.ts` — the code imports the `.sql` itself, which a loader
//! or bundler plugin compiles; `none`, nothing — the types then come from
//! the content mapper (`typedpg serve`). A database has `out`, `sql` or
//! both.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
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
    sql: Option<RawSql>,
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
    sql: Option<RawSql>,
    migrations: Option<RawMigrations>,
    extra_migrations: Option<Vec<PathBuf>>,
    types: Option<BTreeMap<String, String>>,
    int8: Option<Int8>,
}

impl RawDatabase {
    fn is_empty(&self) -> bool {
        self.out.is_none()
            && self.sql.is_none()
            && self.migrations.is_none()
            && self.extra_migrations.is_none()
            && self.types.is_none()
            && self.int8.is_none()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSql {
    include: Vec<String>,
    emit: Option<Emit>,
    out: Option<String>,
    type_names: Option<RawTypeNames>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTypeNames {
    params: Option<String>,
    row: Option<String>,
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

/// What `typedpg gen` writes for a `.sql` file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Emit {
    /// A module beside it, at `sql.out`: its types and its runtime.
    #[default]
    Ts,
    /// Its types only, as `<name>.d.sql.ts`, which TypeScript reads for an
    /// import of `<name>.sql` (`allowArbitraryExtensions`).
    Dts,
    /// Nothing: the content mapper gives the types.
    None,
}

/// A database's `.sql` files.
#[derive(Debug, Clone)]
pub struct SqlConfig {
    /// The patterns, as written: relative to the config's directory.
    pub include: Vec<String>,
    globs: GlobSet,
    pub emit: Emit,
    /// The module `emit: "ts"` writes: a path with `{dir}` (the file's
    /// directory) and `{name}` (its name without `.sql`).
    pub out: String,
    pub type_names: TypeNames,
}

/// The names of a query's types, `{Name}` its name with the first letter
/// capitalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNames {
    pub params: String,
    pub row: String,
}

impl TypeNames {
    /// The names for query `name`: `(params, row)`.
    pub fn of(&self, name: &str) -> (String, String) {
        let mut chars = name.chars();
        let pascal: String = chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        (
            self.params.replace("{Name}", &pascal),
            self.row.replace("{Name}", &pascal),
        )
    }
}

pub const SQL_OUT_DEFAULT: &str = "{dir}/{name}.ts";

impl SqlConfig {
    /// Whether `path` (absolute) is one of these files, its patterns
    /// relative to `root`.
    pub fn matches(&self, root: &Path, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "sql")
            && path
                .strip_prefix(root)
                .is_ok_and(|rel| self.globs.is_match(slashed(rel)))
    }

    /// The directories the files can be in: each pattern's part before its
    /// first wildcard.
    pub fn roots(&self, root: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .include
            .iter()
            .map(|pattern| {
                let mut dir = root.to_path_buf();
                let parts: Vec<&str> = pattern.split('/').collect();
                for part in &parts[..parts.len() - 1] {
                    if part.contains(['*', '?', '[', '{']) {
                        break;
                    }
                    dir.push(part);
                }
                dir
            })
            .collect();
        dirs.sort();
        dirs.dedup();
        // A directory inside another is walked with it.
        let all = dirs.clone();
        dirs.retain(|d| !all.iter().any(|o| o != d && d.starts_with(o)));
        dirs
    }

    /// The files `typedpg gen` writes for the `.sql` file at `path`, `out`
    /// relative to `root` when it doesn't start with `{dir}`.
    pub fn outputs(&self, root: &Path, path: &Path) -> Vec<PathBuf> {
        let dir = path.parent().unwrap_or(Path::new("/"));
        let name = sql_stem(path);
        match self.emit {
            Emit::Ts => {
                let out = root.join(
                    self.out
                        .replace("{dir}", &dir.to_string_lossy())
                        .replace("{name}", name),
                );
                let mut files = vec![normalize_path(&out)];
                files.extend(declaration_path(&files[0]));
                files
            }
            Emit::Dts => vec![dir.join(format!("{name}.d.sql.ts"))],
            Emit::None => Vec::new(),
        }
    }
}

/// A `.sql` file's name without the extension.
pub fn sql_stem(path: &Path) -> &str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.strip_suffix(".sql").unwrap_or(name)
}

/// `path` with `/` separators, as patterns are written.
pub fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` without `.` components and with `..` applied.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The loaded configuration, every path absolute.
#[derive(Debug, Clone)]
pub struct Config {
    /// The config file.
    pub file: PathBuf,
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
    /// The generated module of the queries in TypeScript sources.
    pub out: Option<PathBuf>,
    /// The `.sql` files.
    pub sql: Option<SqlConfig>,
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
        let config = Self::parse(&text, root).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Config {
            file: absolute,
            ..config
        })
    }

    /// Parse a config file's contents, its paths relative to `root` (and
    /// the file `root`'s [`CONFIG_FILE`]).
    pub fn parse(text: &str, root: &Path) -> Result<Config, String> {
        let raw: RawConfig = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let single = RawDatabase {
            out: raw.out,
            sql: raw.sql,
            migrations: raw.migrations,
            extra_migrations: raw.extra_migrations,
            types: raw.types,
            int8: raw.int8,
        };
        let databases = match raw.databases {
            Some(dbs) => {
                if !single.is_empty() {
                    return Err(
                        "`out`, `sql`, `migrations`, `extraMigrations`, `types` and `int8` go \
                         in each entry of `databases` when it is set"
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
        let mut outs: Vec<&Path> = databases.iter().filter_map(|d| d.out.as_deref()).collect();
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
            file: root.join(CONFIG_FILE),
            root: root.to_path_buf(),
            include,
            runtime: raw.runtime.unwrap_or_else(|| "@cubos/typedpg".to_owned()),
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
    if raw.out.is_none() && raw.sql.is_none() {
        return Err(
            "missing `out` (the module generated for the queries in TypeScript sources) \
                    or `sql` (the .sql files of queries)"
                .to_owned(),
        );
    }
    if let Some(out) = &raw.out {
        out_kind(out)?;
    }
    let sql = raw.sql.map(sql_config).transpose()?;
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
    let embed_migrations = table.embed.unwrap_or(false);
    if embed_migrations && raw.out.is_none() {
        return Err(
            "`migrations.embed` needs `out`: the migrations are embedded in that module".to_owned(),
        );
    }
    Ok(DatabaseConfig {
        name,
        out: raw.out.map(|out| root.join(out)),
        sql,
        migrations_dir: root.join(table.dir.unwrap_or_else(|| "migrations".into())),
        extra_migrations: raw
            .extra_migrations
            .unwrap_or_default()
            .iter()
            .map(|p| root.join(p))
            .collect(),
        runner,
        embed_migrations,
        types,
        int8: raw.int8.unwrap_or_default(),
    })
}

fn sql_config(raw: RawSql) -> Result<SqlConfig, String> {
    if raw.include.is_empty() {
        return Err("`sql.include` is empty".to_owned());
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in &raw.include {
        if pattern.starts_with('/') || pattern.split('/').any(|p| p == "..") {
            return Err(format!(
                "`sql.include`: `{pattern}` must be relative to the config's directory, inside it"
            ));
        }
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("`sql.include`: {e}"))?;
        builder.add(glob);
    }
    let globs = builder.build().map_err(|e| format!("`sql.include`: {e}"))?;
    let emit = raw.emit.unwrap_or_default();
    let out = match (emit, raw.out) {
        (Emit::Ts, out) => {
            let out = out.unwrap_or_else(|| SQL_OUT_DEFAULT.to_owned());
            if !out.contains("{name}") {
                return Err(format!("`sql.out` must contain `{{name}}`: {out}"));
            }
            if out.ends_with(".sql") {
                return Err(format!(
                    "`sql.out` must be a module, not a .sql file: {out}"
                ));
            }
            out_kind(Path::new(&out)).map_err(|e| e.replace("`out`", "`sql.out`"))?;
            out
        }
        (_, Some(_)) => {
            return Err(
                "`sql.out` is the module `emit: \"ts\"` writes: remove it, or emit \"ts\""
                    .to_owned(),
            );
        }
        (_, None) => String::new(),
    };
    let names = raw.type_names.unwrap_or_default();
    let type_names = TypeNames {
        params: names.params.unwrap_or_else(|| "{Name}Params".to_owned()),
        row: names.row.unwrap_or_else(|| "{Name}Row".to_owned()),
    };
    for (key, template) in [("params", &type_names.params), ("row", &type_names.row)] {
        if !template.contains("{Name}") {
            return Err(format!(
                "`sql.typeNames.{key}` must contain `{{Name}}`: {template}"
            ));
        }
    }
    if type_names.params == type_names.row {
        return Err("`sql.typeNames`: `params` and `row` are the same name".to_owned());
    }
    Ok(SqlConfig {
        include: raw.include,
        globs,
        emit,
        out,
        type_names,
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
        assert_eq!(db.out, Some(PathBuf::from("/p/src/db.ts")));
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
        let out = db.out.as_deref().unwrap();
        assert_eq!(out_kind(out), Ok(OutKind::EsModule));
        assert_eq!(declaration_path(out), Some(PathBuf::from("/p/db.d.ts")));
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
            Some(PathBuf::from("/p/b.cjs"))
        );
        assert!(c.database(None).unwrap_err().contains("--db (main, wh)"));
        assert!(
            c.database(Some("x"))
                .unwrap_err()
                .contains("no database `x`")
        );
    }

    #[test]
    fn sql_files() {
        let c = Config::parse(
            r#"{ "sql": { "include": ["src/**/queries.sql", "src/legacy/*.sql"],
                         "typeNames": { "params": "I{Name}Params", "row": "I{Name}Result" } } }"#,
            Path::new("/p"),
        )
        .unwrap();
        let db = &c.databases[0];
        assert_eq!(db.out, None);
        let sql = db.sql.as_ref().unwrap();
        assert_eq!(sql.emit, Emit::Ts);
        let root = Path::new("/p");
        assert!(sql.matches(root, Path::new("/p/src/queries.sql")));
        assert!(sql.matches(root, Path::new("/p/src/a/b/queries.sql")));
        assert!(sql.matches(root, Path::new("/p/src/legacy/x.sql")));
        assert!(!sql.matches(root, Path::new("/p/src/legacy/deep/x.sql")));
        assert!(!sql.matches(root, Path::new("/p/src/other.sql")));
        assert!(!sql.matches(root, Path::new("/q/src/queries.sql")));
        assert_eq!(sql.roots(root), [PathBuf::from("/p/src")]);
        assert_eq!(
            sql.outputs(root, Path::new("/p/src/a/queries.sql")),
            [PathBuf::from("/p/src/a/queries.ts")]
        );
        assert_eq!(
            sql.type_names.of("getUser"),
            ("IGetUserParams".to_owned(), "IGetUserResult".to_owned())
        );

        let c = Config::parse(
            r#"{ "out": "db.ts", "sql": { "include": ["q/*.sql"], "out": "gen/{name}.queries.js" } }"#,
            root,
        )
        .unwrap();
        assert_eq!(
            c.databases[0]
                .sql
                .as_ref()
                .unwrap()
                .outputs(root, Path::new("/p/q/a.sql")),
            [
                PathBuf::from("/p/gen/a.queries.js"),
                PathBuf::from("/p/gen/a.queries.d.ts")
            ]
        );
        let c = Config::parse(
            r#"{ "sql": { "include": ["*.sql"], "emit": "dts" } }"#,
            root,
        )
        .unwrap();
        assert_eq!(
            c.databases[0]
                .sql
                .as_ref()
                .unwrap()
                .outputs(root, Path::new("/p/a.sql")),
            [PathBuf::from("/p/a.d.sql.ts")]
        );
        let c = Config::parse(
            r#"{ "sql": { "include": ["*.sql"], "emit": "none" } }"#,
            root,
        )
        .unwrap();
        assert!(
            c.databases[0]
                .sql
                .as_ref()
                .unwrap()
                .outputs(root, Path::new("/p/a.sql"))
                .is_empty()
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
        assert!(err(r#"{ "sql": { "include": [] } }"#).contains("`sql.include` is empty"));
        assert!(err(r#"{ "sql": { "include": ["../x/*.sql"] } }"#).contains("inside it"));
        assert!(
            err(r#"{ "sql": { "include": ["a"], "out": "{dir}/x.ts" } }"#).contains("`{name}`")
        );
        assert!(
            err(r#"{ "sql": { "include": ["a"], "out": "{dir}/{name}.rs" } }"#)
                .contains("`sql.out` must be a .ts")
        );
        assert!(
            err(r#"{ "sql": { "include": ["a"], "emit": "dts", "out": "{name}.ts" } }"#)
                .contains("remove it")
        );
        assert!(
            err(r#"{ "sql": { "include": ["a"], "typeNames": { "row": "Row" } } }"#)
                .contains("`{Name}`")
        );
        assert!(
            err(r#"{ "sql": { "include": ["a"] }, "migrations": { "embed": true } }"#)
                .contains("needs `out`")
        );
    }
}
