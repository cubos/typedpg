//! What a query level guarantees about its rows, beyond each column's
//! nullability: that it yields at least one row (an aggregate without
//! GROUP BY, a FROM-less SELECT, a lookup a foreign key guarantees), and —
//! through each output column's [`Origin`] — which base-table row its
//! values come from. The enclosing level uses these to tell an outer join
//! that always finds a match, a scalar subquery that always returns a row,
//! or a foreign key followed through a subquery, CTE or view.

use std::cell::RefCell;
use std::collections::BTreeSet;

use super::*;
use crate::nonnull::{StrictLog, StrictNode};
use crate::scope::{Origin, ScopeColumn};

/// What one analyzed query level guarantees (see the module docs).
#[derive(Debug, Clone, Default)]
pub(crate) struct LevelSummary {
    /// The level yields at least one row, for each row of the entries it
    /// reads as LATERAL or outer references.
    pub min_one_row: bool,
}

thread_local! {
    /// The summary of the query level analyzed last: set when a SELECT
    /// level's analysis ends, read by its caller right after.
    static LAST_LEVEL: RefCell<LevelSummary> = RefCell::new(LevelSummary::default());
}

pub(crate) fn set_level_summary(summary: LevelSummary) {
    LAST_LEVEL.with(|l| *l.borrow_mut() = summary);
}

/// The summary of the level whose analysis just returned.
pub(crate) fn take_level_summary() -> LevelSummary {
    LAST_LEVEL.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// A literal `TRUE` (`WHERE true`, `ON true`).
pub(crate) fn is_true_const(n: &protobuf::Node) -> bool {
    matches!(
        n.node.as_ref(),
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Boolval(b)),
            isnull: false,
            ..
        })) if b.boolval
    )
}

/// A LIMIT / OFFSET operand that is a constant: `Some(None)` when absent,
/// `ALL` or NULL, `Some(Some(n))` for an integer.
fn const_count(n: &Option<Box<protobuf::Node>>) -> Option<Option<i64>> {
    match n.as_deref().map(|n| n.node.as_ref()) {
        None => Some(None),
        Some(Some(node::Node::AConst(ac))) if ac.isnull => Some(None),
        Some(Some(node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Ival(i)),
            ..
        }))) => Some(Some(i64::from(i.ival))),
        _ => None,
    }
}

/// LIMIT ALL / NULL / n ≥ 1 and OFFSET NULL / 0: a non-empty result
/// stays non-empty.
pub(crate) fn limit_keeps_a_row(sel: &protobuf::SelectStmt) -> bool {
    matches!(const_count(&sel.limit_count), Some(None) | Some(Some(1..)))
        && matches!(const_count(&sel.limit_offset), Some(None) | Some(Some(0)))
}

/// No LIMIT (or ALL / NULL) and OFFSET NULL / 0: every row stays.
fn limit_keeps_every_row(sel: &protobuf::SelectStmt) -> bool {
    matches!(const_count(&sel.limit_count), Some(None))
        && matches!(const_count(&sel.limit_offset), Some(None) | Some(Some(0)))
}

/// A table a foreign key can be relied on to find a referenced row in:
/// a plain or partitioned table, without row security hiding rows.
pub(crate) fn fk_parent_ok(snapshot: &PgCatalog, rel: crate::oid::PgClassOid) -> bool {
    snapshot.pg_class.get(&rel).is_some_and(|c| {
        matches!(
            c.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        )
    }) && !snapshot.row_security.contains(&rel)
}

/// Whether table `child_rel` has an enforced foreign key into `parent_rel`
/// over exactly the `(child column, parent column)` pairs `wanted`, which
/// the migrations leave in force (no `DISABLE TRIGGER ALL` on either
/// table). The constraint is assumed to hold as declared, `NOT VALID` or
/// deferrable as it may be.
pub(crate) fn fk_follows(
    snapshot: &PgCatalog,
    child_rel: crate::oid::PgClassOid,
    parent_rel: crate::oid::PgClassOid,
    wanted: &BTreeSet<(String, String)>,
) -> bool {
    if snapshot.ri_triggers_disabled.contains(&child_rel)
        || snapshot.ri_triggers_disabled.contains(&parent_rel)
    {
        return false;
    }
    let attname = |rel, attnum: i16| {
        snapshot
            .attributes_of(rel)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
    };
    snapshot.pg_constraint.values().any(|con| {
        con.contype == crate::pg_catalog::ConType::ForeignKey
            && con.conrelid == child_rel
            && con.confrelid == Some(parent_rel)
            && con.conenforced
            && !con.conperiod
            // Not a partition's internal clone (`conparentid`): the one
            // referencing a partitioned table also has clones pointing
            // at each partition, which a row may be in another of.
            && snapshot
                .fk_details
                .get(&con.oid)
                .is_none_or(|d| d.parent.is_none())
            && {
                let fk: Option<BTreeSet<(String, String)>> = con
                    .conkey
                    .iter()
                    .zip(&con.confkey)
                    .map(|(&c, &p)| Some((attname(child_rel, c)?, attname(parent_rel, p)?)))
                    .collect();
                fk.is_some_and(|fk| &fk == wanted)
            }
    })
}

/// Whether `=` over `type_oid` is a btree equality — reflexive, so a
/// non-NULL value equals itself.
pub(crate) fn reflexive_eq(snapshot: &PgCatalog, type_oid: crate::oid::PgTypeOid) -> bool {
    snapshot
        .find_operator("=", Some(type_oid), type_oid)
        .is_some_and(|op| {
            snapshot
                .pg_amop
                .iter()
                .any(|a| a.amopopr == op.oid && a.amopmethod == "btree" && a.amopstrategy == 3)
        })
}

/// Whether every row scan `inner` reads is among those scan `outer` reads:
/// the same table, read in full by `outer`, and with inheritance children
/// only if `outer` reads them too.
pub(crate) fn scan_contains(outer: &Origin, inner: &Origin) -> bool {
    outer.relid == inner.relid && outer.all_rows && (outer.with_children || !inner.with_children)
}

/// The conjuncts of an AND (the node itself otherwise).
pub(crate) fn conjuncts<'a>(n: &'a protobuf::Node, out: &mut Vec<&'a protobuf::Node>) {
    match n.node.as_ref() {
        Some(node::Node::BoolExpr(b))
            if protobuf::BoolExprType::try_from(b.boolop)
                == Ok(protobuf::BoolExprType::AndExpr) =>
        {
            for a in &b.args {
                conjuncts(a, out);
            }
        }
        _ => out.push(n),
    }
}

/// Whether this level's FROM and WHERE always leave a row, for each row of
/// the entries it reads as LATERAL or outer references — so an aggregate
/// without GROUP BY sees rows, and the level yields one:
///
/// - every FROM item yields a row (a FROM-less query, a subquery or CTE
///   that always does) and there is no WHERE (or `WHERE true`);
/// - or the FROM clause scans one table `p` in full, and the WHERE is
///   nothing but `p.c = o.d` equalities against non-NULL columns of one
///   entry `o` of an enclosing level, which are either the keys of a
///   foreign key from `o`'s table into `p` (the referenced row is there),
///   or `p`'s own columns read from a row of `p`'s table (`o`'s row itself
///   matches: a btree `=` is reflexive).
///
/// Only rows of a scan count for `o` (not the rows a DML statement
/// writes, which the query's snapshot need not see the referenced row
/// of), and nothing applies while the statement locks rows (EvalPlanQual
/// re-checks a re-fetched row without re-checking a foreign key).
pub(crate) fn input_nonempty(
    sel: &protobuf::SelectStmt,
    scope: &Scope,
    where_log: &StrictLog,
    snapshot: &PgCatalog,
) -> bool {
    let where_true = sel.where_clause.as_deref().is_none_or(is_true_const);
    if where_true
        && sel
            .from_clause
            .iter()
            .all(|n| !matches!(n.node.as_ref(), Some(node::Node::JoinExpr(_))))
        && scope.sources.iter().all(|s| s.min_one_row)
    {
        return true;
    }
    if crate::nonnull::row_locking() || !sel.locking_clause.is_empty() {
        return false;
    }
    let ([item], [p]) = (sel.from_clause.as_slice(), scope.sources.as_slice()) else {
        return false;
    };
    if !matches!(item.node.as_ref(), Some(node::Node::RangeVar(_)))
        || !matches!(p.kind, crate::scope::SourceKind::Relation)
    {
        return false;
    }
    let Some(prel) = p.relid else {
        return false;
    };
    // Every row of `p`'s table is scanned (no ONLY over a partitioned one).
    let Some(p_origin) = p.columns.iter().find_map(|c| c.origin.clone()) else {
        return false;
    };
    if !p_origin.all_rows || !fk_parent_ok(snapshot, prel) {
        return false;
    }
    let mut conj = Vec::new();
    if let Some(w) = sel.where_clause.as_deref()
        && !where_true
    {
        conjuncts(w, &mut conj);
    }
    // (p's column, the enclosing entry's column) per equality.
    let mut pairs: Vec<(&ScopeColumn, &ScopeColumn)> = Vec::new();
    for c in conj {
        let Some(node::Node::AExpr(e)) = c.node.as_ref() else {
            return false;
        };
        if protobuf::AExprKind::try_from(e.kind) != Ok(protobuf::AExprKind::AexprOp)
            || expr::extract_string_fields(&e.name).join(".") != "="
            || !where_log.is_strict(e.location, StrictNode::Op)
        {
            return false;
        }
        let (Some(l), Some(r)) = (
            e.lexpr.as_deref().and_then(|n| scope.plain_column_ref(n)),
            e.rexpr.as_deref().and_then(|n| scope.plain_column_ref(n)),
        ) else {
            return false;
        };
        let own = |c: &ScopeColumn| c.table_alias == p.alias;
        match (own(l), own(r)) {
            (true, false) => pairs.push((l, r)),
            (false, true) => pairs.push((r, l)),
            _ => return false,
        }
    }
    // Without an equality nothing ties the subquery to a row: an outer
    // aggregate query yields its row over an empty table too
    // (`SELECT count(*), (SELECT 1 FROM t LIMIT 1) FROM t`).
    let Some(((_, first), _)) = pairs.split_first() else {
        return false;
    };
    let o_alias = &first.table_alias;
    if pairs
        .iter()
        .any(|(_, o)| &o.table_alias != o_alias || !o.base_not_null)
    {
        return false;
    }
    let Some(o_origins) = pairs
        .iter()
        .map(|(_, o)| o.origin.as_ref())
        .collect::<Option<Vec<&Origin>>>()
    else {
        return false;
    };
    let scan = o_origins[0].scan;
    if o_origins.iter().any(|o| o.scan != scan) {
        return false;
    }
    let Some(p_cols) = pairs
        .iter()
        .map(|(pc, _)| pc.origin.as_ref().map(|o| o.column.clone()))
        .collect::<Option<Vec<String>>>()
    else {
        return false;
    };
    // A foreign key from the enclosing entry's table into `p`.
    let wanted: BTreeSet<(String, String)> = o_origins
        .iter()
        .zip(&p_cols)
        .map(|(o, pc)| (o.column.clone(), pc.clone()))
        .collect();
    if !o_origins[0].with_children && fk_follows(snapshot, o_origins[0].relid, prel, &wanted) {
        return true;
    }
    // The enclosing entry's row is one of `p`'s.
    scan_contains(&p_origin, o_origins[0])
        && pairs
            .iter()
            .zip(&o_origins)
            .zip(&p_cols)
            .all(|(((pc, _), o), pcol)| &o.column == pcol && reflexive_eq(snapshot, pc.type_oid))
}

/// Finish a SELECT level: settle its output columns' origins (dropped
/// under grouping sets, which NULL a column on their own; no longer
/// keeping every row of the table unless the level keeps every row of its
/// single FROM item) and summarize what the level guarantees.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_level(
    sel: &protobuf::SelectStmt,
    scope: &Scope,
    has_aggs: bool,
    input_nonempty: bool,
    columns: &mut [RawColumn],
    snapshot: &PgCatalog,
) -> LevelSummary {
    let grouping_sets = sel
        .group_clause
        .iter()
        .any(|g| matches!(g.node.as_ref(), Some(node::Node::GroupingSet(_))));
    let no_srf = count_srf_calls(&sel.target_list, snapshot) == 0;
    let keeps_every_row = sel.from_clause.len() == 1
        && scope.sources.len() == 1
        && !matches!(
            sel.from_clause[0].node.as_ref(),
            Some(node::Node::JoinExpr(_))
        )
        && sel.where_clause.as_deref().is_none_or(is_true_const)
        && sel.group_clause.is_empty()
        && !has_aggs
        && sel.having_clause.is_none()
        && sel.distinct_clause.is_empty()
        && sel.locking_clause.is_empty()
        && limit_keeps_every_row(sel)
        && no_srf;
    for c in columns.iter_mut() {
        if grouping_sets {
            c.origin = None;
        } else if !keeps_every_row && let Some(o) = &mut c.origin {
            o.all_rows = false;
        }
    }
    let ungrouped_aggregate = has_aggs && sel.group_clause.is_empty();
    LevelSummary {
        min_one_row: limit_keeps_a_row(sel)
            && no_srf
            && sel.having_clause.is_none()
            && sel.locking_clause.is_empty()
            && (ungrouped_aggregate || input_nonempty),
    }
}

/// Give the scans behind `columns` (a CTE's, read once more by another
/// reference) fresh identities: two references are two scans, whose rows
/// are unrelated.
pub(crate) fn fresh_scans(columns: &mut [ScopeColumn]) {
    let mut map: HashMap<u32, u32> = HashMap::new();
    for c in columns {
        if let Some(o) = &mut c.origin {
            o.scan = *map
                .entry(o.scan)
                .or_insert_with(crate::scope::fresh_scan_id);
        }
    }
}

/// The origins of a view's columns: its stored query analyzed again
/// (without recording dependencies — the referencing query depends on the
/// view, not on what it reads). `None` when that fails or no longer
/// matches the view's columns; views of the system schemas are not looked
/// into.
pub(crate) fn view_origins(
    snapshot: &PgCatalog,
    class: &crate::pg_catalog::PgClass,
) -> Option<Vec<Option<Origin>>> {
    if class.relkind != crate::pg_catalog::RelKind::View {
        return None;
    }
    let nsp = snapshot.namespace_name(class.relnamespace)?;
    if nsp == "pg_catalog" || nsp == "information_schema" {
        return None;
    }
    let query = crate::ddl::views::view_query(snapshot, class.oid)?;
    let Some(node::Node::SelectStmt(sel)) = query.node.as_ref() else {
        return None;
    };
    let (result, _) = crate::ddl::depend::collect(|| {
        let mut params = ParamCollector::default();
        analyze_select(sel, snapshot, &mut params)
    });
    let _ = take_level_summary();
    let (cols, _) = result.ok()?;
    let attrs = snapshot.attributes_of(class.oid);
    (cols.len() == attrs.len()
        && attrs
            .iter()
            .zip(&cols)
            .all(|(a, c)| a.atttypid == c.type_oid))
    .then(|| cols.into_iter().map(|c| c.origin).collect())
}
