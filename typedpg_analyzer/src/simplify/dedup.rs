//! Deduplication the schema proves redundant (#173).
//!
//! - [`Rule::Distinct`] drops a plain `SELECT DISTINCT` whose rows are
//!   already distinct.
//! - [`Rule::UnionAll`] turns `UNION` into `UNION ALL` when every arm's rows
//!   are distinct and the arms are pairwise disjoint: some output column is
//!   a different constant in each arm (a "tagged union").
//!
//! # The distinctness proof
//!
//! A `SELECT`'s FROM clause produces each combination of its relations'
//! rows — some null-extended by outer joins — at most once. The output rows
//! are therefore distinct when the output values determine that
//! combination: when, for every relation, they tell which row of it (or
//! none) the output row came from. The proof is a closure over functional
//! dependencies between relations' rows:
//!
//! - **Keys in the output.** A unique key of a relation all whose columns
//!   are bare output columns determines the relation's row. The key must
//!   compare under the equality `DISTINCT` uses (the column type's default
//!   btree operator family, deterministic collations), and a NULL in it must
//!   not hide a second row: every column under a validated NOT NULL (on every inheritance child or partition too), or the index `NULLS
//!   NOT DISTINCT` when the relation is never null-extended (a null-extended
//!   row and a row with a NULL key would then look alike).
//! - **Equalities to a key** (PG's `innerrel_is_unique`): a relation whose
//!   unique key is equated, column by column, to values the other relations
//!   determine matches at most one row. The operator must be a strict
//!   equality of the key's btree operator family, under deterministic
//!   collations. In WHERE — and an inner join that is never null-extended —
//!   the determining relations are the ones the equalities read; in a
//!   null-extended inner join, every other relation of the join (whether
//!   the join produced a row at all depends on them); in a LEFT (RIGHT)
//!   join, the whole preserved side, which decides whether the nullable
//!   relation is matched or null-extended.
//!
//! Only what PG enforces at every statement counts as a key: unique indexes
//! that are immediate (not DEFERRABLE), not partial and on plain columns —
//! the ones behind PRIMARY KEY and UNIQUE constraints included — and, for
//! the `pg_catalog` system views on [`SYSTEM_VIEW_KEYS`], the keys
//! PostgreSQL itself guarantees. Relations the proof can't see through —
//! other views, subqueries it can't prove one row per outer row, functions,
//! CTEs, inheritance parents, partitioned tables, tables under row security
//! — make it give up, and so do set-returning, aggregate and volatile
//! calls. (These are the strict scope's rules, which the simplifications
//! keep; the cardinality proof's scope trusts a partitioned table's global
//! keys, an `ONLY` parent and row security, see [`KeyScope`].)
//!
//! # Errors
//!
//! Dropping `DISTINCT` (or a `UNION`'s deduplication) changes *which* rows
//! PostgreSQL evaluates, not only which it returns: a `DISTINCT` plan must
//! read every row before it can emit (a hash/sort aggregate), while the
//! rewritten plan can stop at a `LIMIT` — its own, the `LIMIT 2` the query
//! runtime adds to `fetch_one`, or one above it — and `UNION` evaluates
//! every arm fully to deduplicate while `UNION ALL` needn't. So an
//! expression that can raise on a row the original evaluates but the
//! rewrite might not would change the error the query raises (or whether
//! it raises one). Both rules therefore also require the affected select
//! — the `DISTINCT` one, every `UNION` arm — to be error-free by
//! [`super::cardinality::select_is_error_free_in`], the same proof the
//! lint fixes use. The enclosing levels need no extra check: they see the
//! same output rows either way (the proof replaces only a dedup that
//! removes none), so their expressions run on the same values; only a
//! `LIMIT` can stop the *inner* select earlier, which is what the
//! affected select's own check covers.

use std::collections::HashSet;

use prost::Message;
use typedpg_pg_query::Equal;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, AExprKind, BoolExprType, JoinType, SetOperation, Token, a_const::Val, node::Node,
};

use super::proof::{FromScope, Item};
use super::{Candidate, Ctx, Edit, Rule};
use crate::Type;
use crate::oid::{PgClassOid, PgCollationOid, PgNamespaceOid, PgOperatorOid, PgTypeOid};
use crate::pg_catalog::{ConType, PgCatalog, ProKind, ProVolatile, RelKind, oid};

/// The most relations one proof follows (a bit each in a `u64`).
const MAX_RELATIONS: usize = 64;

/// [`Rule::Distinct`]: every plain `SELECT DISTINCT` — at the top level or
/// nested anywhere — whose rows are provably distinct without it, when the
/// select is error-free ([`super::cardinality::select_is_error_free_in`]).
pub(crate) fn distinct_candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
    distinct_candidates_with_select(ctx)
        .into_iter()
        .filter(|(select, _)| super::cardinality::select_is_error_free_in(ctx, select))
        .map(|(_, candidate)| candidate)
        .collect()
}

/// [`distinct_candidates`], keeping the `SELECT` each candidate would
/// rewrite (the lint rule judges its fix on that node).
pub(super) fn distinct_candidates_with_select<'a>(
    ctx: &Ctx<'a>,
) -> Vec<(&'a protobuf::SelectStmt, Candidate)> {
    if !ctx.analyzed.spreads.is_empty() {
        return Vec::new();
    }
    let Some(tokens) = scan(ctx.sql) else {
        return Vec::new();
    };
    let ctes = cte_names(ctx.tree);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(s) = node else {
            continue;
        };
        if !is_plain_distinct(s) || !rows_distinct(ctx, &ctes, s, None) {
            continue;
        }
        if let Some(edit) = distinct_keyword_edit(ctx.sql, &tokens, s) {
            out.push((
                s,
                Candidate {
                    rule: Rule::Distinct,
                    edits: vec![edit],
                },
            ));
        }
    }
    out
}

/// [`Rule::UnionAll`]: the top-level statement's `UNION`s, when the arms
/// under them are each distinct and pairwise disjoint.
pub(crate) fn union_all_candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
    if !ctx.analyzed.spreads.is_empty() {
        return Vec::new();
    }
    let Some(Node::SelectStmt(top)) = ctx
        .tree
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
    else {
        return Vec::new();
    };
    // Only a tree of UNIONs: INTERSECT and EXCEPT aren't followed.
    let mut arms = Vec::new();
    let mut unions = Vec::new();
    if !union_arms(top, &mut arms, &mut unions) || unions.is_empty() {
        return Vec::new();
    }
    let ncols = ctx.analyzed.columns.len();
    if arms
        .iter()
        .any(|a| a.target_list.len() != ncols || has_star(a))
    {
        return Vec::new();
    }
    let Some(tokens) = scan(ctx.sql) else {
        return Vec::new();
    };
    let ctes = cte_names(ctx.tree);
    let scopes: Vec<Option<Scope<'_>>> = arms
        .iter()
        .map(|a| Scope::build(ctx, &ctes, &a.from_clause))
        .collect();

    // The output columns an arm's proof may use: those the set operation
    // compares exactly as the arm produces them — every arm's value already
    // of the output type (no coercion anywhere in the tree, so each UNION
    // compares that type too), under a deterministic collation.
    let usable: Vec<bool> = (0..ncols)
        .map(|p| {
            let column = &ctx.analyzed.columns[p];
            let Some(typ) = type_oid(ctx, &column.pg_type) else {
                return false;
            };
            deterministic_type_collation(&column.pg_type)
                && arms.iter().zip(&scopes).all(|(arm, scope)| {
                    arm_value_is_exactly(ctx, scope.as_ref(), target_value(arm, p), typ)
                })
        })
        .collect();

    if !(0..ncols).any(|p| usable[p] && is_tag_column(ctx, &arms, p)) {
        return Vec::new();
    }
    let all_distinct = arms.iter().all(|arm| {
        rows_distinct(ctx, &ctes, arm, Some(&usable))
            || (is_plain_distinct(arm) && usable.iter().all(|&u| u))
    });
    if !all_distinct {
        return Vec::new();
    }
    // UNION deduplicates only after evaluating every arm in full; UNION ALL
    // under a LIMIT can stop an arm early. Only rewrite when nothing an arm
    // evaluates can raise, so the rows either form skips can't hide (or
    // expose) an error. The arms' own error-freedom is the whole check: a
    // set operation has no other expressions of its own, and its ORDER BY
    // evaluates only the rows both forms produce (the arms' outputs).
    if !arms
        .iter()
        .all(|arm| super::cardinality::select_is_error_free_in(ctx, arm))
    {
        return Vec::new();
    }
    let edits: Option<Vec<Edit>> = unions
        .iter()
        .map(|u| union_keyword_edit(&tokens, u))
        .collect();
    match edits {
        Some(edits) => vec![Candidate {
            rule: Rule::UnionAll,
            edits,
        }],
        None => Vec::new(),
    }
}

// ── The proof ───────────────────────────────────────────────────────────────

/// Whether `s`'s rows are provably distinct (the module docs). `usable`
/// restricts the output columns the proof may read, by position.
fn rows_distinct(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    s: &protobuf::SelectStmt,
    usable: Option<&[bool]>,
) -> bool {
    if s.op() != SetOperation::SetopNone
        || s.into_clause.is_some()
        || !s.group_clause.is_empty()
        || s.group_distinct
        || s.having_clause.is_some()
        || !s.values_lists.is_empty()
        || !s.locking_clause.is_empty()
        || !(s.distinct_clause.is_empty() || is_plain_distinct(s))
        || has_unsafe_call(ctx, s)
    {
        return false;
    }
    let Some(scope) = Scope::build(ctx, ctes, &s.from_clause) else {
        return false;
    };

    // The output columns: (relation, attnum) of each bare column reference.
    let mut output: HashSet<(usize, i16)> = HashSet::new();
    for (p, target) in s.target_list.iter().enumerate() {
        let Some(Node::ResTarget(rt)) = &target.node else {
            return false;
        };
        let Some(Node::ColumnRef(cr)) = rt.val.as_ref().and_then(|v| v.node.as_ref()) else {
            continue;
        };
        match column_ref_fields(cr) {
            Fields::Star(qualifier) => {
                if usable.is_some() {
                    // A star's columns have no single position.
                    continue;
                }
                let rels: Vec<usize> = match qualifier {
                    // A bare `*` merges USING columns: leave it out then.
                    None if scope.joins.iter().any(|j| !j.using.is_empty()) => Vec::new(),
                    None => (0..scope.rels.len()).collect(),
                    Some(q) => scope.rel_named(&q).into_iter().collect(),
                };
                for r in rels {
                    let Some(oid) = scope.rels[r].oid else {
                        continue;
                    };
                    for a in ctx.catalog.attributes_of(oid) {
                        if a.attnum > 0 {
                            output.insert((r, a.attnum));
                        }
                    }
                }
            }
            Fields::Names(names) => {
                if usable.is_some_and(|u| !u.get(p).copied().unwrap_or(false)) {
                    continue;
                }
                if let Col::Rel(r, attnum) = scope.resolve(&names) {
                    output.insert((r, attnum));
                }
            }
            Fields::Other => {}
        }
    }

    let n = scope.rels.len();
    let all: u64 = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
    let mut known: u64 = 0;
    for (r, rel) in scope.rels.iter().enumerate() {
        let Some(oid) = rel.oid else {
            continue;
        };
        let determined = unique_keys(ctx, oid).iter().any(|key| {
            key.predicate.is_none()
                && key
                    .columns
                    .iter()
                    .all(|c| c.deterministic && c.default_family && output.contains(&(r, c.attnum)))
                && (key.columns.iter().all(|c| c.notnull)
                    || (key.nulls_not_distinct && !rel.nullable))
        });
        if determined {
            known |= 1 << r;
        }
    }

    let mut deps: Vec<(u64, usize)> = Vec::new();
    // WHERE holds on every output row: a relation whose key the WHERE
    // equates to what other relations determine is determined by them.
    let where_facts = conjuncts(s.where_clause.as_deref())
        .into_iter()
        .flat_map(|c| scope.equality_facts(ctx, c, None))
        .collect::<Vec<_>>();
    for b in 0..n {
        if let Some(det) = scope.key_equated(ctx, b, &where_facts, all & !(1 << b)) {
            deps.push((det, b));
        }
    }
    for join in &scope.joins {
        let mut facts: Vec<EqFact> = Vec::new();
        for c in conjuncts(join.quals) {
            facts.extend(scope.equality_facts(ctx, c, None));
        }
        for name in &join.using {
            facts.extend(scope.using_facts(ctx, name, join.left, join.right));
        }
        match join.kind {
            JoinType::JoinInner => {
                for b in (0..n).filter(|&b| join.rels() & (1 << b) != 0) {
                    let others = join.rels() & !(1 << b);
                    if let Some(det) = scope.key_equated(ctx, b, &facts, others) {
                        deps.push((if join.nullable { others } else { det }, b));
                    }
                }
            }
            JoinType::JoinLeft => {
                if let Some(b) = join.right_rel
                    && scope.key_equated(ctx, b, &facts, join.left).is_some()
                {
                    deps.push((join.left, b));
                }
            }
            JoinType::JoinRight => {
                if let Some(a) = join.left_rel
                    && scope.key_equated(ctx, a, &facts, join.right).is_some()
                {
                    deps.push((join.right, a));
                }
            }
            _ => {}
        }
    }
    loop {
        let before = known;
        for &(det, b) in &deps {
            if det & !known == 0 {
                known |= 1 << b;
            }
        }
        if known == before {
            break;
        }
    }
    known & all == all
}

/// A plain `DISTINCT` (not `DISTINCT ON`): the parser's one-NIL list.
pub(super) fn is_plain_distinct(s: &protobuf::SelectStmt) -> bool {
    matches!(s.distinct_clause.as_slice(), [d] if d.node.is_none())
}

/// A call that may multiply rows (a set-returning function or operator),
/// collapse them (an aggregate without OVER) or have effects (anything
/// [`super::proof::has_volatile`] flags), anywhere in `s` — or a function
/// this can't resolve. Every overload of the name counts, not just the one
/// PG picks.
pub(super) fn has_unsafe_call(ctx: &Ctx<'_>, s: &protobuf::SelectStmt) -> bool {
    let node = protobuf::Node {
        node: Some(Node::SelectStmt(Box::new(s.clone()))),
    };
    if super::proof::has_volatile(ctx, &node) {
        return true;
    }
    let mut stack = vec![NodeRef::SelectStmt(s)];
    while let Some(n) = stack.pop() {
        match n {
            NodeRef::FuncCall(f) => {
                let names = string_list(&f.funcname);
                let (schema, name) = match names.as_slice() {
                    [name] => (None, *name),
                    [schema, name] => (Some(*schema), *name),
                    _ => return true,
                };
                let procs = ctx.catalog.find_functions(schema, name);
                if procs.is_empty()
                    || procs.iter().any(|p| {
                        p.proretset || (f.over.is_none() && p.prokind == ProKind::Aggregate)
                    })
                {
                    return true;
                }
            }
            NodeRef::AExpr(e) => {
                if let Some(name) = string_list(&e.name).last() {
                    let set_returning = ctx.catalog.pg_operator.values().any(|o| {
                        o.oprname == *name
                            && o.oprcode
                                .and_then(|code| ctx.catalog.pg_proc.get(&code))
                                .is_none_or(|p| p.proretset)
                    });
                    if set_returning {
                        return true;
                    }
                }
            }
            _ => {}
        }
        stack.extend(n.children());
    }
    false
}

// ── FROM scope ──────────────────────────────────────────────────────────────

/// A FROM item relation: a base table, or a reference to a single-row CTE.
pub(super) struct Rel {
    /// The base table's oid; `None` for a single-row CTE reference.
    pub(super) oid: Option<PgClassOid>,
    /// The alias, or the table's name.
    pub(super) refname: String,
    pub(super) aliased: bool,
    pub(super) schema: String,
    pub(super) relname: String,
    /// The relation may be null-extended by an outer join above it.
    pub(super) nullable: bool,
    /// A single-row CTE reference: the proof starts with it determined.
    pub(super) single_row: bool,
    /// The columns as this FROM item names them, with their attnums: an
    /// alias's column list (`t AS x(a, b)`) renames the first ones, in
    /// order, and the rest keep their names. Dropped columns are gone from
    /// the catalog, as PG skips them.
    pub(super) columns: Vec<(String, i16)>,
    /// Whether [`Self::columns`] lists every column's real name. A FROM
    /// subquery or CTE whose target names aren't all certain doesn't: an
    /// unknown name can be the one a reference spells, so no lookup may
    /// bind to this relation.
    pub(super) columns_known: bool,
    /// For a CTE reference, a FROM subquery, a VALUES list or a FROM
    /// function: each column's type and whether its collation is
    /// deterministic, by attnum - 1. Empty for a base table.
    cte_columns: Vec<(Option<PgTypeOid>, bool)>,
}

impl Rel {
    /// The attnum of the column this FROM item calls `name`.
    fn attnum(&self, name: &str) -> Option<i16> {
        if !self.columns_known {
            return None;
        }
        let mut found = self.columns.iter().filter(|(n, _)| n == name);
        match (found.next(), found.next()) {
            (Some((_, attnum)), None) => Some(*attnum),
            _ => None,
        }
    }

    /// The attnum a reference in the table's own stored expression (a
    /// partial index's predicate) names: the catalog's real column names,
    /// never the query's alias column list (`FROM t AS x(k, active)` renames
    /// the columns the query sees, but the index predicate is stored with
    /// the table's `attname`s).
    fn resolve_table_column(&self, ctx: &Ctx<'_>, names: &[&str]) -> Option<i16> {
        let oid = self.oid?;
        let name = match names {
            [name] => *name,
            [qualifier, name] if *qualifier == self.refname || *qualifier == self.relname => *name,
            [schema, table, name]
                if !self.aliased && *schema == self.schema && *table == self.relname =>
            {
                *name
            }
            _ => return None,
        };
        ctx.catalog
            .attributes_of(oid)
            .iter()
            .find(|a| a.attnum > 0 && a.attname == name)
            .map(|a| a.attnum)
    }

    /// The column's type and whether its collation compares bytewise.
    pub(super) fn column_info(&self, ctx: &Ctx<'_>, attnum: i16) -> Option<(PgTypeOid, bool)> {
        match self.oid {
            Some(oid) => ctx
                .catalog
                .attributes_of(oid)
                .iter()
                .find(|a| a.attnum == attnum)
                .map(|a| (a.atttypid, deterministic(ctx.catalog, a.attcollation))),
            None => {
                let index = usize::try_from(attnum - 1).ok()?;
                let (typ, deterministic_collation) = self.cte_columns.get(index).copied()?;
                Some((typ?, deterministic_collation))
            }
        }
    }
}

/// The CTEs visible at one query level, by name: `Some` when the CTE's
/// output columns are known (whether or not its body is proven one row),
/// `None` when a reference to it must make the proof give up.
#[derive(Clone, Default)]
pub(super) struct CteEnv {
    ctes: std::collections::HashMap<String, Option<CteInfo>>,
}

/// One single-row CTE: the columns a reference to it yields.
#[derive(Clone)]
pub(super) struct CteInfo {
    pub(super) columns: Vec<CteColumn>,
    /// Whether the body yields at most one row (a reference to a CTE that
    /// doesn't is still listed, so column references in a nested level's
    /// aggregate can be placed, but the relation is never determined).
    pub(super) single_row: bool,
}

/// One column of a single-row CTE.
#[derive(Clone)]
pub(super) struct CteColumn {
    /// `None` when PG's output name isn't certain: an expression PG names
    /// some way this can't reproduce (`v::int` is `v`, a `CASE` is `case`).
    pub(super) name: Option<String>,
    /// `None` when the body's expression type isn't proven.
    pub(super) typ: Option<PgTypeOid>,
    /// Its collation, when known, compares bytewise.
    pub(super) deterministic: bool,
}

impl CteEnv {
    /// The environment in which every CTE name in `names` is a reference
    /// the proof gives up on.
    pub(super) fn blocked(names: &HashSet<String>) -> CteEnv {
        CteEnv {
            ctes: names.iter().map(|name| (name.clone(), None)).collect(),
        }
    }

    /// The environment `name` refers to: `Some` of its info when it is a
    /// single-row CTE, `None` when it isn't one or isn't visible.
    fn get(&self, name: &str) -> Option<Option<&CteInfo>> {
        self.ctes.get(name).map(Option::as_ref)
    }

    /// Add `name` to this environment, single-row when `info` is `Some`.
    pub(super) fn insert(&mut self, name: String, info: Option<CteInfo>) {
        self.ctes.insert(name, info);
    }
}

/// A join in a FROM clause, with its sides' relations as bitmasks.
pub(super) struct Join<'a> {
    pub(super) kind: JoinType,
    pub(super) left: u64,
    pub(super) right: u64,
    /// The side, when it is a single relation.
    pub(super) left_rel: Option<usize>,
    pub(super) right_rel: Option<usize>,
    pub(super) quals: Option<&'a protobuf::Node>,
    pub(super) using: Vec<&'a str>,
    /// The join may be null-extended by an outer join above it.
    pub(super) nullable: bool,
}

impl Join<'_> {
    pub(super) fn rels(&self) -> u64 {
        self.left | self.right
    }
}

/// How much a [`Scope`] trusts a relation's unique keys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyScope {
    /// The `DISTINCT` / `UNION` simplifications: only a plain table (or a
    /// trusted `pg_catalog` view) with no inheritance children and no row
    /// security. A partitioned table, an `ONLY` parent and an RLS table are
    /// refused for every key.
    Strict,
    /// The cardinality proof: a partitioned table's unique keys are global
    /// (PG requires the partition key in them), `FROM ONLY parent` reads
    /// only the parent (whose own keys are unique there), and row security
    /// only filters rows, so keys still bound the count.
    Cardinality,
}

/// The base tables and joins of one FROM clause.
pub(super) struct Scope<'a> {
    pub(super) rels: Vec<Rel>,
    pub(super) joins: Vec<Join<'a>>,
    /// Which relation kinds and keys this scope trusts.
    mode: KeyScope,
}

/// What a column reference names.
pub(super) enum Col {
    /// A column of one of the scope's relations.
    Rel(usize, i16),
    /// Anything else: an outer query's column, an ambiguous name, a whole
    /// row, a name this doesn't resolve.
    Other,
}

pub(super) enum Fields<'a> {
    Names(Vec<&'a str>),
    /// `*` or `qualifier.*`.
    Star(Option<String>),
    Other,
}

/// One proven fact `rel.attnum = <value>` with a strict equality operator,
/// where the value is read from the relations of `other` (constants,
/// parameters and outer-query columns read none).
#[derive(Clone)]
pub(super) struct EqFact {
    rel: usize,
    attnum: i16,
    op: PgOperatorOid,
    /// The operator's declared operand type on the column's side.
    declared: Option<PgTypeOid>,
    other: u64,
}

/// One side of an equality.
struct Operand {
    typ: PgTypeOid,
    column: Option<(usize, i16)>,
    rels: u64,
}

impl<'a> Scope<'a> {
    /// The FROM clause's relations, or `None` when it has anything but
    /// base tables the proof can trust and plain joins of them. Every CTE
    /// name in `ctes` makes the proof give up.
    pub(super) fn build(
        ctx: &Ctx<'_>,
        ctes: &HashSet<String>,
        from: &'a [protobuf::Node],
    ) -> Option<Scope<'a>> {
        let root = FromScope::opaque(None);
        Self::build_inner(
            ctx,
            &CteEnv::blocked(ctes),
            from,
            false,
            &root,
            KeyScope::Strict,
        )
    }

    /// [`Self::build`] with the single-row CTEs visible at this level and
    /// `outer`, the scope of this level's own items and the levels around
    /// it (what a `LATERAL` FROM subquery reads). This is the cardinality
    /// proof's scope: partitioned keys are global, `ONLY` parents and
    /// row-security tables are usable.
    pub(super) fn build_env(
        ctx: &Ctx<'_>,
        env: &CteEnv,
        from: &'a [protobuf::Node],
        outer: &FromScope<'_>,
    ) -> Option<Scope<'a>> {
        Self::build_inner(ctx, env, from, true, outer, KeyScope::Cardinality)
    }

    /// [`Self::build`], also accepting a FROM function that can't return a
    /// set and a FROM subquery as one-row relations (`functions`).
    fn build_inner(
        ctx: &Ctx<'_>,
        env: &CteEnv,
        from: &'a [protobuf::Node],
        functions: bool,
        outer: &FromScope<'_>,
        mode: KeyScope,
    ) -> Option<Scope<'a>> {
        let mut scope = Scope {
            rels: Vec::new(),
            joins: Vec::new(),
            mode,
        };
        for item in from {
            scope.add(ctx, env, item, false, functions, outer)?;
        }
        Some(scope)
    }

    /// The scope of a whole `UPDATE` or `DELETE`: the target relation and
    /// its `FROM` / `USING` items. The cardinality proof's scope (a target
    /// row is written once, so `ONLY` and partitioned targets count).
    pub(super) fn dml(
        ctx: &Ctx<'_>,
        env: &CteEnv,
        relation: &protobuf::RangeVar,
        from: &'a [protobuf::Node],
    ) -> Option<Scope<'a>> {
        let mut scope = Scope {
            rels: Vec::new(),
            joins: Vec::new(),
            mode: KeyScope::Cardinality,
        };
        // A data-modifying statement's target is always a table: a CTE of
        // the same name is not one PG lets it write.
        scope.add_table(ctx, relation, false)?;
        let root = FromScope::opaque(None);
        for item in from {
            scope.add(ctx, env, item, false, true, &root)?;
        }
        Some(scope)
    }

    /// Add one FROM item; its relations as a bitmask. `functions`: a
    /// function item that can't return a set and a proven one-row `LATERAL`
    /// subquery are one-row relations (otherwise a `FROM` function or
    /// subquery is a reference the proof gives up on).
    fn add(
        &mut self,
        ctx: &Ctx<'_>,
        env: &CteEnv,
        item: &'a protobuf::Node,
        nullable: bool,
        functions: bool,
        outer: &FromScope<'_>,
    ) -> Option<u64> {
        match item.node.as_ref()? {
            Node::RangeVar(rv) => self.add_range_var(ctx, env, rv, nullable),
            Node::RangeFunction(rf) => functions
                .then(|| self.add_range_function(ctx, rf, nullable))
                .flatten(),
            Node::RangeSubselect(sub) => functions
                .then(|| self.add_range_subselect(ctx, env, sub, nullable, outer))
                .flatten(),
            Node::JoinExpr(j) => {
                if j.is_natural || j.alias.is_some() || j.join_using_alias.is_some() {
                    return None;
                }
                let kind = j.jointype();
                let (left_nullable, right_nullable) = match kind {
                    JoinType::JoinInner => (nullable, nullable),
                    JoinType::JoinLeft => (nullable, true),
                    JoinType::JoinRight => (true, nullable),
                    JoinType::JoinFull => (true, true),
                    _ => return None,
                };
                let first = self.rels.len();
                let left = self.add(
                    ctx,
                    env,
                    j.larg.as_deref()?,
                    left_nullable,
                    functions,
                    outer,
                )?;
                let middle = self.rels.len();
                let right = self.add(
                    ctx,
                    env,
                    j.rarg.as_deref()?,
                    right_nullable,
                    functions,
                    outer,
                )?;
                let single = |side: &Option<Box<protobuf::Node>>, at: usize| {
                    matches!(
                        side.as_deref().and_then(|n| n.node.as_ref()),
                        Some(Node::RangeVar(_))
                    )
                    .then_some(at)
                };
                let using = j
                    .using_clause
                    .iter()
                    .map(|n| match &n.node {
                        Some(Node::String(s)) => Some(s.sval.as_str()),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                self.joins.push(Join {
                    kind,
                    left,
                    right,
                    left_rel: single(&j.larg, first),
                    right_rel: single(&j.rarg, middle),
                    quals: j.quals.as_deref(),
                    using,
                    nullable,
                });
                Some(left | right)
            }
            _ => None,
        }
    }

    /// Add one FROM reference: a CTE when `env` lists one under that name
    /// (a single-row relation only when its body is proven one row), a base
    /// table otherwise. Its relation as a bitmask.
    fn add_range_var(
        &mut self,
        ctx: &Ctx<'_>,
        env: &CteEnv,
        rv: &protobuf::RangeVar,
        nullable: bool,
    ) -> Option<u64> {
        if self.rels.len() == MAX_RELATIONS || !rv.catalogname.is_empty() {
            return None;
        }
        if rv.schemaname.is_empty()
            && let Some(info) = env.get(&rv.relname)
        {
            let info = info?;
            let alias = rv.alias.as_ref().map(|a| a.aliasname.clone());
            let mut columns: Vec<(String, i16)> = info
                .columns
                .iter()
                .enumerate()
                .map(|(i, column)| {
                    (
                        column.name.clone().unwrap_or_else(unknown_column_name),
                        i as i16 + 1,
                    )
                })
                .collect();
            let renames = rv.alias.as_ref().map_or(&[][..], |a| &a.colnames[..]);
            let columns_known = info
                .columns
                .iter()
                .enumerate()
                .all(|(i, column)| column.name.is_some() || i < renames.len());
            if renames.len() > columns.len() {
                return None;
            }
            for (column, rename) in columns.iter_mut().zip(renames) {
                let Some(Node::String(name)) = &rename.node else {
                    return None;
                };
                column.0 = name.sval.clone();
            }
            let cte_columns = info
                .columns
                .iter()
                .map(|column| (column.typ, column.deterministic))
                .collect();
            self.rels.push(Rel {
                oid: None,
                refname: alias.clone().unwrap_or_else(|| rv.relname.clone()),
                aliased: alias.is_some(),
                schema: String::new(),
                relname: rv.relname.clone(),
                nullable,
                single_row: info.single_row,
                columns,
                columns_known,
                cte_columns,
            });
            return Some(1 << (self.rels.len() - 1));
        }
        self.add_table(ctx, rv, nullable)
    }

    /// Add one FROM function: every item must be a function call that can't
    /// return a set (every overload, as everywhere else), so it yields
    /// exactly one row. `ROWS FROM` of several such functions pads the
    /// shorter results to the longest — still one row — and `WITH
    /// ORDINALITY` only adds a column.
    ///
    /// Its columns' types are known only for the shape this models soundly:
    /// one function item (no `ROWS FROM` of several), no `WITH ORDINALITY`,
    /// every overload agreeing on a scalar return type that is neither
    /// composite, record, `void`, a pseudo-type nor polymorphic, nor a
    /// domain over a composite, and not collatable (a collatable result can
    /// carry a nondeterministic collation from its arguments). Its column
    /// names are known only when an alias column list or a column definition
    /// list covers every output column; otherwise an unqualified reference
    /// might name a column this doesn't list and must not escape to a
    /// farther level.
    fn add_range_function(
        &mut self,
        ctx: &Ctx<'_>,
        rf: &'a protobuf::RangeFunction,
        nullable: bool,
    ) -> Option<u64> {
        if self.rels.len() == MAX_RELATIONS || rf.functions.is_empty() {
            return None;
        }
        let mut name: Option<&'a str> = None;
        // The names a column definition list gives, when it does.
        let mut defined_names: Vec<&'a str> = Vec::new();
        // Whether every output column's name is certain.
        let mut names_known = true;
        // The total number of output columns, when every item's is known.
        let mut output_count: Option<usize> = Some(0);
        // The one column's type, when every item is one scalar function whose
        // overloads agree on a non-collatable type.
        let mut column_type: Option<PgTypeOid> = None;
        let typed = rf.functions.len() == 1 && !rf.ordinality;
        for item in &rf.functions {
            let Some(Node::List(pair)) = item.node.as_ref() else {
                return None;
            };
            let Some(Node::FuncCall(call)) = pair.items.first().and_then(|n| n.node.as_ref())
            else {
                return None;
            };
            let (schema, func) = match string_list(&call.funcname).as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => return None,
            };
            let procs = ctx.catalog.find_functions(schema, func);
            if procs.is_empty() || procs.iter().any(|proc| proc.proretset) {
                return None;
            }
            // Every overload the name could resolve to must agree on the
            // number of output columns and on the scalar return type.
            let mut item_count: Option<usize> = None;
            let mut item_type: Option<PgTypeOid> = None;
            let mut count_agrees = true;
            let mut type_agrees = true;
            for (index, proc) in procs.iter().enumerate() {
                let count = function_output_columns(ctx, proc);
                let scalar = scalar_function_type(ctx, proc.prorettype);
                if index == 0 {
                    item_count = count;
                    item_type = scalar;
                } else {
                    count_agrees &= item_count == count;
                    type_agrees &= item_type == scalar;
                }
            }
            if !count_agrees {
                item_count = None;
            }
            if !type_agrees {
                item_type = None;
            }
            // A column definition list defines this item's columns: how many,
            // and their names.
            let defs = pair.items.get(1).and_then(|n| n.node.as_ref());
            match defs {
                Some(Node::List(defs)) => {
                    let mut def_count = 0;
                    for def in &defs.items {
                        let Some(Node::ColumnDef(def)) = def.node.as_ref() else {
                            return None;
                        };
                        defined_names.push(def.colname.as_str());
                        def_count += 1;
                    }
                    item_count = Some(def_count);
                }
                Some(_) => return None,
                None => names_known = false,
            }
            output_count = match (output_count, item_count) {
                (Some(total), Some(count)) => Some(total + count),
                _ => None,
            };
            if rf.functions.len() == 1 {
                column_type = item_type;
            }
            name.get_or_insert(func);
        }
        let func = name?;
        // `WITH ORDINALITY` appends one output column.
        if rf.ordinality {
            output_count = output_count.and_then(|count| count.checked_add(1));
        }
        // An alias's column list renames the function's output columns; it
        // must cover all of them to make the names certain.
        let mut alias_names: Vec<&'a str> = Vec::new();
        if let Some(alias) = &rf.alias
            && !alias.colnames.is_empty()
        {
            for name in &alias.colnames {
                let Some(Node::String(name)) = &name.node else {
                    return None;
                };
                alias_names.push(name.sval.as_str());
            }
        }
        let (column_names, columns_known) = if alias_names.is_empty() {
            (defined_names, names_known)
        } else {
            let known = output_count == Some(alias_names.len());
            (alias_names, known)
        };
        let columns_known = columns_known && output_count == Some(column_names.len());
        let typed = typed
            && columns_known
            && column_names.len() == 1
            && output_count == Some(1)
            && column_type.is_some();
        let columns: Vec<(String, i16)> = if columns_known {
            column_names
                .into_iter()
                .enumerate()
                .map(|(i, name)| (name.to_owned(), i as i16 + 1))
                .collect()
        } else {
            Vec::new()
        };
        let cte_columns = match column_type {
            Some(typ) if typed => vec![(Some(typ), deterministic_type(ctx, typ))],
            _ => vec![(None, false); columns.len()],
        };
        let alias = rf.alias.as_ref().map(|a| a.aliasname.clone());
        self.rels.push(Rel {
            oid: None,
            refname: alias.clone().unwrap_or_else(|| func.to_owned()),
            aliased: alias.is_some(),
            schema: String::new(),
            relname: func.to_owned(),
            nullable,
            single_row: true,
            columns,
            columns_known,
            cte_columns,
        });
        Some(1 << (self.rels.len() - 1))
    }

    /// The scope a `LATERAL` FROM subquery at this point sees: exactly the
    /// items to its left, one query level of their own, nested in the
    /// levels around this one. A base table keeps its columns and attnums;
    /// a CTE reference, FROM subquery, VALUES list or FROM function is a
    /// [`Item::Value`] with the types its relation lists.
    fn lateral_scope<'s>(&self, ctx: &Ctx<'_>, outer: &'s FromScope<'s>) -> FromScope<'s> {
        let items = self
            .rels
            .iter()
            .map(|rel| match rel.oid {
                Some(relid) if rel.columns_known => Item::Table {
                    schema: (!rel.aliased).then(|| rel.schema.clone()),
                    refname: rel.refname.clone(),
                    rel: relid,
                    nullable: rel.nullable,
                    columns: rel.columns.clone(),
                },
                Some(_) => Item::Opaque {
                    refname: Some(rel.refname.clone()),
                },
                None if rel.columns_known => Item::Value {
                    refname: rel.refname.clone(),
                    columns: rel
                        .columns
                        .iter()
                        .map(|(name, attnum)| match rel.column_info(ctx, *attnum) {
                            Some((typ, deterministic)) => (name.clone(), Some(typ), deterministic),
                            None => (name.clone(), None, false),
                        })
                        .collect(),
                },
                None => Item::Opaque {
                    refname: (!rel.refname.is_empty()).then(|| rel.refname.clone()),
                },
            })
            .collect();
        FromScope::level(items, outer.parent(), true)
    }

    /// Add one FROM subquery — `LATERAL` or not — with its output columns,
    /// as a one-row relation when this level's proof shows it yields at most
    /// one row per evaluation (see
    /// [`super::cardinality::select_at_most_one_row_in`]).
    ///
    /// The body is judged in PostgreSQL's namespace: a `LATERAL` one sees
    /// exactly the items to its left (each a base table, or a value item
    /// with the types [`Rel::column_info`] lists), a plain one none of this
    /// level's, only the levels around it. Its output columns are typed like
    /// a CTE's ([`super::cardinality::select_output_columns`]).
    ///
    /// A subquery the proof can't bound is still listed, so a nested level's
    /// aggregate argument can be placed on its columns, but it is never
    /// determined and no proof succeeds through it. Its output names are an
    /// alias column list's (renaming the first ones), else the subquery's
    /// own: a bare `VALUES` lists `column1`, `column2`, … and a set operation
    /// its left arm's. A target name that isn't certain (PG's
    /// `FigureColname` derives it from the expression) leaves the whole
    /// column list unknown: a reference spelling that name must not bind
    /// here.
    fn add_range_subselect(
        &mut self,
        ctx: &Ctx<'_>,
        env: &CteEnv,
        sub: &'a protobuf::RangeSubselect,
        nullable: bool,
        outer: &FromScope<'_>,
    ) -> Option<u64> {
        if self.rels.len() == MAX_RELATIONS {
            return None;
        }
        let Some(Node::SelectStmt(select)) = sub.subquery.as_deref().and_then(|n| n.node.as_ref())
        else {
            return None;
        };
        // A `LATERAL` subquery reads the row the items to its left describe,
        // as a namespace of its own; without `LATERAL` it sees none of this
        // level's FROM items, only the levels around this one. Either way it
        // is a query level of its own, and its own `WITH` shadows the
        // enclosing level's CTE names: proving the body against the outer
        // environment would read a same-named outer CTE instead of the one
        // the subquery defines.
        let lateral = sub.lateral.then(|| self.lateral_scope(ctx, outer));
        let parent = match &lateral {
            Some(scope) => Some(scope),
            None => outer.parent(),
        };
        let scope = FromScope::of_select(ctx, select, parent);
        // A CTE body in the subquery is analyzed before its `FROM` clause,
        // like PG's `transformWithClause`: it reads the levels around the
        // subquery, never the subquery's own items.
        let empty;
        let around = match scope.parent() {
            Some(parent) => parent,
            None => {
                empty = FromScope::empty(None);
                &empty
            }
        };
        let env = super::cardinality::subquery_cte_env(ctx, env, select, around);
        let single_row =
            super::cardinality::select_at_most_one_row_in(ctx, select.as_ref(), &scope, &env);
        let (names, mut columns_known) = subquery_output_names(select)?;
        let mut columns: Vec<(String, i16)> = Vec::with_capacity(names.len());
        for (index, name) in names.into_iter().enumerate() {
            columns.push((name, i16::try_from(index).ok()?.checked_add(1)?));
        }
        let renames = sub.alias.as_ref().map_or(&[][..], |a| &a.colnames[..]);
        if renames.len() > columns.len() {
            return None;
        }
        for (column, rename) in columns.iter_mut().zip(renames) {
            let Some(Node::String(name)) = &rename.node else {
                return None;
            };
            column.0 = name.sval.clone();
        }
        // An alias column list that renames every uncertain target makes
        // the names certain again.
        if renames.len() == columns.len() {
            columns_known = true;
        }
        let alias = sub.alias.as_ref().map(|a| a.aliasname.clone());
        let cte_columns = super::cardinality::select_output_columns(ctx, select, &env)
            .into_iter()
            .map(|column| (column.typ, column.deterministic))
            .collect();
        self.rels.push(Rel {
            oid: None,
            // Only an aliased subquery has a name a reference can use.
            refname: alias.clone().unwrap_or_default(),
            aliased: alias.is_some(),
            schema: String::new(),
            relname: alias.clone().unwrap_or_default(),
            nullable,
            single_row,
            columns,
            columns_known,
            cte_columns,
        });
        Some(1 << (self.rels.len() - 1))
    }

    /// Add one base-table reference, or an allowlisted system view's; its
    /// relation as a bitmask.
    fn add_table(&mut self, ctx: &Ctx<'_>, rv: &protobuf::RangeVar, nullable: bool) -> Option<u64> {
        if self.rels.len() == MAX_RELATIONS || !rv.catalogname.is_empty() {
            return None;
        }
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        let class = ctx.catalog.resolve_table(schema, &rv.relname)?;
        // A system view on the fixed allowlist is trusted for its keys
        // ([`unique_keys`]); every other non-table is a relation the proof
        // can't see through.
        let trusted_view =
            class.relkind == RelKind::View && is_trusted_system_view(ctx.catalog, class);
        // The strict scope trusts only a plain table with nothing reading
        // rows the index doesn't cover: no inheritance children, no row
        // security. The cardinality proof may trust more (see [`KeyScope`]):
        // a partitioned table's parent keys are global, `FROM ONLY parent`
        // reads the parent row alone, and row security only filters.
        let has_children = ctx
            .catalog
            .pg_inherits
            .iter()
            .any(|i| i.inhparent == class.oid);
        let usable = trusted_view
            || match (self.mode, class.relkind) {
                (KeyScope::Strict, RelKind::Table) => {
                    !has_children && !ctx.catalog.row_security.contains(&class.oid)
                }
                (KeyScope::Cardinality, RelKind::Table) => !has_children || !rv.inh,
                (KeyScope::Cardinality, RelKind::Partitioned) => true,
                _ => false,
            };
        if !usable {
            return None;
        }
        let alias = rv.alias.as_ref().map(|a| a.aliasname.clone());
        let mut attrs: Vec<_> = ctx
            .catalog
            .attributes_of(class.oid)
            .iter()
            .filter(|a| a.attnum > 0)
            .collect();
        attrs.sort_by_key(|a| a.attnum);
        let mut columns: Vec<(String, i16)> = attrs
            .iter()
            .map(|a| (a.attname.clone(), a.attnum))
            .collect();
        let renames = rv.alias.as_ref().map_or(&[][..], |a| &a.colnames[..]);
        if renames.len() > columns.len() {
            return None;
        }
        for (column, rename) in columns.iter_mut().zip(renames) {
            let Some(Node::String(name)) = &rename.node else {
                return None;
            };
            column.0 = name.sval.clone();
        }
        self.rels.push(Rel {
            oid: Some(class.oid),
            refname: alias.clone().unwrap_or_else(|| rv.relname.clone()),
            aliased: alias.is_some(),
            schema: ctx
                .catalog
                .namespace_name(class.relnamespace)
                .unwrap_or_default()
                .to_owned(),
            relname: rv.relname.clone(),
            nullable,
            single_row: false,
            columns,
            columns_known: true,
            cte_columns: Vec::new(),
        });
        Some(1 << (self.rels.len() - 1))
    }

    /// The relation a qualifier names.
    pub(super) fn rel_named(&self, qualifier: &str) -> Option<usize> {
        self.rels.iter().position(|r| r.refname == qualifier)
    }

    /// Resolve a column reference against this scope alone.
    pub(super) fn resolve(&self, names: &[&str]) -> Col {
        // A relation whose output names aren't all certain may hold the
        // column an *unqualified* reference spells: resolving it to another
        // relation could read a different column (and a key fact on it would
        // be about that other column), so nothing unqualified resolves here.
        // A qualified reference names its relation, whose own attnums are
        // checked below (and `None` while its names aren't all known).
        if names.len() == 1 && self.rels.iter().any(|rel| !rel.columns_known) {
            return Col::Other;
        }
        let attnum = |r: usize, name: &str| {
            self.rels[r]
                .attnum(name)
                .map_or(Col::Other, |attnum| Col::Rel(r, attnum))
        };
        match names {
            [name] => {
                let mut found = self
                    .rels
                    .iter()
                    .enumerate()
                    .filter(|(_, rel)| rel.columns.iter().any(|(n, _)| n == name));
                match (found.next(), found.next()) {
                    (Some((r, _)), None) => attnum(r, name),
                    _ => Col::Other,
                }
            }
            [qualifier, name] => match self.rel_named(qualifier) {
                Some(r) => attnum(r, name),
                None => Col::Other,
            },
            [schema, table, name] => {
                let mut found = self.rels.iter().enumerate().filter(|(_, rel)| {
                    !rel.aliased && rel.schema == *schema && rel.relname == *table
                });
                match (found.next(), found.next()) {
                    (Some((r, _)), None) => attnum(r, name),
                    _ => Col::Other,
                }
            }
            _ => Col::Other,
        }
    }

    /// `expr` as one side of an equality the proof reads: a column of this
    /// scope, a parameter, a constant, or — `outer` being the levels this
    /// one is nested in — a column of an outer query, which is fixed for
    /// one evaluation of this level. An outer column may be a base table's
    /// or a value item's (a CTE reference, `FROM` subquery, `VALUES` list or
    /// `FROM` function whose type is known; see [`FromScope::resolve_value`]).
    /// With `scalar_sublinks` (the cardinality
    /// proof's wider form), also a scalar sublink uncorrelated with this
    /// level, a cast and a call to a non-volatile, non-set-returning
    /// function or operator every argument of which is one of these — each
    /// one value per evaluation of the row (see [`Self::call_operand`]).
    fn operand(
        &self,
        ctx: &Ctx<'_>,
        expr: &protobuf::Node,
        outer: Option<&FromScope<'_>>,
        scalar_sublinks: bool,
    ) -> Option<Operand> {
        match expr.node.as_ref()? {
            Node::ColumnRef(cr) => {
                let Fields::Names(names) = column_ref_fields(cr) else {
                    return None;
                };
                // A relation whose output names aren't all certain may hold
                // the column an unqualified reference spells. Reading it as
                // an outer level's would make its per-row value look fixed,
                // so no key fact is read from such a scope.
                if names.len() == 1 && self.rels.iter().any(|rel| !rel.columns_known) {
                    return None;
                }
                let (rel, attnum, (typ, deterministic_collation)) = match self.resolve(&names) {
                    Col::Rel(r, attnum) => {
                        (Some(r), attnum, self.rels[r].column_info(ctx, attnum)?)
                    }
                    Col::Other => {
                        // The nearest level holding the qualifier hides any
                        // farther one: it can't be an outer relation, even
                        // when its own columns aren't all known.
                        if names.len() > 1 {
                            let qualifier = match names.as_slice() {
                                [qualifier, _] => *qualifier,
                                [_, qualifier, _] => *qualifier,
                                _ => return None,
                            };
                            if self.rel_named(qualifier).is_some() {
                                return None;
                            }
                        }
                        // A column this scope's own relations don't hold:
                        // an outer level's, either a base table's or a
                        // value item's (see [`FromScope::resolve_value`]).
                        let (typ, deterministic_collation) = outer?.resolve_value(ctx, cr)?;
                        (None, 0, (typ, deterministic_collation))
                    }
                };
                deterministic_collation.then_some(Operand {
                    typ,
                    column: rel.map(|r| (r, attnum)),
                    rels: rel.map_or(0, |r| 1 << r),
                })
            }
            Node::SubLink(sub) if scalar_sublinks => self.scalar_sublink_operand(ctx, sub),
            Node::ParamRef(p) => {
                let index = usize::try_from(p.number).ok()?.checked_sub(1)?;
                let param = ctx.analyzed.params.get(index)?;
                deterministic_type_collation(&param.pg_type).then_some(())?;
                Some(Operand {
                    typ: type_oid(ctx, &param.pg_type)?,
                    column: None,
                    rels: 0,
                })
            }
            Node::AConst(c) if !c.isnull => Some(Operand {
                typ: match c.val.as_ref()? {
                    Val::Ival(_) => oid::INT4,
                    Val::Sval(_) => oid::UNKNOWN,
                    Val::Boolval(_) => oid::BOOL,
                    _ => return None,
                },
                column: None,
                rels: 0,
            }),
            Node::TypeCast(cast) if scalar_sublinks => {
                let type_name = cast.type_name.as_ref()?;
                // A typmod (`varchar(10)`, `numeric(8,2)`, an array's
                // bounds) runs the type's length-coercion function after
                // the value is made: what it yields isn't the inner
                // expression's value, and a user-defined coercion may be
                // volatile. The type alone doesn't tell which function it
                // is, so this refuses rather than resolve it.
                if !type_name.typmods.is_empty() || !type_name.array_bounds.is_empty() {
                    return None;
                }
                let arg = self.operand(ctx, cast.arg.as_deref()?, outer, scalar_sublinks)?;
                let typ = type_name_oid(ctx, type_name)?;
                // A conversion that runs a volatile function could yield
                // another value each time the row pair is evaluated.
                let deterministic_collation = deterministic_type(ctx, typ);
                (deterministic_collation && coercion_is_nonvolatile(ctx, arg.typ, typ)).then_some(
                    Operand {
                        typ,
                        column: None,
                        rels: arg.rels,
                    },
                )
            }
            Node::FuncCall(call) if scalar_sublinks => {
                self.call_operand(ctx, call, outer, scalar_sublinks)
            }
            Node::AExpr(expr) if scalar_sublinks && expr.kind() == AExprKind::AexprOp => {
                self.operator_operand(ctx, expr, outer, scalar_sublinks)
            }
            _ => None,
        }
    }

    /// A call's one value per evaluation of the row, when the routine it
    /// resolves to is neither volatile nor set-returning nor an
    /// aggregate/window function and every argument is an
    /// [`Self::operand`]. `rels` is the union of the arguments'.
    ///
    /// A STABLE routine returns the same value for the same arguments
    /// within a statement, so a call whose arguments read only determined
    /// relations (or fixed outer columns) is one value — which a key
    /// equality can bound to one row. A VOLATILE one is re-evaluated per
    /// row pair and may match several rows, so it stays refused.
    ///
    /// Only a call that spells every parameter is read: a parameter left to
    /// its default (`pick()` for `pick(x bigint DEFAULT nextval(…))`) is
    /// expanded into an expression this doesn't see, evaluated at the call
    /// site, and a named argument may leave an earlier default in place
    /// even when the count matches. A `VARIADIC` routine or call packs its
    /// arguments into an array through a function this doesn't resolve.
    fn call_operand(
        &self,
        ctx: &Ctx<'_>,
        call: &protobuf::FuncCall,
        outer: Option<&FromScope<'_>>,
        scalar_sublinks: bool,
    ) -> Option<Operand> {
        let (schema, name, notation) = call_syntax(call)?;
        let mut operands = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            operands.push(self.operand(
                ctx,
                crate::functions::call_arg_value(arg),
                outer,
                scalar_sublinks,
            )?);
        }
        let types: Vec<PgTypeOid> = operands.iter().map(|operand| operand.typ).collect();
        let resolved = crate::functions::resolve_function(
            ctx.catalog,
            schema,
            name,
            &types,
            &notation,
            false,
            None,
        )
        .ok()?;
        if resolved.is_aggregate || resolved.is_window || resolved.is_set_returning {
            return None;
        }
        let proc = ctx.catalog.pg_proc.get(&resolved.oid)?;
        if proc.provolatile == ProVolatile::Volatile {
            return None;
        }
        // `proargtypes` is the routine's declared IN parameters (`pronargs`):
        // fewer supplied arguments leave any trailing ones to their defaults.
        if resolved.provariadic.is_some() || proc.proargtypes.len() != operands.len() {
            return None;
        }
        if resolved.arg_types.len() != operands.len()
            || operands
                .iter()
                .zip(&resolved.arg_types)
                .any(|(operand, target)| !coercion_is_nonvolatile(ctx, operand.typ, *target))
        {
            return None;
        }
        deterministic_type(ctx, resolved.return_type_oid).then_some(Operand {
            typ: resolved.return_type_oid,
            column: None,
            rels: operands.iter().fold(0, |rels, operand| rels | operand.rels),
        })
    }

    /// An operator expression's one value per evaluation: [`Self::call_operand`]
    /// for the function behind the operator.
    fn operator_operand(
        &self,
        ctx: &Ctx<'_>,
        expr: &protobuf::AExpr,
        outer: Option<&FromScope<'_>>,
        scalar_sublinks: bool,
    ) -> Option<Operand> {
        let name = string_list(&expr.name).join(".");
        let left = match expr.lexpr.as_deref() {
            Some(left) => Some(self.operand(ctx, left, outer, scalar_sublinks)?),
            None => None,
        };
        let right = self.operand(ctx, expr.rexpr.as_deref()?, outer, scalar_sublinks)?;
        let op = ctx.catalog.find_operator(
            &name,
            left.as_ref().map(|operand| operand.typ),
            right.typ,
        )?;
        let proc = op.code.and_then(|code| ctx.catalog.pg_proc.get(&code))?;
        if proc.provolatile == ProVolatile::Volatile
            || proc.proretset
            || proc.prokind != ProKind::Function
        {
            return None;
        }
        let coerced = left
            .as_ref()
            .zip(op.left_type_oid)
            .is_none_or(|(operand, target)| coercion_is_nonvolatile(ctx, operand.typ, target))
            && coercion_is_nonvolatile(ctx, right.typ, op.right_type_oid);
        deterministic_type(ctx, op.result_type_oid).then_some(())?;
        coerced.then_some(Operand {
            typ: op.result_type_oid,
            column: None,
            rels: left.map_or(0, |operand| operand.rels) | right.rels,
        })
    }

    /// The one value a scalar sublink yields per evaluation, when it does
    /// not read a column of this level: it may read outer levels and its
    /// own, and a volatile function inside it runs once with it. Its type is
    /// that of the bare column of its own `FROM` its target list selects;
    /// anything else is left to the proof's give-up answer.
    fn scalar_sublink_operand(&self, ctx: &Ctx<'_>, sub: &protobuf::SubLink) -> Option<Operand> {
        if sub.sub_link_type != protobuf::SubLinkType::ExprSublink as i32 {
            return None;
        }
        let Some(Node::SelectStmt(select)) = sub.subselect.as_deref().and_then(|n| n.node.as_ref())
        else {
            return None;
        };
        if select.op() != SetOperation::SetopNone || !select.values_lists.is_empty() {
            return None;
        }
        let [target] = select.target_list.as_slice() else {
            return None;
        };
        let Some(Node::ResTarget(target)) = &target.node else {
            return None;
        };
        let Some(Node::ColumnRef(column)) = target.val.as_deref().and_then(|v| v.node.as_ref())
        else {
            return None;
        };
        let Fields::Names(names) = column_ref_fields(column) else {
            return None;
        };
        // Its own FROM only: a CTE name is a reference the proof gives up on,
        // shadowing a table of the same name it may read.
        let env = CteEnv::blocked(&cte_names(ctx.tree));
        let root = FromScope::opaque(None);
        let scope = Scope::build_env(ctx, &env, &select.from_clause, &root)?;
        if !self.sublink_uncorrelated(&scope, sub) {
            return None;
        }
        let Col::Rel(r, attnum) = scope.resolve(&names) else {
            return None;
        };
        let (typ, deterministic_collation) = scope.rels[r].column_info(ctx, attnum)?;
        deterministic_collation.then_some(Operand {
            typ,
            column: None,
            rels: 0,
        })
    }

    /// Whether nothing in the sublink's subquery reads a column of this
    /// level, and the subquery is a single plain `SELECT` whose references
    /// all resolve in its own FROM (or an outer level above this one). A
    /// nested CTE, sublink or FROM subquery opens another level whose
    /// references this can't place, so it gives up; `false` when unsure.
    fn sublink_uncorrelated(&self, local: &Scope<'_>, sub: &protobuf::SubLink) -> bool {
        let Some(root) = sub.subselect.as_deref().and_then(|n| n.node.as_ref()) else {
            return false;
        };
        let NodeRef::SelectStmt(select) = root.to_ref() else {
            return false;
        };
        if select.op() != SetOperation::SetopNone
            || select.with_clause.is_some()
            || !select.values_lists.is_empty()
        {
            return false;
        }
        let mut stack = vec![root.to_ref()];
        while let Some(node) = stack.pop() {
            match node {
                // A nested query has its own FROM and may bind a name this
                // level also has: the reference can't be placed from here.
                NodeRef::SubLink(_) | NodeRef::RangeSubselect(_) | NodeRef::CommonTableExpr(_) => {
                    return false;
                }
                NodeRef::ColumnRef(column) => {
                    let Fields::Names(names) = column_ref_fields(column) else {
                        return false;
                    };
                    if !matches!(local.resolve(&names), Col::Rel(..)) && self.may_reference(&names)
                    {
                        return false;
                    }
                }
                _ => {}
            }
            stack.extend(node.children());
        }
        true
    }

    /// Whether a column reference with these fields may bind to one of this
    /// scope's relations, under [`Self::resolve`]'s rules: a column (a
    /// system column too), a bare table name (a whole-row reference), or a
    /// composite column's field (`c.field` and `c.field.subfield` spell the
    /// column's first name as the reference's first).
    fn may_reference(&self, names: &[&str]) -> bool {
        // An unknown output name could be the first field of any reference,
        // so a nested query may always bind to such a relation.
        if self.rels.iter().any(|rel| !rel.columns_known) {
            return true;
        }
        let has = |r: usize, name: &str| {
            super::proof::is_system_column(name)
                || self.rels[r].columns.iter().any(|(n, _)| n == name)
        };
        if names
            .first()
            .is_some_and(|first| (0..self.rels.len()).any(|r| has(r, first)))
        {
            return true;
        }
        match names {
            [name] => self.rel_named(name).is_some(),
            [qualifier, name] => self.rel_named(qualifier).is_some_and(|r| has(r, name)),
            [schema, table, name] => {
                let mut found = self.rels.iter().enumerate().filter(|(_, rel)| {
                    !rel.aliased && rel.schema == *schema && rel.relname == *table
                });
                matches!((found.next(), found.next()), (Some((r, _)), None) if has(r, name))
            }
            _ => true,
        }
    }

    /// The facts `column = value` an `=` conjunct proves.
    pub(super) fn equality_facts(
        &self,
        ctx: &Ctx<'_>,
        conjunct: &protobuf::Node,
        outer: Option<&FromScope<'_>>,
    ) -> Vec<EqFact> {
        self.facts_from_conjunct(ctx, conjunct, outer, false)
    }

    /// [`Self::equality_facts`], the cardinality proof's wider form: a scalar
    /// sublink uncorrelated with this level counts as a value operand, and a
    /// value a volatile cast decides does not. The `DISTINCT` proof keeps
    /// the narrow form.
    pub(super) fn cardinality_facts(
        &self,
        ctx: &Ctx<'_>,
        conjunct: &protobuf::Node,
        outer: Option<&FromScope<'_>>,
    ) -> Vec<EqFact> {
        self.facts_from_conjunct(ctx, conjunct, outer, true)
    }

    fn facts_from_conjunct(
        &self,
        ctx: &Ctx<'_>,
        conjunct: &protobuf::Node,
        outer: Option<&FromScope<'_>>,
        cardinality: bool,
    ) -> Vec<EqFact> {
        let Some(Node::AExpr(e)) = &conjunct.node else {
            return Vec::new();
        };
        let name = match string_list(&e.name).as_slice() {
            ["="] => "=",
            ["pg_catalog", "="] => "pg_catalog.=",
            _ => return Vec::new(),
        };
        if e.kind() != AExprKind::AexprOp {
            return Vec::new();
        }
        let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
            return Vec::new();
        };
        let (Some(l), Some(r)) = (
            self.operand(ctx, l, outer, cardinality),
            self.operand(ctx, r, outer, cardinality),
        ) else {
            return Vec::new();
        };
        self.facts_of(ctx, name, &l, &r, cardinality)
    }

    /// The facts `USING (name)` proves between the join's sides.
    pub(super) fn using_facts(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        left: u64,
        right: u64,
    ) -> Vec<EqFact> {
        self.using_facts_with(ctx, name, left, right, false)
    }

    /// [`Self::using_facts`], refusing a value a volatile cast decides.
    pub(super) fn cardinality_using_facts(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        left: u64,
        right: u64,
    ) -> Vec<EqFact> {
        self.using_facts_with(ctx, name, left, right, true)
    }

    fn using_facts_with(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        left: u64,
        right: u64,
        check_casts: bool,
    ) -> Vec<EqFact> {
        let side = |mask: u64| {
            let mut found = (0..self.rels.len())
                .filter(|&r| mask & (1 << r) != 0)
                .filter(|&r| {
                    self.rels[r].columns_known
                        && self.rels[r].columns.iter().any(|(n, _)| n == name)
                })
                .map(|r| (r, self.rels[r].attnum(name)));
            match (found.next(), found.next()) {
                (Some((r, Some(attnum))), None) => {
                    let (typ, deterministic_collation) = self.rels[r].column_info(ctx, attnum)?;
                    deterministic_collation.then_some(Operand {
                        typ,
                        column: Some((r, attnum)),
                        rels: 1 << r,
                    })
                }
                _ => None,
            }
        };
        match (side(left), side(right)) {
            (Some(l), Some(r)) => self.facts_of(ctx, "=", &l, &r, check_casts),
            _ => Vec::new(),
        }
    }

    fn facts_of(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        l: &Operand,
        r: &Operand,
        check_casts: bool,
    ) -> Vec<EqFact> {
        let Some(op) = ctx.catalog.find_operator(name, Some(l.typ), r.typ) else {
            return Vec::new();
        };
        let strict = op
            .code
            .and_then(|code| ctx.catalog.pg_proc.get(&code))
            .is_some_and(|p| p.proisstrict && p.provolatile != ProVolatile::Volatile);
        if !strict {
            return Vec::new();
        }
        // The value that reaches the operator must be the one the operand
        // reads: a volatile cast function on the way, run per row, could
        // turn one value into several. The resolved operand types are what
        // a polymorphic operator was unified with.
        if check_casts
            && (!op
                .left_type_oid
                .is_some_and(|target| coercion_is_nonvolatile(ctx, l.typ, target))
                || !coercion_is_nonvolatile(ctx, r.typ, op.right_type_oid))
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some((rel, attnum)) = l.column
            && r.rels & (1 << rel) == 0
        {
            out.push(EqFact {
                rel,
                attnum,
                op: op.oid,
                declared: op.declared_left_type_oid,
                other: r.rels,
            });
        }
        if let Some((rel, attnum)) = r.column
            && l.rels & (1 << rel) == 0
        {
            let declared = ctx.catalog.pg_operator.get(&op.oid).map(|o| o.oprright);
            out.push(EqFact {
                rel,
                attnum,
                op: op.oid,
                declared,
                other: l.rels,
            });
        }
        out
    }
}

/// The schema, name and notation of a call whose syntax forms no argument
/// this can't see: no aggregate or window decoration, and no named notation
/// (a named argument may leave an earlier parameter to its default even when
/// the count matches).
fn call_syntax(
    call: &protobuf::FuncCall,
) -> Option<(Option<&str>, &str, crate::functions::CallNotation)> {
    if call.over.is_some()
        || call.agg_star
        || call.agg_within_group
        || !call.agg_order.is_empty()
        || call.agg_filter.is_some()
        || call.func_variadic
    {
        return None;
    }
    let (schema, name) = function_name(&call.funcname)?;
    let notation = crate::functions::CallNotation::of(call).ok()?;
    if !notation.names.is_empty() {
        return None;
    }
    Some((schema, name, notation))
}

/// Whether every routine `call` may resolve to spells all its parameters at
/// the call site: no parameter left to a default whose expression would run
/// per evaluation, no named notation and no `VARIADIC` routine or call.
/// [`Scope::call_operand`] makes the same refusal with the routine it
/// resolves; here the call's types aren't known, so every overload counts.
pub(super) fn call_spells_every_parameter(ctx: &Ctx<'_>, call: &protobuf::FuncCall) -> bool {
    let Some((schema, name, _)) = call_syntax(call) else {
        return false;
    };
    let procs = ctx.catalog.find_functions(schema, name);
    !procs.is_empty()
        && procs.iter().all(|proc| {
            proc.prokind == ProKind::Function
                && !proc.proretset
                && proc.provariadic.is_none()
                && proc.proargtypes.len() == call.args.len()
        })
}

/// Whether converting a value of `source` to the operator's declared
/// `target` runs no volatile function: no conversion, an untyped literal
/// (read by the target's input function as the query is parsed), a binary
/// coercion, or a cast function that isn't VOLATILE. An I/O conversion
/// through a type's input or output function, and a cast this can't read,
/// are refused.
///
/// [`crate::ddl::functions::is_binary_coercible`] also accepts a
/// polymorphic target the source fits (`anyenum` for an enum): the
/// operator reads the value as its own type — no conversion runs at all.
fn coercion_is_nonvolatile(ctx: &Ctx<'_>, source: PgTypeOid, target: PgTypeOid) -> bool {
    if source == target || source == oid::UNKNOWN {
        return true;
    }
    if crate::ddl::functions::is_binary_coercible(ctx.catalog, source, target)
        || ctx.catalog.unwrap_domain(source) == target
    {
        return true;
    }
    let cast = ctx
        .catalog
        .cast_by_pair
        .get(&(source, target))
        .and_then(|oid| ctx.catalog.pg_cast.get(oid));
    match cast {
        Some(cast) if cast.castmethod == crate::pg_catalog::CastMethod::Function => cast
            .castfunc
            .and_then(|func| ctx.catalog.pg_proc.get(&func))
            .is_some_and(|proc| proc.provolatile != ProVolatile::Volatile),
        _ => false,
    }
}

/// The conjuncts a set of equality facts was read from, for matching a
/// partial index's predicate: WHERE conjuncts for WHERE facts, a join's ON
/// conjuncts for that join's facts.
pub(super) struct PredicateSources<'a> {
    pub(super) conjuncts: &'a [&'a protobuf::Node],
    /// The `=` facts those conjuncts (and the join's `USING`) prove: only
    /// a strict equality proves its column non-NULL.
    pub(super) facts: &'a [EqFact],
}

impl<'a> Scope<'a> {
    /// When `facts` equate every column of a unique key of relation `b` to
    /// values read from relations within `allowed`: the relations read.
    /// Partial indexes never count (the `DISTINCT` proof's behavior).
    pub(super) fn key_equated(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        facts: &[EqFact],
        allowed: u64,
    ) -> Option<u64> {
        self.key_equated_with(ctx, b, facts, allowed, None)
    }

    /// [`Self::key_equated`], also accepting a partial unique index whose
    /// predicate `sources` imply.
    pub(super) fn key_equated_with(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        facts: &[EqFact],
        allowed: u64,
        sources: Option<&PredicateSources<'_>>,
    ) -> Option<u64> {
        let oid = self.rels[b].oid?;
        unique_keys(ctx, oid).iter().find_map(|key| {
            if let Some(predicate) = &key.predicate {
                let sources = sources?;
                if !self.predicate_implied(ctx, b, predicate, sources) {
                    return None;
                }
            }
            let mut det = 0u64;
            for c in &key.columns {
                if !c.deterministic {
                    return None;
                }
                let fact = facts.iter().find(|f| {
                    f.rel == b
                        && f.attnum == c.attnum
                        && f.other & !allowed == 0
                        && operand_type_matches(ctx, c.typ, f.declared)
                        && in_btree_equality(ctx, &c.family, f.op)
                })?;
                det |= fact.other;
            }
            Some(det)
        })
    }

    /// Whether every conjunct of a partial index's `predicate` holds on
    /// every row `sources` describe.
    fn predicate_implied(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        predicate: &protobuf::Node,
        sources: &PredicateSources<'_>,
    ) -> bool {
        let conjuncts = conjuncts(Some(predicate));
        !conjuncts.is_empty()
            && conjuncts
                .iter()
                .all(|predicate| self.predicate_conjunct_implied(ctx, b, predicate, sources))
    }

    fn predicate_conjunct_implied(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        predicate: &protobuf::Node,
        sources: &PredicateSources<'_>,
    ) -> bool {
        let Some(predicate) = self.canonical_predicate(ctx, b, predicate) else {
            return false;
        };
        // A bare boolean column's predicate is also implied by the
        // comparisons that force its value: `col = true`, `true = col` and
        // `col IS TRUE` for `col`; `col = false`, `false = col`,
        // `col IS FALSE` and `NOT col` for `NOT col`.
        let boolean_fact = match predicate.node.as_ref() {
            Some(Node::Var(var)) => i16::try_from(var.varattno)
                .ok()
                .map(|attnum| (attnum, true)),
            Some(Node::BoolExpr(expr))
                if expr.boolop() == BoolExprType::NotExpr && expr.args.len() == 1 =>
            {
                canonical_var_attnum(&expr.args[0]).map(|attnum| (attnum, false))
            }
            _ => None,
        };
        if let Some((attnum, value)) = boolean_fact
            && self.boolean_column(ctx, b, attnum)
            && builtin_boolean_equality(ctx.catalog)
            && sources.conjuncts.iter().any(|source| {
                let source = self.canonical_source(b, source);
                boolean_conjunct_fixes(&source, attnum, value)
            })
        {
            return true;
        }
        // `col IS NOT NULL` holds wherever a strict equality on `col` does:
        // a fact is only recorded for a strict operator, whose result can't
        // be true with a NULL operand.
        if let Some(Node::NullTest(test)) = &predicate.node
            && test.nulltesttype() == protobuf::NullTestType::IsNotNull
            && let Some(Node::Var(var)) = test.arg.as_deref().and_then(|a| a.node.as_ref())
        {
            let attnum = i16::try_from(var.varattno).ok();
            if sources
                .facts
                .iter()
                .any(|fact| fact.rel == b && Some(fact.attnum) == attnum)
            {
                return true;
            }
        }
        sources.conjuncts.iter().any(|source| {
            let source = self.canonical_source(b, source);
            predicate.equal(&source)
        })
    }

    /// Whether relation `b`'s column `attnum` is a `boolean`.
    fn boolean_column(&self, ctx: &Ctx<'_>, b: usize, attnum: i16) -> bool {
        self.rels[b].oid.is_some_and(|relid| {
            ctx.catalog
                .attributes_of(relid)
                .iter()
                .any(|attribute| attribute.attnum == attnum && attribute.atttypid == oid::BOOL)
        })
    }

    /// `node` with every column reference resolved to relation `b` and
    /// replaced by the column's attnum, when `node` is one of the predicate
    /// shapes the proof reads. A stored predicate names the table's real
    /// columns, so it resolves by `attname`, not by the query's aliases.
    fn canonical_predicate(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        node: &protobuf::Node,
    ) -> Option<protobuf::Node> {
        let mut canonical = node.clone();
        let mut resolved = true;
        canonical.rewrite(&mut |_| true, &mut |node| {
            let Some(Node::ColumnRef(column)) = &node.node else {
                return;
            };
            let fields = column_ref_fields(column);
            let Fields::Names(names) = fields else {
                resolved = false;
                return;
            };
            match self.rels[b].resolve_table_column(ctx, &names) {
                Some(attnum) => *node = canonical_column(attnum),
                None => resolved = false,
            }
        });
        (resolved && supported_predicate(&canonical)).then_some(canonical)
    }

    /// `node` with every column reference that resolves to relation `b`
    /// replaced by the column's attnum; other references are left as they
    /// are (they can't equal a canonical column).
    fn canonical_source(&self, b: usize, node: &protobuf::Node) -> protobuf::Node {
        let mut canonical = node.clone();
        canonical.rewrite(&mut |_| true, &mut |node| {
            let Some(Node::ColumnRef(column)) = &node.node else {
                return;
            };
            let Fields::Names(names) = column_ref_fields(column) else {
                return;
            };
            if let Col::Rel(rel, attnum) = self.resolve(&names)
                && rel == b
            {
                *node = canonical_column(attnum);
            }
        });
        canonical
    }
}

/// A column reference resolved to an attnum, in a form no raw parse tree
/// produces: a placeholder `Var`.
fn canonical_column(attnum: i16) -> protobuf::Node {
    protobuf::Node {
        node: Some(Node::Var(Box::new(protobuf::Var {
            varattno: i32::from(attnum),
            location: 0,
            ..Default::default()
        }))),
    }
}

/// The attnum a canonical [`canonical_column`] `Var` names.
fn canonical_var_attnum(node: &protobuf::Node) -> Option<i16> {
    let Some(Node::Var(var)) = &node.node else {
        return None;
    };
    i16::try_from(var.varattno).ok()
}

/// Whether an unqualified `=` between two booleans resolves to PG's own
/// `booleq`: a user operator of the same signature (allocated above the
/// built-in range) could return true for any pair, so it proves nothing.
fn builtin_boolean_equality(catalog: &PgCatalog) -> bool {
    catalog
        .find_operator("=", Some(oid::BOOL), oid::BOOL)
        .is_some_and(|operator| {
            operator.oid.get() < FIRST_NORMAL_OBJECT_ID
                && catalog
                    .pg_operator
                    .get(&operator.oid)
                    .is_some_and(|declared| {
                        declared.oprleft == Some(oid::BOOL) && declared.oprright == oid::BOOL
                    })
        })
}

/// Whether canonical conjunct `node` forces boolean column `attnum` to
/// `value`: `col = true` or `true = col` and `col IS TRUE` for `true`;
/// `col = false`, `false = col`, `col IS FALSE` and `NOT col` for `false`.
/// The equality must be written unqualified as `=`: an unqualified boolean
/// comparison resolves to `pg_catalog.booleq`, while `OPERATOR(schema.=)`
/// could name an operator with another meaning.
fn boolean_conjunct_fixes(node: &protobuf::Node, attnum: i16, value: bool) -> bool {
    let is_column = |node: &Option<Box<protobuf::Node>>| {
        node.as_deref().and_then(canonical_var_attnum) == Some(attnum)
    };
    let is_value = |node: &Option<Box<protobuf::Node>>| {
        matches!(
            node.as_deref().and_then(|node| node.node.as_ref()),
            Some(Node::AConst(constant))
                if !constant.isnull
                    && matches!(constant.val.as_ref(),
                        Some(Val::Boolval(boolean)) if boolean.boolval == value)
        )
    };
    match node.node.as_ref() {
        Some(Node::AExpr(expr)) if expr.kind() == AExprKind::AexprOp => {
            let name = string_list(&expr.name);
            name.len() == 1
                && name[0] == "="
                && ((is_column(&expr.lexpr) && is_value(&expr.rexpr))
                    || (is_value(&expr.lexpr) && is_column(&expr.rexpr)))
        }
        Some(Node::BooleanTest(test)) => {
            is_column(&test.arg)
                && matches!(
                    (value, test.booltesttype()),
                    (true, protobuf::BoolTestType::IsTrue)
                        | (false, protobuf::BoolTestType::IsFalse)
                )
        }
        Some(Node::BoolExpr(expr))
            if !value && expr.boolop() == BoolExprType::NotExpr && expr.args.len() == 1 =>
        {
            canonical_var_attnum(&expr.args[0]) == Some(attnum)
        }
        _ => false,
    }
}

/// Whether `node` is one of the predicate shapes a partial index may count
/// with: a bare boolean column, `NOT column`, `column IS [NOT] NULL`,
/// `column = / <> constant`, or `column [NOT] IN (constants)` (also spelled
/// `= ANY` / `<> ALL (ARRAY[...])`).
fn supported_predicate(node: &protobuf::Node) -> bool {
    let Some(node) = node.node.as_ref() else {
        return false;
    };
    let is_var = |node: &Option<Box<protobuf::Node>>| {
        matches!(
            node.as_deref().and_then(|n| n.node.as_ref()),
            Some(Node::Var(_))
        )
    };
    match node {
        Node::Var(_) => true,
        Node::NullTest(test) => is_var(&test.arg),
        Node::BoolExpr(expr) => {
            expr.boolop() == BoolExprType::NotExpr
                && expr.args.len() == 1
                && matches!(expr.args[0].node.as_ref(), Some(Node::Var(_)))
        }
        Node::AExpr(expr) => match expr.kind() {
            AExprKind::AexprOp => {
                matches!(string_list(&expr.name).last(), Some(&"=" | &"<>"))
                    && ((is_var(&expr.lexpr) && is_constant(&expr.rexpr))
                        || (is_constant(&expr.lexpr) && is_var(&expr.rexpr)))
            }
            AExprKind::AexprIn => {
                matches!(string_list(&expr.name).last(), Some(&"=" | &"<>"))
                    && is_var(&expr.lexpr)
                    && matches!(
                        expr.rexpr.as_deref().and_then(|n| n.node.as_ref()),
                        Some(Node::List(list)) if list.items.iter().all(is_const_node)
                    )
            }
            AExprKind::AexprOpAny | AExprKind::AexprOpAll => {
                matches!(string_list(&expr.name).last(), Some(&"=" | &"<>"))
                    && is_var(&expr.lexpr)
                    && matches!(
                        expr.rexpr.as_deref().and_then(|n| n.node.as_ref()),
                        Some(Node::AArrayExpr(array)) if array.elements.iter().all(is_const_node)
                    )
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_constant(node: &Option<Box<protobuf::Node>>) -> bool {
    node.as_deref().is_some_and(is_const_node)
}

fn is_const_node(node: &protobuf::Node) -> bool {
    matches!(node.node, Some(Node::AConst(_)))
}

/// The equality operator reads the column as its own type: no coercion
/// that could change its value on the way.
///
/// [`crate::ddl::functions::is_binary_coercible`] is PG's full rule
/// (`IsBinaryCoercible`, parse_coerce.c): besides domains and binary casts
/// it also accepts a polymorphic operator's declared pseudo-type
/// (`anyenum` for `enum_eq`, `anyarray` for `array_eq`) when the column's
/// type fits it. That only widens the match to the operator PG resolved the
/// equality to; [`in_btree_equality`] still requires it to be the btree
/// equality of the key column's own operator family.
fn operand_type_matches(ctx: &Ctx<'_>, column: PgTypeOid, declared: Option<PgTypeOid>) -> bool {
    declared.is_some_and(|d| {
        d == column
            || d == ctx.catalog.unwrap_domain(column)
            || crate::ddl::functions::is_binary_coercible(ctx.catalog, column, d)
    })
}

/// `op` is the btree equality (strategy 3) of operator family `family`.
fn in_btree_equality(ctx: &Ctx<'_>, family: &(String, PgNamespaceOid), op: PgOperatorOid) -> bool {
    ctx.catalog.pg_amop.iter().any(|a| {
        a.amopmethod == "btree"
            && a.amopstrategy == 3
            && a.amopopr == op
            && a.amopfamily == family.0
            && a.amopfamilynamespace == family.1
    })
}

// ── System views ────────────────────────────────────────────────────────────

/// The `pg_catalog` system views whose uniqueness PostgreSQL itself
/// guarantees, with the columns of each such key.
///
/// Only these views are trusted; a view of the same name in another schema,
/// a user view (even one named after these) and every statistics view —
/// `pg_stat_activity` included, which has no promised key — stay relations
/// the proof gives up on.
const SYSTEM_VIEW_KEYS: &[(&str, &[&[&str]])] = &[
    // One row per configuration parameter.
    ("pg_settings", &[&["name"]]),
    // One row per role.
    ("pg_roles", &[&["oid"], &["rolname"]]),
    // One row per role that can log in (`pg_user` is `pg_shadow`'s view).
    ("pg_user", &[&["usename"]]),
    // One row per table / view / index of a schema.
    ("pg_tables", &[&["schemaname", "tablename"]]),
    ("pg_views", &[&["schemaname", "viewname"]]),
    ("pg_indexes", &[&["schemaname", "indexname"]]),
];

/// OIDs below this are PG's own objects (`FirstNormalObjectId`): the
/// allowlist names system views by name, and only the seeded system view of
/// that name is trusted — a user view in `pg_catalog` (a dropped system view
/// replaced by one of the same name) gets a normal OID.
const FIRST_NORMAL_OBJECT_ID: u32 = 16_384;

/// Whether `class` is one of the allowlisted system views: a `pg_catalog`
/// view seeded as PG's own object (not a user view of the same name), whose
/// definition the migrations never replaced or altered (it would keep the
/// OID, so the seeded definition's guarantees would no longer hold).
fn is_trusted_system_view(catalog: &PgCatalog, class: &crate::pg_catalog::PgClass) -> bool {
    class.relkind == RelKind::View
        && class.oid.get() < FIRST_NORMAL_OBJECT_ID
        && !catalog.relation_was_modified(class.oid)
        && catalog.namespace_name(class.relnamespace) == Some("pg_catalog")
        && SYSTEM_VIEW_KEYS
            .iter()
            .any(|(name, _)| *name == class.relname)
}

/// The allowlisted keys of a system view, as unique keys: the view has no
/// index to name an operator family, so each column's is its type's default
/// btree one, and its collation must be deterministic like a table's. No
/// view attribute is a stored validated NOT NULL, so no key column claims
/// one (the cardinality proof needs none; `DISTINCT` still refuses).
fn system_view_keys(
    catalog: &PgCatalog,
    attrs: &[crate::pg_catalog::PgAttribute],
    keys: &[&[&str]],
) -> Vec<UniqueKey> {
    keys.iter()
        .filter_map(|key| {
            let mut columns = Vec::with_capacity(key.len());
            for name in *key {
                let attr = attrs.iter().find(|a| a.attnum > 0 && a.attname == *name)?;
                let family = default_btree_family(catalog, attr.atttypid)?;
                columns.push(KeyColumn {
                    attnum: attr.attnum,
                    typ: attr.atttypid,
                    notnull: false,
                    family,
                    default_family: true,
                    deterministic: deterministic(catalog, attr.attcollation),
                });
            }
            Some(UniqueKey {
                columns,
                nulls_not_distinct: false,
                predicate: None,
            })
        })
        .collect()
}

/// The operator family of `typ`'s default btree operator class.
fn default_btree_family(catalog: &PgCatalog, typ: PgTypeOid) -> Option<(String, PgNamespaceOid)> {
    crate::ddl::opclass::default_opclass_id(catalog, typ, "btree")
        .and_then(|d| catalog.pg_opclass.iter().find(|c| c.oid == d))
        .map(|d| (d.opcfamily.clone(), d.opcfamilynamespace))
}

// ── Unique keys ─────────────────────────────────────────────────────────────

struct KeyColumn {
    attnum: i16,
    typ: PgTypeOid,
    /// A validated NOT NULL ([`super::proof::column_not_null`]): never raw
    /// `attnotnull`, which PG 18 sets for a `NOT VALID` one too.
    notnull: bool,
    /// The index's operator family, by name and namespace.
    family: (String, PgNamespaceOid),
    /// That family is the column type's default btree one — the equality
    /// `DISTINCT` and `UNION` compare with.
    default_family: bool,
    /// The index's and the column's collations are deterministic.
    deterministic: bool,
}

struct UniqueKey {
    columns: Vec<KeyColumn>,
    nulls_not_distinct: bool,
    /// A partial index's predicate, decoded; `None` for a full index.
    predicate: Option<Box<protobuf::Node>>,
}

/// The unique keys PG enforces at every statement on relation `relid`:
/// unique btree indexes — PRIMARY KEY and UNIQUE constraints' included —
/// that are immediate, valid, not exclusion and on plain columns. A partial
/// index's predicate is kept for the cardinality proof; the `DISTINCT`
/// proof still ignores partial indexes. An index the catalog marks invalid
/// enforces nothing yet — a partitioned parent's `ON ONLY` index until every
/// partition has an attached index — so it is never a key.
fn unique_keys(ctx: &Ctx<'_>, relid: PgClassOid) -> Vec<UniqueKey> {
    let catalog = ctx.catalog;
    let attrs = catalog.attributes_of(relid);
    // A trusted system view has no index: its keys are PostgreSQL's own.
    if let Some(class) = catalog.pg_class.get(&relid)
        && is_trusted_system_view(catalog, class)
    {
        return SYSTEM_VIEW_KEYS
            .iter()
            .find(|(name, _)| *name == class.relname)
            .map_or_else(Vec::new, |(_, keys)| system_view_keys(catalog, attrs, keys));
    }
    let mut out = Vec::new();
    'index: for index in catalog.pg_index.values() {
        if index.indrelid != relid
            || !index.indisunique
            || index.indisexclusion
            || catalog.nonimmediate_indexes.contains(&index.indexrelid)
            || catalog.invalid_indexes.contains(&index.indexrelid)
        {
            continue;
        }
        let predicate = match &index.indpred {
            None => None,
            Some(ast) => match protobuf::Node::decode(ast.ast.as_slice()) {
                Ok(predicate) => Some(Box::new(predicate)),
                Err(_) => continue 'index,
            },
        };
        let nkeys = usize::try_from(index.indnkeyatts).unwrap_or(0);
        if nkeys == 0 || index.indkey.len() < nkeys {
            continue;
        }
        let mut columns = Vec::with_capacity(nkeys);
        for i in 0..nkeys {
            let attnum = index.indkey[i];
            let Some(attr) = attrs.iter().find(|a| a.attnum == attnum && attnum > 0) else {
                continue 'index;
            };
            let Some(opclass) = index
                .indclass
                .get(i)
                .copied()
                .flatten()
                .and_then(|o| catalog.pg_opclass.iter().find(|c| c.oid == o))
            else {
                continue 'index;
            };
            if opclass.opcmethod != "btree" {
                continue 'index;
            }
            let family = (opclass.opcfamily.clone(), opclass.opcfamilynamespace);
            let default_family =
                crate::ddl::opclass::default_opclass_id(catalog, attr.atttypid, "btree")
                    .and_then(|d| catalog.pg_opclass.iter().find(|c| c.oid == d))
                    .is_some_and(|d| d.opcfamily == family.0 && d.opcfamilynamespace == family.1);
            columns.push(KeyColumn {
                attnum,
                typ: attr.atttypid,
                notnull: super::proof::column_not_null(catalog, relid, attnum),
                family,
                default_family,
                deterministic: deterministic(catalog, index.indcollation.get(i).copied().flatten())
                    && deterministic(catalog, attr.attcollation),
            });
        }
        out.push(UniqueKey {
            columns,
            nulls_not_distinct: catalog
                .index_keys
                .get(&index.indexrelid)
                .is_some_and(|k| k.nulls_not_distinct),
            predicate,
        });
    }
    out
}

/// Whether relation `relid` provably holds at most one row: a complete,
/// immediate, non-partial unique key of it whose every column is NOT NULL,
/// compares under a deterministic collation, and is fixed to one value by a
/// validated, enforced CHECK constraint the relation itself carries.
///
/// The check must be `col` (a boolean, fixing it true), `NOT col`, or
/// `col = <literal>` / `<literal> = col`, possibly inside an `AND`. A CHECK
/// passes on NULL too, so NOT NULL is required: the check alone would let
/// the column be NULL or the fixed value, two distinct keys. The collation
/// must be deterministic, or a case-insensitive comparison in the check
/// could admit several byte strings while the index tells them apart. The
/// relation must have no inheritance children — a child may hold rows the
/// parent's index and check don't cover, and `NO INHERIT` checks need not
/// reach it.
pub(super) fn singleton_table(ctx: &Ctx<'_>, relid: PgClassOid) -> bool {
    if ctx
        .catalog
        .pg_inherits
        .iter()
        .any(|inherit| inherit.inhparent == relid)
    {
        return false;
    }
    let attrs = ctx.catalog.attributes_of(relid);
    unique_keys(ctx, relid).iter().any(|key| {
        key.predicate.is_none()
            && key
                .columns
                .iter()
                .all(|column| column.notnull && column.deterministic)
            && key.columns.iter().all(|column| {
                let Some(attr) = attrs.iter().find(|attr| attr.attnum == column.attnum) else {
                    return false;
                };
                check_fixes_column(ctx.catalog, relid, &attr.attname, attr.atttypid)
            })
    })
}

/// Whether a validated, enforced CHECK constraint of `relid` forces column
/// `name` of type `ty` to one value (see [`singleton_table`]).
fn check_fixes_column(catalog: &PgCatalog, relid: PgClassOid, name: &str, ty: PgTypeOid) -> bool {
    catalog.pg_constraint.values().any(|constraint| {
        constraint.conrelid == relid
            && constraint.contype == ConType::Check
            && constraint.convalidated
            && constraint.conenforced
            && catalog
                .check_defs
                .get(&constraint.oid)
                .is_some_and(|check| {
                    matches!(&check.expr,
                    crate::ddl::tables::check_inherit::StoredExpr::Written(expr)
                        if check_fixes_value(catalog, expr, name, ty))
                })
    })
}

/// Whether check expression `expr` implies that its column `name` has one
/// value: `col` for a boolean, `NOT col`, `col = <literal>` or the literal
/// on the left, inside any `AND` (every conjunct holds on a passing row).
/// The equality is only read when the catalog holds no user-defined `=`
/// operator at all (see [`user_defined_equality_operator_exists`]); the
/// boolean forms involve no operator.
fn check_fixes_value(
    catalog: &PgCatalog,
    expr: &protobuf::Node,
    name: &str,
    ty: PgTypeOid,
) -> bool {
    let is_named = |node: &Option<Box<protobuf::Node>>| {
        node.as_deref()
            .and_then(column_name)
            .is_some_and(|column| column == name)
    };
    let Some(expr) = expr.node.as_ref() else {
        return false;
    };
    match expr {
        Node::BoolExpr(bool) if bool.boolop() == BoolExprType::AndExpr => bool
            .args
            .iter()
            .any(|arg| check_fixes_value(catalog, arg, name, ty)),
        Node::ColumnRef(column) => {
            ty == oid::BOOL
                && matches!(column_ref_fields(column), Fields::Names(names)
                    if names.last().is_some_and(|column| *column == name))
        }
        Node::BoolExpr(bool) if bool.boolop() == BoolExprType::NotExpr && bool.args.len() == 1 => {
            ty == oid::BOOL
                && matches!(bool.args[0].node.as_ref(), Some(Node::ColumnRef(column))
                    if matches!(column_ref_fields(column), Fields::Names(names)
                        if names.last().is_some_and(|column| *column == name)))
        }
        Node::AExpr(expr) if expr.kind() == AExprKind::AexprOp => {
            let op = string_list(&expr.name);
            op.len() == 1
                && op[0] == "="
                // The CHECK's `=` was bound under the `search_path` in effect
                // when the constraint was created; re-resolving it under
                // today's could find another operator than the one stored.
                // With no user-defined `=` in the catalog there is nothing
                // else it could have bound to.
                && !user_defined_equality_operator_exists(catalog)
                && ((is_named(&expr.lexpr) && fixed_literal(catalog, &expr.rexpr, ty))
                    || (fixed_literal(catalog, &expr.lexpr, ty) && is_named(&expr.rexpr)))
        }
        _ => false,
    }
}

/// Whether the catalog holds a user-defined operator named `=` (outside
/// `pg_catalog`, in the normal OID range). Such an operator can mean
/// anything — `loose.=` returning true for every pair is a valid one — and a
/// CHECK expression stores only the name it was written with, so its `=`
/// may have been bound to one of these under the creating session's
/// `search_path`.
fn user_defined_equality_operator_exists(catalog: &PgCatalog) -> bool {
    catalog.pg_operator.values().any(|operator| {
        operator.oprname == "="
            && operator.oid.get() >= FIRST_NORMAL_OBJECT_ID
            && catalog.namespace_name(operator.oprnamespace) != Some("pg_catalog")
    })
}

/// The name a column reference in a stored expression names: its last
/// string field (`col`, `t.col`, `s.t.col`).
fn column_name(node: &protobuf::Node) -> Option<&str> {
    let Node::ColumnRef(column) = node.node.as_ref()? else {
        return None;
    };
    let Fields::Names(names) = column_ref_fields(column) else {
        return None;
    };
    names.last().copied()
}

/// Whether `node` is a non-NULL literal that fixes a column of type `ty` to
/// one value under PG's own equality: a boolean literal, an integer literal,
/// or a string literal for a built-in type (whose input function is PG's
/// own and deterministic). The `=` PG resolves must be a built-in operator
/// over `ty` — a user operator named `=` could be true for many values — and
/// a user type's input function isn't modeled, so its literals are refused.
fn fixed_literal(catalog: &PgCatalog, node: &Option<Box<protobuf::Node>>, ty: PgTypeOid) -> bool {
    let Some(Node::AConst(constant)) = node.as_deref().and_then(|node| node.node.as_ref()) else {
        return false;
    };
    if constant.isnull {
        return false;
    }
    let literal = match constant.val.as_ref() {
        Some(Val::Boolval(_)) if ty == oid::BOOL => oid::BOOL,
        Some(Val::Ival(_)) if ty.get() < 10_000 => oid::INT4,
        Some(Val::Sval(_)) if ty.get() < 10_000 => oid::UNKNOWN,
        _ => return false,
    };
    catalog
        .find_operator("=", Some(ty), literal)
        .is_some_and(|operator| {
            operator.oid.get() < FIRST_NORMAL_OBJECT_ID
                && catalog
                    .pg_operator
                    .get(&operator.oid)
                    .is_some_and(|declared| {
                        declared.oprleft.is_some_and(|left| {
                            left == ty
                                || crate::ddl::functions::is_binary_coercible(catalog, ty, left)
                        }) && declared.oprright.get() < 10_000
                    })
        })
}

/// Whether a collation compares equal strings as equal: none (the database
/// default, which PG doesn't allow to be nondeterministic), or one
/// `pg_collation.collisdeterministic` records as deterministic — `C`,
/// `POSIX`, a locale or ICU collation, or a user collation copied from them.
/// A collation the catalog doesn't know, and a nondeterministic one (ICU
/// with a non-binary level), are refused.
fn deterministic(catalog: &PgCatalog, collation: Option<PgCollationOid>) -> bool {
    collation.is_none_or(|c| {
        catalog
            .pg_collation
            .get(&c)
            .is_some_and(|collation| collation.collisdeterministic)
    })
}

/// [`deterministic`] for a type as the analysis reports it (the collation
/// is named only when not the database default).
pub(super) fn deterministic_type_collation(t: &Type) -> bool {
    match t {
        Type::Basic { collation, .. } | Type::Domain { collation, .. } => collation
            .as_deref()
            .is_none_or(|c| matches!(c, "C" | "POSIX")),
        _ => true,
    }
}

pub(super) fn type_oid(ctx: &Ctx<'_>, t: &Type) -> Option<PgTypeOid> {
    match t {
        Type::Basic { schema, name, .. }
        | Type::Domain { schema, name, .. }
        | Type::Enum { schema, name, .. }
        | Type::Range { schema, name, .. }
        | Type::Composite { schema, name, .. } => ctx
            .catalog
            .resolve_type_by_name(Some(schema), name)
            .map(|t| t.oid),
        Type::Array { element, .. } => ctx.catalog.array_type_of(type_oid(ctx, element)?),
        Type::AnonymousRecord { .. } => None,
    }
}

/// The type a `TypeCast`'s `TypeName` names, array bounds included.
fn type_name_oid(ctx: &Ctx<'_>, type_name: &protobuf::TypeName) -> Option<PgTypeOid> {
    let names = string_list(&type_name.names);
    let typ = match names.as_slice() {
        [name] => ctx.catalog.resolve_type_by_name(None, name),
        [schema, name] => ctx.catalog.resolve_type_by_name(Some(schema), name),
        _ => None,
    }?;
    if type_name.array_bounds.is_empty() {
        Some(typ.oid)
    } else {
        ctx.catalog.array_type_of(typ.oid)
    }
}

/// Whether a value of type `typ` is under a deterministic collation: the
/// type's own (`typcollation`), else its base type's for a domain, is none
/// or a bytewise one.
fn deterministic_type(ctx: &Ctx<'_>, typ: PgTypeOid) -> bool {
    let Some(ty) = ctx.catalog.pg_type.get(&typ) else {
        return false;
    };
    let collation = ty.typcollation.or_else(|| {
        ctx.catalog
            .pg_type
            .get(&ctx.catalog.unwrap_domain(typ))
            .and_then(|base| base.typcollation)
    });
    deterministic(ctx.catalog, collation)
}

/// The number of output columns a non-set-returning function expands to in
/// `FROM`, when known: its `OUT` / `INOUT` / `TABLE` parameters, the
/// attributes of a named composite return type (or a domain over one), else
/// one scalar column. `None` for `record` without such parameters — a column
/// definition list supplies those.
fn function_output_columns(ctx: &Ctx<'_>, proc: &crate::pg_catalog::PgProc) -> Option<usize> {
    let out = proc
        .proargmodes
        .iter()
        .zip(&proc.proallargtypes)
        .filter(|(mode, _)| {
            matches!(
                mode,
                crate::pg_catalog::ArgMode::Out
                    | crate::pg_catalog::ArgMode::InOut
                    | crate::pg_catalog::ArgMode::Table
            )
        })
        .count();
    if out > 0 {
        return Some(out);
    }
    if proc.prorettype == oid::RECORD {
        return None;
    }
    let base = ctx.catalog.unwrap_domain(proc.prorettype);
    let ty = ctx.catalog.get_type(base)?;
    if ty.typtype == crate::pg_catalog::TypType::Composite {
        return Some(
            ctx.catalog
                .attributes_of(ty.typrelid?)
                .iter()
                .filter(|attribute| attribute.attnum > 0)
                .count(),
        );
    }
    Some(1)
}

/// The type of the single column a non-set-returning function returns, when
/// the proof may type it: a defined scalar type that is neither composite,
/// `record`, `void`, a pseudo-type nor polymorphic, nor a domain over a
/// composite, and not collatable (no `typcollation` on the type or the base
/// of a domain). A collatable result can carry a nondeterministic collation
/// from its arguments, which this can't see.
fn scalar_function_type(ctx: &Ctx<'_>, typ: PgTypeOid) -> Option<PgTypeOid> {
    let base = ctx.catalog.unwrap_domain(typ);
    let ty = ctx.catalog.get_type(base)?;
    if !ty.typisdefined
        || matches!(
            ty.typtype,
            crate::pg_catalog::TypType::Composite | crate::pg_catalog::TypType::Pseudo
        )
        || ty.typcollation.is_some()
    {
        return None;
    }
    if typ != base
        && ctx
            .catalog
            .get_type(typ)
            .is_some_and(|domain| domain.typcollation.is_some())
    {
        return None;
    }
    Some(typ)
}

/// The last name of a function call, with its schema when written.
fn function_name(nodes: &[protobuf::Node]) -> Option<(Option<&str>, &str)> {
    match string_list(nodes).as_slice() {
        [name] => Some((None, *name)),
        [schema, name] => Some((Some(*schema), *name)),
        _ => None,
    }
}

/// The names PG gives a subquery's output columns, with whether every one is
/// certain: a bare `VALUES` names them `column1`, `column2`, …, a set
/// operation takes its left arm's, and a plain `SELECT` follows
/// [`target_output_name`]. An uncertain name becomes
/// [`unknown_column_name`]: no reference can spell it.
fn subquery_output_names(select: &protobuf::SelectStmt) -> Option<(Vec<String>, bool)> {
    if select.op() != SetOperation::SetopNone {
        return subquery_output_names(select.larg.as_deref()?);
    }
    if let Some(first) = select.values_lists.first() {
        let Some(Node::List(list)) = &first.node else {
            return None;
        };
        return Some((
            (0..list.items.len())
                .map(|index| format!("column{}", index + 1))
                .collect(),
            true,
        ));
    }
    let mut known = true;
    let names = select
        .target_list
        .iter()
        .map(|target| match &target.node {
            Some(Node::ResTarget(target)) => {
                Some(target_output_name(target).unwrap_or_else(|| {
                    known = false;
                    unknown_column_name()
                }))
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((names, known))
}

/// The name PG gives a subquery target-list entry, when it is certain: the
/// written alias, or a bare column reference's name. `FigureColname` derives
/// other names from the expression (a cast from its argument, a call from the
/// function, `CASE` as `case`), which this doesn't reproduce, so they are
/// unknown.
fn target_output_name(target: &protobuf::ResTarget) -> Option<String> {
    if !target.name.is_empty() {
        return Some(target.name.clone());
    }
    match target
        .val
        .as_deref()
        .and_then(|value| value.node.as_ref())?
    {
        Node::ColumnRef(column) => match column_ref_fields(column) {
            Fields::Names(names) => names.last().map(|name| (*name).to_owned()),
            _ => None,
        },
        _ => None,
    }
}

/// A placeholder for a column whose PG name isn't certain. No SQL identifier
/// can spell it, so a reference never binds to it: the relation's column
/// list stays uncertain for every lookup.
fn unknown_column_name() -> String {
    "\0unknown".to_owned()
}

// ── UNION ───────────────────────────────────────────────────────────────────

/// The arms of a tree of UNIONs (`false` on an INTERSECT or EXCEPT), and
/// its `UNION`s without `ALL`.
fn union_arms<'a>(
    s: &'a protobuf::SelectStmt,
    arms: &mut Vec<&'a protobuf::SelectStmt>,
    unions: &mut Vec<&'a protobuf::SelectStmt>,
) -> bool {
    match s.op() {
        SetOperation::SetopNone => {
            arms.push(s);
            true
        }
        SetOperation::SetopUnion => {
            if !s.all {
                unions.push(s);
            }
            let (Some(l), Some(r)) = (s.larg.as_deref(), s.rarg.as_deref()) else {
                return false;
            };
            union_arms(l, arms, unions) && union_arms(r, arms, unions)
        }
        _ => false,
    }
}

fn has_star(s: &protobuf::SelectStmt) -> bool {
    s.target_list.iter().any(|t| {
        matches!(&t.node, Some(Node::ResTarget(rt))
            if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
                Some(Node::ColumnRef(cr)) if matches!(column_ref_fields(cr), Fields::Star(_))))
    })
}

fn target_value(s: &protobuf::SelectStmt, p: usize) -> Option<&protobuf::Node> {
    match &s.target_list.get(p)?.node {
        Some(Node::ResTarget(rt)) => rt.val.as_deref(),
        _ => None,
    }
}

/// An arm's value at an output position is of exactly type `typ`, with a
/// deterministic collation: a column of that type, a parameter of it, or a
/// constant that takes it.
fn arm_value_is_exactly(
    ctx: &Ctx<'_>,
    scope: Option<&Scope<'_>>,
    value: Option<&protobuf::Node>,
    typ: PgTypeOid,
) -> bool {
    let Some(value) = value.and_then(|v| v.node.as_ref()) else {
        return false;
    };
    match value {
        // An untyped literal or NULL takes the other arms' type.
        Node::AConst(c) => match c.val.as_ref() {
            None => c.isnull,
            Some(Val::Sval(_)) => true,
            Some(Val::Ival(_)) => typ == oid::INT4,
            Some(Val::Boolval(_)) => typ == oid::BOOL,
            Some(_) => false,
        },
        Node::ParamRef(p) => usize::try_from(p.number)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| ctx.analyzed.params.get(i))
            .is_some_and(|param| {
                deterministic_type_collation(&param.pg_type)
                    && type_oid(ctx, &param.pg_type) == Some(typ)
            }),
        Node::ColumnRef(cr) => {
            let (Some(scope), Fields::Names(names)) = (scope, column_ref_fields(cr)) else {
                return false;
            };
            let Col::Rel(r, attnum) = scope.resolve(&names) else {
                return false;
            };
            scope.rels[r].column_info(ctx, attnum).is_some_and(
                |(column, deterministic_collation)| column == typ && deterministic_collation,
            )
        }
        _ => false,
    }
}

/// Output position `p` is a different non-NULL constant in every arm,
/// under the column's type: all integers (`int4`), all strings (`text`) or
/// all booleans.
fn is_tag_column(ctx: &Ctx<'_>, arms: &[&protobuf::SelectStmt], p: usize) -> bool {
    let Some(typ) = type_oid(ctx, &ctx.analyzed.columns[p].pg_type) else {
        return false;
    };
    let mut seen: HashSet<String> = HashSet::new();
    for arm in arms {
        let Some(Node::AConst(c)) = target_value(arm, p).and_then(|v| v.node.as_ref()) else {
            return false;
        };
        let value = match (c.val.as_ref(), typ) {
            (Some(Val::Ival(i)), oid::INT4) => i.ival.to_string(),
            (Some(Val::Sval(s)), oid::TEXT) => s.sval.clone(),
            (Some(Val::Boolval(b)), oid::BOOL) => b.boolval.to_string(),
            _ => return false,
        };
        if !seen.insert(value) {
            return false;
        }
    }
    true
}

// ── Text edits ──────────────────────────────────────────────────────────────

fn scan(sql: &str) -> Option<Vec<protobuf::ScanToken>> {
    typedpg_pg_query::scan(sql).ok().map(|r| r.tokens)
}

fn is_comment(t: &protobuf::ScanToken) -> bool {
    matches!(t.token(), Token::SqlComment | Token::CComment)
}

/// Remove `s`'s `DISTINCT` keyword: the token right before its first
/// target, right after `SELECT`.
fn distinct_keyword_edit(
    sql: &str,
    tokens: &[protobuf::ScanToken],
    s: &protobuf::SelectStmt,
) -> Option<Edit> {
    let Some(Node::ResTarget(first)) = s.target_list.first().and_then(|t| t.node.as_ref()) else {
        return None;
    };
    let at = first.location;
    if at < 0 {
        return None;
    }
    let mut before = tokens
        .iter()
        .filter(|t| t.end <= at && !is_comment(t))
        .rev();
    let distinct = before.next()?;
    if distinct.token() != Token::Distinct || before.next()?.token() != Token::Select {
        return None;
    }
    let start = usize::try_from(distinct.start).ok()?;
    let end = usize::try_from(distinct.end).ok()?;
    let target = usize::try_from(at).ok()?;
    // Take the blank after the keyword along when nothing else is there.
    let end = if sql.get(end..target)?.chars().all(char::is_whitespace) {
        target
    } else {
        end
    };
    Some(Edit {
        range: start..end,
        text: String::new(),
    })
}

/// Make `s`'s `UNION` (or `UNION DISTINCT`) a `UNION ALL`. Its keyword is
/// the last `UNION` before the right arm's first located node: only `(`,
/// `SELECT` and `ALL`/`DISTINCT` can stand between them.
fn union_keyword_edit(tokens: &[protobuf::ScanToken], s: &protobuf::SelectStmt) -> Option<Edit> {
    let rarg = s.rarg.as_deref()?;
    let mut first: Option<i32> = None;
    let mut stack = vec![NodeRef::SelectStmt(rarg)];
    while let Some(n) = stack.pop() {
        if let Some(l) = n.location() {
            first = Some(first.map_or(l, |f| f.min(l)));
        }
        stack.extend(n.children());
    }
    let first = first?;
    let union_at = tokens
        .iter()
        .rposition(|t| t.end <= first && t.token() == Token::Union)?;
    let between_ok = tokens[union_at + 1..]
        .iter()
        .take_while(|t| t.end <= first)
        .all(|t| {
            is_comment(t)
                || matches!(
                    t.token(),
                    Token::Ascii40 | Token::Select | Token::Distinct | Token::All
                )
        });
    if !between_ok {
        return None;
    }
    let union = &tokens[union_at];
    let next = tokens[union_at + 1..].iter().find(|t| !is_comment(t))?;
    match next.token() {
        Token::Distinct => Some(Edit {
            range: usize::try_from(next.start).ok()?..usize::try_from(next.end).ok()?,
            text: "ALL".to_owned(),
        }),
        Token::All => None,
        _ => {
            let end = usize::try_from(union.end).ok()?;
            Some(Edit {
                range: end..end,
                text: " ALL".to_owned(),
            })
        }
    }
}

// ── Small helpers ───────────────────────────────────────────────────────────

/// Every CTE name defined anywhere in the statement: a table reference
/// spelled like one may be the CTE.
pub(super) fn cte_names(tree: &protobuf::ParseResult) -> HashSet<String> {
    tree.nodes()
        .into_iter()
        .filter_map(|(n, _)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.clone()),
            _ => None,
        })
        .collect()
}

/// The top-level AND conjuncts of a qualification.
pub(super) fn conjuncts(qual: Option<&protobuf::Node>) -> Vec<&protobuf::Node> {
    let mut out = Vec::new();
    let mut stack: Vec<&protobuf::Node> = qual.into_iter().collect();
    while let Some(n) = stack.pop() {
        match &n.node {
            Some(Node::BoolExpr(b)) if b.boolop() == BoolExprType::AndExpr => {
                stack.extend(b.args.iter().rev());
            }
            _ => out.push(n),
        }
    }
    out
}

fn string_list(nodes: &[protobuf::Node]) -> Vec<&str> {
    nodes
        .iter()
        .filter_map(|n| match &n.node {
            Some(Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn column_ref_fields(cr: &protobuf::ColumnRef) -> Fields<'_> {
    let mut names = Vec::with_capacity(cr.fields.len());
    for (i, f) in cr.fields.iter().enumerate() {
        match &f.node {
            Some(Node::String(s)) => names.push(s.sval.as_str()),
            Some(Node::AStar(_)) if i + 1 == cr.fields.len() => {
                return match names.as_slice() {
                    [] => Fields::Star(None),
                    [q] => Fields::Star(Some((*q).to_owned())),
                    _ => Fields::Other,
                };
            }
            _ => return Fields::Other,
        }
    }
    Fields::Names(names)
}
