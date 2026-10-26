//! Stack for deep queries.
//!
//! The analyzer recurses once per level of a query's tree, a few kilobytes
//! a level, and libpg_query takes trees 10,000 levels deep — far more than
//! a thread's stack holds (2 MB for a spawned one, 8 MB for the main
//! thread). So each analysis runs on a segment `stacker` grows, sized for
//! the deepest query libpg_query takes; and the expression and SELECT
//! analyses, through which most of the recursion goes, grow another when
//! one runs low.
//!
//! Measured, analyzing the deepest query of each shape libpg_query takes
//! needs up to about 70 MB: a UNION chain of 8,734 arms, ~7.9 kB an arm
//! (4,995 nested calls ~58 MB, 4,995-term `1 + 1 + …` ~43 MB, 1,665 nested
//! subqueries ~30 MB). A segment is mapped, not allocated: a query only
//! uses the pages it reaches.

/// The stack an analysis starts with at least: what the deepest query
/// measured needs, with margin.
const RED_ZONE: usize = 96 << 20;

/// The segment grown when less is left.
const SEGMENT: usize = 256 << 20;

/// The stack a recursive step needs before it grows more.
const STEP_RED_ZONE: usize = 1 << 20;

/// The segment a recursive step grows.
const STEP_SEGMENT: usize = 64 << 20;

/// Run `f`, an analysis, on enough stack for the deepest query.
pub(crate) fn with_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}

/// Run `f`, a step of a recursive analysis, growing the stack first when
/// it runs low.
pub(crate) fn step<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(STEP_RED_ZONE, STEP_SEGMENT, f)
}
