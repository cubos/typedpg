//! The npm runtime's copy of the migration runner, as WebAssembly.
//!
//! `@cubos/typedpg`'s runner and the Rust one must agree: they share the
//! pure logic — how a migration splits into statements, what drift is, how a
//! name or a tracking table validates, how errors and drift read — and the
//! orchestration itself. This crate exports both once, from
//! [`typedpg_core`], so the two runners cannot drift apart: the migration
//! machine ([`typedpg_core::migrations::machine`]) decides every query,
//! script and message, and the TypeScript side only performs each step on
//! the caller's driver.
//!
//! # ABI
//!
//! There is no `wasm-bindgen`: the module needs no imports, and the JS side
//! is a few small wrappers around the exports below. All strings are UTF-8.
//! Pointers are byte offsets into this module's exported linear memory.
//!
//! - `alloc(len: usize) -> usize` — a buffer of `max(len, 1)` bytes, for JS
//!   to write an input into. `dealloc(ptr, len)` frees it; both sides must
//!   pass the same `len`.
//! - `output_ptr() -> usize`, `output_len() -> usize` — the single result
//!   buffer. JS is single-threaded, and reads it before the next call; no
//!   other call may run in between.
//! - Every operation returns `0` on success and `1` on failure, the UTF-8
//!   message in the output buffer. The pure helpers' inputs come from JS
//!   strings, so their only failure is input that is not UTF-8; the cache
//!   operations also reject an invalid config or a malformed encoded input,
//!   and the machine also rejects a malformed binary input. Anything else (an
//!   allocation failure, a bug) traps, which JS sees as a
//!   `WebAssembly.RuntimeError` — never silently.
//!
//! On success the output buffer holds, per operation:
//!
//! | export | output |
//! |--------|--------|
//! | `parse_migration_name`, `strip_bom` | UTF-8 text |
//! | `validate_table_name` | empty |
//! | `compare_migration_names` | one byte: `<`, `=` or `>` |
//! | `has_no_transaction_marker` | one byte: `0` or `1` |
//! | `migrator_new` | nothing; the machine handle is the return value (`0` on failure) |
//! | `migrator_step` | the encoded step |
//! | `migrator_free` | nothing |
//! | `cache_databases` | the config's databases |
//! | `cache_migration_files` | the selected migration file names |
//! | `cache_schema_identity`, `cache_key` | UTF-8 hex |
//!
//! # The loaders' cache
//!
//! The `.sql` loaders' cache recipe ([`typedpg_core::loader_cache`]) is
//! exported so the npm runtime computes the same keys as `typedpg build`.
//! Inputs and outputs continue the same format:
//!
//! ```text
//! cache_databases: the config's UTF-8 JSON (the whole input)
//!   output: u32 databases; per database: string name, u32 dirs;
//!           per dir: string
//! cache_migration_files:
//!   u32 directories; per directory: u32 names; per name: string
//!   output: u32 files; per file: u32 directory, string name
//! cache_schema_identity:
//!   u32 config_len, bytes; u32 databases;
//!   per database: string name, u32 migrations;
//!   per migration: string name, bytes
//!   output: the hex identity
//! cache_key: string identity, string relative_path, string content
//!   output: the hex key
//! ```
//!
//! # The migration machine
//!
//! `migrator_new(input_ptr, input_len) -> usize` decodes an input and returns
//! a handle (`0` on failure, the message in the output buffer).
//! `migrator_step(handle, reply_ptr, reply_len) -> i32` decodes the reply,
//! advances the machine and puts the encoded step in the output buffer.
//! `migrator_free(handle)` drops it; handles let several runs interleave
//! across awaits in one JS process. `typedpg_npm/src/core.ts` encodes and
//! decodes the same format.
//!
//! The format is hand-written — a tag byte, little-endian `u32` lengths and
//! a null flag for an optional string — rather than serde or bincode, to
//! keep the module small. `string` is a `u32` byte length followed by that
//! many UTF-8 bytes; `optional X` is a `u8` tag (`0` absent, `1` present)
//! followed by `X` when present, so a null differs from an empty string.
//!
//! Input (`migrator_new`):
//!
//! ```text
//! u8 action                    0 Up, 1 Status, 2 Revert, 3 Accept
//!   Revert: optional string name, u8 force
//!   Accept: string name
//! string table
//! i64 lock_id
//! u8 use_transaction
//! u8 fail_on_drift
//! u32 migration count
//!   per migration: string name, string sql, optional string down_sql,
//!                  u8 no_transaction, u8 down_no_transaction
//! ```
//!
//! Reply (`migrator_step`'s input):
//!
//! ```text
//! u8 tag                       0 Start, 1 Rows, 2 Done, 3 DbError
//!   Rows: u32 rows; per row: u32 cols; per value: optional string
//!   DbError: optional string code, string message, optional string severity,
//!            optional string detail, optional string hint,
//!            optional u32 position, optional u32 internal_position,
//!            optional string internal_query
//! ```
//!
//! Step (`migrator_step`'s output):
//!
//! ```text
//! u8 tag                       0 Query, 1 Execute, 2 Sleep, 3 Warn,
//!                              4 Finish, 5 Fail
//!   Query:   string sql, u32 params; per param: optional string
//!   Execute: string sql
//!   Sleep:   u64 ms
//!   Warn:    string message
//!   Finish:  u8 outcome
//!              0 Applied:  u32 names; per name: string
//!              1 Status:   u32 rows; per row: string name, u8 applied,
//!                          optional string applied_at, u8 drifted
//!              2 Reverted: string name
//!              3 Accepted
//!   Fail:    u8 failure         0 Message: string, 1 Db
//! ```

use std::cmp::Ordering;
use std::sync::Mutex;

use typedpg_core::migrations::machine::Migrator;
use typedpg_core::{config, migration_marker, migrations};

mod codec;
mod loader_cache;

/// The output buffer. A `Mutex` because statics must be `Sync`; the module
/// is single-threaded, so it is never contended.
static OUTPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The live migration machines, by handle (`index + 1`). A `Vec` of
/// `Option`s so freeing leaves a reusable hole and a stale handle finds
/// nothing rather than another run's machine.
static MIGRATORS: Mutex<Vec<Option<Box<Migrator>>>> = Mutex::new(Vec::new());

/// The message for an input JS passed as invalid UTF-8 (impossible through
/// `TextEncoder`, but the ABI allows any bytes).
const INVALID_UTF8: &str = "typedpg_wasm: input is not valid UTF-8";

/// Replace the output buffer with `bytes`; the operation succeeded.
fn set_output(bytes: Vec<u8>) -> i32 {
    let mut out = match OUTPUT.lock() {
        Ok(out) => out,
        // A panic while the buffer was locked poisons it; the bytes are
        // still sound, so recover instead of trapping on every later call.
        Err(poisoned) => poisoned.into_inner(),
    };
    *out = bytes;
    0
}

/// Put `message` in the output buffer and report failure.
fn fail(message: &str) -> i32 {
    set_output(message.as_bytes().to_vec());
    1
}

/// The UTF-8 text at `ptr..ptr + len`.
///
/// # Safety
///
/// `ptr..ptr + len` must be initialized memory of this module, as the ABI
/// requires; it must stay alive for the returned borrow.
unsafe fn text_at<'a>(ptr: usize, len: usize) -> Option<&'a str> {
    std::str::from_utf8(unsafe { bytes_at(ptr, len) }?).ok()
}

/// The bytes at `ptr..ptr + len`.
///
/// # Safety
///
/// `ptr..ptr + len` must be initialized memory of this module, as the ABI
/// requires; it must stay alive for the returned borrow.
unsafe fn bytes_at<'a>(ptr: usize, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr == 0 {
        return None;
    }
    // SAFETY: the caller guarantees the range is initialized and in bounds.
    Some(unsafe { std::slice::from_raw_parts(ptr as *const u8, len) })
}

/// Allocate `len` bytes for JS to write an input into.
///
/// `len` is rounded up to one byte so JS can always call `dealloc` with the
/// same length it passed here, even for an empty input.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> usize {
    let mut buffer = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr as usize
}

/// Free a buffer returned by [`alloc`], with the same `len` it was allocated
/// with.
#[unsafe(no_mangle)]
pub extern "C" fn dealloc(ptr: usize, len: usize) {
    if ptr == 0 {
        return;
    }
    // SAFETY: `ptr` came from `alloc(len)`, which allocated
    // `max(len, 1)` bytes with the global allocator; the capacity must match.
    unsafe {
        drop(Vec::from_raw_parts(ptr as *mut u8, 0, len.max(1)));
    }
}

/// The output buffer's address, valid until the next operation.
#[unsafe(no_mangle)]
pub extern "C" fn output_ptr() -> usize {
    match OUTPUT.lock() {
        Ok(out) => out.as_ptr() as usize,
        Err(poisoned) => poisoned.into_inner().as_ptr() as usize,
    }
}

/// The output buffer's length in bytes.
#[unsafe(no_mangle)]
pub extern "C" fn output_len() -> usize {
    match OUTPUT.lock() {
        Ok(out) => out.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    }
}

/// A migration file stem's version (numeric prefix), or why it is malformed.
#[unsafe(no_mangle)]
pub extern "C" fn parse_migration_name(
    name_ptr: usize,
    name_len: usize,
    path_ptr: usize,
    path_len: usize,
) -> i32 {
    let Some(name) = (unsafe { text_at(name_ptr, name_len) }) else {
        return fail(INVALID_UTF8);
    };
    let Some(path) = (unsafe { text_at(path_ptr, path_len) }) else {
        return fail(INVALID_UTF8);
    };
    match migrations::parse_migration_name(name) {
        Ok(version) => set_output(version.as_bytes().to_vec()),
        Err(error) => fail(&error.message(path)),
    }
}

/// Compare two migration names by version, then by name.
#[unsafe(no_mangle)]
pub extern "C" fn compare_migration_names(
    a_ptr: usize,
    a_len: usize,
    b_ptr: usize,
    b_len: usize,
) -> i32 {
    let Some(a) = (unsafe { text_at(a_ptr, a_len) }) else {
        return fail(INVALID_UTF8);
    };
    let Some(b) = (unsafe { text_at(b_ptr, b_len) }) else {
        return fail(INVALID_UTF8);
    };
    ordering(migrations::compare_migration_names(a, b))
}

/// Validate a tracking-table name as `MigrationsConfig::validate` does; on
/// failure the output holds the message.
#[unsafe(no_mangle)]
pub extern "C" fn validate_table_name(ptr: usize, len: usize) -> i32 {
    let Some(table) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    match config::validate_table_name(table) {
        Ok(()) => set_output(Vec::new()),
        Err(error) => fail(&error.to_string()),
    }
}

/// The input without a leading byte-order mark.
#[unsafe(no_mangle)]
pub extern "C" fn strip_bom(ptr: usize, len: usize) -> i32 {
    let Some(sql) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    set_output(migration_marker::strip_bom(sql).as_bytes().to_vec())
}

/// Whether the input's first line is the `-- no-transaction` marker.
#[unsafe(no_mangle)]
pub extern "C" fn has_no_transaction_marker(ptr: usize, len: usize) -> i32 {
    let Some(sql) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    set_output(vec![u8::from(migration_marker::has_no_transaction_marker(
        sql,
    ))])
}

/// The databases a config declares, as [`typedpg_core::loader_cache::databases`]
/// reads it.
#[unsafe(no_mangle)]
pub extern "C" fn cache_databases(ptr: usize, len: usize) -> i32 {
    cache_export(ptr, len, loader_cache::databases)
}

/// The files of each directory's listing that the cache recipe hashes.
#[unsafe(no_mangle)]
pub extern "C" fn cache_migration_files(ptr: usize, len: usize) -> i32 {
    cache_export(ptr, len, loader_cache::migration_files)
}

/// The schema identity of a config and its migrations.
#[unsafe(no_mangle)]
pub extern "C" fn cache_schema_identity(ptr: usize, len: usize) -> i32 {
    cache_export(ptr, len, loader_cache::schema_identity)
}

/// The cache key of a `.sql` file.
#[unsafe(no_mangle)]
pub extern "C" fn cache_key(ptr: usize, len: usize) -> i32 {
    cache_export(ptr, len, loader_cache::key)
}

/// Run a cache operation over the input at `ptr..ptr + len`.
fn cache_export(ptr: usize, len: usize, f: fn(&[u8]) -> Result<Vec<u8>, String>) -> i32 {
    let Some(input) = (unsafe { bytes_at(ptr, len) }) else {
        return fail("typedpg_wasm: the cache input is not readable");
    };
    match f(input) {
        Ok(output) => set_output(output),
        Err(message) => fail(&message),
    }
}

/// A comparison as the one byte JS reads: `<`, `=` or `>`.
fn ordering(ordering: Ordering) -> i32 {
    set_output(vec![match ordering {
        Ordering::Less => b'<',
        Ordering::Equal => b'=',
        Ordering::Greater => b'>',
    }])
}

/// Create a migration machine from an encoded input; `0` on failure, the
/// message in the output buffer.
#[unsafe(no_mangle)]
pub extern "C" fn migrator_new(ptr: usize, len: usize) -> usize {
    let Some(bytes) = (unsafe { bytes_at(ptr, len) }) else {
        fail("typedpg_wasm: the migration input is not readable");
        return 0;
    };
    match codec::decode_input(bytes) {
        Ok(input) => {
            let mut migrators = match MIGRATORS.lock() {
                Ok(migrators) => migrators,
                Err(poisoned) => poisoned.into_inner(),
            };
            migrators.push(Some(Box::new(Migrator::new(input))));
            migrators.len()
        }
        Err(message) => {
            fail(&message);
            0
        }
    }
}

/// Advance a migration machine with an encoded reply; the encoded step goes
/// in the output buffer.
#[unsafe(no_mangle)]
pub extern "C" fn migrator_step(handle: usize, ptr: usize, len: usize) -> i32 {
    let Some(bytes) = (unsafe { bytes_at(ptr, len) }) else {
        return fail("typedpg_wasm: the migration reply is not readable");
    };
    let reply = match codec::decode_reply(bytes) {
        Ok(reply) => reply,
        Err(message) => return fail(&message),
    };
    let step = {
        let mut migrators = match MIGRATORS.lock() {
            Ok(migrators) => migrators,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(index) = handle.checked_sub(1) else {
            drop(migrators);
            return fail("typedpg_wasm: unknown migration handle");
        };
        let Some(Some(migrator)) = migrators.get_mut(index) else {
            drop(migrators);
            return fail("typedpg_wasm: unknown migration handle");
        };
        migrator.step(reply)
    };
    set_output(codec::encode_step(&step))
}

/// Drop a migration machine. Freeing twice, or an unknown handle, is a
/// no-op.
#[unsafe(no_mangle)]
pub extern "C" fn migrator_free(handle: usize) {
    let Some(index) = handle.checked_sub(1) else {
        return;
    };
    let mut migrators = match MIGRATORS.lock() {
        Ok(migrators) => migrators,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(slot) = migrators.get_mut(index) {
        *slot = None;
    }
}
