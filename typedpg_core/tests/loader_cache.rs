//! The cache recipe's output must not change: an existing cache — written by
//! `typedpg build` before the recipe moved into `typedpg_core` — stays valid.
//!
//! The old algorithm lived in `typedpg_npm/src/sqlfiles.ts`; it is
//! re-implemented here step for step (the `databases`, `migrations` and hash
//! steps), and the expected identity and key were recorded from that
//! TypeScript implementation. The fixture covers the shapes that make the
//! order subtle: several databases, a directory spelled as a string and as
//! `{ "dir" }`, the default `migrations` directory, `extraMigrations`, a
//! `.down.sql` file, a non-`.sql` file, and the same file name in two
//! directories of one database.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use typedpg_core::loader_cache::{
    DatabaseMigrations, databases, key, migration_files, schema_identity,
};

/// The identity the old TypeScript implementation produced for the fixture.
const EXPECTED_IDENTITY: &str = "0c5150f3a5595a50853b2ee0e72de02671b22bf714ac5e86161ddadb89e4db0b";

/// The key it produced for `q/users.sql`.
const EXPECTED_KEY: &str = "52cbab702fbc9a811464ba8661d29b45f6a85378c717c294a99e92e60f545d63";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cache-project")
}

#[test]
fn the_new_recipe_matches_the_old_typescript_one() {
    let root = fixture();
    let config = std::fs::read(root.join("typedpg.config.json")).unwrap();
    let text = std::str::from_utf8(&config).unwrap();

    let old_databases = old_databases(&root, text);
    let new_databases = databases(text).unwrap();
    assert_eq!(
        new_databases
            .iter()
            .map(|db| db.name.as_str())
            .collect::<Vec<_>>(),
        old_databases
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
    );
    for (db, (_, old_dirs)) in new_databases.iter().zip(&old_databases) {
        // Core keeps the directories as written; each caller resolves them,
        // exactly as the old implementation did.
        let dirs: Vec<PathBuf> = db.dirs.iter().map(|dir| root.join(dir)).collect();
        assert_eq!(&dirs, old_dirs, "database {}", db.name);
    }

    let old_identity = old_schema_identity(&config, &old_databases);
    let new_migrations: Vec<DatabaseMigrations> = new_databases
        .iter()
        .map(|db| {
            let dirs: Vec<PathBuf> = db.dirs.iter().map(|dir| root.join(dir)).collect();
            (db.name.clone(), read_migrations(&dirs))
        })
        .collect();
    let new_identity = schema_identity(&config, &new_migrations);
    assert_eq!(new_identity, old_identity);
    assert_eq!(new_identity, EXPECTED_IDENTITY);

    let sql = root.join("q/users.sql");
    let content = std::fs::read_to_string(&sql).unwrap();
    let old_key = old_key(&old_identity, "q/users.sql", &content);
    let new_key = key(&new_identity, "q/users.sql", &content);
    assert_eq!(new_key, old_key);
    assert_eq!(new_key, EXPECTED_KEY);
}

/// The selection's order is the old one's, including its tie rule.
#[test]
fn the_selected_migrations_match_the_old_order() {
    let root = fixture();
    let config = std::fs::read_to_string(root.join("typedpg.config.json")).unwrap();
    for db in databases(&config).unwrap() {
        let dirs: Vec<PathBuf> = db.dirs.iter().map(|dir| root.join(dir)).collect();
        let listings: Vec<Vec<String>> = dirs.iter().map(|dir| list_dir(dir)).collect();
        let selected: Vec<(usize, String)> = migration_files(&listings)
            .into_iter()
            .map(|m| (m.directory, m.name))
            .collect();
        let old: Vec<(usize, String)> = old_migrations(&dirs)
            .into_iter()
            .map(|(directory, name, _)| (directory, name))
            .collect();
        assert_eq!(selected, old, "database {}", db.name);
    }
}

/// The old `databases(root, text)`: names and resolved directories.
fn old_databases(root: &Path, text: &str) -> Vec<(String, Vec<PathBuf>)> {
    let config: serde_json::Value = serde_json::from_str(text).unwrap();
    let entries: Vec<(String, serde_json::Value)> = match config.get("databases") {
        Some(serde_json::Value::Object(databases)) => {
            let mut entries: Vec<(String, serde_json::Value)> = databases
                .iter()
                .map(|(name, db)| (name.clone(), db.clone()))
                .collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            entries
        }
        _ => vec![("default".to_owned(), config)],
    };
    entries
        .into_iter()
        .map(|(name, db)| {
            let dir = match db.get("migrations") {
                Some(serde_json::Value::String(dir)) => dir.clone(),
                Some(serde_json::Value::Object(migrations)) => migrations
                    .get("dir")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("migrations")
                    .to_owned(),
                _ => "migrations".to_owned(),
            };
            let mut dirs = vec![root.join(dir)];
            if let Some(extra) = db
                .get("extraMigrations")
                .and_then(serde_json::Value::as_array)
            {
                for dir in extra {
                    dirs.push(root.join(dir.as_str().unwrap()));
                }
            }
            (name, dirs)
        })
        .collect()
}

/// The old `migrations(dirs)`: each directory's raw listing, concatenated and
/// stably sorted by file name bytes. A file's directory is kept to compare
/// the tie order.
fn old_migrations(dirs: &[PathBuf]) -> Vec<(usize, String, Vec<u8>)> {
    let mut files = Vec::new();
    for (directory, dir) in dirs.iter().enumerate() {
        for name in list_dir(dir) {
            if name.ends_with(".sql") && !name.ends_with(".down.sql") {
                let bytes = std::fs::read(dir.join(&name)).unwrap();
                files.push((directory, name, bytes));
            }
        }
    }
    // `Array.prototype.sort` is stable, as `sort_by` is.
    files.sort_by(|a, b| a.1.as_bytes().cmp(b.1.as_bytes()));
    files
}

fn old_schema_identity(config: &[u8], databases: &[(String, Vec<PathBuf>)]) -> String {
    let mut h = Sha256::new();
    h.update(b"typedpg-schema-1\n");
    h.update(config);
    h.update(b"\n");
    for (name, dirs) in databases {
        h.update(format!("db:{name}\n").as_bytes());
        for (_, file, sql) in old_migrations(dirs) {
            h.update(file.as_bytes());
            h.update(b"\0");
            h.update(sql);
            h.update(b"\0");
        }
    }
    hex(&h.finalize())
}

fn old_key(identity: &str, relative_path: &str, content: &str) -> String {
    let mut h = Sha256::new();
    for part in [
        "typedpg-cache-1",
        env!("CARGO_PKG_VERSION"),
        identity,
        relative_path,
        content,
    ] {
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

/// What the new core callers do: list, select, read.
fn read_migrations(dirs: &[PathBuf]) -> Vec<(String, Vec<u8>)> {
    let listings: Vec<Vec<String>> = dirs.iter().map(|dir| list_dir(dir)).collect();
    migration_files(&listings)
        .into_iter()
        .map(|m| {
            let bytes = std::fs::read(dirs[m.directory].join(&m.name)).unwrap();
            (m.name, bytes)
        })
        .collect()
}

fn list_dir(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
