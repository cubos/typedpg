//! Proofs the simplification rules share. Each answers only from what PG
//! enforces at every statement — `attnotnull`, validated non-deferrable
//! constraints — never from what the analyzer's nullability inference
//! trusts (CHECKs, `NOT VALID`, deferrable constraints). Every helper errs
//! on the side of "not proven".

use typedpg_pg_query::protobuf::{ColumnRef, Node};

use super::Ctx;
use crate::oid::PgClassOid;

/// A column reference resolved to the base-table column it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedColumn {
    pub rel: PgClassOid,
    pub attnum: i16,
    /// An outer join (or a grouping set) can make the reference NULL even
    /// where the column can't be.
    pub nullable_side: bool,
}

/// The FROM scope of one query level (a SELECT, or an UPDATE/DELETE),
/// with the scopes of the levels it is nested in.
pub(crate) struct FromScope<'a> {
    _parent: Option<&'a FromScope<'a>>,
}

impl<'a> FromScope<'a> {
    /// The scope a SELECT's clauses see.
    pub(crate) fn of_select(
        _ctx: &Ctx<'_>,
        _select: &'a typedpg_pg_query::protobuf::SelectStmt,
        parent: Option<&'a FromScope<'a>>,
    ) -> FromScope<'a> {
        FromScope { _parent: parent }
    }

    /// The column `col` reads, or `None` when it isn't provably a plain
    /// base-table column.
    pub(crate) fn resolve(&self, _ctx: &Ctx<'_>, _col: &ColumnRef) -> Option<ResolvedColumn> {
        None
    }
}

/// Whether `expr`, evaluated in `scope`, can never be NULL by what PG
/// enforces.
pub(crate) fn strictly_not_null(_ctx: &Ctx<'_>, _expr: &Node, _scope: &FromScope<'_>) -> bool {
    false
}

/// Whether `node` may call a volatile function (or is otherwise
/// nondeterministic). Errs towards `true`.
pub(crate) fn has_volatile(_ctx: &Ctx<'_>, _node: &Node) -> bool {
    true
}
