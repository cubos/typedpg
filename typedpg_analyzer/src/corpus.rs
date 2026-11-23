//! The corpus recorder (the `record_corpus` feature): every query
//! [`crate::PgCatalog::analyze`] accepts is appended, with the migrations
//! its catalog applied before it, to a JSON-lines file in the directory
//! `$TYPEDPG_RECORD_CORPUS` names (nothing is recorded without it). A
//! catalog's migrations are written once, as
//! `{"schema": <id>, "ddl": [...]}`, and each query as
//! `{"schema": <id>, "query": "..."}`; the lint oracle replays them.
//! Every successful migration also writes the list of the catalog's
//! successful migrations so far, as `{"migrations": <id>, "ddl": [...]}`:
//! the migration oracle (`typedpg_lint/tests/migration_oracle.rs`) lints
//! them.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::sync::{Arc, Mutex};

/// The schemas this process wrote already, and its file.
static STATE: Mutex<Option<(HashSet<u64>, std::fs::File)>> = Mutex::new(None);

/// Record `migrations`, the successful migrations of a catalog so far.
pub(crate) fn record_migrations(migrations: &[Arc<str>]) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ("migrations", migrations).hash(&mut hasher);
    let id = hasher.finish();
    let ddl: Vec<&str> = migrations.iter().map(|d| &**d).collect();
    write(id, |out| {
        out.push_str(&serde_json::json!({ "migrations": id.to_string(), "ddl": ddl }).to_string());
        out.push('\n');
    });
}

/// Append what `line` writes, once per `id` and process.
fn write(id: u64, line: impl FnOnce(&mut String)) {
    let Some(dir) = std::env::var_os("TYPEDPG_RECORD_CORPUS") else {
        return;
    };
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.is_none() {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("corpus-{}.jsonl", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        *state = Some((HashSet::new(), file));
    }
    let Some((written, file)) = state.as_mut() else {
        return;
    };
    if !written.insert(id) {
        return;
    }
    let mut out = String::new();
    line(&mut out);
    let _ = file.write_all(out.as_bytes());
}

pub(crate) fn record(ddl: &[Arc<str>], query: &str) {
    let Some(dir) = std::env::var_os("TYPEDPG_RECORD_CORPUS") else {
        return;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ddl.hash(&mut hasher);
    let id = hasher.finish();
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.is_none() {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("corpus-{}.jsonl", std::process::id()));
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        *state = Some((HashSet::new(), file));
    }
    let Some((written, file)) = state.as_mut() else {
        return;
    };
    let mut out = String::new();
    if written.insert(id) {
        let ddl: Vec<&str> = ddl.iter().map(|d| &**d).collect();
        out.push_str(&serde_json::json!({ "schema": id.to_string(), "ddl": ddl }).to_string());
        out.push('\n');
    }
    out.push_str(&serde_json::json!({ "schema": id.to_string(), "query": query }).to_string());
    out.push('\n');
    let _ = file.write_all(out.as_bytes());
}
