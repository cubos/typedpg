//! Expand `GROUP BY` clauses with `GROUPING SETS`/`ROLLUP`/`CUBE` into the
//! flat list of grouping sets that PG would generate, and derive which
//! columns are *omitted* by at least one set.
//!
//! The result drives the nullability promotion in
//! [`crate::nullability::NullabilityContext::grouping_omitted`]: a column
//! that appears in every set keeps its base nullability, while one that is
//! present in some sets but absent from others must be reported as nullable
//! in the projection (PG fills those rows with NULL).
//!
//! For ordinary `GROUP BY a, b` (no `GroupingSet` nodes), the function
//! returns an empty set — every column is in the single grouping set, so no
//! promotion is needed.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use typedpg_pg_query::Equal;
use typedpg_pg_query::NodeBox;
use typedpg_pg_query::ast::{self, GroupingSetKind, JoinType};

use crate::error::AnalyzeError;
use crate::expr;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{ConType, PgCatalog};
use crate::resolve::ExpandedTarget;
use crate::scope::{Scope, TableSource};

/// Result of expanding a `GROUP BY` clause that contains
/// `GROUPING SETS`/`ROLLUP`/`CUBE`.
#[derive(Debug, Default)]
pub(crate) struct GroupingExpansion {
    /// Columns present in some grouping sets but absent from others — must
    /// be reported as nullable in the projection.
    pub omitted: HashSet<(String, String)>,
    /// Whether any grouping set is empty (i.e. aggregates the whole input).
    /// Non-COUNT aggregates can return NULL for that row when the input is
    /// empty, so they must be promoted to nullable.
    pub has_empty_set: bool,
    /// With grouping sets, the columns every set groups by — only those
    /// can functionally determine others (PG's groupClauseCommonVars).
    /// `None` without grouping sets: every grouped column counts.
    pub common: Option<HashSet<(String, String)>>,
    /// The (star-expanded) target-list positions that are grouping items
    /// of their own (named by an ordinal or an output alias), other than
    /// the item their key reads ([`Self::omitted`]), which some grouping
    /// set omits: PG's Agg NULLs such an entry in those sets' rows even
    /// where an equal expression is grouped by an item every set holds
    /// (`SELECT g AS a, g AS b … GROUP BY a, ROLLUP (b)` NULLs `b`).
    pub omitted_targets: HashSet<usize>,
}

/// A grouping item: the target-list position PG's grouping clause points
/// at (its `ressortgroupref`), `None` for an expression the target list
/// doesn't hold (a resjunk entry), and the item's key — its column, or
/// its expression's [`expr_key`].
type GroupItem = (Option<usize>, (String, String));

/// Resolve `group_clause` into the columns that some grouping set omits and
/// whether any of those sets is empty.
///
/// Only plain `ColumnRef`s are tracked; non-column expressions in the
/// `GROUP BY` (e.g. `date_trunc('day', ts)`) contribute nothing — the
/// projection references them by the inner column anyway, and that column
/// might or might not be in scope.
pub(crate) fn expand_grouping_sets(
    group_clause: &[Option<ast::Node>],
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> GroupingExpansion {
    // Too many sets is an error of its own (parseCheckAggregates); don't
    // materialize them.
    if group_clause.is_empty() || grouping_set_count(group_clause) > MAX_GROUPING_SETS {
        return GroupingExpansion::default();
    }

    // Per top-level entry, the alternatives it contributes (each alternative
    // is one grouping set fragment).
    let mut per_entry: Vec<Vec<HashSet<GroupItem>>> = Vec::new();
    let mut saw_grouping_set = false;
    for node in group_clause {
        let alts = alternatives_for(node, scope, targets, snapshot, &mut saw_grouping_set);
        per_entry.push(alts);
    }

    if !saw_grouping_set {
        // Plain `GROUP BY a, b, …` — single grouping set, no omissions, and
        // (assuming `group_clause` is non-empty) not the empty grouping set.
        return GroupingExpansion::default();
    }

    // Cartesian product of alternatives — each combination yields one final
    // grouping set (the union of one alternative from each entry).
    let mut sets: Vec<HashSet<GroupItem>> = vec![HashSet::new()];
    for alts in &per_entry {
        if alts.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(sets.len() * alts.len());
        for prefix in &sets {
            for alt in alts {
                let mut combined = prefix.clone();
                combined.extend(alt.iter().cloned());
                next.push(combined);
            }
        }
        sets = next;
    }

    if sets.is_empty() {
        return GroupingExpansion::default();
    }

    // An entry of the target list is one item, however its key was spelled.
    let in_every_set = |item: &GroupItem| match item.0 {
        Some(t) => sets.iter().all(|s| s.iter().any(|i| i.0 == Some(t))),
        None => sets.iter().all(|s| s.contains(item)),
    };
    let items: HashSet<&GroupItem> = sets.iter().flatten().collect();
    // A reference to a grouped expression outside the entry that is its
    // grouping item reads the first grouping column equal to it (setrefs
    // matches the Agg's input target list in order: the target list's
    // entries first, resjunk ones after): the key is NULL where that item
    // is omitted.
    let mut first: HashMap<&(String, String), &GroupItem> = HashMap::new();
    for item in &items {
        let order = |i: &GroupItem| (i.0.is_none(), i.0);
        first
            .entry(&item.1)
            .and_modify(|f| {
                if order(item) < order(f) {
                    *f = item;
                }
            })
            .or_insert(item);
    }
    let omitted = first
        .iter()
        .filter(|(_, item)| !in_every_set(item))
        .map(|(key, _)| (*key).clone())
        .collect();
    // An entry that is an item of its own, not the one its key reads.
    let omitted_targets = items
        .iter()
        .filter(|item| item.0.is_some() && first.get(&item.1).map(|f| f.0) != Some(item.0))
        .filter(|item| !in_every_set(item))
        .filter_map(|item| item.0)
        .collect();
    let keys = |s: &HashSet<GroupItem>| -> HashSet<(String, String)> {
        s.iter().map(|(_, key)| key.clone()).collect()
    };
    let mut intersection = keys(&sets[0]);
    for s in &sets[1..] {
        let s = keys(s);
        intersection.retain(|x| s.contains(x));
    }
    let has_empty_set = sets.iter().any(|s| s.is_empty());
    GroupingExpansion {
        omitted,
        has_empty_set,
        common: Some(intersection),
        omitted_targets,
    }
}

/// PG's limit on the number of grouping sets a GROUP BY expands to.
const MAX_GROUPING_SETS: u64 = 4096;

/// The number of grouping sets `group_clause` expands to — the product of
/// its entries' alternatives, as PG's expand_grouping_sets counts them
/// (saturating).
fn grouping_set_count(group_clause: &[Option<ast::Node>]) -> u64 {
    fn alternatives(node: &Option<ast::Node>) -> u64 {
        let Some(ast::Node::GroupingSet(gs)) = node.as_ref() else {
            return 1;
        };
        let n = gs.content.len() as u64;
        match gs.kind {
            GroupingSetKind::GroupingSetRollup => n + 1,
            GroupingSetKind::GroupingSetCube => 1u64.checked_shl(n as u32).unwrap_or(u64::MAX),
            GroupingSetKind::GroupingSetSets => gs
                .content
                .iter()
                .fold(0u64, |acc, c| acc.saturating_add(alternatives(c)))
                .max(1),
            _ => 1,
        }
    }
    group_clause
        .iter()
        .fold(1u64, |acc, n| acc.saturating_mul(alternatives(n)))
}

/// Alternatives contributed by one node in the top-level `group_clause`.
fn alternatives_for(
    node: &Option<ast::Node>,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<GroupItem>> {
    match node.as_ref() {
        Some(ast::Node::GroupingSet(gs)) => {
            *saw_grouping_set = true;
            alternatives_for_grouping_set(gs, scope, targets, snapshot, saw_grouping_set)
        }
        _ => vec![singleton_set(node, scope, targets, snapshot)],
    }
}

fn alternatives_for_grouping_set(
    gs: &ast::GroupingSet,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<GroupItem>> {
    let kind = gs.kind;
    match kind {
        GroupingSetKind::GroupingSetEmpty => vec![HashSet::new()],
        GroupingSetKind::GroupingSetSimple => {
            // `(a, b)` — a single set with the union of its members.
            let mut set = HashSet::new();
            for item in &gs.content {
                set.extend(singleton_set(item, scope, targets, snapshot));
            }
            vec![set]
        }
        GroupingSetKind::GroupingSetRollup => {
            // ROLLUP(a, b, c) → [{a,b,c}, {a,b}, {a}, {}]
            let items: Vec<HashSet<GroupItem>> = gs
                .content
                .iter()
                .map(|n| singleton_set(n, scope, targets, snapshot))
                .collect();
            let mut alts = Vec::with_capacity(items.len() + 1);
            for cut in (0..=items.len()).rev() {
                let mut s = HashSet::new();
                for item in &items[..cut] {
                    s.extend(item.iter().cloned());
                }
                alts.push(s);
            }
            alts
        }
        GroupingSetKind::GroupingSetCube => {
            // CUBE(a, b) → powerset — 2^n sets.
            let items: Vec<HashSet<GroupItem>> = gs
                .content
                .iter()
                .map(|n| singleton_set(n, scope, targets, snapshot))
                .collect();
            let n = items.len();
            let mut alts = Vec::with_capacity(1usize << n.min(16));
            for mask in 0..(1u32 << n) {
                let mut s = HashSet::new();
                for (i, item) in items.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        s.extend(item.iter().cloned());
                    }
                }
                alts.push(s);
            }
            alts
        }
        GroupingSetKind::GroupingSetSets => {
            // GROUPING SETS (s1, s2, …) — concatenate the alternatives of
            // each child. Each child is itself either an expression (one
            // alt = singleton) or a nested `GroupingSet`.
            let mut alts = Vec::new();
            for child in &gs.content {
                alts.extend(alternatives_for(
                    child,
                    scope,
                    targets,
                    snapshot,
                    saw_grouping_set,
                ));
            }
            if alts.is_empty() {
                vec![HashSet::new()]
            } else {
                alts
            }
        }
    }
}

/// The members of an implicit row `(a, b)` in a GROUP BY list or grouping
/// set: PG's flatten_grouping_sets turns it into a list of grouping
/// expressions (`GROUP BY (a, b)` is `GROUP BY a, b`; `ROLLUP((a, b))`
/// rolls the pair up as one unit). An explicit `ROW(a, b)` stays one
/// composite expression.
pub(crate) fn implicit_row_items(node: &Option<ast::Node>) -> Option<&Vec<Option<ast::Node>>> {
    match node.as_ref() {
        Some(ast::Node::RowExpr(r)) if r.row_format == ast::CoercionForm::CoerceImplicitCast => {
            Some(&r.args)
        }
        _ => None,
    }
}

/// Resolve a GROUP BY leaf as a grouping item. Returns a singleton set
/// holding the item when the leaf denotes a column or an expression —
/// directly, or through a projection ordinal / output alias
/// ([`group_leaf`]) — or an empty set otherwise (does not drive
/// nullability promotion).
fn singleton_set(
    node: &Option<ast::Node>,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> HashSet<GroupItem> {
    if let Some(items) = implicit_row_items(node) {
        return items
            .iter()
            .flat_map(|i| singleton_set(i, scope, targets, snapshot))
            .collect();
    }
    let (leaf, target) = group_leaf(node, scope, targets, snapshot);
    match leaf {
        GroupLeaf::Column(key) => HashSet::from([(target, key)]),
        GroupLeaf::Expr(fingerprint) => {
            HashSet::from([(target, (EXPR_KEY.to_owned(), fingerprint))])
        }
        GroupLeaf::Other | GroupLeaf::Unresolved => HashSet::new(),
    }
}

/// What a GROUP BY leaf denotes.
enum GroupLeaf {
    /// A plain column `(table_alias, column_name)`.
    Column((String, String)),
    /// An expression, by its [`expr_key`].
    Expr(String),
    /// Some other expression (or an out-of-range ordinal, reported elsewhere).
    Other,
    /// A column reference that neither resolves nor names an output column.
    Unresolved,
}

/// PG's `findTargetlistEntrySQL92` for a GROUP BY leaf: an integer constant
/// is a position in the (star-expanded) target list, a bare name is an
/// input column first and an output-column alias otherwise, anything else is
/// an expression of its own — which `findTargetlistEntrySQL99` matches to
/// the first target-list entry equal to it. Returns what the leaf denotes
/// and the position of the entry it is, if any.
fn group_leaf(
    node: &Option<ast::Node>,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> (GroupLeaf, Option<usize>) {
    let target_column = |rt: &ast::ResTarget| match rt.val.as_ref() {
        Some(v) => match resolve_group_column(Some(v), scope, snapshot) {
            Some(key) => GroupLeaf::Column(key),
            None => GroupLeaf::Expr(expr_key(&rt.val, scope, snapshot)),
        },
        None => GroupLeaf::Other,
    };
    // findTargetlistEntrySQL99: the first entry equal to the expression.
    let equal_target = |node: &Option<ast::Node>, column: Option<&ColKey>| {
        targets.iter().position(|t| match t {
            ExpandedTarget::Expr(rt) => rt.val.as_ref().is_some_and(|v| match column {
                Some(key) => resolve_group_column(Some(v), scope, snapshot).as_ref() == Some(key),
                None => same_level_exprs_equal(&rt.val, node, scope, snapshot),
            }),
            ExpandedTarget::StarColumn {
                column: Some((alias, col)),
                ..
            } => column.is_some_and(|(a, c)| a == alias && c == col),
            ExpandedTarget::StarColumn { column: None, .. } => false,
        })
    };
    let expression =
        |node: &Option<ast::Node>| match resolve_group_column(node.as_ref(), scope, snapshot) {
            Some(key) => {
                let target = equal_target(node, Some(&key));
                (GroupLeaf::Column(key), target)
            }
            None => (
                GroupLeaf::Expr(expr_key(node, scope, snapshot)),
                equal_target(node, None),
            ),
        };
    match node.as_ref() {
        Some(ast::Node::AConst(ac)) => match &ac.val {
            Some(ast::AConstValue::Integer(i)) => {
                let position = (i.ival as usize).wrapping_sub(1);
                match targets.get(position) {
                    Some(ExpandedTarget::Expr(rt)) => (target_column(rt), Some(position)),
                    Some(ExpandedTarget::StarColumn {
                        column: Some((alias, col)),
                        ..
                    }) => (
                        GroupLeaf::Column((alias.to_string(), col.to_string())),
                        Some(position),
                    ),
                    _ => (GroupLeaf::Other, None),
                }
            }
            _ => (GroupLeaf::Other, None),
        },
        Some(ast::Node::ColumnRef(cr)) => {
            if resolve_group_column(node.as_ref(), scope, snapshot).is_some() {
                return expression(node);
            }
            let alias = match cr.fields.as_slice() {
                [f] => match f.as_ref() {
                    Some(ast::Node::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                },
                _ => None,
            };
            let target = alias.and_then(|a| {
                targets.iter().enumerate().find_map(|(i, t)| match t {
                    ExpandedTarget::Expr(rt) if crate::resolve::target_resname(rt) == a => {
                        Some((i, *rt))
                    }
                    _ => None,
                })
            });
            target.map_or((GroupLeaf::Unresolved, None), |(i, rt)| {
                (target_column(rt), Some(i))
            })
        }
        Some(_) => expression(node),
        None => (GroupLeaf::Other, None),
    }
}

/// The alias part of a grouping key that is an expression rather than a
/// column (no FROM entry can be named so).
pub(crate) const EXPR_KEY: &str = "\u{0}expr";

/// The key of an expression in grouping sets: its parse tree without
/// locations or column qualifiers (`t.g + 1` and `g + 1` are one key —
/// erring towards matching, which only makes more values nullable), its
/// casts that are no coercion dropped ([`strip_noop_casts`]: `g::int + 1`
/// is `g + 1` on an `int` column), its whole-row field selections written
/// as the columns they are ([`whole_row_fields`]: `(t).g` is `t.g`) and its
/// type names resolved as the GROUP BY check compares them
/// ([`resolve_type_names`]: `CAST(g AS bigint)` is `g::int8`).
pub(crate) fn expr_key(node: &Option<ast::Node>, scope: &Scope, snapshot: &PgCatalog) -> String {
    let node = own_whole_row_fields(node.as_ref(), scope);
    let node = strip_noop_casts(node.as_ref(), scope, snapshot, &|_| true);
    crate::resolve::node_fingerprint(&crate::resolve::predtest_unqualify(
        resolve_type_names(&node, snapshot).as_ref(),
    ))
}

/// [`crate::resolve::node_fingerprint`] of `node` with its casts that are
/// no coercion dropped ([`strip_noop_casts`]), its whole-row field
/// selections written as columns ([`whole_row_fields`]) and its type names
/// resolved ([`resolve_type_names`]) — the same for two expressions PG's
/// `equal()` finds equal once transformed, up to how columns are spelled.
pub(crate) fn typed_fingerprint(node: &ast::Node, scope: &Scope, snapshot: &PgCatalog) -> String {
    let node = own_whole_row_fields(Some(node), scope);
    let node = strip_noop_casts(node.as_ref(), scope, snapshot, &|_| true);
    crate::resolve::node_fingerprint(&resolve_type_names(&node, snapshot))
}

/// `node` with every type name it holds resolved to its type — PG
/// compares *transformed* expressions, where `CAST(a AS bigint)`,
/// `a::int8` and `a::pg_catalog.int8` are one coercion, as are `x::_int4`
/// and `x::int[]`: the name gives way to the type's OID (with the array
/// bounds it accounts for; a typmod stays). A name that doesn't resolve
/// stays as written.
pub(crate) fn resolve_type_names(
    node: &Option<ast::Node>,
    snapshot: &PgCatalog,
) -> Option<ast::Node> {
    use typedpg_pg_query::NodeMut;
    let mut tree = ast::ParseResult {
        version: 0,
        stmts: vec![ast::RawStmt {
            stmt: node.clone(),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    // SAFETY: the tree is neither moved nor dropped while the pointers are
    // used; only `TypeName`s are dereferenced, each once (no `TypeName`
    // holds another), and the `String` children whose list is replaced are
    // never dereferenced afterwards.
    unsafe {
        for (n, _) in tree.nodes_mut() {
            if let NodeMut::TypeName(tn) = n
                && let Some(oid) = crate::ddl::util::resolve_type_name(&*tn, snapshot)
            {
                (*tn).type_oid = oid.get();
                (*tn).names = Vec::new();
                (*tn).array_bounds = Vec::new();
                (*tn).pct_type = false;
            }
        }
    }
    tree.stmts.pop().and_then(|s| s.stmt)
}

/// Resolve a `GROUP BY` entry as a single column against `scope`. Returns the
/// `(table_alias, column_name)` for a plain `ColumnRef` (or a cast of one
/// that is no coercion, [`strip_noop_casts`], or a field `(t).c` of a
/// whole row, [`whole_row_fields`]), or `None` for any other shape
/// (expression, grouping set, select-list alias, …).
fn resolve_group_column(
    node: Option<&ast::Node>,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Option<ColKey> {
    let node = own_whole_row_fields(node, scope);
    let node = strip_noop_casts(node.as_ref(), scope, snapshot, &|_| true);
    let ast::Node::ColumnRef(cr) = node.as_ref()? else {
        return None;
    };
    let parts = expr::extract_string_fields(&cr.fields);
    if parts.len() != cr.fields.len() {
        return None;
    }
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] | [_, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return None,
    };
    scope
        .resolve_column(table, column, None)
        .ok()
        .map(|c| (c.table_alias.clone(), c.name.clone()))
}

/// `node` with the casts that leave their operand as it is replaced by
/// the operand: PG's `coerce_type` returns its input unchanged when the
/// target type is the input's, and `coerce_type_typmod` when the typmod is
/// too, so `GROUP BY a::int` on an `int` column groups by the column itself
/// (and `ROLLUP (a::int)` leaves `a` NULL in the total row), `a + 1` is the
/// grouped `a::int + 1` and `(a + 1)::int`. A cast changing the typmod —
/// `v::varchar` on a `varchar(5)` column — stays a coercion. Of the column
/// references, only those `own` accepts are looked at; subqueries are left
/// as they are.
fn strip_noop_casts(
    node: Option<&ast::Node>,
    scope: &Scope,
    snapshot: &PgCatalog,
    own: &dyn Fn(&ast::ColumnRef) -> bool,
) -> Option<ast::Node> {
    let mut out = node.cloned();
    if let Some(out) = out.as_mut() {
        out.rewrite(&mut |n| !matches!(n, ast::Node::SelectStmt(_)), &mut |n| {
            if let ast::Node::TypeCast(tc) = n
                && tc.arg.is_some()
                && tc
                    .type_name
                    .as_ref()
                    .is_some_and(|tn| is_noop_cast(&tc.arg, tn, scope, snapshot, own))
                && let Some(arg) = tc.arg.take()
            {
                *n = arg;
            }
        });
    }
    out
}

/// Whether casting `arg` to `tn` is no coercion: `arg` already has that
/// type, and that typmod (both unspecified included). A constant or a
/// parameter takes the type it is cast to instead, and an operand holding
/// a subquery or a column `own` refuses isn't typed here: neither is
/// reported.
fn is_noop_cast(
    arg: &Option<ast::Node>,
    tn: &ast::TypeName,
    scope: &Scope,
    snapshot: &PgCatalog,
    own: &dyn Fn(&ast::ColumnRef) -> bool,
) -> bool {
    use typedpg_pg_query::NodeRef;
    let (type_oid, typmod) = match arg.as_ref() {
        None | Some(ast::Node::AConst(_) | ast::Node::ParamRef(_)) => return false,
        Some(ast::Node::ColumnRef(cr)) => {
            if !own(cr) {
                return false;
            }
            let parts = expr::extract_string_fields(&cr.fields);
            if parts.len() != cr.fields.len() {
                return false;
            }
            let (table, column) = match parts.as_slice() {
                [col] => (None, col.as_str()),
                [tbl, col] | [_, tbl, col] => (Some(tbl.as_str()), col.as_str()),
                _ => return false,
            };
            match scope.resolve_column(table, column, None) {
                Ok(col) => (col.type_oid, col.typmod),
                Err(_) => return false,
            }
        }
        Some(n) => {
            if n.nodes().iter().any(|(m, _)| match m {
                NodeRef::SelectStmt(_) => true,
                NodeRef::ColumnRef(cr) => !own(cr),
                _ => false,
            }) {
                return false;
            }
            let null_ctx = crate::nullability::NullabilityContext::default();
            let mut params = crate::param_collector::ParamCollector::default();
            match expr::infer_expr(
                arg,
                expr::Ctx::new(scope, &null_ctx, snapshot),
                &mut params,
                expr::TypeGoal::NONE,
            ) {
                Ok(t) => (t.type_oid, t.typmod),
                Err(_) => return false,
            }
        }
    };
    let Some(target) = crate::ddl::util::resolve_type_name(tn, snapshot) else {
        return false;
    };
    // A typmod we can't encode (a type whose `typmodin` isn't modeled)
    // isn't known to match.
    let target_typmod = match crate::typmod::encode(snapshot, target, tn) {
        Ok(None) if !tn.typmods.is_empty() => return false,
        Ok(m) => m,
        Err(_) => return false,
    };
    target == type_oid && target_typmod == typmod
}

// ──────────────────────────────────────────────────────────────────────────────
// Query levels — the part of PG's ParseState chain aggregates care about
// ──────────────────────────────────────────────────────────────────────────────
//
// PG decides which query level an aggregate belongs to from its arguments
// (check_agg_arguments: the nearest level any of their column references
// reaches, or the level it is written in), checks it against *that* level's
// clause (check_agglevels_and_constraints: in `SELECT … WHERE x IN (SELECT
// count(t.a) FROM u)` the aggregate is in WHERE), and lets it make that
// level a grouped query. parseCheckAggregates then walks every expression
// of a grouped level — sublinks included — for column references of the
// level that are neither grouped nor inside one of its aggregates.
//
// The analyzer works on the raw tree, so it keeps a frame per query level
// being analyzed (the names its FROM clause exposes, the clause being
// analyzed) and, per analyzed SELECT, its namespace — enough to tell which
// level a column reference inside a sublink resolves to.

/// A column's identity at its query level: `(range-table alias, column)`.
type ColKey = (String, String);

/// The names one query level's FROM clause exposes to column references.
#[derive(Debug, Default)]
pub(crate) struct Namespace {
    sources: Vec<NsSource>,
    /// What the JOIN USING columns of this level stand for (see
    /// [`join_flattening`]).
    flat: HashMap<ColKey, Flat>,
}

/// PG's flatten_join_alias_vars for a JOIN USING column: an INNER / LEFT
/// join's is the left input column, a RIGHT join's the right one — coerced
/// when its type differs from the merged column's — and a FULL join's
/// `COALESCE(left, right)`.
#[derive(Debug, Clone)]
enum Flat {
    Var(ColKey),
    Coerced(ColKey),
    Coalesce(ColKey, ColKey),
}

#[derive(Debug)]
struct NsSource {
    alias: String,
    /// The columns an unqualified reference reaches.
    visible: HashSet<String>,
    /// The columns a qualified reference reaches.
    all: HashSet<String>,
}

impl Namespace {
    fn of(sources: &[TableSource]) -> Self {
        Namespace {
            flat: HashMap::new(),
            sources: sources
                .iter()
                .map(|s| NsSource {
                    alias: s.alias.clone(),
                    visible: s
                        .visible_columns()
                        .chain(s.system_columns.iter().filter(|_| !s.table_only))
                        .map(|c| c.name.clone())
                        .collect(),
                    all: s
                        .columns
                        .iter()
                        .chain(&s.system_columns)
                        .map(|c| c.name.clone())
                        .collect(),
                })
                .collect(),
        }
    }

    fn has_alias(&self, alias: &str) -> bool {
        self.sources.iter().any(|s| s.alias == alias)
    }

    /// Whether entry `alias` has a column `col` a qualified reference
    /// reaches.
    fn has_column(&self, alias: &str, col: &str) -> bool {
        self.sources
            .iter()
            .any(|s| s.alias == alias && s.all.contains(col))
    }

    /// What column `key` of this level stands for once JOIN USING columns
    /// are flattened (`None`: itself).
    fn flatten(&self, key: &ColKey) -> Option<Flat> {
        let mut flat = self.flat.get(key)?.clone();
        // A USING column over an inner join's USING column.
        for _ in 0..self.flat.len() {
            match &flat {
                Flat::Var(k) => match self.flat.get(k) {
                    Some(inner) => flat = inner.clone(),
                    None => break,
                },
                Flat::Coerced(k) => match self.flat.get(k) {
                    Some(Flat::Var(inner) | Flat::Coerced(inner)) => {
                        flat = Flat::Coerced(inner.clone());
                    }
                    _ => break,
                },
                Flat::Coalesce(..) => break,
            }
        }
        Some(flat)
    }

    /// The plain columns column `key` of this level reads, flattened.
    fn flat_columns(&self, key: &ColKey) -> Vec<ColKey> {
        match self.flatten(key) {
            None => vec![key.clone()],
            Some(Flat::Var(k) | Flat::Coerced(k)) => vec![k],
            Some(Flat::Coalesce(a, b)) => vec![a, b],
        }
    }

    /// The canonical name of a reference to column `key` of this level.
    fn canonical_name(&self, key: &ColKey) -> String {
        match self.flatten(key) {
            None => column_key(&key.0, &key.1),
            Some(Flat::Var(k)) => column_key(&k.0, &k.1),
            Some(Flat::Coerced(k)) => format!("\u{1}cast{}", column_key(&k.0, &k.1)),
            Some(Flat::Coalesce(a, b)) => format!(
                "\u{1}coalesce{}{}",
                column_key(&a.0, &a.1),
                column_key(&b.0, &b.1)
            ),
        }
    }

    /// The alias of the entry an unqualified `col` resolves to here.
    fn column(&self, col: &str) -> Option<&str> {
        self.sources
            .iter()
            .find(|s| s.visible.contains(col))
            .map(|s| s.alias.as_str())
    }
}

/// The JOIN USING columns of SELECT `sel`'s FROM clause, whose entries are
/// `sources`, flattened as PG's flatten_join_alias_vars does (see [`Flat`]).
///
/// The scope keeps a USING join's merged columns in an entry of their own
/// placed before the join's left side, so the FROM clause's shape says
/// which entries each join's sides span. NATURAL joins (which merge only
/// when the sides share a name) and aliased joins (whose inner entries are
/// gone) make that correspondence unsure: no flattening then.
fn join_flattening(
    sel: &ast::SelectStmt,
    sources: &[TableSource],
    shadowed: &[TableSource],
) -> HashMap<ColKey, Flat> {
    /// One output column of a FROM item: its name, what it stands for and
    /// its type.
    type OutColumn = (String, Flat, PgTypeOid);

    /// Where a FROM item's entries are: at `sources[start..]`, or — inside
    /// an aliased join, whose entries PG hides behind the alias — among
    /// the shadowed entries that join entry (by its alias) hides, by name.
    #[derive(Clone, Copy)]
    enum Place<'a> {
        Span(usize),
        Shadowed(&'a str),
    }

    struct Ctx<'a> {
        sources: &'a [TableSource],
        shadowed: &'a [TableSource],
        out: HashMap<ColKey, Flat>,
    }

    /// How many scope entries a FROM item yields (an aliased join is one);
    /// `None` when unsure.
    fn count(n: &Option<ast::Node>) -> Option<usize> {
        match n.as_ref()? {
            ast::Node::JoinExpr(j) if j.alias.is_some() => Some(1),
            ast::Node::JoinExpr(j) => {
                Some(count(&j.larg)? + count(&j.rarg)? + usize::from(!j.using_clause.is_empty()))
            }
            _ => Some(1),
        }
    }

    /// The name PG gives a FROM item's entry.
    fn entry_name(n: &Option<ast::Node>) -> Option<String> {
        match n.as_ref()? {
            ast::Node::RangeVar(rv) => Some(
                rv.alias
                    .as_ref()
                    .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone()),
            ),
            ast::Node::RangeSubselect(rs) => rs.alias.as_ref().map(|a| a.aliasname.clone()),
            ast::Node::RangeFunction(rf) => rf.alias.as_ref().map(|a| a.aliasname.clone()),
            ast::Node::RangeTableSample(ts) => entry_name(&ts.relation),
            _ => None,
        }
    }

    /// The output columns of FROM item `n` (PG's join output order: the
    /// USING columns, then the left side's others, then the right side's),
    /// recording what the USING and aliased-join columns stand for.
    fn item<'a>(
        n: &Option<ast::Node>,
        place: Place<'a>,
        cx: &mut Ctx<'a>,
    ) -> Option<Vec<OutColumn>> {
        let hidden = |shadowed: &'a [TableSource], join: &str, name: &str| {
            shadowed.iter().find(|s| {
                s.hidden
                    .as_ref()
                    .is_some_and(|h| h.join == join && h.name == name)
            })
        };
        let ast::Node::JoinExpr(j) = n.as_ref()? else {
            let source = match place {
                Place::Span(start) => cx.sources.get(start)?,
                Place::Shadowed(join) => hidden(cx.shadowed, join, &entry_name(n)?)?,
            };
            return Some(
                source
                    .columns
                    .iter()
                    .map(|c| {
                        (
                            c.name.clone(),
                            Flat::Var((source.alias.clone(), c.name.clone())),
                            c.type_oid,
                        )
                    })
                    .collect(),
            );
        };
        if j.is_natural {
            return None;
        }
        if j.larg.is_none() || j.rarg.is_none() {
            return None;
        }
        let (larg, rarg) = (&j.larg, &j.rarg);
        let using = crate::expr::extract_string_fields(&j.using_clause);
        // An aliased join's entries are behind its alias; its own entry is
        // where the join is.
        let alias_entry: Option<&'a TableSource> = match (j.alias.as_ref(), place) {
            (None, _) => None,
            (Some(_), Place::Span(at)) => Some(cx.sources.get(at)?),
            (Some(a), Place::Shadowed(join)) => Some(hidden(cx.shadowed, join, &a.aliasname)?),
        };
        let (inner, merged_at) = match (alias_entry, place) {
            (Some(entry), _) => (Place::Shadowed(&entry.alias), None),
            (None, Place::Shadowed(join)) => (Place::Shadowed(join), None),
            (None, Place::Span(start)) => {
                let left = start + usize::from(!using.is_empty());
                (Place::Span(left), (!using.is_empty()).then_some(start))
            }
        };
        let right_place = match inner {
            Place::Span(left) => Place::Span(left + count(larg)?),
            shadowed => shadowed,
        };
        let left = item(larg, inner, cx)?;
        let right = item(rarg, right_place, cx)?;
        let mut output: Vec<OutColumn> = Vec::new();
        let merged_entry: Option<&TableSource> = match merged_at {
            Some(at) => Some(cx.sources.get(at)?),
            None => None,
        };
        for (i, name) in using.iter().enumerate() {
            let find = |side: &[OutColumn]| side.iter().find(|c| &c.0 == name).cloned();
            let ((_, l, lt), (_, r, rt)) = (find(&left)?, find(&right)?);
            // The merged column's type: the join's common one, as the
            // scope recorded it.
            let merged_type = match merged_entry.or(alias_entry) {
                Some(entry) => entry.columns.get(i)?.type_oid,
                None => lt,
            };
            let var = |f: Flat, t: PgTypeOid| match f {
                Flat::Var(k) if t != merged_type => Flat::Coerced(k),
                f => f,
            };
            let plain = |f: &Flat| match f {
                Flat::Var(k) | Flat::Coerced(k) => Some(k.clone()),
                Flat::Coalesce(..) => None,
            };
            // buildMergedJoinVar: an INNER join prefers whichever side
            // needs no coercion.
            let flat = match j.jointype {
                JoinType::JoinRight => var(r, rt),
                JoinType::JoinFull => Flat::Coalesce(plain(&l)?, plain(&r)?),
                JoinType::JoinInner if lt != merged_type && rt == merged_type => r,
                _ => var(l, lt),
            };
            if let Some(m) = merged_entry {
                cx.out.insert((m.alias.clone(), name.clone()), flat.clone());
            }
            output.push((name.clone(), flat, merged_type));
        }
        output.extend(
            left.into_iter()
                .chain(right)
                .filter(|c| !using.contains(&c.0)),
        );
        if let (Some(alias), Some(entry)) = (j.alias.as_ref(), alias_entry) {
            // The aliased join's entry: its columns, renamed by the alias'
            // column list, stand for the join's output columns.
            if entry.refname() != alias.aliasname || entry.columns.len() != output.len() {
                return None;
            }
            let renamed: Vec<OutColumn> = entry
                .columns
                .iter()
                .zip(output)
                .map(|(c, (_, flat, t))| {
                    cx.out
                        .insert((entry.alias.clone(), c.name.clone()), flat.clone());
                    (c.name.clone(), flat, t)
                })
                .collect();
            return Some(renamed);
        }
        Some(output)
    }

    let mut cx = Ctx {
        sources,
        shadowed,
        out: HashMap::new(),
    };
    let mut start = 0;
    for from_item in &sel.from_clause {
        if item(from_item, Place::Span(start), &mut cx).is_none() {
            return HashMap::new();
        }
        match count(from_item) {
            Some(n) => start += n,
            None => return HashMap::new(),
        }
    }
    if start != sources.len() {
        return HashMap::new();
    }
    cx.out
}

/// Where a column reference points: `level` levels out from the innermost
/// level of the [`Chain`] it was resolved against, and — when that level's
/// names are known — the entry alias and column it names (`None` for a
/// whole-row reference like `t` or `t.*`).
#[derive(Debug)]
struct RefTarget {
    level: usize,
    target: Option<(String, Option<String>)>,
}

/// The namespaces of the query levels a reference can reach, innermost
/// first.
#[derive(Clone, Default)]
struct Chain {
    /// Per level, its namespace; `None` for a level whose FROM clause is
    /// still being analyzed (a LATERAL item's parent, a CTE's or a FROM
    /// subquery's enclosing level, a JOIN condition's sublink's).
    levels: Vec<Option<Rc<Namespace>>>,
    /// Per level whose namespace is `None`, the entries of it the
    /// innermost level can see at all (a LATERAL item's left siblings, a
    /// JOIN condition's inputs; none for a plain FROM subquery or a CTE):
    /// a reference reaching that level belongs to it only if one of these
    /// has the name — else it goes on outward, as PG's search does past a
    /// level whose matching entries are all invisible. `None`: unknown,
    /// see [`Self::reachable`].
    visible: Vec<Option<Rc<Namespace>>>,
    /// Every entry of the enclosing levels the innermost level can see:
    /// what decides for a level whose visible entries are unknown.
    reachable: Rc<Namespace>,
}

impl Chain {
    /// A chain of one level, `ns`.
    fn single(ns: Option<Rc<Namespace>>) -> Self {
        Chain {
            levels: vec![ns],
            visible: vec![None],
            reachable: Rc::default(),
        }
    }

    /// Level `level`'s namespace, if known.
    fn namespace(&self, level: usize) -> Option<Rc<Namespace>> {
        self.levels.get(level).cloned().flatten()
    }

    /// Whether a name reaching level `level`, whose namespace is unknown,
    /// belongs to it: `has` says whether a namespace holds it.
    fn pending_owns(&self, level: usize, has: impl Fn(&Namespace) -> bool) -> bool {
        match self.visible.get(level).cloned().flatten() {
            Some(visible) => has(&visible),
            None => has(&self.reachable),
        }
    }

    /// PG's transformColumnRef level search: an unqualified name is a
    /// column of the nearest level exposing it (else a whole-row reference
    /// to the nearest entry of that name); a qualified one belongs to the
    /// nearest level with an entry of that name.
    fn resolve(&self, cr: &ast::ColumnRef) -> Option<RefTarget> {
        let parts: Vec<Option<&str>> = cr
            .fields
            .iter()
            .map(|f| match f.as_ref() {
                Some(ast::Node::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        match parts.as_slice() {
            [Some(col)] => {
                for (level, ns) in self.levels.iter().enumerate() {
                    let Some(ns) = ns else {
                        if self.pending_owns(level, |ns| ns.column(col).is_some()) {
                            return Some(RefTarget {
                                level,
                                target: None,
                            });
                        }
                        continue;
                    };
                    if let Some(alias) = ns.column(col) {
                        return Some(RefTarget {
                            level,
                            target: Some((alias.to_owned(), Some((*col).to_owned()))),
                        });
                    }
                }
                // Not a column: a whole-row reference to the nearest entry
                // of that name.
                self.levels
                    .iter()
                    .enumerate()
                    .find_map(|(level, ns)| match ns {
                        Some(ns) if ns.has_alias(col) => Some(RefTarget {
                            level,
                            target: Some(((*col).to_owned(), None)),
                        }),
                        None if self.pending_owns(level, |ns| ns.has_alias(col)) => {
                            Some(RefTarget {
                                level,
                                target: None,
                            })
                        }
                        _ => None,
                    })
            }
            [.., Some(table), last] => {
                for (level, ns) in self.levels.iter().enumerate() {
                    match ns {
                        None if self.pending_owns(level, |ns| ns.has_alias(table)) => {
                            return Some(RefTarget {
                                level,
                                target: None,
                            });
                        }
                        Some(ns) if ns.has_alias(table) => {
                            return Some(RefTarget {
                                level,
                                target: Some(((*table).to_owned(), last.map(str::to_owned))),
                            });
                        }
                        _ => {}
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// For a qualified reference `t.f` naming no column of its entry, the
    /// function `f` it calls on the entry's whole row (column notation, as
    /// the expression analysis resolved it) and that whole-row reference
    /// (`t.*`).
    fn column_notation(&self, cr: &ast::ColumnRef) -> Option<(String, ast::Node)> {
        let (last, prefix) = cr.fields.split_last()?;
        let Some(ast::Node::String(f)) = last.as_ref() else {
            return None;
        };
        let Some(ast::Node::String(table)) = prefix.last()?.as_ref() else {
            return None;
        };
        let RefTarget {
            level,
            target: Some((alias, Some(_))),
        } = self.resolve(cr)?
        else {
            return None;
        };
        let ns = self.namespace(level)?;
        if alias != table.sval || ns.has_column(&alias, &f.sval) {
            return None;
        }
        let mut fields = prefix.to_vec();
        fields.push(Some(ast::Node::AStar(NodeBox::new(ast::AStar {}))));
        let whole_row = ast::Node::ColumnRef(NodeBox::new(ast::ColumnRef {
            fields,
            location: cr.location,
        }));
        Some((f.sval.clone(), whole_row))
    }

    /// For a field selection `(t).f` (or `(t.*).f`) on an entry's whole
    /// row where `f` is a column of the entry, the same expression written
    /// `t.f` (any further indirection kept on it): ParseComplexProjection
    /// makes it that column's Var (scanNSItemForColumn), so both spellings
    /// are PG-`equal()` once transformed.
    fn whole_row_field(&self, ind: &ast::AIndirection) -> Option<ast::Node> {
        let (first, rest) = ind.indirection.split_first()?;
        let Some(ast::Node::String(field)) = first.as_ref() else {
            return None;
        };
        let Some(ast::Node::ColumnRef(cr)) = ind.arg.as_ref() else {
            return None;
        };
        let RefTarget {
            level,
            target: Some((alias, None)),
        } = self.resolve(cr)?
        else {
            return None;
        };
        if !self.namespace(level)?.has_column(&alias, &field.sval) {
            return None;
        }
        let column = ast::Node::ColumnRef(NodeBox::new(ast::ColumnRef {
            fields: vec![Some(string_node(&alias)), first.clone()],
            location: cr.location,
        }));
        Some(if rest.is_empty() {
            column
        } else {
            ast::Node::AIndirection(NodeBox::new(ast::AIndirection {
                arg: Some(column),
                indirection: rest.to_vec(),
            }))
        })
    }

    fn enter(&mut self, ns: Rc<Namespace>) {
        self.levels.insert(0, Some(ns));
        self.visible.insert(0, None);
    }

    fn leave(&mut self) {
        self.levels.remove(0);
        self.visible.remove(0);
    }
}

/// One query level being analyzed.
#[derive(Default)]
struct Frame {
    /// Its FROM clause's names, once processed.
    ns: Option<Rc<Namespace>>,
    /// Before that, the entries of it the level being analyzed inside it
    /// can see (see [`with_visible_entries`]; `None`: unknown).
    visible: Option<Rc<Namespace>>,
    /// PG's ParseExprKindName of the clause being analyzed, when that
    /// clause forbids aggregates — what an aggregate of this level found in
    /// a sublink there is rejected with.
    clause: Option<&'static str>,
    /// A sublink holds an aggregate belonging to this level.
    outer_aggs: bool,
    /// Arguments of the `GROUPING(…)` calls in sublinks that belong to this
    /// level, canonicalized against it (see [`canonical`]), with their
    /// locations.
    outer_grouping: Vec<(Vec<Option<ast::Node>>, i32)>,
}

/// What an analyzed SELECT level exposes to the walks of enclosing levels.
#[derive(Clone)]
pub(crate) struct LevelInfo {
    ns: Rc<Namespace>,
    /// PG's `hasAggs`: an aggregate or `GROUPING(…)` of this level exists,
    /// in its own clauses or in a sublink.
    pub has_aggs: bool,
    /// Per output column, whether its collation is explicit (a plain
    /// SELECT's; a set operation's columns carry implicit ones).
    pub explicit_collations: Vec<bool>,
    /// A plain SELECT's output column names (PG's resnames).
    pub output_names: Vec<String>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    /// Keyed by the SelectStmt's address; cleared when the outermost level
    /// is left, so no address outlives its tree.
    static LEVELS: RefCell<HashMap<usize, LevelInfo>> = RefCell::new(HashMap::new());
    /// Whole-row references coerce_record_to_complex took apart, by the
    /// reference's location: the entry and its columns (see
    /// [`note_expanded_whole_row`]). Cleared with [`LEVELS`].
    static EXPANDED_ROWS: RefCell<HashMap<i32, (String, Vec<String>)>> =
        RefCell::new(HashMap::new());
}

/// The whole-row reference to entry `alias` at `location` was coerced to a
/// composite: PG's `coerce_record_to_complex` replaced it with a Var per
/// column (`expandNSItemVars`), so the grouped-column check sees those
/// columns, not the whole row (`SELECT s::c … GROUP BY s.a, s.b` is fine,
/// `GROUP BY s` is not).
pub(crate) fn note_expanded_whole_row(location: i32, alias: &str, columns: Vec<String>) {
    if location < 0 {
        return;
    }
    EXPANDED_ROWS.with(|r| {
        r.borrow_mut().insert(location, (alias.to_owned(), columns));
    });
}

/// The columns the whole-row reference to `alias` at `location` stands
/// for, when it was expanded (see [`note_expanded_whole_row`]).
fn expanded_whole_row(location: i32, alias: &str) -> Option<Vec<String>> {
    EXPANDED_ROWS.with(|r| {
        r.borrow()
            .get(&location)
            .filter(|(a, _)| a == alias)
            .map(|(_, cols)| cols.clone())
    })
}

fn level_key(sel: &ast::SelectStmt) -> usize {
    sel as *const ast::SelectStmt as usize
}

/// A query level starts (`QueryLevel::enter`).
pub(crate) fn enter_query_level() {
    FRAMES.with(|f| f.borrow_mut().push(Frame::default()));
}

/// A query level ends (`QueryLevel`'s drop).
pub(crate) fn leave_query_level() {
    let outermost = FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        f.pop();
        f.is_empty()
    });
    if outermost {
        LEVELS.with(|l| l.borrow_mut().clear());
        EXPANDED_ROWS.with(|r| r.borrow_mut().clear());
    }
}

/// The current level is SELECT `sel`, whose FROM clause exposes `sources`.
pub(crate) fn register_level(
    sel: &ast::SelectStmt,
    sources: &[TableSource],
    shadowed: &[TableSource],
) {
    let ns = Rc::new(Namespace {
        flat: join_flattening(sel, sources, shadowed),
        ..Namespace::of(sources)
    });
    FRAMES.with(|f| {
        if let Some(top) = f.borrow_mut().last_mut() {
            top.ns = Some(ns.clone());
        }
    });
    LEVELS.with(|l| {
        l.borrow_mut().insert(
            level_key(sel),
            LevelInfo {
                ns,
                has_aggs: false,
                explicit_collations: Vec::new(),
                output_names: Vec::new(),
            },
        )
    });
}

/// What the analysis of SELECT `sel` recorded, if it was analyzed.
pub(crate) fn level_info(sel: &ast::SelectStmt) -> Option<LevelInfo> {
    LEVELS.with(|l| l.borrow().get(&level_key(sel)).cloned())
}

/// Record SELECT `sel`'s output column names and which of its columns
/// have an explicit collation.
pub(crate) fn set_output_columns(sel: &ast::SelectStmt, names: Vec<String>, explicit: Vec<bool>) {
    LEVELS.with(|l| {
        if let Some(info) = l.borrow_mut().get_mut(&level_key(sel)) {
            info.output_names = names;
            info.explicit_collations = explicit;
        }
    });
}

/// Run `f` with the current level analyzing a clause that forbids
/// aggregates (`Some(PG's ParseExprKindName)`) or allows them (`None`).
pub(crate) fn with_clause<R>(clause: Option<&'static str>, f: impl FnOnce() -> R) -> R {
    let prev = FRAMES.with(|fr| {
        fr.borrow_mut()
            .last_mut()
            .map(|top| std::mem::replace(&mut top.clause, clause))
    });
    let out = f();
    if let Some(prev) = prev {
        FRAMES.with(|fr| {
            if let Some(top) = fr.borrow_mut().last_mut() {
                top.clause = prev;
            }
        });
    }
    out
}

/// Run `f` with the current level's entries seen by the query level `f`
/// analyzes inside it being `entries`: the entries to its left for a
/// LATERAL item, a JOIN condition's or FROM function's inputs for a
/// sublink there, none for a plain FROM subquery or a CTE. Only matters
/// while the current level's FROM clause is being analyzed (see
/// [`Chain::visible`]).
pub(crate) fn with_visible_entries<R>(entries: &[TableSource], f: impl FnOnce() -> R) -> R {
    let ns = Rc::new(Namespace::of(entries));
    let prev = FRAMES.with(|fr| {
        fr.borrow_mut()
            .last_mut()
            .map(|top| top.visible.replace(ns))
    });
    let out = f();
    if let Some(prev) = prev {
        FRAMES.with(|fr| {
            if let Some(top) = fr.borrow_mut().last_mut() {
                top.visible = prev;
            }
        });
    }
    out
}

/// The chain seen from the current level, whose own FROM items (so far)
/// are `scope.sources`.
fn current_chain(scope: &Scope) -> Chain {
    FRAMES.with(|f| {
        let f = f.borrow();
        // The level's registered namespace once its FROM clause is done
        // (it knows the JOIN USING flattening), else the entries so far.
        let own = f
            .last()
            .and_then(|top| top.ns.clone())
            .unwrap_or_else(|| Rc::new(Namespace::of(&scope.sources)));
        let mut levels = vec![Some(own)];
        levels.extend(f.iter().rev().skip(1).map(|fr| fr.ns.clone()));
        let mut visible = vec![None];
        visible.extend(f.iter().rev().skip(1).map(|fr| fr.visible.clone()));
        let reachable: Vec<TableSource> = scope
            .outer_tiers
            .iter()
            .flatten()
            .chain(&scope.outer_sources)
            .cloned()
            .collect();
        Chain {
            levels,
            visible,
            reachable: Rc::new(Namespace::of(&reachable)),
        }
    })
}

/// `n` itself when it holds a node: an absent field yields nothing.
fn present(n: &Option<ast::Node>) -> Option<&Option<ast::Node>> {
    n.as_ref().map(|_| n)
}

/// An aggregate, `GROUPING(…)` or window call written at some level.
enum Call<'a> {
    /// Its aggregated arguments (ORDER BY and FILTER included), which decide
    /// its level, and its direct arguments (an ordered-set aggregate's),
    /// which are evaluated once per group like any grouped expression.
    Aggregate {
        aggregated: Vec<&'a Option<ast::Node>>,
        direct: Vec<&'a Option<ast::Node>>,
        location: i32,
    },
    Grouping(&'a ast::GroupingFunc),
    Window(i32),
}

fn call_of<'a>(node: &'a Option<ast::Node>, snapshot: &PgCatalog) -> Option<Call<'a>> {
    fn sort_exprs(order: &[Option<ast::Node>]) -> impl Iterator<Item = &Option<ast::Node>> {
        order.iter().filter_map(|o| match o.as_ref() {
            Some(ast::Node::SortBy(sb)) => present(&sb.node),
            _ => Some(o),
        })
    }
    match node.as_ref()? {
        ast::Node::FuncCall(fc) if fc.over.is_some() => Some(Call::Window(fc.location)),
        ast::Node::FuncCall(fc) if crate::resolve::is_aggregate_call(fc, snapshot) => {
            let (mut aggregated, direct): (Vec<_>, Vec<_>) = if fc.agg_within_group {
                (
                    sort_exprs(&fc.agg_order).collect(),
                    fc.args.iter().collect(),
                )
            } else {
                (
                    fc.args.iter().chain(sort_exprs(&fc.agg_order)).collect(),
                    Vec::new(),
                )
            };
            aggregated.extend(present(&fc.agg_filter));
            Some(Call::Aggregate {
                aggregated,
                direct,
                location: fc.location,
            })
        }
        ast::Node::JsonObjectAgg(_) | ast::Node::JsonArrayAgg(_) => {
            let ctor = match node.as_ref()? {
                ast::Node::JsonObjectAgg(a) => a.constructor.as_deref(),
                ast::Node::JsonArrayAgg(a) => a.constructor.as_deref(),
                _ => None,
            };
            let location = ctor.map_or(-1, |c| c.location);
            if ctor.is_some_and(|c| c.over.is_some()) {
                return Some(Call::Window(location));
            }
            Some(Call::Aggregate {
                aggregated: crate::resolve::expr_children(node),
                direct: Vec::new(),
                location,
            })
        }
        ast::Node::GroupingFunc(g) => Some(Call::Grouping(g)),
        _ => None,
    }
}

/// The PARTITION BY / ORDER BY expressions of a window definition.
fn window_key_exprs(wd: &ast::WindowDef) -> impl Iterator<Item = &Option<ast::Node>> {
    wd.partition_clause
        .iter()
        .chain(wd.order_clause.iter().filter_map(|o| match o.as_ref() {
            Some(ast::Node::SortBy(sb)) => present(&sb.node),
            _ => None,
        }))
}

/// The expressions of a window definition, frame offsets included.
fn window_def_exprs(wd: &ast::WindowDef) -> impl Iterator<Item = &Option<ast::Node>> {
    window_key_exprs(wd)
        .chain(present(&wd.start_offset))
        .chain(present(&wd.end_offset))
}

/// The direct sub-expressions of `node` at its own query level — a window
/// call's window definition included.
fn level_children(node: &Option<ast::Node>) -> Vec<&Option<ast::Node>> {
    let mut out = crate::resolve::expr_children(node);
    let over = match node.as_ref() {
        Some(ast::Node::FuncCall(fc)) => fc.over.as_deref(),
        Some(ast::Node::JsonObjectAgg(a)) => {
            a.constructor.as_deref().and_then(|c| c.over.as_deref())
        }
        Some(ast::Node::JsonArrayAgg(a)) => {
            a.constructor.as_deref().and_then(|c| c.over.as_deref())
        }
        _ => None,
    };
    out.extend(over.into_iter().flat_map(window_def_exprs));
    out
}

/// Every expression SELECT `sel` evaluates at its own level: its clauses,
/// its FROM items' join conditions and function arguments; not the
/// subqueries it contains.
fn select_level_exprs(sel: &ast::SelectStmt) -> Vec<&Option<ast::Node>> {
    fn from_item<'a>(n: &'a Option<ast::Node>, out: &mut Vec<&'a Option<ast::Node>>) {
        match n.as_ref() {
            Some(ast::Node::JoinExpr(j)) => {
                for side in [&j.larg, &j.rarg].into_iter().filter_map(present) {
                    from_item(side, out);
                }
                out.extend(present(&j.quals));
            }
            Some(ast::Node::RangeFunction(rf)) => {
                for f in &rf.functions {
                    if let Some(ast::Node::List(pair)) = f.as_ref() {
                        out.extend(pair.items.first());
                    }
                }
            }
            Some(ast::Node::RangeTableSample(ts)) => {
                if ts.relation.is_some() {
                    from_item(&ts.relation, out);
                }
                out.extend(ts.args.iter());
                out.extend(present(&ts.repeatable));
            }
            _ => {}
        }
    }
    let mut out: Vec<&Option<ast::Node>> = Vec::new();
    for t in &sel.target_list {
        if let Some(ast::Node::ResTarget(rt)) = t.as_ref() {
            out.extend(present(&rt.val));
        }
    }
    for f in &sel.from_clause {
        from_item(f, &mut out);
    }
    out.extend(present(&sel.where_clause));
    out.extend(sel.group_clause.iter());
    out.extend(present(&sel.having_clause));
    for w in &sel.window_clause {
        if let Some(ast::Node::WindowDef(wd)) = w.as_ref() {
            out.extend(window_def_exprs(wd));
        }
    }
    out.extend(sel.distinct_clause.iter().filter(|n| n.is_some()));
    for s in &sel.sort_clause {
        if let Some(ast::Node::SortBy(sb)) = s.as_ref() {
            out.extend(present(&sb.node));
        }
    }
    out.extend(present(&sel.limit_offset));
    out.extend(present(&sel.limit_count));
    for row in &sel.values_lists {
        if let Some(ast::Node::List(l)) = row.as_ref() {
            out.extend(l.items.iter());
        }
    }
    out
}

/// Post-order visit of `node` and its same-level sub-expressions: PG's
/// order of transforming the calls, each after its arguments.
fn visit_level_post<'a>(node: &'a Option<ast::Node>, f: &mut dyn FnMut(&'a Option<ast::Node>)) {
    for c in level_children(node) {
        visit_level_post(c, f);
    }
    f(node);
}

/// Run `f` on every same-level expression of the SELECT `node` with
/// `chain` entered into its level, and likewise for its set-operation arms,
/// FROM subqueries and CTEs as deeper levels (sublinks inside the
/// expressions are `f`'s business). Levels the analysis never registered
/// are skipped.
fn visit_select(
    node: &Option<ast::Node>,
    depth: usize,
    chain: &mut Chain,
    f: &mut dyn FnMut(&Option<ast::Node>, usize, &mut Chain),
) {
    if let Some(ast::Node::SelectStmt(sel)) = node.as_ref() {
        visit_select_stmt(sel, depth, chain, f);
    }
}

fn visit_select_stmt(
    sel: &ast::SelectStmt,
    depth: usize,
    chain: &mut Chain,
    f: &mut dyn FnMut(&Option<ast::Node>, usize, &mut Chain),
) {
    fn from_subqueries<'a>(n: &'a Option<ast::Node>, out: &mut Vec<&'a Option<ast::Node>>) {
        match n.as_ref() {
            Some(ast::Node::JoinExpr(j)) => {
                for side in [&j.larg, &j.rarg].into_iter().filter_map(present) {
                    from_subqueries(side, out);
                }
            }
            Some(ast::Node::RangeSubselect(rs)) => out.extend(present(&rs.subquery)),
            _ => {}
        }
    }
    let Some(info) = level_info(sel) else {
        return;
    };
    chain.enter(info.ns);
    for arm in [&sel.larg, &sel.rarg].into_iter().flatten() {
        visit_select_stmt(arm, depth + 1, chain, f);
    }
    if let Some(with) = &sel.with_clause {
        for cte in &with.ctes {
            if let Some(ast::Node::CommonTableExpr(c)) = cte.as_ref() {
                visit_select(&c.ctequery, depth + 1, chain, f);
            }
        }
    }
    let mut subs = Vec::new();
    for item in &sel.from_clause {
        from_subqueries(item, &mut subs);
    }
    for s in subs {
        visit_select(s, depth + 1, chain, f);
    }
    for e in select_level_exprs(sel) {
        f(e, depth, chain);
    }
    chain.leave();
}

/// PG's check_agg_arguments / find_minimum_var_level: how many levels out
/// from the innermost level of `chain` a call with these arguments belongs
/// to — the nearest level any column reference in them reaches (a
/// sublink's references to its own levels aside), or 0 without any.
fn call_level(args: &[&Option<ast::Node>], chain: &mut Chain) -> usize {
    fn walk(node: &Option<ast::Node>, depth: usize, chain: &mut Chain, min: &mut Option<usize>) {
        match node.as_ref() {
            Some(ast::Node::ColumnRef(cr)) => {
                if let Some(t) = chain.resolve(cr)
                    && t.level >= depth
                {
                    let up = t.level - depth;
                    *min = Some(min.map_or(up, |m| m.min(up)));
                }
            }
            Some(ast::Node::SubLink(sl)) => {
                if sl.testexpr.is_some() {
                    walk(&sl.testexpr, depth, chain, min);
                }
                if sl.subselect.is_some() {
                    visit_select(&sl.subselect, depth + 1, chain, &mut |e, d, c| {
                        walk(e, d, c, min)
                    });
                }
            }
            Some(ast::Node::JsonArrayQueryConstructor(q)) => {
                if q.query.is_some() {
                    visit_select(&q.query, depth + 1, chain, &mut |e, d, c| {
                        walk(e, d, c, min)
                    });
                }
            }
            _ => {
                for c in level_children(node) {
                    walk(c, depth, chain, min);
                }
            }
        }
    }
    let mut min = None;
    for a in args {
        walk(a, 0, chain, &mut min);
    }
    min.unwrap_or(0)
}

/// What PG's check_agg_arguments_walker finds in expressions written at
/// the innermost level of a chain, sublinks included: the nearest level
/// (`0`: that one) a column reference, and an aggregate or `GROUPING(…)`
/// call, belongs to — each with its location.
#[derive(Default)]
struct ArgLevels {
    var: Option<(usize, i32)>,
    agg: Option<(usize, i32)>,
}

fn arg_levels(args: &[&Option<ast::Node>], chain: &mut Chain, snapshot: &PgCatalog) -> ArgLevels {
    fn lower(slot: &mut Option<(usize, i32)>, level: usize, location: i32) {
        if slot.is_none_or(|(l, _)| level < l) {
            *slot = Some((level, location));
        }
    }
    fn walk(
        node: &Option<ast::Node>,
        depth: usize,
        chain: &mut Chain,
        snapshot: &PgCatalog,
        out: &mut ArgLevels,
    ) {
        match node.as_ref() {
            Some(ast::Node::ColumnRef(cr)) => {
                if let Some(t) = chain.resolve(cr)
                    && t.level >= depth
                {
                    lower(&mut out.var, t.level - depth, cr.location);
                }
                return;
            }
            Some(ast::Node::SubLink(sl)) => {
                if sl.testexpr.is_some() {
                    walk(&sl.testexpr, depth, chain, snapshot, out);
                }
                if sl.subselect.is_some() {
                    visit_select(&sl.subselect, depth + 1, chain, &mut |e, d, c| {
                        walk(e, d, c, snapshot, out)
                    });
                }
                return;
            }
            Some(ast::Node::JsonArrayQueryConstructor(q)) => {
                if q.query.is_some() {
                    visit_select(&q.query, depth + 1, chain, &mut |e, d, c| {
                        walk(e, d, c, snapshot, out)
                    });
                }
                return;
            }
            _ => {}
        }
        // An aggregate's level, seen from where it is written; then on
        // into its arguments, as PG's walker goes on.
        let call = match call_of(node, snapshot) {
            Some(Call::Aggregate {
                aggregated,
                location,
                ..
            }) => Some((call_level(&aggregated, chain), location)),
            Some(Call::Grouping(g)) => {
                let args: Vec<&Option<ast::Node>> = g.args.iter().collect();
                Some((call_level(&args, chain), g.location))
            }
            Some(Call::Window(_)) | None => None,
        };
        if let Some((up, location)) = call
            && up >= depth
        {
            lower(&mut out.agg, up - depth, location);
        }
        for c in level_children(node) {
            walk(c, depth, chain, snapshot, out);
        }
    }
    let mut out = ArgLevels::default();
    for a in args {
        walk(a, 0, chain, snapshot, &mut out);
    }
    out
}

/// PG's check_agg_arguments for an aggregate written at the innermost
/// level of `chain`, belonging `level` levels out: no aggregate of that
/// level among its aggregated arguments, and no variable of a lower level
/// nor aggregate of that level or a lower one among its direct arguments —
/// sublinks included (`avg((SELECT count(t.x) FROM u))` nests `t`'s
/// aggregates).
fn check_agg_arguments(
    aggregated: &[&Option<ast::Node>],
    direct: &[&Option<ast::Node>],
    level: usize,
    chain: &mut Chain,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let span = crate::error::SourceSpan::from_node_qname;
    let nested = |location: i32| crate::pgmsg::nested_aggregate(span(location)).finalize_implicit();
    if let Some((l, location)) = arg_levels(aggregated, chain, snapshot).agg
        && l <= level
    {
        return Err(nested(location));
    }
    if direct.is_empty() {
        return Ok(());
    }
    let found = arg_levels(direct, chain, snapshot);
    if let Some((l, location)) = found.var
        && l < level
    {
        return Err(
            crate::pgmsg::outer_aggregate_direct_argument_variable(span(location))
                .finalize_implicit(),
        );
    }
    if let Some((l, location)) = found.agg
        && l <= level
    {
        return Err(nested(location));
    }
    Ok(())
}

/// `node` with every column reference that resolves, from the innermost
/// level of `chain`, to level `level` (not those inside its sublinks)
/// replaced by the entry and column it names — so that two spellings of
/// the same expression (`t.a + 1`, `a + 1`) compare equal under
/// [`Equal`], the analyzer's stand-in for PG's `equal()` on transformed
/// expressions — with its type names resolved ([`resolve_type_names`]).
/// Given `scope`, the scope of level `level`, the casts over its columns
/// that are no coercion are dropped too ([`strip_noop_casts`]), as PG's
/// transformation drops them.
fn canonical(
    node: &Option<ast::Node>,
    chain: &Chain,
    level: usize,
    scope: Option<&Scope>,
    snapshot: &PgCatalog,
) -> Option<ast::Node> {
    use typedpg_pg_query::{NodeMut, NodeRef};
    let node = &column_notation_calls(whole_row_fields(node.as_ref(), chain).as_ref(), chain);
    let stripped;
    let node = match scope {
        Some(scope) => {
            stripped = strip_noop_casts(node.as_ref(), scope, snapshot, &|cr| {
                chain.resolve(cr).is_some_and(|t| t.level == level)
            });
            &stripped
        }
        None => node,
    };
    let mut tree = ast::ParseResult {
        version: 0,
        stmts: vec![ast::RawStmt {
            stmt: resolve_type_names(node, snapshot),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    let mut inner: HashSet<usize> = HashSet::new();
    let mut resolved: HashMap<usize, String> = HashMap::new();
    for (n, _) in tree.nodes() {
        let sub = match n {
            NodeRef::SubLink(sl) => sl.subselect.as_ref(),
            NodeRef::JsonArrayQueryConstructor(q) => q.query.as_ref(),
            _ => None,
        };
        if let Some(sub) = sub {
            for (m, _) in sub.nodes() {
                if let NodeRef::ColumnRef(cr) = m {
                    inner.insert(cr as *const ast::ColumnRef as usize);
                }
            }
        }
    }
    for (n, _) in tree.nodes() {
        if let NodeRef::ColumnRef(cr) = n {
            let key = cr as *const ast::ColumnRef as usize;
            if !inner.contains(&key)
                && let Some(RefTarget {
                    level: l,
                    target: Some((alias, column)),
                }) = chain.resolve(cr)
                && l == level
            {
                let name = match (column, chain.namespace(level)) {
                    (Some(col), Some(ns)) => ns.canonical_name(&(alias, col)),
                    (column, _) => column_key(&alias, column.as_deref().unwrap_or("*")),
                };
                resolved.insert(key, name);
            }
        }
    }
    // SAFETY: the tree is neither moved nor dropped while the pointers are
    // used; only the resolved column references are written, replacing
    // their `fields` (whose `String` children no pointer is dereferenced
    // through afterwards).
    unsafe {
        for (n, _) in tree.nodes_mut() {
            if let NodeMut::ColumnRef(cr) = n
                && let Some(name) = resolved.get(&(cr as usize))
            {
                (*cr).fields = vec![Some(string_node(name))];
            }
        }
    }
    tree.stmts.pop().and_then(|s| s.stmt)
}

/// `node` with each field selection `(t).f` on an entry's whole row (not
/// inside its sublinks) that names a column of the entry written `t.f`
/// ([`Chain::whole_row_field`]).
fn whole_row_fields(node: Option<&ast::Node>, chain: &Chain) -> Option<ast::Node> {
    let mut out = node.cloned();
    if let Some(out) = out.as_mut() {
        out.rewrite(&mut |n| !matches!(n, ast::Node::SelectStmt(_)), &mut |n| {
            if let ast::Node::AIndirection(ind) = n
                && let Some(column) = chain.whole_row_field(ind)
            {
                *n = column;
            }
        });
    }
    out
}

/// [`whole_row_fields`] against the entries of `scope`'s own level.
fn own_whole_row_fields(node: Option<&ast::Node>, scope: &Scope) -> Option<ast::Node> {
    whole_row_fields(
        node,
        &Chain::single(Some(Rc::new(Namespace::of(&scope.sources)))),
    )
}

/// `node` with each column notation `t.f` of its level (not inside its
/// sublinks) written as the call `f(t.*)` it stands for, so both spellings
/// compare equal.
fn column_notation_calls(node: Option<&ast::Node>, chain: &Chain) -> Option<ast::Node> {
    let mut out = node.cloned();
    if let Some(out) = out.as_mut() {
        out.rewrite(&mut |n| !matches!(n, ast::Node::SelectStmt(_)), &mut |n| {
            if let ast::Node::ColumnRef(cr) = n
                && let Some((name, whole_row)) = chain.column_notation(cr)
            {
                let call = ast::FuncCall {
                    funcname: vec![Some(string_node(&name))],
                    args: vec![Some(whole_row)],
                    funcformat: ast::CoercionForm::CoerceExplicitCall,
                    location: cr.location,
                    ..Default::default()
                };
                *n = ast::Node::FuncCall(NodeBox::new(call));
            }
        });
    }
    out
}

/// The name [`canonical`] gives a reference to column `column` of `alias`.
fn column_key(alias: &str, column: &str) -> String {
    format!("\u{1}{alias}\u{1}{column}")
}

fn string_node(s: &str) -> ast::Node {
    ast::Node::String(NodeBox::new(ast::String { sval: s.to_owned() }))
}

/// Whether two expressions of the current level are PG-`equal()` (up to
/// how their column references are spelled).
pub(crate) fn same_level_exprs_equal(
    a: &Option<ast::Node>,
    b: &Option<ast::Node>,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> bool {
    let chain = Chain::single(Some(Rc::new(Namespace::of(&scope.sources))));
    canonical(a, &chain, 0, Some(scope), snapshot).equal(&canonical(
        b,
        &chain,
        0,
        Some(scope),
        snapshot,
    ))
}

/// The locations of the first aggregate, `GROUPING(…)` and window call in
/// `node` that belong to the current level — PG's placement rules only
/// concern those (in `SELECT (SELECT 1 FROM u WHERE count(t.a) > 0) FROM
/// t` the aggregate is the outer query's, and allowed there). "First" in
/// PG's transform order, a call after its arguments: in `sum(count(x))`
/// the rule fails at `count`.
#[derive(Default)]
pub(crate) struct LevelCalls {
    pub aggregate: Option<i32>,
    pub grouping: Option<i32>,
    pub window: Option<i32>,
}

pub(crate) fn level_calls(
    node: &Option<ast::Node>,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> LevelCalls {
    let mut chain = current_chain(scope);
    let mut out = LevelCalls::default();
    let mut calls: Vec<Call<'_>> = Vec::new();
    visit_level_post(node, &mut |n| calls.extend(call_of(n, snapshot)));
    for call in calls {
        match call {
            Call::Aggregate {
                aggregated,
                location,
                ..
            } => {
                if out.aggregate.is_none() && call_level(&aggregated, &mut chain) == 0 {
                    out.aggregate = Some(location);
                }
            }
            Call::Grouping(g) => {
                let args: Vec<&Option<ast::Node>> = g.args.iter().collect();
                if out.grouping.is_none() && call_level(&args, &mut chain) == 0 {
                    out.grouping = Some(g.location);
                }
            }
            Call::Window(location) => {
                out.window.get_or_insert(location);
            }
        }
    }
    out
}

/// How many levels out aggregate call `fc`, written at the current level,
/// belongs: the innermost level of the columns its aggregated arguments
/// (ORDER BY and FILTER included) read — `0` for this level's own.
pub(crate) fn aggregate_levels_up(fc: &ast::FuncCall, scope: &Scope) -> usize {
    let sort_exprs = fc.agg_order.iter().filter_map(|o| match o.as_ref() {
        Some(ast::Node::SortBy(sb)) => present(&sb.node),
        _ => Some(o),
    });
    let mut aggregated: Vec<&Option<ast::Node>> = if fc.agg_within_group {
        sort_exprs.collect()
    } else {
        fc.args.iter().chain(sort_exprs).collect()
    };
    aggregated.extend(present(&fc.agg_filter));
    levels_up_of(&aggregated, scope)
}

/// How many levels out an aggregate whose aggregated expressions are
/// `aggregated`, written at the current level, belongs.
pub(crate) fn levels_up_of(aggregated: &[&Option<ast::Node>], scope: &Scope) -> usize {
    call_level(aggregated, &mut current_chain(scope))
}

/// Hand a call found at the current level that belongs `levels_up` levels
/// out to that level: an error when the clause that level is analyzing
/// forbids it, else it makes that level grouped. `grouping` carries a
/// `GROUPING(…)` call's arguments, canonicalized against its level.
fn attribute_outer(
    levels_up: usize,
    grouping: Option<Vec<Option<ast::Node>>>,
    location: i32,
) -> Result<(), AnalyzeError> {
    let is_grouping = grouping.is_some();
    let clause = FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        let n = f.len();
        if levels_up >= n {
            return None;
        }
        let frame = &mut f[n - 1 - levels_up];
        if frame.clause.is_some() {
            return frame.clause;
        }
        match grouping {
            Some(args) => frame.outer_grouping.push((args, location)),
            None => frame.outer_aggs = true,
        }
        None
    });
    match clause {
        Some(c) if is_grouping => Err(crate::clause::grouping_not_allowed(c, Some(location))),
        Some(c) => Err(crate::clause::aggregate_not_allowed(c, Some(location))),
        None => Ok(()),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// GROUP BY validation (PG's parseCheckAggregates)
// ──────────────────────────────────────────────────────────────────────────────

/// End of a SELECT level's analysis: settle which level every aggregate and
/// `GROUPING(…)` written here belongs to (outer ones are handed out — see
/// [`attribute_outer`]), record whether the level has aggregates, and, for
/// a grouped level, run PG's `parseCheckAggregates`:
///
/// - `GROUPING(…)` arguments must be grouping expressions of the level
///   (`finalize_grouping_exprs`, 42803);
/// - every column reference of the level outside its aggregates — in the
///   select list, ORDER BY, DISTINCT ON, window definitions and HAVING, and
///   inside their sublinks — must be grouped, part of a grouped expression,
///   or functionally dependent on a grouped primary key
///   (`check_ungrouped_columns`, 42803).
///
/// Expressions are compared the way PG's `equal()` compares transformed
/// trees, up to how column references are spelled (see [`canonical`]).
pub(crate) fn finish_select_level(
    sel: &ast::SelectStmt,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let mut chain = current_chain(scope);

    let mut own_aggs = false;
    let mut own_grouping: Vec<&ast::GroupingFunc> = Vec::new();
    let mut calls: Vec<Call<'_>> = Vec::new();
    // In PG's transform order, a call after its arguments: in
    // `max(sum(y))`, `sum` (an outer level's) meets that level's clause
    // rule before `max` is checked for nesting.
    for e in select_level_exprs(sel) {
        visit_level_post(e, &mut |n| calls.extend(call_of(n, snapshot)));
    }
    for call in calls {
        match call {
            Call::Aggregate {
                aggregated,
                direct,
                location,
            } => {
                let level = call_level(&aggregated, &mut chain);
                check_agg_arguments(&aggregated, &direct, level, &mut chain, snapshot)?;
                match level {
                    0 => own_aggs = true,
                    up => attribute_outer(up, None, location)?,
                }
            }
            Call::Grouping(g) => {
                if g.args.len() >= 32 {
                    return Err(crate::pgmsg::grouping_too_many_arguments(
                        crate::error::SourceSpan::from_node_qname(g.location),
                    )
                    .finalize_implicit());
                }
                let args: Vec<&Option<ast::Node>> = g.args.iter().collect();
                match call_level(&args, &mut chain) {
                    0 => own_grouping.push(g),
                    up => {
                        let canon = g
                            .args
                            .iter()
                            .map(|a| canonical(a, &chain, up, None, snapshot))
                            .collect();
                        attribute_outer(up, Some(canon), g.location)?;
                    }
                }
            }
            Call::Window(_) => {}
        }
    }
    let (outer_aggs, outer_grouping) = FRAMES.with(|f| {
        f.borrow_mut()
            .last_mut()
            .map(|top| (top.outer_aggs, std::mem::take(&mut top.outer_grouping)))
            .unwrap_or_default()
    });
    let has_aggs = own_aggs || outer_aggs || !own_grouping.is_empty() || !outer_grouping.is_empty();
    LEVELS.with(|l| {
        if let Some(info) = l.borrow_mut().get_mut(&level_key(sel)) {
            info.has_aggs = has_aggs;
        }
    });
    if !has_aggs && sel.group_clause.is_empty() && sel.having_clause.is_none() {
        return Ok(());
    }
    if grouping_set_count(&sel.group_clause) > MAX_GROUPING_SETS {
        return Err(crate::pgmsg::too_many_grouping_sets().finalize_implicit());
    }

    let mut own = Chain::single(chain.namespace(0));
    let group = GroupExprs::collect(sel, scope, targets, &own, snapshot);

    // finalize_grouping_exprs.
    let grouping_error = |location: i32| {
        crate::error::RawError::new(
            AnalyzeError::GroupingError(
                "arguments to GROUPING must be grouping expressions of the associated query \
                 level"
                    .into(),
            ),
            crate::error::SourceSpan::from_node_qname(location),
            None,
        )
        .finalize_implicit()
    };
    for g in own_grouping {
        if g.args
            .iter()
            .any(|a| !group.contains(&canonical(a, &own, 0, Some(scope), snapshot)))
        {
            return Err(grouping_error(g.location));
        }
    }
    for (args, location) in &outer_grouping {
        if args.iter().any(|a| !group.contains(a)) {
            return Err(grouping_error(*location));
        }
    }

    // Primary-key functional dependency (check_functional_grouping): a
    // table whose whole primary key is grouped — by every grouping set —
    // determines all of its columns.
    let expansion = expand_grouping_sets(&sel.group_clause, scope, targets, snapshot);
    let own_ns = own.namespace(0).unwrap_or_default();
    let common: HashSet<ColKey> = match expansion.common {
        Some(common) => common
            .iter()
            .flat_map(|k| match own_ns.flatten(k) {
                None => vec![k.clone()],
                Some(Flat::Var(k)) => vec![k],
                Some(_) => Vec::new(),
            })
            .collect(),
        None => group.vars.clone(),
    };
    // Each such table's alias, with the primary key the query then relies on.
    let mut fully_grouped: HashMap<String, crate::oid::PgConstraintOid> = HashMap::new();
    // The tables behind an aliased join are range-table entries too (its
    // columns flatten to theirs).
    for src in scope.sources.iter().chain(&scope.shadowed_sources) {
        let Some(qn) = &src.source_qn else {
            continue;
        };
        let Some(class) = snapshot.resolve_table(Some(&qn.schema), &qn.name) else {
            continue;
        };
        let attrs = snapshot.attributes_of(class.oid);
        if let Some(pk) = snapshot
            .pg_constraint_values()
            .find(|c| c.conrelid == class.oid && matches!(c.contype, ConType::PrimaryKey))
        {
            let all_grouped = !pk.conkey.is_empty()
                && pk.conkey.iter().all(|&attnum| {
                    attrs
                        .iter()
                        .find(|a| a.attnum == attnum)
                        .is_some_and(|a| common.contains(&(src.alias.clone(), a.attname.clone())))
                });
            if all_grouped {
                fully_grouped.insert(src.alias.clone(), pk.oid);
            }
        }
    }

    let check = UngroupedCheck {
        snapshot,
        scope,
        group: &group,
        fully_grouped: &fully_grouped,
    };
    // The select list (a `*` contributes plain column references), the
    // resjunk entries PG adds for ORDER BY / DISTINCT ON / window
    // definitions, then HAVING. Items naming a select-list entry (by
    // position or output name) are that entry.
    let output_names: HashSet<String> = targets
        .iter()
        .filter_map(|t| match t {
            ExpandedTarget::Expr(rt) => Some(crate::resolve::target_resname(rt)),
            _ => None,
        })
        .collect();
    let names_target = |n: &Option<ast::Node>| match n.as_ref() {
        Some(ast::Node::AConst(_)) => true,
        Some(ast::Node::ColumnRef(cr)) => matches!(
            expr::extract_string_fields(&cr.fields).as_slice(),
            [name] if output_names.contains(name)
        ),
        _ => false,
    };
    for t in targets {
        match *t {
            ExpandedTarget::Expr(rt) => {
                if rt.val.is_some() {
                    check.expr(&rt.val, 0, &mut own)?;
                }
            }
            ExpandedTarget::StarColumn {
                column: Some((alias, col)),
                location,
            } => {
                for (alias, col) in own_ns.flat_columns(&(alias.to_owned(), col.to_owned())) {
                    check.column(&alias, Some(&col), 0, location)?;
                }
            }
            ExpandedTarget::StarColumn { column: None, .. } => {}
        }
    }
    for s in &sel.sort_clause {
        if let Some(ast::Node::SortBy(sb)) = s.as_ref()
            && sb.node.is_some()
            && !names_target(&sb.node)
        {
            check.expr(&sb.node, 0, &mut own)?;
        }
    }
    for d in sel.distinct_clause.iter().filter(|n| n.is_some()) {
        if !names_target(d) {
            check.expr(d, 0, &mut own)?;
        }
    }
    for w in &sel.window_clause {
        if let Some(ast::Node::WindowDef(wd)) = w.as_ref() {
            for e in window_key_exprs(wd) {
                check.expr(e, 0, &mut own)?;
            }
        }
    }
    if sel.having_clause.is_some() {
        check.expr(&sel.having_clause, 0, &mut own)?;
    }
    Ok(())
}

/// A call of a VALUES list's own level in its ORDER BY. Parse analysis
/// accepts it as a resjunk sort key, but a VALUES query gets no Agg or
/// WindowAgg node, so initializing the expression fails at every execution
/// (`ExecInitExprRec`, XX000).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnplannedCall {
    Aggregate,
    Grouping,
    Window,
}

impl UnplannedCall {
    /// `ExecInitExprRec`'s message for the node.
    pub(crate) fn message(self) -> &'static str {
        match self {
            UnplannedCall::Aggregate => "Aggref found in non-Agg plan node",
            UnplannedCall::Grouping => "GroupingFunc found in non-Agg plan node",
            UnplannedCall::Window => "WindowFunc found in non-WindowAgg plan node",
        }
    }
}

thread_local! {
    /// Per analyzed VALUES level (keyed by its SelectStmt's address), the
    /// first call of its ORDER BY the executor can't run — read once the
    /// statement is analyzed, where a VALUES certain to be planned fails
    /// with it ([`values_sort_call`]). Cleared before each statement.
    static VALUES_SORT_CALLS: RefCell<HashMap<usize, (UnplannedCall, i32)>> =
        RefCell::new(HashMap::new());
}

/// Forget what earlier statements' VALUES levels recorded.
pub(crate) fn clear_values_sort_calls() {
    VALUES_SORT_CALLS.with(|v| v.borrow_mut().clear());
}

/// The call of VALUES `sel`'s ORDER BY its execution fails on, if any.
pub(crate) fn values_sort_call(sel: &ast::SelectStmt) -> Option<(UnplannedCall, i32)> {
    VALUES_SORT_CALLS.with(|v| v.borrow().get(&level_key(sel)).copied())
}

/// Run `f` with the current level's namespace being `sources`: a VALUES
/// list's ORDER BY sees its columns (`column1`, …), which its rows don't.
pub(crate) fn with_own_namespace<R>(sources: &[TableSource], f: impl FnOnce() -> R) -> R {
    let ns = Rc::new(Namespace::of(sources));
    let prev = FRAMES.with(|fr| fr.borrow_mut().last_mut().map(|top| top.ns.replace(ns)));
    let out = f();
    if let Some(prev) = prev {
        FRAMES.with(|fr| {
            if let Some(top) = fr.borrow_mut().last_mut() {
                top.ns = prev;
            }
        });
    }
    out
}

/// [`finish_select_level`] for VALUES `sel`, whose rows see `rows_scope`
/// and whose ORDER BY also sees its own columns, `sort_sources`. The rows
/// hold no aggregate, `GROUPING(…)` or window call of the level (rejected
/// as they are analyzed); an outer level's is handed to it. A call of the
/// level in the ORDER BY — directly, or an aggregate in a sublink there —
/// is recorded for [`values_sort_call`].
pub(crate) fn finish_values_level(
    sel: &ast::SelectStmt,
    rows_scope: &Scope,
    sort_sources: &[TableSource],
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let rows_chain = current_chain(rows_scope);
    let mut sort_chain = rows_chain.clone();
    sort_chain.levels[0] = Some(Rc::new(Namespace::of(sort_sources)));
    let rows: Vec<&Option<ast::Node>> = sel
        .values_lists
        .iter()
        .filter_map(|row| match row.as_ref() {
            Some(ast::Node::List(l)) => Some(l.items.iter()),
            _ => None,
        })
        .flatten()
        .collect();
    let sort: Vec<&Option<ast::Node>> = sel
        .sort_clause
        .iter()
        .filter_map(|s| match s.as_ref() {
            Some(ast::Node::SortBy(sb)) => present(&sb.node),
            _ => None,
        })
        .collect();
    for (exprs, mut chain) in [(rows, rows_chain), (sort.clone(), sort_chain.clone())] {
        let mut calls: Vec<Call<'_>> = Vec::new();
        for e in exprs {
            visit_level_post(e, &mut |n| calls.extend(call_of(n, snapshot)));
        }
        for call in calls {
            match call {
                Call::Aggregate {
                    aggregated,
                    direct,
                    location,
                } => {
                    let level = call_level(&aggregated, &mut chain);
                    check_agg_arguments(&aggregated, &direct, level, &mut chain, snapshot)?;
                    if level > 0 {
                        attribute_outer(level, None, location)?;
                    }
                }
                Call::Grouping(g) => {
                    let args: Vec<&Option<ast::Node>> = g.args.iter().collect();
                    let up = call_level(&args, &mut chain);
                    if up > 0 {
                        let canon = g
                            .args
                            .iter()
                            .map(|a| canonical(a, &chain, up, None, snapshot))
                            .collect();
                        attribute_outer(up, Some(canon), g.location)?;
                    }
                }
                Call::Window(_) => {}
            }
        }
    }
    // The executor initializes the sort keys in order, each top-down.
    fn first(
        node: &Option<ast::Node>,
        chain: &mut Chain,
        snapshot: &PgCatalog,
    ) -> Option<(UnplannedCall, i32)> {
        let found = match call_of(node, snapshot) {
            Some(Call::Window(location)) => Some((UnplannedCall::Window, location)),
            Some(Call::Aggregate {
                aggregated,
                location,
                ..
            }) => (call_level(&aggregated, chain) == 0)
                .then_some((UnplannedCall::Aggregate, location)),
            Some(Call::Grouping(g)) => {
                let args: Vec<&Option<ast::Node>> = g.args.iter().collect();
                (call_level(&args, chain) == 0).then_some((UnplannedCall::Grouping, g.location))
            }
            None => None,
        };
        found.or_else(|| {
            level_children(node)
                .into_iter()
                .find_map(|c| first(c, chain, snapshot))
        })
    }
    let mut chain = sort_chain;
    let own = sort.iter().find_map(|e| first(e, &mut chain, snapshot));
    // An aggregate of the level in a sublink of the ORDER BY: the sublink's
    // parameter holds it.
    let (outer_aggs, outer_grouping) = FRAMES.with(|f| {
        f.borrow_mut()
            .last_mut()
            .map(|top| {
                let g = std::mem::take(&mut top.outer_grouping);
                (top.outer_aggs, g.first().map(|(_, l)| *l))
            })
            .unwrap_or_default()
    });
    let call = own
        .or(outer_grouping.map(|l| (UnplannedCall::Grouping, l)))
        .or(outer_aggs.then_some((UnplannedCall::Aggregate, -1)));
    if let Some(call) = call {
        VALUES_SORT_CALLS.with(|v| v.borrow_mut().insert(level_key(sel), call));
    }
    Ok(())
}

/// The GROUP BY expressions of a level (PG's `groupClauses`): the plain
/// columns it groups by, and every grouping expression canonicalized.
struct GroupExprs {
    vars: HashSet<ColKey>,
    exprs: Vec<Option<ast::Node>>,
    have_non_var: bool,
}

impl GroupExprs {
    fn collect(
        sel: &ast::SelectStmt,
        scope: &Scope,
        targets: &[ExpandedTarget<'_>],
        own: &Chain,
        snapshot: &PgCatalog,
    ) -> Self {
        let mut out = GroupExprs {
            vars: HashSet::new(),
            exprs: Vec::new(),
            have_non_var: false,
        };
        let empty = Namespace::default();
        let ns: &Namespace = own
            .levels
            .first()
            .and_then(|n| n.as_deref())
            .unwrap_or(&empty);
        let mut stack: Vec<&Option<ast::Node>> = sel.group_clause.iter().rev().collect();
        while let Some(g) = stack.pop() {
            if let Some(ast::Node::GroupingSet(gs)) = g.as_ref() {
                stack.extend(gs.content.iter().rev());
                continue;
            }
            if let Some(items) = implicit_row_items(g) {
                stack.extend(items.iter().rev());
                continue;
            }
            // findTargetlistEntrySQL92: a position, or a name that is no
            // input column but an output column, stands for that
            // select-list entry.
            let target = match g.as_ref() {
                Some(ast::Node::AConst(ac)) => match &ac.val {
                    Some(ast::AConstValue::Integer(i)) => {
                        targets.get((i.ival as usize).wrapping_sub(1)).copied()
                    }
                    _ => None,
                },
                Some(ast::Node::ColumnRef(cr))
                    if resolve_group_column(g.as_ref(), scope, snapshot).is_none() =>
                {
                    match expr::extract_string_fields(&cr.fields).as_slice() {
                        [name] => targets
                            .iter()
                            .find(|t| {
                                matches!(t, ExpandedTarget::Expr(rt)
                                    if &crate::resolve::target_resname(rt) == name)
                            })
                            .copied(),
                        _ => None,
                    }
                }
                _ => None,
            };
            let expr = match target {
                Some(ExpandedTarget::Expr(rt)) => match rt.val.as_ref() {
                    Some(_) => &rt.val,
                    None => continue,
                },
                Some(ExpandedTarget::StarColumn {
                    column: Some((alias, col)),
                    ..
                }) => {
                    out.add_column((alias.to_owned(), col.to_owned()), ns);
                    continue;
                }
                Some(ExpandedTarget::StarColumn { column: None, .. }) => continue,
                None => g,
            };
            // A cast that is no coercion is the column itself, and so is a
            // field selected from the whole row.
            let expr = &whole_row_fields(expr.as_ref(), own);
            let expr = &strip_noop_casts(expr.as_ref(), scope, snapshot, &|cr| {
                matches!(own.resolve(cr), Some(RefTarget { level: 0, .. }))
            });
            if let Some(ast::Node::ColumnRef(cr)) = expr.as_ref()
                && let Some(RefTarget {
                    level: 0,
                    target: Some((alias, Some(col))),
                }) = own.resolve(cr)
                && own.column_notation(cr).is_none()
            {
                out.add_column((alias, col), ns);
                continue;
            }
            out.have_non_var = true;
            out.exprs
                .push(canonical(expr, own, 0, Some(scope), snapshot));
        }
        out
    }

    /// A grouping column of the level — a plain one, or a JOIN USING
    /// column standing for an expression.
    fn add_column(&mut self, key: ColKey, ns: &Namespace) {
        match ns.flatten(&key) {
            None => {
                self.vars.insert(key.clone());
            }
            Some(Flat::Var(k)) => {
                self.vars.insert(k);
            }
            Some(_) => self.have_non_var = true,
        }
        self.exprs
            .push(Some(ast::Node::ColumnRef(NodeBox::new(ast::ColumnRef {
                fields: vec![Some(string_node(&ns.canonical_name(&key)))],
                location: 0,
            }))));
    }

    fn contains(&self, canonical_expr: &Option<ast::Node>) -> bool {
        self.exprs.iter().any(|e| e.equal(canonical_expr))
    }
}

/// PG's `check_ungrouped_columns_walker` over one grouped level.
struct UngroupedCheck<'a> {
    snapshot: &'a PgCatalog,
    scope: &'a Scope,
    group: &'a GroupExprs,
    fully_grouped: &'a HashMap<String, crate::oid::PgConstraintOid>,
}

impl UngroupedCheck<'_> {
    /// Check `node`, `depth` sublink levels below the grouped level;
    /// `chain` starts at `node`'s level and ends at the grouped one.
    fn expr(
        &self,
        node: &Option<ast::Node>,
        depth: usize,
        chain: &mut Chain,
    ) -> Result<(), AnalyzeError> {
        match node.as_ref() {
            None | Some(ast::Node::AConst(_)) | Some(ast::Node::ParamRef(_)) => {
                return Ok(());
            }
            _ => {}
        }
        match call_of(node, self.snapshot) {
            Some(Call::Aggregate {
                aggregated, direct, ..
            }) => {
                let level = call_level(&aggregated, chain);
                if level == depth {
                    // An aggregate of the grouped level: its arguments are
                    // fine, its direct arguments are evaluated per group.
                    for d in direct {
                        self.expr(d, depth, chain)?;
                    }
                    return Ok(());
                }
                if level > depth {
                    return Ok(());
                }
            }
            Some(Call::Grouping(g)) => {
                let args: Vec<&Option<ast::Node>> = g.args.iter().collect();
                if call_level(&args, chain) == depth {
                    return Ok(());
                }
            }
            _ => {}
        }
        // A whole row coerced to a composite is no longer the whole-row
        // reference a `GROUP BY s` groups by, but its columns (below).
        let expanded_row = match node.as_ref() {
            Some(ast::Node::ColumnRef(cr)) => matches!(
                chain.resolve(cr),
                Some(RefTarget { target: Some((alias, None)), .. })
                    if expanded_whole_row(cr.location, &alias).is_some()
            ),
            _ => false,
        };
        if depth == 0
            && !expanded_row
            && self.group.have_non_var
            && self
                .group
                .contains(&canonical(node, chain, 0, Some(self.scope), self.snapshot))
        {
            return Ok(());
        }
        match node.as_ref() {
            Some(ast::Node::ColumnRef(cr)) => {
                if let Some(RefTarget {
                    level,
                    target: Some((alias, col)),
                }) = chain.resolve(cr)
                    && level == depth
                {
                    // Column notation `t.f` is the call `f(t.*)`: it reads
                    // the whole row.
                    if let Some((_, whole_row)) = chain.column_notation(cr) {
                        return self.expr(&Some(whole_row), depth, chain);
                    }
                    let named = match col {
                        Some(col) => vec![col],
                        // A whole row coerced to a composite is its columns.
                        None => match expanded_whole_row(cr.location, &alias) {
                            Some(cols) => cols,
                            None => return self.column(&alias, None, depth, cr.location),
                        },
                    };
                    for col in named {
                        // A JOIN USING column reads the columns it flattens to.
                        let key = (alias.clone(), col);
                        let columns = match chain.namespace(depth) {
                            Some(ns) => ns.flat_columns(&key),
                            None => vec![key],
                        };
                        for (alias, col) in columns {
                            self.column(&alias, Some(&col), depth, cr.location)?;
                        }
                    }
                }
                Ok(())
            }
            // A field selected from a whole row reads that column only.
            Some(ast::Node::AIndirection(ind)) if chain.whole_row_field(ind).is_some() => {
                let column = chain.whole_row_field(ind).expect("checked above");
                self.expr(&Some(column), depth, chain)
            }
            Some(ast::Node::SubLink(sl)) => {
                if sl.testexpr.is_some() {
                    self.expr(&sl.testexpr, depth, chain)?;
                }
                match sl.subselect.as_ref() {
                    Some(_) => self.select(&sl.subselect, depth + 1, chain),
                    None => Ok(()),
                }
            }
            Some(ast::Node::JsonArrayQueryConstructor(q)) => match q.query.as_ref() {
                Some(_) => self.select(&q.query, depth + 1, chain),
                None => Ok(()),
            },
            _ => {
                for c in level_children(node) {
                    self.expr(c, depth, chain)?;
                }
                Ok(())
            }
        }
    }

    /// Check every expression of the sublink query `node`.
    fn select(
        &self,
        node: &Option<ast::Node>,
        depth: usize,
        chain: &mut Chain,
    ) -> Result<(), AnalyzeError> {
        let mut result = Ok(());
        visit_select(node, depth, chain, &mut |e, d, c| {
            if result.is_ok() {
                result = self.expr(e, d, c);
            }
        });
        result
    }

    /// A reference to column `col` (`None`: the whole row) of entry `alias`
    /// of the grouped level.
    fn column(
        &self,
        alias: &str,
        col: Option<&str>,
        depth: usize,
        location: i32,
    ) -> Result<(), AnalyzeError> {
        if let Some(col) = col
            && self
                .group
                .vars
                .contains(&(alias.to_owned(), col.to_owned()))
        {
            return Ok(());
        }
        if let Some(pk) = self.fully_grouped.get(alias) {
            // check_functional_grouping: the query now depends on the key
            // (query->constraintDeps).
            crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::new(
                crate::ddl::depend::PG_CONSTRAINT_RELID,
                pk.into_nonzero(),
                0,
            ));
            return Ok(());
        }
        // An entry an aliased join hides goes by its name; a JOIN USING
        // column reads as the join side PG flattens it to.
        let hidden = self
            .scope
            .shadowed_sources
            .iter()
            .find(|s| s.alias == alias && s.hidden.is_some());
        let shown_alias = if let Some(s) = hidden {
            s.refname()
        } else if crate::scope::is_hidden_alias(alias) {
            col.and_then(|c| {
                self.scope
                    .sources
                    .iter()
                    .find(|s| s.join_hidden.contains(c))
                    .map(|s| s.alias.as_str())
            })
            .unwrap_or(alias)
        } else {
            alias
        };
        let col = col.unwrap_or("*");
        let span = crate::error::SourceSpan::from_node_qname(location);
        if depth > 0 {
            return Err(
                crate::pgmsg::subquery_uses_ungrouped_column(shown_alias, col, span)
                    .finalize_implicit(),
            );
        }
        Err(crate::error::RawError::new(
            AnalyzeError::GroupingError(format!(
                "column \"{shown_alias}.{col}\" must appear in the GROUP BY clause or be used in \
                 an aggregate function"
            )),
            span,
            Some(format!(
                "add `{shown_alias}.{col}` to the GROUP BY clause, or wrap it in an aggregate \
                 like max({col})"
            )),
        )
        .with_primary_label("not in GROUP BY")
        .finalize_implicit())
    }
}
