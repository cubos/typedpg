//! Stack for deep queries.
//!
//! Parsing, decoding and later walking a parse tree recurse once per level
//! of it, and libpg_query takes up to 10,000 levels
//! (`PG_QUERY_PROTOBUF_MAX_DEPTH`): `SELECT 1 + 1 + …` with 4,995 terms. A
//! thread's stack (2 MB for a spawned one, 8 MB for the main thread) holds
//! a few hundred. So every call into libpg_query runs with at least
//! [`RED_ZONE`] of stack, on a segment `stacker` grows when there is less,
//! and libpg_query measures its own budget (`max_stack_depth`) against what
//! is left of it (`typedpg_remaining_stack`, patches.rs).

/// What a call into libpg_query gets at least: measured, parsing the
/// deepest query libpg_query takes and decoding it needs 10.6 MB (libpg_query's
/// budget is capped at 2 MB, plus its 512 kB of slop; prost's decoding
/// takes about 800 bytes a level).
const RED_ZONE: usize = 16 << 20;

/// The segment grown when less is left.
const SEGMENT: usize = 32 << 20;

/// Run `f`, a call into libpg_query, with at least [`RED_ZONE`] of stack.
pub(crate) fn with_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}

/// The stack left, for libpg_query's budget (see the module docs); 0 when
/// unknown, which keeps libpg_query's default.
#[unsafe(no_mangle)]
extern "C" fn typedpg_remaining_stack() -> usize {
    stacker::remaining_stack().unwrap_or(0)
}
