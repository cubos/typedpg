//! Stack for deep queries.
//!
//! Converting a C parse tree recurses once per level of it, and PostgreSQL's
//! trees go up to 10,000 levels (the converter's limit, as libpg_query's
//! was): `SELECT 1 + 1 + …` with 4,995 terms. A thread's
//! stack (2 MB for a spawned one, 8 MB for the main thread) holds far fewer.
//! So every call into the parser runs with at least [`RED_ZONE`] of stack, on
//! a segment `stacker` grows when there is less, and PostgreSQL measures its
//! own budget (`max_stack_depth`) against what is left of it
//! (`typedpg_remaining_stack`, csrc/pg/shim.c).
//!
//! The conversion of the tree into the AST grows the stack itself, segment
//! by segment, as it descends (`grow` in convert.rs): only PostgreSQL's own
//! budget (capped at 2 MB, plus its 512 kB of slop) has to fit up front.

/// What a call into the parser gets at least.
const RED_ZONE: usize = 16 << 20;

/// The segment grown when less is left.
const SEGMENT: usize = 32 << 20;

/// Run `f`, a call into the parser (and the conversion of its tree), with at
/// least [`RED_ZONE`] of stack.
pub(crate) fn with_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}

/// The stack left, for PostgreSQL's budget (see the module docs); 0 when
/// unknown, which keeps PostgreSQL's default.
#[unsafe(no_mangle)]
extern "C" fn typedpg_remaining_stack() -> usize {
    stacker::remaining_stack().unwrap_or(0)
}
