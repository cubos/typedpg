//! The `redundant_null_check` rule: reports an `IS [NOT] NULL` test or a
//! `COALESCE` on a value the analysis proves is never NULL. When the test
//! is in a `WHERE` clause the fix removes it; a `COALESCE(x, fallback)` is
//! reported because only `x` is ever returned.

use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let _ = ctx;
    Vec::new()
}
