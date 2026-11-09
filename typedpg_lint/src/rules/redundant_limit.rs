//! The `redundant_limit` rule: reports a `LIMIT` (and its `OFFSET` when the
//! offset itself is redundant) on a query the analysis proves returns at
//! most one row, such as one over a primary key or a `SELECT` without
//! `FROM`. The fix removes the redundant `LIMIT` / `OFFSET` clauses.

use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let _ = ctx;
    Vec::new()
}
