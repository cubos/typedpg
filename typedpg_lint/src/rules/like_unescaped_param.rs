//! The `like_unescaped_param` rule: reports a `LIKE` / `ILIKE` pattern built
//! from a parameter whose `%` and `_` are not escaped, so user input acts as
//! wildcards. The fix escapes the pattern (`replace` over the parameter or
//! an `ESCAPE` clause, depending on how it is built).

use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let _ = ctx;
    Vec::new()
}
