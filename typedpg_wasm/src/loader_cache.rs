//! The `.sql` loaders' cache recipe, as the wasm exports' encoding.
//!
//! The logic is [`typedpg_core::loader_cache`]; this module only decodes each
//! input and encodes each output, in the hand-written format of
//! `typedpg_wasm/src/codec.rs` (a `u32` length and bytes; see the ABI table in
//! `lib.rs`). `typedpg_npm/src/core.ts` mirrors it.

use typedpg_core::loader_cache::{self, DatabaseMigrations};

use crate::codec::{Reader, Writer};

/// Decode a `cache_databases` input (the config's UTF-8 JSON) and encode the
/// databases: `u32 count; per database: string name, u32 dirs; per dir:
/// string`.
pub fn databases(input: &[u8]) -> Result<Vec<u8>, String> {
    let json = std::str::from_utf8(input)
        .map_err(|_| "typedpg_wasm: the config is not valid UTF-8".to_owned())?;
    let databases = loader_cache::databases(json)?;
    let mut out = Writer::new();
    out.u32(databases.len() as u32);
    for database in databases {
        out.string(&database.name);
        out.u32(database.dirs.len() as u32);
        for dir in database.dirs {
            out.string(&dir);
        }
    }
    Ok(out.into_bytes())
}

/// Decode a `cache_migration_files` input (each directory's raw listing) and
/// encode the selected files: `u32 count; per file: u32 directory, string
/// name`.
pub fn migration_files(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = Reader::new(input);
    let count = reader.u32()? as usize;
    let mut directories = Vec::with_capacity(count);
    for _ in 0..count {
        let names = reader.u32()? as usize;
        let mut directory = Vec::with_capacity(names);
        for _ in 0..names {
            directory.push(reader.string()?);
        }
        directories.push(directory);
    }
    reader.finish()?;
    let files = loader_cache::migration_files(&directories);
    let mut out = Writer::new();
    out.u32(files.len() as u32);
    for file in files {
        out.u32(file.directory as u32);
        out.string(&file.name);
    }
    Ok(out.into_bytes())
}

/// Decode a `cache_schema_identity` input (the config's bytes and each
/// database's selected migrations) and encode the hex identity.
pub fn schema_identity(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = Reader::new(input);
    let config_len = reader.u32()? as usize;
    let config = reader.bytes(config_len)?.to_vec();
    let count = reader.u32()? as usize;
    let mut databases: Vec<DatabaseMigrations> = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string()?;
        let migrations = reader.u32()? as usize;
        let mut files = Vec::with_capacity(migrations);
        for _ in 0..migrations {
            let file = reader.string()?;
            let sql_len = reader.u32()? as usize;
            let sql = reader.bytes(sql_len)?.to_vec();
            files.push((file, sql));
        }
        databases.push((name, files));
    }
    reader.finish()?;
    Ok(loader_cache::schema_identity(&config, &databases).into_bytes())
}

/// Decode a `cache_key` input (identity, relative path, content) and encode
/// the hex key.
pub fn key(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = Reader::new(input);
    let identity = reader.string()?;
    let relative_path = reader.string()?;
    let content = reader.string()?;
    reader.finish()?;
    Ok(loader_cache::key(&identity, &relative_path, &content).into_bytes())
}
