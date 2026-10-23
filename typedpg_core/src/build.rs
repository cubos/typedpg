//! Build-script support for migration change detection.
//!
//! A proc macro that reads files (like `typedpg`'s `sql!`) cannot, on stable
//! Rust, tell the compiler that those files are build inputs for free: the
//! only mechanism is `include_str!`, which copies the files' contents into
//! the crate's metadata. `sql!` pays that for the migrations it reads, so
//! editing or deleting one re-runs the analysis. Adding one to the directory
//! is what this build script covers: Cargo watches the directory itself, and
//! a file dependency cannot.
//!
//! Calling [`track_migrations`] from a `build.rs` fixes this. It emits a
//! `cargo:rerun-if-changed` directive for every migration directory (Cargo
//! scans directories recursively, so newly *added* files are detected too) and
//! a `cargo:rustc-env` fingerprint of the migration contents. When that
//! fingerprint changes, Cargo recompiles the crate, re-running every `sql!`
//! invocation against the new schema.
//!
//! # Usage
//!
//! Add a `build.rs` next to your `Cargo.toml`:
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     typedpg::build::track_migrations();
//! }
//! ```
//!
//! And declare `typedpg` as a build dependency:
//!
//! ```toml
//! # Cargo.toml
//! [build-dependencies]
//! typedpg = "0.1"
//! ```

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::Config;

/// Name of the `cargo:rustc-env` variable carrying the migration fingerprint.
///
/// Changing its value is what forces Cargo to recompile the consuming crate
/// (and thus re-run every `sql!`). The macro does not need to read it — the
/// recompilation alone re-runs the macros from scratch.
const FINGERPRINT_ENV: &str = "TYPEDPG_MIGRATIONS_FINGERPRINT";

/// Wire migration files into Cargo's change-detection so `sql!` is re-checked
/// whenever they change.
///
/// Call this from your crate's `build.rs`. It reads
/// `[package.metadata.typedpg]` from the crate's `Cargo.toml`, then, for
/// every configured migration directory — the default database, every named
/// database under `[databases.*]`, and any `extra_migrations`:
///
/// - emits `cargo:rerun-if-changed`, so Cargo re-runs this build script when a
///   `.sql` file in that directory is added, removed, or modified;
/// - folds the file contents into a `cargo:rustc-env` fingerprint, so the crate
///   itself is recompiled — and every `sql!` re-analyzed — when the migrations
///   change.
///
/// `sql!` already records the migration files it read as dependencies of the
/// generated code (through `include_str!`), so editing or deleting one
/// re-analyzes the queries without this. What a file dependency cannot cover
/// is a migration **added** to the directory: Cargo watches the directory
/// itself here, which is what catches it.
///
/// `Cargo.toml` is tracked as well, so changing the migrations path also
/// triggers a rebuild.
///
/// # Panics
///
/// Panics if the crate's `Cargo.toml` cannot be read or parsed. A missing
/// `[package.metadata.typedpg]` section is *not* an error — defaults are used
/// in that case; only a genuinely malformed manifest is fatal.
pub fn track_migrations() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is always set by Cargo for build scripts");
    let manifest_path = PathBuf::from(manifest_dir);
    let cargo_toml = manifest_path.join("Cargo.toml");

    // A change to the config itself (e.g. a new migrations path) must also
    // re-run this script.
    emit_rerun_if_changed(&cargo_toml);

    let config = Config::from_cargo_toml(&cargo_toml).unwrap_or_else(|e| {
        panic!(
            "typedpg::build::track_migrations: failed to load {}: {e}",
            cargo_toml.display(),
        )
    });

    let dirs = migration_dirs(&config, &manifest_path);
    for dir in &dirs {
        // Pointing `rerun-if-changed` at a directory makes Cargo watch it
        // recursively — covering files added in the future, not just edits.
        emit_rerun_if_changed(dir);
    }

    let fingerprint = fingerprint_dirs(&dirs);
    println!("cargo:rustc-env={FINGERPRINT_ENV}={fingerprint}");
}

/// Collect every migration directory across the default and named databases,
/// including the compile-time-only `extra_migrations`. Deduplicated, with the
/// first occurrence order preserved.
fn migration_dirs(config: &Config, base: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    all.push(config.database.migrations_dir(base));
    all.extend(config.database.extra_migrations_dirs(base));
    for entry in config.databases.values() {
        all.push(entry.database.migrations_dir(base));
        all.extend(entry.database.extra_migrations_dirs(base));
    }

    let mut deduped = Vec::with_capacity(all.len());
    for dir in all {
        if !deduped.contains(&dir) {
            deduped.push(dir);
        }
    }
    deduped
}

/// Emit a `cargo:rerun-if-changed` directive for `path`.
fn emit_rerun_if_changed(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

/// SHA-256 over the name and content of every applied (`.sql`, non-`.down.sql`)
/// migration file across all directories.
///
/// `.down.sql` rollback scripts are excluded because they do not affect `sql!`
/// type-checking, matching what the proc macro itself consumes. The result is
/// stable regardless of filesystem iteration order, and missing directories
/// contribute nothing (so creating one later changes the fingerprint).
fn fingerprint_dirs(dirs: &[PathBuf]) -> String {
    let mut hasher = Sha256::new();

    for dir in dirs {
        hasher.update(dir.to_string_lossy().as_bytes());
        hasher.update([0]);

        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_up_migration(p))
            .collect();
        files.sort();

        for file in files {
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            hasher.update(name.as_bytes());
            hasher.update([0]);
            match std::fs::read(&file) {
                Ok(content) => hasher.update(&content),
                Err(_) => hasher.update(b"<unreadable>"),
            }
            hasher.update([0]);
        }
    }

    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// True for `*.sql` migration files that are not `*.down.sql` rollback scripts.
fn is_up_migration(path: &Path) -> bool {
    let name = path.to_string_lossy();
    name.ends_with(".sql") && !name.ends_with(".down.sql")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0001_a.sql"), "CREATE TABLE a ();").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        assert_eq!(fingerprint_dirs(&dirs), fingerprint_dirs(&dirs));
    }

    #[test]
    fn fingerprint_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("0001_a.sql");
        std::fs::write(&file, "SELECT 1;").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        let before = fingerprint_dirs(&dirs);
        std::fs::write(&file, "SELECT 2;").unwrap();
        assert_ne!(before, fingerprint_dirs(&dirs));
    }

    #[test]
    fn fingerprint_changes_when_file_added() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0001_a.sql"), "SELECT 1;").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        let before = fingerprint_dirs(&dirs);
        std::fs::write(dir.path().join("0002_b.sql"), "SELECT 2;").unwrap();
        assert_ne!(before, fingerprint_dirs(&dirs));
    }

    #[test]
    fn fingerprint_ignores_down_migrations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0001_a.sql"), "SELECT 1;").unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        let before = fingerprint_dirs(&dirs);
        std::fs::write(dir.path().join("0001_a.down.sql"), "DROP TABLE a;").unwrap();
        assert_eq!(before, fingerprint_dirs(&dirs));
    }

    #[test]
    fn fingerprint_handles_missing_dir() {
        let dirs = vec![PathBuf::from("/nonexistent/typedpg/migrations")];
        // Must not panic, and must be stable.
        assert_eq!(fingerprint_dirs(&dirs), fingerprint_dirs(&dirs));
    }

    #[test]
    fn migration_dirs_dedupes() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[package.metadata.typedpg.database]
migrations = "./migrations"
extra_migrations = ["./migrations"]
"#;
        let config: Config = toml.parse().unwrap();
        let dirs = migration_dirs(&config, Path::new("/project"));
        assert_eq!(dirs, vec![PathBuf::from("/project/./migrations")]);
    }
}
