//! The loaders' cache: the ES module of each `.sql` file, compiled by
//! `typedpg build` (or by a loader that found none), so that importing a
//! `.sql` file runs no analysis.
//!
//! An entry's key is a SHA-256 of everything its module depends on: the
//! typedpg version, the config file, every migration of every database, the
//! file's path (which says its database) and its content. The runtime's
//! loaders compute the same key without this binary
//! (`typedpg_npm/src/sqlcache.ts`), so the recipe below is a contract
//! between the two: change it in both, and bump [`FORMAT`].

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::{Config, slashed};

/// The recipe's version.
pub const FORMAT: &str = "typedpg-cache-1";

/// The directory of the cache: `TYPEDPG_CACHE_DIR`, or
/// `node_modules/.cache/typedpg` beside the config file.
pub fn dir(config: &Config) -> PathBuf {
    match std::env::var_os("TYPEDPG_CACHE_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => config.root.join("node_modules/.cache/typedpg"),
    }
}

/// A SHA-256 of the config file and every database's migrations: what the
/// modules of every `.sql` file depend on besides the file.
pub fn schema_identity(config: &Config) -> Result<String, String> {
    let text = std::fs::read(&config.file)
        .map_err(|e| format!("failed to read {}: {e}", config.file.display()))?;
    let mut h = Sha256::new();
    h.update(b"typedpg-schema-1\n");
    h.update(&text);
    h.update(b"\n");
    for db in &config.databases {
        h.update(format!("db:{}\n", db.name).as_bytes());
        for (name, sql) in migrations(&db.migrations_dirs())? {
            h.update(&name);
            h.update(b"\0");
            h.update(&sql);
            h.update(b"\0");
        }
    }
    Ok(hex(&h.finalize()))
}

/// A migration's file name and contents.
type Migration = (Vec<u8>, Vec<u8>);

/// The migrations of `dirs` as the catalog applies them: `.sql` files but
/// `.down.sql` ones, by file name (bytes), a directory's before the next's
/// on a tie.
fn migrations(dirs: &[PathBuf]) -> Result<Vec<Migration>, String> {
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut in_dir = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".sql") && !name.ends_with(".down.sql") {
                let sql = std::fs::read(entry.path()).map_err(|e| {
                    format!("failed to read migration '{}': {e}", entry.path().display())
                })?;
                in_dir.push((name.into_bytes(), sql));
            }
        }
        in_dir.sort();
        files.extend(in_dir);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// The key of the `.sql` file at `path`, with `content`.
pub fn key(config: &Config, identity: &str, path: &Path, content: &str) -> String {
    let rel = path.strip_prefix(&config.root).unwrap_or(path);
    let mut h = Sha256::new();
    for part in [
        FORMAT,
        env!("CARGO_PKG_VERSION"),
        identity,
        &slashed(rel),
        content,
    ] {
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

/// Write the entry `key`, atomically: a loader may be reading.
pub fn write(dir: &Path, key: &str, module: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{key}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, module)?;
    std::fs::rename(&tmp, dir.join(format!("{key}.js")))
}

pub fn exists(dir: &Path, key: &str) -> bool {
    dir.join(format!("{key}.js")).is_file()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
