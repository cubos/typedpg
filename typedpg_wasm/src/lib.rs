//! The npm runtime's copy of the migration helpers, as WebAssembly.
//!
//! `@cubos/typedpg`'s runner reimplements the Rust runner's pure logic — how
//! a migration splits into statements, what drift is, how a name or a
//! tracking table validates, how errors and drift read. This crate exports
//! that logic once, from [`typedpg_core`], so the two runners cannot drift
//! apart. The runner's I/O (the advisory lock, transactions, inserts, …)
//! stays in TypeScript, on the caller's driver.
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
//!   message in the output buffer. Inputs come from JS strings, so the only
//!   failure is input that is not UTF-8; anything else (an allocation
//!   failure, a bug) traps, which JS sees as a `WebAssembly.RuntimeError` —
//!   never silently.
//!
//! On success the output buffer holds, per operation:
//!
//! | export | output |
//! |--------|--------|
//! | `split_statements` | little-endian `(start: u32, len: u32)` pairs into the input |
//! | `normalize`, `hash`, `drift_message`, `locate_position`, `strip_bom` | UTF-8 text |
//! | `hash_version` | little-endian `i32` |
//! | `parse_migration_name` | the UTF-8 version (the numeric prefix) |
//! | `validate_table_name` | empty |
//! | `compare_versions`, `compare_migration_names` | one byte: `<`, `=` or `>` |
//! | `has_no_transaction_marker` | one byte: `0` or `1` |

use std::cmp::Ordering;
use std::sync::Mutex;

use typedpg_core::{config, migration_marker, migrations};

/// The output buffer. A `Mutex` because statics must be `Sync`; the module
/// is single-threaded, so it is never contended.
static OUTPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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
    if len == 0 {
        return Some("");
    }
    // SAFETY: the caller guarantees the range is initialized and in bounds.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    std::str::from_utf8(bytes).ok()
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

/// The statements of the input, as `(start, len)` byte pairs.
#[unsafe(no_mangle)]
pub extern "C" fn split_statements(ptr: usize, len: usize) -> i32 {
    let Some(sql) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    let mut out = Vec::new();
    for statement in migrations::split_statements(sql) {
        // `split_statements` returns slices of `sql`.
        let start = statement.as_ptr() as usize - sql.as_ptr() as usize;
        out.extend_from_slice(&(start as u32).to_le_bytes());
        out.extend_from_slice(&(statement.len() as u32).to_le_bytes());
    }
    set_output(out)
}

/// The normalized text drift detection compares.
#[unsafe(no_mangle)]
pub extern "C" fn normalize(ptr: usize, len: usize) -> i32 {
    let Some(sql) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    set_output(migrations::normalize(sql).into_bytes())
}

/// The stored drift hash of the input.
#[unsafe(no_mangle)]
pub extern "C" fn hash(ptr: usize, len: usize) -> i32 {
    let Some(sql) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    set_output(migrations::hash(sql).into_bytes())
}

/// The normalization whose hashes the tracking table stores.
#[unsafe(no_mangle)]
pub extern "C" fn hash_version() -> i32 {
    set_output(migrations::HASH_VERSION.to_le_bytes().to_vec())
}

/// The drift message: name, diff of `stored` against `file` (when
/// `has_stored` is non-zero) and what to do about it.
#[unsafe(no_mangle)]
pub extern "C" fn drift_message(
    name_ptr: usize,
    name_len: usize,
    stored_ptr: usize,
    stored_len: usize,
    has_stored: u32,
    file_ptr: usize,
    file_len: usize,
    fail_on_drift: u32,
) -> i32 {
    let Some(name) = (unsafe { text_at(name_ptr, name_len) }) else {
        return fail(INVALID_UTF8);
    };
    let stored = if has_stored == 0 {
        None
    } else {
        match unsafe { text_at(stored_ptr, stored_len) } {
            Some(stored) => Some(stored),
            None => return fail(INVALID_UTF8),
        }
    };
    let Some(file) = (unsafe { text_at(file_ptr, file_len) }) else {
        return fail(INVALID_UTF8);
    };
    set_output(migrations::drift_message(name, stored, file, fail_on_drift != 0).into_bytes())
}

/// PG's error position as a line and a caret, over the whole migration.
///
/// `base` is the byte offset in `sql` of the text that was sent; `position`
/// is PG's 1-based character index into it.
#[unsafe(no_mangle)]
pub extern "C" fn locate_position(ptr: usize, len: usize, base: u32, position: u32) -> i32 {
    let Some(sql) = (unsafe { text_at(ptr, len) }) else {
        return fail(INVALID_UTF8);
    };
    set_output(migrations::locate_position(sql, base as usize, position).into_bytes())
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

/// Compare two migration versions as numbers (`2` before `10`).
#[unsafe(no_mangle)]
pub extern "C" fn compare_versions(a_ptr: usize, a_len: usize, b_ptr: usize, b_len: usize) -> i32 {
    let Some(a) = (unsafe { text_at(a_ptr, a_len) }) else {
        return fail(INVALID_UTF8);
    };
    let Some(b) = (unsafe { text_at(b_ptr, b_len) }) else {
        return fail(INVALID_UTF8);
    };
    ordering(migrations::compare_versions(a, b))
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

/// A comparison as the one byte JS reads: `<`, `=` or `>`.
fn ordering(ordering: Ordering) -> i32 {
    set_output(vec![match ordering {
        Ordering::Less => b'<',
        Ordering::Equal => b'=',
        Ordering::Greater => b'>',
    }])
}
