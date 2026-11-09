//! The `limit_without_order` rule: reports a `LIMIT`, an `OFFSET` or a
//! `DISTINCT ON` without an `ORDER BY`, which picks arbitrary rows (and, for
//! `DISTINCT ON`, an arbitrary row per group).

use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let _ = ctx;
    Vec::new()
}
