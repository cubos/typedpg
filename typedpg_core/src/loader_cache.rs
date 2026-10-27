//! The `.sql` loaders' cache: the key recipe, and the config interpretation it
//! depends on.
//!
//! An entry is keyed by a SHA-256 of everything its module depends on: the
//! typedpg version, the config file, every database's migrations, and the
//! file's path and content. The binary (`typedpg_ts`) fills the cache and the
//! npm runtime's loaders read it without a binary, so both must compute the
//! same key: the recipe lives here once, and each side keeps the filesystem —
//! this module sees config text, directory listings and file bytes, never a
//! [`std::fs`] call.
//!
//! - [`databases`] — what the config says: each database's name and migration
//!   directories, sorted by name.
//! - [`migration_files`] — which names in each directory listing the recipe
//!   hashes, in its order.
//! - [`schema_identity`] — the hash of the config and those migrations.
//! - [`key`] — the hash of the identity, the file's path and its content.
//!
//! The config is read with `serde_json` into narrow typed structs: only the
//! keys [`databases`] interprets are decoded, and everything else is skipped
//! with [`serde::de::IgnoredAny`]. That keeps the parser small — this module
//! ships inside a JS string in `typedpg_wasm` — and needs no nesting cap of
//! its own: the typed structs have a fixed depth (bounded by `serde_json`'s
//! 128-level recursion limit anyway), and ignored subtrees are skipped
//! iteratively, not recursed into.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::value::MapAccessDeserializer;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use sha2::{Digest, Sha256};

/// The key recipe's version, part of every key.
pub const FORMAT: &str = "typedpg-cache-1";

/// The identity recipe's version, part of every schema identity.
const SCHEMA_FORMAT: &str = "typedpg-schema-1";

/// The typedpg version, part of every key: a new release recompiles the
/// modules. It is the workspace version, the one the npm packages carry (a
/// release guard keeps them equal).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A migration's file name and bytes.
pub type Migration = (String, Vec<u8>);

/// A database's name and its migrations, in the order
/// [`migration_files`] selected them.
pub type DatabaseMigrations = (String, Vec<Migration>);

/// A database as the config declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    /// `default` for the single-database form.
    pub name: String,
    /// The migration directories as written (relative to the config's
    /// directory): the runner's first, then `extraMigrations` in order.
    pub dirs: Vec<String>,
}

/// The databases a config declares, sorted by name.
///
/// This is exactly the interpretation `typedpg_ts`'s config model applies to
/// these keys: the single-database form is `default` with the top-level keys;
/// otherwise `databases` names each one, and `migrations` is a directory or
/// `{ "dir" }` (default `migrations`), followed by `extraMigrations`.
pub fn databases(config_json: &str) -> Result<Vec<Database>, String> {
    let Object(Config {
        databases,
        out,
        sql,
        migrations,
        extra_migrations,
        types,
    }) = serde_json::from_str(config_json)
        .map_err(|error| format!("invalid config JSON: {error}"))?;
    match databases {
        Some(databases) => {
            if out.is_some()
                || sql.is_some()
                || migrations.is_some()
                || extra_migrations.is_some()
                || types.is_some()
            {
                return Err(
                    "`out`, `sql`, `migrations`, `extraMigrations` and `types` go in each entry \
                     of `databases` when it is set"
                        .to_owned(),
                );
            }
            if databases.is_empty() {
                return Err("`databases` is empty".to_owned());
            }
            Ok(databases
                .into_iter()
                .map(|(name, Object(database))| Database {
                    name,
                    dirs: dirs(database.migrations, database.extra_migrations),
                })
                .collect())
        }
        None => Ok(vec![Database {
            name: "default".to_owned(),
            dirs: dirs(migrations, extra_migrations),
        }]),
    }
}

/// One database's migration directories, the runner's first.
fn dirs(migrations: Option<Migrations>, extra: Option<Vec<String>>) -> Vec<String> {
    let mut dirs = vec![match migrations {
        None => DEFAULT_MIGRATIONS_DIR.to_owned(),
        Some(Migrations::Dir(dir)) => dir,
        Some(Migrations::Table(table)) => table
            .dir
            .unwrap_or_else(|| DEFAULT_MIGRATIONS_DIR.to_owned()),
    }];
    dirs.extend(extra.unwrap_or_default());
    dirs
}

/// The directory a database's migrations are in when the config doesn't say.
const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

/// A migration file [`migration_files`] selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedMigration {
    /// The index of the directory (in the listing passed in) it is in.
    pub directory: usize,
    /// Its file name.
    pub name: String,
}

/// The migration files of `directories` (each a directory's raw listing, in
/// directory order) that the catalog applies: `.sql` files but `.down.sql`
/// ones, sorted by file name (bytes), a directory's before the next's on a
/// tie.
///
/// The order matters because [`schema_identity`] hashes the files as given,
/// and two directories may hold migrations with the same file name.
pub fn migration_files(directories: &[Vec<String>]) -> Vec<SelectedMigration> {
    let mut files = Vec::new();
    for (directory, names) in directories.iter().enumerate() {
        for name in names {
            if name.ends_with(".sql") && !name.ends_with(".down.sql") {
                files.push(SelectedMigration {
                    directory,
                    name: name.clone(),
                });
            }
        }
    }
    // Stable: equal names keep their directory order.
    files.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    files
}

/// A SHA-256 of `config` and every database's migrations, as
/// [`migration_files`] ordered them.
pub fn schema_identity(config: &[u8], databases: &[DatabaseMigrations]) -> String {
    let mut h = Sha256::new();
    h.update(SCHEMA_FORMAT.as_bytes());
    h.update(b"\n");
    h.update(config);
    h.update(b"\n");
    for (name, migrations) in databases {
        h.update(format!("db:{name}\n").as_bytes());
        for (file, sql) in migrations {
            h.update(file.as_bytes());
            h.update(b"\0");
            h.update(sql);
            h.update(b"\0");
        }
    }
    hex(&h.finalize())
}

/// The key of the `.sql` file at `relative_path` (relative to the config's
/// directory, with `/`), with `content`.
pub fn key(identity: &str, relative_path: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in [FORMAT, VERSION, identity, relative_path, content] {
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The config keys [`databases`] reads: the single-database form's top-level
/// ones and `databases` itself. Everything else is ignored, as the JS loaders
/// ignored it (the binary's full model rejects unknown keys before the loader
/// cache is consulted).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The multi-database form; `None` when the key is missing or `null`.
    databases: Option<BTreeMap<String, Object<DatabaseConfig>>>,
    /// A key that belongs in each `databases` entry: set and not `null`
    /// beside `databases` is an error.
    out: Option<IgnoredAny>,
    sql: Option<IgnoredAny>,
    migrations: Option<Migrations>,
    extra_migrations: Option<Vec<String>>,
    types: Option<IgnoredAny>,
}

/// One `databases` entry: the keys of a database the recipe reads.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseConfig {
    migrations: Option<Migrations>,
    extra_migrations: Option<Vec<String>>,
}

/// A JSON object, not the array of fields a derived struct also accepts: a
/// config and each `databases` entry must be spelled `{...}`.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Object<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

/// `migrations`: a directory, or the runner's table object with an optional
/// `dir`. The `expecting` text keeps the wording a wrong shape got before.
enum Migrations {
    Dir(String),
    Table(MigrationsTable),
}

#[derive(Deserialize)]
struct MigrationsTable {
    /// `None` for a missing or `null` key: the default directory.
    dir: Option<String>,
}

impl<'de> Deserialize<'de> for Migrations {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MigrationsVisitor;

        impl<'de> Visitor<'de> for MigrationsVisitor {
            type Value = Migrations;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a directory or an object with `dir`")
            }

            fn visit_str<E: de::Error>(self, dir: &str) -> Result<Migrations, E> {
                Ok(Migrations::Dir(dir.to_owned()))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Migrations, A::Error> {
                MigrationsTable::deserialize(MapAccessDeserializer::new(map)).map(Migrations::Table)
            }
        }

        deserializer.deserialize_any(MigrationsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_single_database_form_is_default() {
        let dbs = databases(r#"{ "out": "src/db.ts" }"#).unwrap();
        assert_eq!(
            dbs,
            [Database {
                name: "default".to_owned(),
                dirs: vec!["migrations".to_owned()],
            }]
        );
    }

    #[test]
    fn migrations_are_a_directory_or_an_object() {
        let dbs = databases(
            r#"{ "out": "db.ts", "migrations": "db/m",
                 "extraMigrations": ["../shared", "./more"] }"#,
        )
        .unwrap();
        assert_eq!(dbs[0].dirs, ["db/m", "../shared", "./more"]);

        let dbs = databases(
            r#"{ "out": "db.ts",
                 "migrations": { "dir": "db/m", "table": "app.schema_log", "lockId": 7 } }"#,
        )
        .unwrap();
        assert_eq!(dbs[0].dirs, ["db/m"]);

        let dbs = databases(r#"{ "out": "db.ts", "migrations": { "table": "t" } }"#).unwrap();
        assert_eq!(dbs[0].dirs, ["migrations"]);
        let dbs = databases(r#"{ "out": "db.ts", "migrations": { "dir": null } }"#).unwrap();
        assert_eq!(dbs[0].dirs, ["migrations"]);
        let dbs = databases(r#"{ "out": "db.ts", "migrations": null }"#).unwrap();
        assert_eq!(dbs[0].dirs, ["migrations"]);
        let dbs = databases(r#"{ "out": "db.ts", "extraMigrations": null }"#).unwrap();
        assert_eq!(dbs[0].dirs, ["migrations"]);
    }

    #[test]
    fn databases_are_sorted_by_name() {
        let dbs = databases(
            r#"{ "databases": {
                     "wh": { "out": "b.ts", "migrations": "wh/m" },
                     "main": { "out": "a.ts", "extraMigrations": ["shared"] },
                     "Z": { "out": "z.ts" }
                 } }"#,
        )
        .unwrap();
        assert_eq!(
            dbs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["Z", "main", "wh"]
        );
        assert_eq!(dbs[1].dirs, ["migrations", "shared"]);
        assert_eq!(dbs[2].dirs, ["wh/m"]);

        // `JSON.parse` keeps the last of a repeated key.
        let dbs = databases(
            r#"{ "databases": { "x": { "out": "a.ts" },
                                 "x": { "out": "b.ts", "migrations": "last" } } }"#,
        )
        .unwrap();
        assert_eq!(dbs.len(), 1);
        assert_eq!(dbs[0].dirs, ["last"]);
    }

    #[test]
    fn invalid_shapes_are_errors() {
        let err = |text: &str| databases(text).unwrap_err();
        assert!(
            err(r#"{ "out": "a.ts", "databases": { "x": { "out": "x.ts" } } }"#)
                .contains("go in each entry")
        );
        assert!(err(r#"{ "databases": {} }"#).contains("`databases` is empty"));
        assert!(
            err(r#"{ "databases": { "x": { "out": "x.ts", "migrations": 5 } } }"#)
                .contains("a directory or an object with `dir`")
        );
        assert!(
            err(r#"{ "databases": { "x": { "migrations": { "dir": 5 } } } }"#)
                .contains("expected a string")
        );
        assert!(
            err(r#"{ "out": "a.ts", "extraMigrations": "x" }"#).contains("expected a sequence")
        );
        assert!(err(r#"{ "databases": [] }"#).contains("expected a map"));
        assert!(err(r#"[1] "#).contains("an object"));
        assert!(
            err(r#"[null, null, null, null, null, null]"#).contains("an object"),
            "a struct's array form is not a config object"
        );
        assert!(
            err(r#"{ "databases": { "x": ["m", null] } }"#).contains("an object"),
            "a struct's array form is not a database object"
        );
        assert!(err("not json").contains("expected"));
        assert!(err(r#"{ "out": "a.ts", }"#).contains("trailing comma"));
        assert!(err(r#"{ "out": "a.ts" } trailing"#).contains("trailing characters"));
        assert!(err(r#"{ "databases": { "x": "y" } }"#).contains("expected an object"));
    }

    /// Values the recipe ignores still have to be valid JSON, and a deeply
    /// nested one is skipped iteratively rather than recursed into, so it
    /// needs no depth cap.
    #[test]
    fn ignored_values_are_still_validated() {
        let err = databases(r#"{ "out": "db.ts", "types": { "a": [1, ] } }"#).unwrap_err();
        assert!(err.contains("expected value"), "{err}");

        let deep = format!(r#"{{ "out": {} }}"#, "[".repeat(129) + &"]".repeat(129));
        assert_eq!(databases(&deep).unwrap()[0].dirs, ["migrations"]);
    }

    #[test]
    fn migration_files_are_the_sql_files_by_name() {
        let directories = vec![
            vec![
                "b.sql".to_owned(),
                "a.down.sql".to_owned(),
                "a.sql".to_owned(),
                "x.txt".to_owned(),
            ],
            vec![
                "a.sql".to_owned(),
                "c.SQL".to_owned(),
                "b.down.sql".to_owned(),
            ],
        ];
        assert_eq!(
            migration_files(&directories),
            [
                SelectedMigration {
                    directory: 0,
                    name: "a.sql".to_owned()
                },
                SelectedMigration {
                    directory: 1,
                    name: "a.sql".to_owned()
                },
                SelectedMigration {
                    directory: 0,
                    name: "b.sql".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_key_is_hex_and_depends_on_its_parts() {
        let identity = schema_identity(b"{}", &[]);
        let entry = key(&identity, "src/q.sql", "SELECT 1;");
        assert_eq!(entry.len(), 64);
        assert_eq!(entry, key(&identity, "src/q.sql", "SELECT 1;"));
        assert_ne!(entry, key(&identity, "src/q.sql", "SELECT 2;"));
        assert_ne!(entry, key(&identity, "src/other.sql", "SELECT 1;"));
    }
}
