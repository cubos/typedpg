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
use typedpg_pg_query::protobuf::{self, GroupingSetKind, JoinType, node};

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
}

/// Resolve `group_clause` into the columns that some grouping set omits and
/// whether any of those sets is empty.
///
/// Only plain `ColumnRef`s are tracked; non-column expressions in the
/// `GROUP BY` (e.g. `date_trunc('day', ts)`) contribute nothing — the
/// projection references them by the inner column anyway, and that column
/// might or might not be in scope.
pub(crate) fn expand_grouping_sets(
    group_clause: &[protobuf::Node],
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
    let mut per_entry: Vec<Vec<HashSet<(String, String)>>> = Vec::new();
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
    let mut sets: Vec<HashSet<(String, String)>> = vec![HashSet::new()];
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

    let union: HashSet<(String, String)> = sets.iter().flatten().cloned().collect();
    let mut intersection = sets[0].clone();
    for s in &sets[1..] {
        intersection.retain(|x| s.contains(x));
    }
    let omitted = union.difference(&intersection).cloned().collect();
    let has_empty_set = sets.iter().any(|s| s.is_empty());
    GroupingExpansion {
        omitted,
        has_empty_set,
        common: Some(intersection),
    }
}

/// PG's limit on the number of grouping sets a GROUP BY expands to.
const MAX_GROUPING_SETS: u64 = 4096;

/// The number of grouping sets `group_clause` expands to — the product of
/// its entries' alternatives, as PG's expand_grouping_sets counts them
/// (saturating).
fn grouping_set_count(group_clause: &[protobuf::Node]) -> u64 {
    fn alternatives(node: &protobuf::Node) -> u64 {
        let Some(node::Node::GroupingSet(gs)) = node.node.as_ref() else {
            return 1;
        };
        let n = gs.content.len() as u64;
        match GroupingSetKind::try_from(gs.kind).unwrap_or(GroupingSetKind::Undefined) {
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
    node: &protobuf::Node,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<(String, String)>> {
    match node.node.as_ref() {
        Some(node::Node::GroupingSet(gs)) => {
            *saw_grouping_set = true;
            alternatives_for_grouping_set(gs, scope, targets, snapshot, saw_grouping_set)
        }
        _ => vec![singleton_set(node, scope, targets, snapshot)],
    }
}

fn alternatives_for_grouping_set(
    gs: &protobuf::GroupingSet,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
    saw_grouping_set: &mut bool,
) -> Vec<HashSet<(String, String)>> {
    let kind = GroupingSetKind::try_from(gs.kind).unwrap_or(GroupingSetKind::Undefined);
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
            let items: Vec<HashSet<(String, String)>> = gs
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
            let items: Vec<HashSet<(String, String)>> = gs
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
        GroupingSetKind::Undefined => vec![HashSet::new()],
    }
}

/// The members of an implicit row `(a, b)` in a GROUP BY list or grouping
/// set: PG's flatten_grouping_sets turns it into a list of grouping
/// expressions (`GROUP BY (a, b)` is `GROUP BY a, b`; `ROLLUP((a, b))`
/// rolls the pair up as one unit). An explicit `ROW(a, b)` stays one
/// composite expression.
pub(crate) fn implicit_row_items(node: &protobuf::Node) -> Option<&Vec<protobuf::Node>> {
    match node.node.as_ref() {
        Some(node::Node::RowExpr(r))
            if r.row_format == protobuf::CoercionForm::CoerceImplicitCast as i32 =>
        {
            Some(&r.args)
        }
        _ => None,
    }
}

/// Resolve a GROUP BY leaf as a single column. Returns a singleton set
/// `{(table_alias, column_name)}` when the leaf denotes a column — directly,
/// or through a projection ordinal / output alias ([`group_leaf`]) — or an
/// empty set otherwise (opaque expression — does not drive nullability
/// promotion).
fn singleton_set(
    node: &protobuf::Node,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> HashSet<(String, String)> {
    if let Some(items) = implicit_row_items(node) {
        return items
            .iter()
            .flat_map(|i| singleton_set(i, scope, targets, snapshot))
            .collect();
    }
    match group_leaf(node, scope, targets, snapshot) {
        GroupLeaf::Column(key) => HashSet::from([key]),
        GroupLeaf::Expr(fingerprint) => HashSet::from([(EXPR_KEY.to_owned(), fingerprint)]),
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
/// an expression of its own.
fn group_leaf(
    node: &protobuf::Node,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> GroupLeaf {
    let target_column = |rt: &protobuf::ResTarget| match rt.val.as_deref() {
        Some(v) => match resolve_group_column(v, scope) {
            Some(key) => GroupLeaf::Column(key),
            None => GroupLeaf::Expr(expr_key(v, snapshot)),
        },
        None => GroupLeaf::Other,
    };
    match node.node.as_ref() {
        Some(node::Node::AConst(ac)) => match &ac.val {
            Some(protobuf::a_const::Val::Ival(i)) => {
                match targets.get((i.ival as usize).wrapping_sub(1)) {
                    Some(ExpandedTarget::Expr(rt)) => target_column(rt),
                    Some(ExpandedTarget::StarColumn {
                        column: Some((alias, col)),
                        ..
                    }) => GroupLeaf::Column((alias.to_string(), col.to_string())),
                    _ => GroupLeaf::Other,
                }
            }
            _ => GroupLeaf::Other,
        },
        Some(node::Node::ColumnRef(cr)) => {
            if let Some(key) = resolve_group_column(node, scope) {
                return GroupLeaf::Column(key);
            }
            let alias = match cr.fields.as_slice() {
                [f] => match f.node.as_ref() {
                    Some(node::Node::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                },
                _ => None,
            };
            let target = alias.and_then(|a| {
                targets.iter().find_map(|t| match t {
                    ExpandedTarget::Expr(rt) if rt.name == a => Some(*rt),
                    _ => None,
                })
            });
            target.map_or(GroupLeaf::Unresolved, target_column)
        }
        Some(_) => GroupLeaf::Expr(expr_key(node, snapshot)),
        None => GroupLeaf::Other,
    }
}

/// The alias part of a grouping key that is an expression rather than a
/// column (no FROM entry can be named so).
pub(crate) const EXPR_KEY: &str = "\u{0}expr";

/// The key of an expression in grouping sets: its parse tree without
/// locations or column qualifiers (`t.g + 1` and `g + 1` are one key —
/// erring towards matching, which only makes more values nullable), its
/// type names resolved as the GROUP BY check compares them
/// ([`resolve_type_names`]: `CAST(g AS bigint)` is `g::int8`).
pub(crate) fn expr_key(node: &protobuf::Node, snapshot: &PgCatalog) -> String {
    crate::resolve::node_fingerprint(&crate::resolve::predtest_unqualify(&resolve_type_names(
        node, snapshot,
    )))
}

/// [`crate::resolve::node_fingerprint`] of `node` with its type names
/// resolved ([`resolve_type_names`]) — the same for two expressions PG's
/// `equal()` finds equal once transformed, up to how columns are spelled.
pub(crate) fn typed_fingerprint(node: &protobuf::Node, snapshot: &PgCatalog) -> String {
    crate::resolve::node_fingerprint(&resolve_type_names(node, snapshot))
}

/// `node` with every type name it holds resolved to its type — PG
/// compares *transformed* expressions, where `CAST(a AS bigint)`,
/// `a::int8` and `a::pg_catalog.int8` are one coercion, as are `x::_int4`
/// and `x::int[]`: the name gives way to the type's OID (with the array
/// bounds it accounts for; a typmod stays). A name that doesn't resolve
/// stays as written.
pub(crate) fn resolve_type_names(node: &protobuf::Node, snapshot: &PgCatalog) -> protobuf::Node {
    use typedpg_pg_query::NodeMut;
    let mut tree = protobuf::ParseResult {
        version: 0,
        stmts: vec![protobuf::RawStmt {
            stmt: Some(Box::new(node.clone())),
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
    tree.stmts
        .pop()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .unwrap_or_default()
}

/// Resolve a `GROUP BY` entry as a single column against `scope`. Returns the
/// `(table_alias, column_name)` for a plain `ColumnRef`, or `None` for any
/// other shape (expression, grouping set, select-list alias, …).
fn resolve_group_column(node: &protobuf::Node, scope: &Scope) -> Option<ColKey> {
    let node::Node::ColumnRef(cr) = node.node.as_ref()? else {
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
                })
                .collect(),
        }
    }

    fn has_alias(&self, alias: &str) -> bool {
        self.sources.iter().any(|s| s.alias == alias)
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
    sel: &protobuf::SelectStmt,
    sources: &[TableSource],
    shadowed: &[TableSource],
) -> HashMap<ColKey, Flat> {
    /// One output column of a FROM item: its name, what it stands for and
    /// its type.
    type OutColumn = (String, Flat, PgTypeOid);

    /// Where a FROM item's entries are: at `sources[start..]`, or — inside
    /// an aliased join, whose entries PG hides behind the alias — among
    /// the shadowed entries, by name.
    #[derive(Clone, Copy)]
    enum Place {
        Span(usize),
        Shadowed,
    }

    struct Ctx<'a> {
        sources: &'a [TableSource],
        shadowed: &'a [TableSource],
        out: HashMap<ColKey, Flat>,
    }

    /// How many scope entries a FROM item yields (an aliased join is one);
    /// `None` when unsure.
    fn count(n: &protobuf::Node) -> Option<usize> {
        match n.node.as_ref()? {
            node::Node::JoinExpr(j) if j.alias.is_some() => Some(1),
            node::Node::JoinExpr(j) => Some(
                count(j.larg.as_deref()?)?
                    + count(j.rarg.as_deref()?)?
                    + usize::from(!j.using_clause.is_empty()),
            ),
            _ => Some(1),
        }
    }

    /// The name PG gives a FROM item's entry.
    fn entry_name(n: &protobuf::Node) -> Option<String> {
        match n.node.as_ref()? {
            node::Node::RangeVar(rv) => Some(
                rv.alias
                    .as_ref()
                    .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone()),
            ),
            node::Node::RangeSubselect(rs) => rs.alias.as_ref().map(|a| a.aliasname.clone()),
            node::Node::RangeFunction(rf) => rf.alias.as_ref().map(|a| a.aliasname.clone()),
            node::Node::RangeTableSample(ts) => entry_name(ts.relation.as_deref()?),
            _ => None,
        }
    }

    /// The output columns of FROM item `n` (PG's join output order: the
    /// USING columns, then the left side's others, then the right side's),
    /// recording what the USING and aliased-join columns stand for.
    fn item(n: &protobuf::Node, place: Place, cx: &mut Ctx<'_>) -> Option<Vec<OutColumn>> {
        let node::Node::JoinExpr(j) = n.node.as_ref()? else {
            let source = match place {
                Place::Span(start) => cx.sources.get(start)?,
                Place::Shadowed => {
                    let name = entry_name(n)?;
                    cx.shadowed.iter().rev().find(|s| s.alias == name)?
                }
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
        let (larg, rarg) = (j.larg.as_deref()?, j.rarg.as_deref()?);
        let using = crate::expr::extract_string_fields(&j.using_clause);
        // An aliased join's entries are behind its alias; its own entry is
        // where the join is.
        let (inner, merged_at) = match (j.alias.as_ref(), place) {
            (Some(_), _) | (None, Place::Shadowed) => (Place::Shadowed, None),
            (None, Place::Span(start)) => {
                let left = start + usize::from(!using.is_empty());
                (Place::Span(left), (!using.is_empty()).then_some(start))
            }
        };
        let right_place = match inner {
            Place::Span(left) => Place::Span(left + count(larg)?),
            Place::Shadowed => Place::Shadowed,
        };
        let left = item(larg, inner, cx)?;
        let right = item(rarg, right_place, cx)?;
        let mut output: Vec<OutColumn> = Vec::new();
        let merged_entry: Option<&TableSource> = match merged_at {
            Some(at) => Some(cx.sources.get(at)?),
            None => None,
        };
        let alias_entry: Option<&TableSource> = match (j.alias.as_ref(), place) {
            (None, _) => None,
            (Some(_), Place::Span(at)) => Some(cx.sources.get(at)?),
            (Some(a), Place::Shadowed) => {
                Some(cx.shadowed.iter().rev().find(|s| s.alias == a.aliasname)?)
            }
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
            let flat = match JoinType::try_from(j.jointype) {
                Ok(JoinType::JoinRight) => var(r, rt),
                Ok(JoinType::JoinFull) => Flat::Coalesce(plain(&l)?, plain(&r)?),
                Ok(JoinType::JoinInner) if lt != merged_type && rt == merged_type => r,
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
            if entry.alias != alias.aliasname || entry.columns.len() != output.len() {
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
/// first. `None` stands for a level whose FROM clause is still being
/// analyzed (a LATERAL item's parent): a reference reaching it is taken to
/// belong to it.
#[derive(Clone, Default)]
struct Chain(Vec<Option<Rc<Namespace>>>);

impl Chain {
    /// PG's transformColumnRef level search: an unqualified name is a
    /// column of the nearest level exposing it (else a whole-row reference
    /// to the nearest entry of that name); a qualified one belongs to the
    /// nearest level with an entry of that name.
    fn resolve(&self, cr: &protobuf::ColumnRef) -> Option<RefTarget> {
        let parts: Vec<Option<&str>> = cr
            .fields
            .iter()
            .map(|f| match f.node.as_ref() {
                Some(node::Node::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        match parts.as_slice() {
            [Some(col)] => {
                for (level, ns) in self.0.iter().enumerate() {
                    let Some(ns) = ns else {
                        return Some(RefTarget {
                            level,
                            target: None,
                        });
                    };
                    if let Some(alias) = ns.column(col) {
                        return Some(RefTarget {
                            level,
                            target: Some((alias.to_owned(), Some((*col).to_owned()))),
                        });
                    }
                }
                self.0
                    .iter()
                    .position(|ns| ns.as_ref().is_some_and(|ns| ns.has_alias(col)))
                    .map(|level| RefTarget {
                        level,
                        target: Some(((*col).to_owned(), None)),
                    })
            }
            [.., Some(table), last] => {
                for (level, ns) in self.0.iter().enumerate() {
                    match ns {
                        None => {
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

    fn enter(&mut self, ns: Rc<Namespace>) {
        self.0.insert(0, Some(ns));
    }

    fn leave(&mut self) {
        self.0.remove(0);
    }
}

/// One query level being analyzed.
#[derive(Default)]
struct Frame {
    /// Its FROM clause's names, once processed.
    ns: Option<Rc<Namespace>>,
    /// PG's ParseExprKindName of the clause being analyzed, when that
    /// clause forbids aggregates — what an aggregate of this level found in
    /// a sublink there is rejected with.
    clause: Option<&'static str>,
    /// A sublink holds an aggregate belonging to this level.
    outer_aggs: bool,
    /// Arguments of the `GROUPING(…)` calls in sublinks that belong to this
    /// level, canonicalized against it (see [`canonical`]), with their
    /// locations.
    outer_grouping: Vec<(Vec<protobuf::Node>, i32)>,
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
}

fn level_key(sel: &protobuf::SelectStmt) -> usize {
    sel as *const protobuf::SelectStmt as usize
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
    }
}

/// The current level is SELECT `sel`, whose FROM clause exposes `sources`.
pub(crate) fn register_level(
    sel: &protobuf::SelectStmt,
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
pub(crate) fn level_info(sel: &protobuf::SelectStmt) -> Option<LevelInfo> {
    LEVELS.with(|l| l.borrow().get(&level_key(sel)).cloned())
}

/// Record SELECT `sel`'s output column names and which of its columns
/// have an explicit collation.
pub(crate) fn set_output_columns(
    sel: &protobuf::SelectStmt,
    names: Vec<String>,
    explicit: Vec<bool>,
) {
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
        let mut chain = vec![Some(own)];
        chain.extend(f.iter().rev().skip(1).map(|fr| fr.ns.clone()));
        Chain(chain)
    })
}

/// An aggregate, `GROUPING(…)` or window call written at some level.
enum Call<'a> {
    /// Its aggregated arguments (ORDER BY and FILTER included), which decide
    /// its level, and its direct arguments (an ordered-set aggregate's),
    /// which are evaluated once per group like any grouped expression.
    Aggregate {
        aggregated: Vec<&'a protobuf::Node>,
        direct: Vec<&'a protobuf::Node>,
        location: i32,
    },
    Grouping(&'a protobuf::GroupingFunc),
    Window(i32),
}

fn call_of<'a>(node: &'a protobuf::Node, snapshot: &PgCatalog) -> Option<Call<'a>> {
    fn sort_exprs(order: &[protobuf::Node]) -> impl Iterator<Item = &protobuf::Node> {
        order.iter().filter_map(|o| match o.node.as_ref() {
            Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
            _ => Some(o),
        })
    }
    match node.node.as_ref()? {
        node::Node::FuncCall(fc) if fc.over.is_some() => Some(Call::Window(fc.location)),
        node::Node::FuncCall(fc) if crate::resolve::is_aggregate_call(fc, snapshot) => {
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
            aggregated.extend(fc.agg_filter.as_deref());
            Some(Call::Aggregate {
                aggregated,
                direct,
                location: fc.location,
            })
        }
        node::Node::JsonObjectAgg(_) | node::Node::JsonArrayAgg(_) => {
            let ctor = match node.node.as_ref()? {
                node::Node::JsonObjectAgg(a) => a.constructor.as_deref(),
                node::Node::JsonArrayAgg(a) => a.constructor.as_deref(),
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
        node::Node::GroupingFunc(g) => Some(Call::Grouping(g)),
        _ => None,
    }
}

/// The PARTITION BY / ORDER BY expressions of a window definition.
fn window_key_exprs(wd: &protobuf::WindowDef) -> impl Iterator<Item = &protobuf::Node> {
    wd.partition_clause.iter().chain(
        wd.order_clause
            .iter()
            .filter_map(|o| match o.node.as_ref() {
                Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                _ => None,
            }),
    )
}

/// The expressions of a window definition, frame offsets included.
fn window_def_exprs(wd: &protobuf::WindowDef) -> impl Iterator<Item = &protobuf::Node> {
    window_key_exprs(wd)
        .chain(wd.start_offset.as_deref())
        .chain(wd.end_offset.as_deref())
}

/// The direct sub-expressions of `node` at its own query level — a window
/// call's window definition included.
fn level_children(node: &protobuf::Node) -> Vec<&protobuf::Node> {
    let mut out = crate::resolve::expr_children(node);
    let over = match node.node.as_ref() {
        Some(node::Node::FuncCall(fc)) => fc.over.as_deref(),
        Some(node::Node::JsonObjectAgg(a)) => {
            a.constructor.as_deref().and_then(|c| c.over.as_deref())
        }
        Some(node::Node::JsonArrayAgg(a)) => {
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
fn select_level_exprs(sel: &protobuf::SelectStmt) -> Vec<&protobuf::Node> {
    fn from_item<'a>(n: &'a protobuf::Node, out: &mut Vec<&'a protobuf::Node>) {
        match n.node.as_ref() {
            Some(node::Node::JoinExpr(j)) => {
                for side in [&j.larg, &j.rarg].into_iter().flatten() {
                    from_item(side, out);
                }
                out.extend(j.quals.as_deref());
            }
            Some(node::Node::RangeFunction(rf)) => {
                for f in &rf.functions {
                    if let Some(node::Node::List(pair)) = f.node.as_ref() {
                        out.extend(pair.items.first());
                    }
                }
            }
            Some(node::Node::RangeTableSample(ts)) => {
                if let Some(r) = ts.relation.as_deref() {
                    from_item(r, out);
                }
                out.extend(ts.args.iter());
                out.extend(ts.repeatable.as_deref());
            }
            _ => {}
        }
    }
    let mut out: Vec<&protobuf::Node> = Vec::new();
    for t in &sel.target_list {
        if let Some(node::Node::ResTarget(rt)) = t.node.as_ref() {
            out.extend(rt.val.as_deref());
        }
    }
    for f in &sel.from_clause {
        from_item(f, &mut out);
    }
    out.extend(sel.where_clause.as_deref());
    out.extend(sel.group_clause.iter());
    out.extend(sel.having_clause.as_deref());
    for w in &sel.window_clause {
        if let Some(node::Node::WindowDef(wd)) = w.node.as_ref() {
            out.extend(window_def_exprs(wd));
        }
    }
    out.extend(sel.distinct_clause.iter().filter(|n| n.node.is_some()));
    for s in &sel.sort_clause {
        if let Some(node::Node::SortBy(sb)) = s.node.as_ref() {
            out.extend(sb.node.as_deref());
        }
    }
    out.extend(sel.limit_offset.as_deref());
    out.extend(sel.limit_count.as_deref());
    for row in &sel.values_lists {
        if let Some(node::Node::List(l)) = row.node.as_ref() {
            out.extend(l.items.iter());
        }
    }
    out
}

/// Pre-order visit of `node` and its same-level sub-expressions.
fn visit_level<'a>(node: &'a protobuf::Node, f: &mut dyn FnMut(&'a protobuf::Node)) {
    f(node);
    for c in level_children(node) {
        visit_level(c, f);
    }
}

/// Run `f` on every same-level expression of the SELECT `node` with
/// `chain` entered into its level, and likewise for its set-operation arms,
/// FROM subqueries and CTEs as deeper levels (sublinks inside the
/// expressions are `f`'s business). Levels the analysis never registered
/// are skipped.
fn visit_select(
    node: &protobuf::Node,
    depth: usize,
    chain: &mut Chain,
    f: &mut dyn FnMut(&protobuf::Node, usize, &mut Chain),
) {
    if let Some(node::Node::SelectStmt(sel)) = node.node.as_ref() {
        visit_select_stmt(sel, depth, chain, f);
    }
}

fn visit_select_stmt(
    sel: &protobuf::SelectStmt,
    depth: usize,
    chain: &mut Chain,
    f: &mut dyn FnMut(&protobuf::Node, usize, &mut Chain),
) {
    fn from_subqueries<'a>(n: &'a protobuf::Node, out: &mut Vec<&'a protobuf::Node>) {
        match n.node.as_ref() {
            Some(node::Node::JoinExpr(j)) => {
                for side in [&j.larg, &j.rarg].into_iter().flatten() {
                    from_subqueries(side, out);
                }
            }
            Some(node::Node::RangeSubselect(rs)) => out.extend(rs.subquery.as_deref()),
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
            if let Some(node::Node::CommonTableExpr(c)) = cte.node.as_ref()
                && let Some(q) = c.ctequery.as_deref()
            {
                visit_select(q, depth + 1, chain, f);
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
fn call_level(args: &[&protobuf::Node], chain: &mut Chain) -> usize {
    fn walk(node: &protobuf::Node, depth: usize, chain: &mut Chain, min: &mut Option<usize>) {
        match node.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) => {
                if let Some(t) = chain.resolve(cr)
                    && t.level >= depth
                {
                    let up = t.level - depth;
                    *min = Some(min.map_or(up, |m| m.min(up)));
                }
            }
            Some(node::Node::SubLink(sl)) => {
                if let Some(t) = sl.testexpr.as_deref() {
                    walk(t, depth, chain, min);
                }
                if let Some(sub) = sl.subselect.as_deref() {
                    visit_select(sub, depth + 1, chain, &mut |e, d, c| walk(e, d, c, min));
                }
            }
            Some(node::Node::JsonArrayQueryConstructor(q)) => {
                if let Some(sub) = q.query.as_deref() {
                    visit_select(sub, depth + 1, chain, &mut |e, d, c| walk(e, d, c, min));
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

/// `node` with every column reference that resolves, from the innermost
/// level of `chain`, to level `level` (not those inside its sublinks)
/// replaced by the entry and column it names — so that two spellings of
/// the same expression (`t.a + 1`, `a + 1`) compare equal under
/// [`Equal`], the analyzer's stand-in for PG's `equal()` on transformed
/// expressions — with its type names resolved ([`resolve_type_names`]).
fn canonical(
    node: &protobuf::Node,
    chain: &Chain,
    level: usize,
    snapshot: &PgCatalog,
) -> protobuf::Node {
    use typedpg_pg_query::{NodeMut, NodeRef};
    let mut tree = protobuf::ParseResult {
        version: 0,
        stmts: vec![protobuf::RawStmt {
            stmt: Some(Box::new(resolve_type_names(node, snapshot))),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    let mut inner: HashSet<usize> = HashSet::new();
    let mut resolved: HashMap<usize, String> = HashMap::new();
    for (n, _) in tree.nodes() {
        let sub = match n {
            NodeRef::SubLink(sl) => sl.subselect.as_deref(),
            NodeRef::JsonArrayQueryConstructor(q) => q.query.as_deref(),
            _ => None,
        };
        if let Some(sub) = sub.and_then(|s| s.node.as_ref()) {
            for (m, _) in sub.nodes() {
                if let NodeRef::ColumnRef(cr) = m {
                    inner.insert(cr as *const protobuf::ColumnRef as usize);
                }
            }
        }
    }
    for (n, _) in tree.nodes() {
        if let NodeRef::ColumnRef(cr) = n {
            let key = cr as *const protobuf::ColumnRef as usize;
            if !inner.contains(&key)
                && let Some(RefTarget {
                    level: l,
                    target: Some((alias, column)),
                }) = chain.resolve(cr)
                && l == level
            {
                let name = match (column, chain.0.get(level).cloned().flatten()) {
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
                (*cr).fields = vec![string_node(name)];
            }
        }
    }
    tree.stmts
        .pop()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .unwrap_or_default()
}

/// The name [`canonical`] gives a reference to column `column` of `alias`.
fn column_key(alias: &str, column: &str) -> String {
    format!("\u{1}{alias}\u{1}{column}")
}

fn string_node(s: &str) -> protobuf::Node {
    protobuf::Node {
        node: Some(node::Node::String(protobuf::String { sval: s.to_owned() })),
    }
}

/// Whether two expressions of the current level are PG-`equal()` (up to
/// how their column references are spelled).
pub(crate) fn same_level_exprs_equal(
    a: &protobuf::Node,
    b: &protobuf::Node,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> bool {
    let chain = Chain(vec![Some(Rc::new(Namespace::of(&scope.sources)))]);
    canonical(a, &chain, 0, snapshot).equal(&canonical(b, &chain, 0, snapshot))
}

/// The locations of the first aggregate, `GROUPING(…)` and window call in
/// `node` that belong to the current level — PG's placement rules only
/// concern those (in `SELECT (SELECT 1 FROM u WHERE count(t.a) > 0) FROM
/// t` the aggregate is the outer query's, and allowed there).
#[derive(Default)]
pub(crate) struct LevelCalls {
    pub aggregate: Option<i32>,
    pub grouping: Option<i32>,
    pub window: Option<i32>,
}

pub(crate) fn level_calls(
    node: &protobuf::Node,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> LevelCalls {
    let mut chain = current_chain(scope);
    let mut out = LevelCalls::default();
    let mut calls: Vec<Call<'_>> = Vec::new();
    visit_level(node, &mut |n| calls.extend(call_of(n, snapshot)));
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
                let args: Vec<&protobuf::Node> = g.args.iter().collect();
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

/// Hand a call found at the current level that belongs `levels_up` levels
/// out to that level: an error when the clause that level is analyzing
/// forbids it, else it makes that level grouped. `grouping` carries a
/// `GROUPING(…)` call's arguments, canonicalized against its level.
fn attribute_outer(
    levels_up: usize,
    grouping: Option<Vec<protobuf::Node>>,
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
    sel: &protobuf::SelectStmt,
    scope: &Scope,
    targets: &[ExpandedTarget<'_>],
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let mut chain = current_chain(scope);

    let mut own_aggs = false;
    let mut own_grouping: Vec<&protobuf::GroupingFunc> = Vec::new();
    let mut calls: Vec<Call<'_>> = Vec::new();
    for e in select_level_exprs(sel) {
        visit_level(e, &mut |n| calls.extend(call_of(n, snapshot)));
    }
    for call in calls {
        match call {
            Call::Aggregate {
                aggregated,
                location,
                ..
            } => match call_level(&aggregated, &mut chain) {
                0 => own_aggs = true,
                up => attribute_outer(up, None, location)?,
            },
            Call::Grouping(g) => {
                if g.args.len() >= 32 {
                    return Err(crate::pgmsg::grouping_too_many_arguments(
                        crate::error::SourceSpan::from_node_qname(g.location),
                    )
                    .finalize_implicit());
                }
                let args: Vec<&protobuf::Node> = g.args.iter().collect();
                match call_level(&args, &mut chain) {
                    0 => own_grouping.push(g),
                    up => {
                        let canon = g
                            .args
                            .iter()
                            .map(|a| canonical(a, &chain, up, snapshot))
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

    let mut own = Chain(vec![chain.0[0].clone()]);
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
            .any(|a| !group.contains(&canonical(a, &own, 0, snapshot)))
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
    let own_ns = own.0[0].clone().unwrap_or_default();
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
    let mut fully_grouped: HashSet<String> = HashSet::new();
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
                fully_grouped.insert(src.alias.clone());
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
    let output_names: HashSet<&str> = targets
        .iter()
        .filter_map(|t| match t {
            ExpandedTarget::Expr(rt) if !rt.name.is_empty() => Some(rt.name.as_str()),
            _ => None,
        })
        .collect();
    let names_target = |n: &protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(_)) => true,
        Some(node::Node::ColumnRef(cr)) => matches!(
            expr::extract_string_fields(&cr.fields).as_slice(),
            [name] if output_names.contains(name.as_str())
        ),
        _ => false,
    };
    for t in targets {
        match *t {
            ExpandedTarget::Expr(rt) => {
                if let Some(v) = rt.val.as_deref() {
                    check.expr(v, 0, &mut own)?;
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
        if let Some(node::Node::SortBy(sb)) = s.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
            && !names_target(inner)
        {
            check.expr(inner, 0, &mut own)?;
        }
    }
    for d in sel.distinct_clause.iter().filter(|n| n.node.is_some()) {
        if !names_target(d) {
            check.expr(d, 0, &mut own)?;
        }
    }
    for w in &sel.window_clause {
        if let Some(node::Node::WindowDef(wd)) = w.node.as_ref() {
            for e in window_key_exprs(wd) {
                check.expr(e, 0, &mut own)?;
            }
        }
    }
    if let Some(h) = sel.having_clause.as_deref() {
        check.expr(h, 0, &mut own)?;
    }
    Ok(())
}

/// The GROUP BY expressions of a level (PG's `groupClauses`): the plain
/// columns it groups by, and every grouping expression canonicalized.
struct GroupExprs {
    vars: HashSet<ColKey>,
    exprs: Vec<protobuf::Node>,
    have_non_var: bool,
}

impl GroupExprs {
    fn collect(
        sel: &protobuf::SelectStmt,
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
        let ns: &Namespace = own.0.first().and_then(|n| n.as_deref()).unwrap_or(&empty);
        let mut stack: Vec<&protobuf::Node> = sel.group_clause.iter().rev().collect();
        while let Some(g) = stack.pop() {
            if let Some(node::Node::GroupingSet(gs)) = g.node.as_ref() {
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
            let target = match g.node.as_ref() {
                Some(node::Node::AConst(ac)) => match &ac.val {
                    Some(protobuf::a_const::Val::Ival(i)) => {
                        targets.get((i.ival as usize).wrapping_sub(1)).copied()
                    }
                    _ => None,
                },
                Some(node::Node::ColumnRef(cr)) if resolve_group_column(g, scope).is_none() => {
                    match expr::extract_string_fields(&cr.fields).as_slice() {
                        [name] => targets
                            .iter()
                            .find(|t| matches!(t, ExpandedTarget::Expr(rt) if &rt.name == name))
                            .copied(),
                        _ => None,
                    }
                }
                _ => None,
            };
            let expr = match target {
                Some(ExpandedTarget::Expr(rt)) => match rt.val.as_deref() {
                    Some(v) => v,
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
            if let Some(node::Node::ColumnRef(cr)) = expr.node.as_ref()
                && let Some(RefTarget {
                    level: 0,
                    target: Some((alias, Some(col))),
                }) = own.resolve(cr)
            {
                out.add_column((alias, col), ns);
                continue;
            }
            out.have_non_var = true;
            out.exprs.push(canonical(expr, own, 0, snapshot));
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
        self.exprs.push(protobuf::Node {
            node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
                fields: vec![string_node(&ns.canonical_name(&key))],
                location: 0,
            })),
        });
    }

    fn contains(&self, canonical_expr: &protobuf::Node) -> bool {
        self.exprs.iter().any(|e| e.equal(canonical_expr))
    }
}

/// PG's `check_ungrouped_columns_walker` over one grouped level.
struct UngroupedCheck<'a> {
    snapshot: &'a PgCatalog,
    scope: &'a Scope,
    group: &'a GroupExprs,
    fully_grouped: &'a HashSet<String>,
}

impl UngroupedCheck<'_> {
    /// Check `node`, `depth` sublink levels below the grouped level;
    /// `chain` starts at `node`'s level and ends at the grouped one.
    fn expr(
        &self,
        node: &protobuf::Node,
        depth: usize,
        chain: &mut Chain,
    ) -> Result<(), AnalyzeError> {
        match node.node.as_ref() {
            None | Some(node::Node::AConst(_)) | Some(node::Node::ParamRef(_)) => {
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
                let args: Vec<&protobuf::Node> = g.args.iter().collect();
                if call_level(&args, chain) == depth {
                    return Ok(());
                }
            }
            _ => {}
        }
        if depth == 0
            && self.group.have_non_var
            && self
                .group
                .contains(&canonical(node, chain, 0, self.snapshot))
        {
            return Ok(());
        }
        match node.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) => {
                if let Some(RefTarget {
                    level,
                    target: Some((alias, col)),
                }) = chain.resolve(cr)
                    && level == depth
                {
                    let Some(col) = col else {
                        return self.column(&alias, None, depth, cr.location);
                    };
                    // A JOIN USING column reads the columns it flattens to.
                    let key = (alias, col);
                    let columns = match chain.0.get(depth).cloned().flatten() {
                        Some(ns) => ns.flat_columns(&key),
                        None => vec![key],
                    };
                    for (alias, col) in columns {
                        self.column(&alias, Some(&col), depth, cr.location)?;
                    }
                }
                Ok(())
            }
            Some(node::Node::SubLink(sl)) => {
                if let Some(t) = sl.testexpr.as_deref() {
                    self.expr(t, depth, chain)?;
                }
                match sl.subselect.as_deref() {
                    Some(sub) => self.select(sub, depth + 1, chain),
                    None => Ok(()),
                }
            }
            Some(node::Node::JsonArrayQueryConstructor(q)) => match q.query.as_deref() {
                Some(sub) => self.select(sub, depth + 1, chain),
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
        node: &protobuf::Node,
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
        if self.fully_grouped.contains(alias) {
            return Ok(());
        }
        // A JOIN USING column reads as the join side PG flattens it to.
        let shown_alias = if crate::scope::is_hidden_alias(alias) {
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
