//! The loaders' cache: the ES module of each `.sql` file, compiled by
//! `typedpg build` (or by a loader that found none), so that importing a
//! `.sql` file runs no analysis.
//!
//! The key recipe — the typedpg version, the config file, every database's
//! migrations, the file's path and its content — lives in
//! [`typedpg_core::loader_cache`], shared with the runtime's loaders
//! (`typedpg_npm/src/sqlfiles.ts`, through `typedpg_wasm`): a cache entry
//! `typedpg build` filled is read by a production install without this
//! binary. This module supplies the filesystem around it: the cache
//! directory, reading the migrations, and writing an entry.

use std::path::{Path, PathBuf};

use typedpg_core::loader_cache;

use crate::config::{Config, slashed};

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
    let databases = config
        .databases
        .iter()
        .map(|db| Ok((db.name.clone(), migrations(&db.migrations_dirs())?)))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(loader_cache::schema_identity(&text, &databases))
}

/// The migrations of `dirs` as the catalog applies them, in the recipe's
/// order: [`loader_cache::migration_files`] selects them from each directory's
/// listing, and this reads the chosen files.
fn migrations(dirs: &[PathBuf]) -> Result<Vec<loader_cache::Migration>, String> {
    let mut listed = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        listed.push((dir, names));
    }
    let listings: Vec<Vec<String>> = listed.iter().map(|(_, names)| names.clone()).collect();
    loader_cache::migration_files(&listings)
        .into_iter()
        .map(|selected| {
            let path = listed[selected.directory].0.join(&selected.name);
            let sql = std::fs::read(&path)
                .map_err(|e| format!("failed to read migration '{}': {e}", path.display()))?;
            Ok((selected.name, sql))
        })
        .collect()
}

/// The key of the `.sql` file at `path`, with `content`.
pub fn key(config: &Config, identity: &str, path: &Path, content: &str) -> String {
    let rel = path.strip_prefix(&config.root).unwrap_or(path);
    loader_cache::key(identity, &slashed(rel), content)
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
