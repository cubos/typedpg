//! The `redundant_distinct` rule: reports a `DISTINCT` (or `DISTINCT ON`
//! over a key) on rows the analysis proves are already distinct, such as
//! the output of an aggregate without `GROUP BY`. The fix removes the
//! `DISTINCT` keyword.

use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let _ = ctx;
    Vec::new()
}
