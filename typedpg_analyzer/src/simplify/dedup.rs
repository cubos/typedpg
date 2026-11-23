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

use typedpg_pg_query::Equal;
use typedpg_pg_query::ast::{
    self, AConstValue, AExprKind, BoolExprType, JoinType, Node, SetOperation, Token,
};
use typedpg_pg_query::{NodeBox, NodeRef};

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
) -> Vec<(&'a ast::SelectStmt, Candidate)> {
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
    let Some(Node::SelectStmt(top)) = ctx.tree.stmts.first().and_then(|s| s.stmt.as_ref()) else {
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
    s: &ast::SelectStmt,
    usable: Option<&[bool]>,
) -> bool {
    if s.op != SetOperation::SetopNone
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
        let Some(Node::ResTarget(rt)) = target else {
            return false;
        };
        let Some(Node::ColumnRef(cr)) = rt.val.as_ref() else {
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
    let where_facts = conjuncts(s.where_clause.as_ref())
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
pub(super) fn is_plain_distinct(s: &ast::SelectStmt) -> bool {
    matches!(s.distinct_clause.as_slice(), [d] if d.is_none())
}

/// A call that may multiply rows (a set-returning function or operator),
/// collapse them (an aggregate without OVER) or have effects (anything
/// [`super::proof::has_volatile`] flags), anywhere in `s` — or a function
/// this can't resolve. Every overload of the name counts, not just the one
/// PG picks.
pub(super) fn has_unsafe_call(ctx: &Ctx<'_>, s: &ast::SelectStmt) -> bool {
    if super::proof::has_volatile_in(ctx, Some(NodeRef::SelectStmt(s))) {
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
    /// Whether [`Self::columns`] lists every column at its position, the
    /// ones whose names aren't certain under a placeholder no reference
    /// spells (a FROM subquery or CTE without a `*` target). A name it does
    /// list is then that column: PG would reject the reference as ambiguous
    /// if an uncertain name were the same.
    pub(super) positioned: bool,
    /// For a CTE reference, a FROM subquery, a VALUES list or a FROM
    /// function: each column's type and whether its collation is
    /// deterministic, by attnum - 1. Empty for a base table.
    pub(super) cte_columns: Vec<(Option<PgTypeOid>, bool)>,
    /// The item may read the items to its left (a `LATERAL` subquery, a
    /// `FROM` function): its one row is per row of theirs, so it is a fixed
    /// row of the level only once they are.
    pub(super) reads_left: bool,
    /// For a relation with no table of its own (`oid` is `None`): sets of
    /// its output columns, by attnum, no two of its rows share under their
    /// types' default equality — a `DISTINCT ON` subquery's or view's keys
    /// (see [`super::cardinality::distinct_on_output_key`]).
    pub(super) derived_keys: Vec<Vec<i16>>,
}

impl Rel {
    /// The attnum of the column this FROM item calls `name`.
    fn attnum(&self, name: &str) -> Option<i16> {
        if !self.columns_known && !self.positioned {
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
    pub(super) quals: Option<&'a ast::Node>,
    pub(super) using: Vec<&'a str>,
    /// The `using` names whose two sides have the same type under a
    /// deterministic collation: the merged column is then one side's own
    /// value, with no coercion to a common type (PG's `buildMergedJoinVar`).
    pub(super) using_uncoerced: Vec<&'a str>,
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
    /// A relation whose columns aren't all known (an opaque DML item, or a
    /// CTE / subquery with an uncertain output name) may hold the column:
    /// nothing may be proven from it, and it must not fall through to an
    /// outer level, whose per-row value would look fixed.
    Opaque,
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
    /// For a fact on an expression rather than a column (`attnum` is then
    /// `0`): the function called and the attnums of the plain columns of
    /// `rel` it is called with, in order — what an expression index's key
    /// column stores (see [`IndexCall`]).
    call: Option<IndexCall>,
}

/// A call to the routine `proc` whose every argument is a plain column of
/// one relation, by attnum: the one shape of an index expression the
/// cardinality proof matches against a query's.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct IndexCall {
    proc: crate::oid::PgProcOid,
    args: Vec<i16>,
}

/// One side of an equality.
struct Operand {
    typ: PgTypeOid,
    column: Option<(usize, i16)>,
    rels: u64,
    /// The operand is an [`IndexCall`] over relation `.0`'s columns.
    call: Option<(usize, IndexCall)>,
}

impl<'a> Scope<'a> {
    /// The FROM clause's relations, or `None` when it has anything but
    /// base tables the proof can trust and plain joins of them. Every CTE
    /// name in `ctes` makes the proof give up.
    pub(super) fn build(
        ctx: &Ctx<'_>,
        ctes: &HashSet<String>,
        from: &'a [Option<ast::Node>],
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
        from: &'a [Option<ast::Node>],
        outer: &FromScope<'_>,
    ) -> Option<Scope<'a>> {
        Self::build_inner(ctx, env, from, true, outer, KeyScope::Cardinality)
    }

    /// [`Self::build`], also accepting a FROM function that can't return a
    /// set and a FROM subquery as one-row relations (`functions`).
    fn build_inner(
        ctx: &Ctx<'_>,
        env: &CteEnv,
        from: &'a [Option<ast::Node>],
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
            scope.add(ctx, env, item.as_ref(), false, functions, outer)?;
        }
        Some(scope)
    }

    /// The scope of a whole `UPDATE` or `DELETE`: the target relation and
    /// its `FROM` / `USING` items. The cardinality proof's scope (a target
    /// row is written once, so `ONLY` and partitioned targets count).
    ///
    /// A target row is written once however many `FROM` / `USING` rows it
    /// joins, so only the target must be determined: an item this can't
    /// model (a set-returning function, a view, a foreign table, a join with
    /// one) is registered as an opaque relation instead of aborting the
    /// proof. An opaque relation is never determined and its columns are
    /// unknown, so a reference to it proves nothing; references qualified by
    /// the target or by a modelled item keep working.
    pub(super) fn dml(
        ctx: &Ctx<'_>,
        env: &CteEnv,
        relation: &ast::RangeVar,
        from: &'a [Option<ast::Node>],
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
            // `add` may have pushed some of a join's relations before
            // failing on another side: drop them so the item is one opaque
            // relation, not a partial one a reference could bind to.
            let rels = scope.rels.len();
            let joins = scope.joins.len();
            if scope
                .add(ctx, env, item.as_ref(), false, true, &root)
                .is_none()
            {
                scope.rels.truncate(rels);
                scope.joins.truncate(joins);
                scope.add_opaque(item.as_ref());
            }
        }
        Some(scope)
    }

    /// Add one FROM item the proof can't model as a single opaque relation:
    /// never determined, with no known columns, under the refname a
    /// reference can use (`alias`, else a table's or function's name).
    fn add_opaque(&mut self, item: Option<&'a ast::Node>) {
        if self.rels.len() == MAX_RELATIONS {
            return;
        }
        let (refname, aliased, relname) = match item {
            Some(Node::RangeVar(rv)) => {
                let alias = rv.alias.as_ref().map(|a| a.aliasname.clone());
                (
                    alias.clone().unwrap_or_else(|| rv.relname.clone()),
                    alias.is_some(),
                    rv.relname.clone(),
                )
            }
            Some(Node::RangeSubselect(sub)) => {
                let alias = sub.alias.as_ref().map(|a| a.aliasname.clone());
                (
                    alias.clone().unwrap_or_default(),
                    alias.is_some(),
                    alias.unwrap_or_default(),
                )
            }
            Some(Node::RangeFunction(rf)) => {
                let alias = rf.alias.as_ref().map(|a| a.aliasname.clone());
                let func = rf
                    .functions
                    .first()
                    .and_then(|item| match item.as_ref() {
                        Some(Node::List(pair)) => pair.items.first(),
                        _ => None,
                    })
                    .and_then(|item| match item.as_ref() {
                        Some(Node::FuncCall(call)) => string_list(&call.funcname).last().copied(),
                        _ => None,
                    });
                let name = alias.clone().or_else(|| func.map(str::to_owned));
                (
                    name.clone().unwrap_or_default(),
                    alias.is_some(),
                    name.unwrap_or_default(),
                )
            }
            Some(Node::JoinExpr(j)) => {
                let alias = j.alias.as_ref().map(|a| a.aliasname.clone());
                (
                    alias.clone().unwrap_or_default(),
                    alias.is_some(),
                    alias.unwrap_or_default(),
                )
            }
            _ => (String::new(), false, String::new()),
        };
        self.rels.push(Rel {
            oid: None,
            refname,
            aliased,
            schema: String::new(),
            relname,
            nullable: false,
            single_row: false,
            columns: Vec::new(),
            columns_known: false,
            positioned: false,
            cte_columns: Vec::new(),
            reads_left: false,
            derived_keys: Vec::new(),
        });
    }

    /// Add one FROM item; its relations as a bitmask. `functions`: a
    /// function item that can't return a set and a proven one-row `LATERAL`
    /// subquery are one-row relations (otherwise a `FROM` function or
    /// subquery is a reference the proof gives up on).
    fn add(
        &mut self,
        ctx: &Ctx<'_>,
        env: &CteEnv,
        item: Option<&'a ast::Node>,
        nullable: bool,
        functions: bool,
        outer: &FromScope<'_>,
    ) -> Option<u64> {
        match item? {
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
                let kind = j.jointype;
                let (left_nullable, right_nullable) = match kind {
                    JoinType::JoinInner => (nullable, nullable),
                    JoinType::JoinLeft => (nullable, true),
                    JoinType::JoinRight => (true, nullable),
                    JoinType::JoinFull => (true, true),
                    _ => return None,
                };
                let first = self.rels.len();
                let left = self.add(ctx, env, j.larg.as_ref(), left_nullable, functions, outer)?;
                let middle = self.rels.len();
                let right =
                    self.add(ctx, env, j.rarg.as_ref(), right_nullable, functions, outer)?;
                let single = |side: Option<&ast::Node>, at: usize| {
                    matches!(side, Some(Node::RangeVar(_))).then_some(at)
                };
                let using = j
                    .using_clause
                    .iter()
                    .map(|n| match n {
                        Some(Node::String(s)) => Some(s.sval.as_str()),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                let using_uncoerced = using
                    .iter()
                    .copied()
                    .filter(|name| {
                        let info = |mask: u64| {
                            let (r, attnum) = self.merged_column(mask, name)?;
                            self.rels[r].column_info(ctx, attnum)
                        };
                        matches!(
                            (info(left), info(right)),
                            (Some((l, true)), Some((r, true))) if l == r
                        )
                    })
                    .collect();
                self.joins.push(Join {
                    kind,
                    left,
                    right,
                    left_rel: single(j.larg.as_ref(), first),
                    right_rel: single(j.rarg.as_ref(), middle),
                    quals: j.quals.as_ref(),
                    using,
                    using_uncoerced,
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
        rv: &ast::RangeVar,
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
                let Some(Node::String(name)) = rename else {
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
                positioned: true,
                cte_columns,
                reads_left: false,
                derived_keys: Vec::new(),
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
        rf: &'a ast::RangeFunction,
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
            let Some(Node::List(pair)) = item.as_ref() else {
                return None;
            };
            let Some(Node::FuncCall(call)) = pair.items.first().and_then(|n| n.as_ref()) else {
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
            let defs = pair.items.get(1).and_then(|n| n.as_ref());
            match defs {
                Some(Node::List(defs)) => {
                    let mut def_count = 0;
                    for def in &defs.items {
                        let Some(Node::ColumnDef(def)) = def.as_ref() else {
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
                let Some(Node::String(name)) = name else {
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
            positioned: columns_known,
            cte_columns,
            reads_left: !self.rels.is_empty(),
            derived_keys: Vec::new(),
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
    /// one row per evaluation, or its own `LIMIT` / `FETCH FIRST` is the
    /// constant `0` or `1` without `WITH TIES` (see
    /// [`super::cardinality::select_at_most_one_row_in`] and
    /// [`super::cardinality::limit_bounds_one`]).
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
        sub: &'a ast::RangeSubselect,
        nullable: bool,
        outer: &FromScope<'_>,
    ) -> Option<u64> {
        if self.rels.len() == MAX_RELATIONS {
            return None;
        }
        let Some(Node::SelectStmt(select)) = sub.subquery.as_ref() else {
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
            super::cardinality::select_at_most_one_row_in(ctx, select.as_ref(), &scope, &env)
                || super::cardinality::limit_bounds_one(select);
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
            let Some(Node::String(name)) = rename else {
                return None;
            };
            column.0 = name.sval.clone();
        }
        // An alias column list that renames every uncertain target makes
        // the names certain again.
        // (Not past a `*` target: it expands to columns this can't count,
        // so the list may rename only some of them.)
        let has_star = target_list_has_star(select);
        if renames.len() == columns.len() && !has_star {
            columns_known = true;
        }
        if has_star {
            columns_known = false;
        }
        let alias = sub.alias.as_ref().map(|a| a.aliasname.clone());
        let cte_columns = super::cardinality::select_output_columns(ctx, select, &env, parent)
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
            positioned: !has_star,
            cte_columns,
            reads_left: sub.lateral,
            derived_keys: super::cardinality::distinct_on_output_key(ctx, select)
                .into_iter()
                .collect(),
        });
        Some(1 << (self.rels.len() - 1))
    }

    /// Add one base-table reference, or an allowlisted system view's; its
    /// relation as a bitmask.
    fn add_table(&mut self, ctx: &Ctx<'_>, rv: &ast::RangeVar, nullable: bool) -> Option<u64> {
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
        // Any other view is one relation to the cardinality proof only when
        // its body's `DISTINCT ON` keys it (see [`Self::add_view`]).
        if class.relkind == RelKind::View && !trusted_view {
            return match self.mode {
                KeyScope::Cardinality => self.add_view(ctx, rv, class.oid, nullable),
                KeyScope::Strict => None,
            };
        }
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
            let Some(Node::String(name)) = rename else {
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
            positioned: true,
            cte_columns: Vec::new(),
            reads_left: false,
            derived_keys: Vec::new(),
        });
        Some(1 << (self.rels.len() - 1))
    }

    /// Add a reference to a user view whose body is a `SELECT DISTINCT ON`
    /// keyed by output columns (see
    /// [`super::cardinality::distinct_on_output_key`]): a relation with no
    /// table, its columns the view's own (by `pg_attribute`, renamed by an
    /// alias column list), each typed with its collation, and that key as
    /// its one derived key. PG expands the view as a subquery, so its rows
    /// are the body's, at most one per key value. Any other view is a
    /// relation the proof can't see through.
    fn add_view(
        &mut self,
        ctx: &Ctx<'_>,
        rv: &ast::RangeVar,
        view: PgClassOid,
        nullable: bool,
    ) -> Option<u64> {
        let Some(Node::SelectStmt(body)) = ctx.catalog.view_body(view)?.ast.as_ref() else {
            return None;
        };
        let key = super::cardinality::distinct_on_output_key(ctx, body)?;
        let mut attrs: Vec<_> = ctx
            .catalog
            .attributes_of(view)
            .iter()
            .filter(|a| a.attnum > 0)
            .collect();
        attrs.sort_by_key(|a| a.attnum);
        // A view's columns are its body's targets, in order, and a view
        // never drops one.
        if attrs.len() != body.target_list.len()
            || attrs
                .iter()
                .enumerate()
                .any(|(i, a)| usize::try_from(a.attnum).ok() != Some(i + 1))
        {
            return None;
        }
        let mut columns: Vec<(String, i16)> = attrs
            .iter()
            .map(|a| (a.attname.clone(), a.attnum))
            .collect();
        let renames = rv.alias.as_ref().map_or(&[][..], |a| &a.colnames[..]);
        if renames.len() > columns.len() {
            return None;
        }
        for (column, rename) in columns.iter_mut().zip(renames) {
            let Some(Node::String(name)) = rename else {
                return None;
            };
            column.0 = name.sval.clone();
        }
        let cte_columns = attrs
            .iter()
            .map(|a| (Some(a.atttypid), deterministic(ctx.catalog, a.attcollation)))
            .collect();
        let alias = rv.alias.as_ref().map(|a| a.aliasname.clone());
        let class = ctx.catalog.pg_class.get(&view)?;
        self.rels.push(Rel {
            oid: None,
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
            positioned: true,
            cte_columns,
            reads_left: false,
            derived_keys: vec![key],
        });
        Some(1 << (self.rels.len() - 1))
    }

    /// The relation a qualifier names.
    pub(super) fn rel_named(&self, qualifier: &str) -> Option<usize> {
        self.rels.iter().position(|r| r.refname == qualifier)
    }

    /// Resolve a column reference against this scope alone.
    ///
    /// A relation whose output names aren't all certain — an opaque DML
    /// item always — may hold the column a reference spells: an unqualified
    /// reference, or one qualified by that relation, is [`Col::Opaque`]
    /// rather than resolving to another relation (whose column would be a
    /// different one) or to an outer level (whose per-row value looks fixed).
    /// References qualified by a relation whose columns are all known still
    /// resolve as before. A relation that lists every column at its
    /// position ([`Rel::positioned`]) resolves the names it lists: an
    /// uncertain name equal to one of them would make PG reject the
    /// reference as ambiguous, so only a name it doesn't list is opaque.
    pub(super) fn resolve(&self, names: &[&str]) -> Col {
        let any_unknown = || self.rels.iter().any(|rel| !rel.columns_known);
        // Items whose columns aren't even listed by position (an opaque DML
        // item, a `*` subquery): any name may be theirs.
        let any_opaque = || {
            self.rels
                .iter()
                .any(|rel| !rel.columns_known && !rel.positioned)
        };
        let attnum = |r: usize, name: &str| {
            let rel = &self.rels[r];
            if !rel.columns_known && !rel.positioned {
                return Col::Opaque;
            }
            match rel.attnum(name) {
                Some(attnum) => Col::Rel(r, attnum),
                None if rel.columns_known => Col::Other,
                None => Col::Opaque,
            }
        };
        match names {
            [name] => {
                // The name could bind to a relation that doesn't list it.
                if any_opaque() {
                    return Col::Opaque;
                }
                let mut found = self
                    .rels
                    .iter()
                    .enumerate()
                    .filter(|(_, rel)| rel.columns.iter().any(|(n, _)| n == name));
                match (found.next(), found.next()) {
                    (Some((r, _)), None) => attnum(r, name),
                    _ if any_unknown() => Col::Opaque,
                    _ => self.resolve_using(name),
                }
            }
            [qualifier, name] => match self.rel_named(qualifier) {
                Some(r) => attnum(r, name),
                // An opaque item without a refname this lists could still be
                // the one the qualifier names (a join item, a function).
                None if any_opaque() => Col::Opaque,
                None => Col::Other,
            },
            [schema, table, name] => {
                let mut found = self.rels.iter().enumerate().filter(|(_, rel)| {
                    !rel.aliased && rel.schema == *schema && rel.relname == *table
                });
                match (found.next(), found.next()) {
                    (Some((r, _)), None) => attnum(r, name),
                    _ if any_opaque() => Col::Opaque,
                    _ => Col::Other,
                }
            }
            _ => Col::Other,
        }
    }

    /// The relation and attnum an unqualified reference to a join's USING
    /// column names (PG merges the two same-named inputs into one output
    /// column): the left input's for an INNER or LEFT join, the right
    /// input's for a RIGHT join. A FULL join merges them with `COALESCE`,
    /// which is not a plain column, so it gives up. `Other` when the name
    /// isn't a USING column or the side doesn't have exactly one.
    fn resolve_using(&self, name: &str) -> Col {
        for join in self.joins.iter().rev() {
            if !join.using.contains(&name) {
                continue;
            }
            let side = match join.kind {
                JoinType::JoinInner | JoinType::JoinLeft => join.left,
                JoinType::JoinRight => join.right,
                _ => return Col::Other,
            };
            // The merged column is that side's own value only when neither
            // side needs a coercion to the common type: with a `bigint` and
            // a `double precision` side it is a converted value, which can't
            // stand for the column.
            if !join.using_uncoerced.contains(&name) {
                return Col::Other;
            }
            // A relation outside the join with the same column makes the
            // name another binding (in another join's `ON`, which sees only
            // its own inputs) or ambiguous: not this merged column.
            let inside = join.left | join.right;
            if (0..self.rels.len()).any(|r| {
                inside & (1u64 << r) == 0 && self.rels[r].columns.iter().any(|(n, _)| n == name)
            }) {
                return Col::Other;
            }
            return self
                .merged_column(side, name)
                .map_or(Col::Other, |(r, attnum)| Col::Rel(r, attnum));
        }
        Col::Other
    }

    /// The column of one relation that the join input `mask` outputs as
    /// `name`: the one relation of `mask` that has it or, when `mask` is a
    /// join whose USING merges `name` (its two inputs both have it), that
    /// join's merged column — PG's `buildMergedJoinVar` takes the left
    /// input's for an INNER or LEFT join, the right input's for a RIGHT
    /// join, recursively down a chain of USING joins. A FULL join merges
    /// with `COALESCE`, and a merge needing a coercion to a common type
    /// yields a converted value: neither is a column. PG rejects a USING
    /// name an input outputs twice, so another input holding it is not a
    /// case to resolve.
    fn merged_column(&self, mask: u64, name: &str) -> Option<(usize, i16)> {
        let mut found = (0..self.rels.len())
            .filter(|&r| mask & (1u64 << r) != 0)
            .filter(|&r| self.rels[r].columns.iter().any(|(n, _)| n == name));
        match (found.next(), found.next()) {
            (Some(r), None) => Some((r, self.rels[r].attnum(name)?)),
            (Some(_), Some(_)) => {
                let join = self.joins.iter().find(|join| join.rels() == mask)?;
                if !join.using_uncoerced.contains(&name) {
                    return None;
                }
                let side = match join.kind {
                    JoinType::JoinInner | JoinType::JoinLeft => join.left,
                    JoinType::JoinRight => join.right,
                    _ => return None,
                };
                self.merged_column(side, name)
            }
            _ => None,
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
    /// function or operator every argument of which is one of these, and a
    /// `CASE`, `COALESCE`, `IS [NOT] NULL`, boolean test or `AND` / `OR` /
    /// `NOT` of them — each one value per evaluation of the row (see
    /// [`Self::call_operand`]).
    fn operand(
        &self,
        ctx: &Ctx<'_>,
        expr: Option<&ast::Node>,
        outer: Option<&FromScope<'_>>,
        scalar_sublinks: bool,
        env: Option<&CteEnv>,
    ) -> Option<Operand> {
        match expr? {
            Node::ColumnRef(cr) => {
                let Fields::Names(names) = column_ref_fields(cr) else {
                    return None;
                };
                // A relation whose output names aren't all certain may hold
                // the column an unqualified reference spells. Reading it as
                // an outer level's would make its per-row value look fixed,
                // so no key fact is read from such a scope.
                if names.len() == 1
                    && self
                        .rels
                        .iter()
                        .any(|rel| !rel.columns_known && !rel.positioned)
                {
                    return None;
                }
                let (rel, attnum, (typ, deterministic_collation)) = match self.resolve(&names) {
                    Col::Rel(r, attnum) => {
                        (Some(r), attnum, self.rels[r].column_info(ctx, attnum)?)
                    }
                    // A relation whose output names aren't all certain may
                    // hold the column this reference spells. Reading it as
                    // an outer level's would make its per-row value look
                    // fixed, so no key fact is read.
                    Col::Opaque => return None,
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
                    call: None,
                })
            }
            Node::SubLink(sub) if scalar_sublinks => {
                self.scalar_sublink_operand(ctx, sub, outer, env)
            }
            // A spread's placeholder stands for each of its items in turn,
            // no single value.
            Node::ParamRef(p) if super::cardinality::is_spread_param(ctx, p.number) => None,
            Node::ParamRef(p) => {
                let index = usize::try_from(p.number).ok()?.checked_sub(1)?;
                let param = ctx.analyzed.params.get(index)?;
                deterministic_type_collation(&param.pg_type).then_some(())?;
                Some(Operand {
                    typ: type_oid(ctx, &param.pg_type)?,
                    column: None,
                    rels: 0,
                    call: None,
                })
            }
            Node::AConst(c) if !c.isnull => Some(Operand {
                typ: match c.val.as_ref()? {
                    AConstValue::Integer(_) => oid::INT4,
                    AConstValue::String(_) => oid::UNKNOWN,
                    AConstValue::Boolean(_) => oid::BOOL,
                    _ => return None,
                },
                column: None,
                rels: 0,
                call: None,
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
                let arg = self.operand(ctx, cast.arg.as_ref(), outer, scalar_sublinks, env)?;
                let typ = type_name_oid(ctx, type_name)?;
                // A conversion that runs a volatile function could yield
                // another value each time the row pair is evaluated.
                let deterministic_collation = deterministic_type(ctx, typ);
                (deterministic_collation && coercion_is_nonvolatile(ctx, arg.typ, typ)).then_some(
                    Operand {
                        typ,
                        column: None,
                        rels: arg.rels,
                        call: None,
                    },
                )
            }
            Node::FuncCall(call) if scalar_sublinks => {
                self.call_operand(ctx, call, outer, scalar_sublinks, env)
            }
            Node::AExpr(expr) if scalar_sublinks && expr.kind == AExprKind::AexprOp => {
                self.operator_operand(ctx, expr, outer, scalar_sublinks, env)
            }
            // A test of determined values is one value too: `IS [NOT] NULL`,
            // `IS [NOT] TRUE` / `FALSE` / `UNKNOWN`, and `AND` / `OR` /
            // `NOT` of boolean operands (none of them calls a function).
            Node::NullTest(test) if scalar_sublinks => {
                let arg = self.operand(ctx, test.arg.as_ref(), outer, scalar_sublinks, env)?;
                Some(Operand {
                    typ: oid::BOOL,
                    column: None,
                    rels: arg.rels,
                    call: None,
                })
            }
            Node::BooleanTest(test) if scalar_sublinks => {
                let arg = self.operand(ctx, test.arg.as_ref(), outer, scalar_sublinks, env)?;
                (arg.typ == oid::BOOL).then_some(Operand {
                    typ: oid::BOOL,
                    column: None,
                    rels: arg.rels,
                    call: None,
                })
            }
            Node::BoolExpr(expr) if scalar_sublinks => {
                let mut rels = 0;
                for arg in &expr.args {
                    let arg = self.operand(ctx, arg.as_ref(), outer, scalar_sublinks, env)?;
                    (arg.typ == oid::BOOL).then_some(())?;
                    rels |= arg.rels;
                }
                Some(Operand {
                    typ: oid::BOOL,
                    column: None,
                    rels,
                    call: None,
                })
            }
            Node::CoalesceExpr(expr) if scalar_sublinks => {
                let mut values = Vec::with_capacity(expr.args.len());
                for arg in &expr.args {
                    values.push(self.branch_operand(ctx, arg.as_ref(), outer, env)?);
                }
                self.common_operand(ctx, &values, 0)
            }
            Node::CaseExpr(case) if scalar_sublinks => self.case_operand(ctx, case, outer, env),
            _ => None,
        }
    }

    /// A `CASE` / `COALESCE` branch's value: an [`Self::operand`], or a
    /// `NULL` constant (an `unknown` value, until the construct's common
    /// type resolves it).
    fn branch_operand(
        &self,
        ctx: &Ctx<'_>,
        expr: Option<&ast::Node>,
        outer: Option<&FromScope<'_>>,
        env: Option<&CteEnv>,
    ) -> Option<Operand> {
        match expr {
            None => Some(Operand {
                typ: oid::UNKNOWN,
                column: None,
                rels: 0,
                call: None,
            }),
            Some(Node::AConst(c)) if c.isnull => Some(Operand {
                typ: oid::UNKNOWN,
                column: None,
                rels: 0,
                call: None,
            }),
            _ => self.operand(ctx, expr, outer, true, env),
        }
    }

    /// The value of a construct choosing among `values` (a `CASE`'s results,
    /// a `COALESCE`'s arguments), each converted to their common type the
    /// way PG's `select_common_type` picks it (in PG's order: a `CASE` lists
    /// its `ELSE` first), when no conversion runs a volatile function and
    /// the type's equality is deterministic. `rels` adds what decides the
    /// choice (a `CASE`'s conditions) to what the values read.
    fn common_operand(&self, ctx: &Ctx<'_>, values: &[Operand], rels: u64) -> Option<Operand> {
        let types: Vec<PgTypeOid> = values.iter().map(|value| value.typ).collect();
        let typ = crate::coerce::select_common_type_from_oids(&types, ctx.catalog)?;
        if !crate::coerce::verify_common_type_from_oids(typ, &types, ctx.catalog)
            || !values
                .iter()
                .all(|value| coercion_is_nonvolatile(ctx, value.typ, typ))
            || !deterministic_type(ctx, typ)
        {
            return None;
        }
        Some(Operand {
            typ,
            column: None,
            rels: values.iter().fold(rels, |rels, value| rels | value.rels),
            call: None,
        })
    }

    /// A `CASE`'s one value per evaluation of the row: every condition and
    /// result an operand (a searched `CASE`'s conditions boolean, a simple
    /// one's tests a non-volatile `=` between its argument and each `WHEN`
    /// value, as PG's `transformCaseExpr` builds them), joined by
    /// [`Self::common_operand`]. A missing `ELSE` is `NULL`.
    fn case_operand(
        &self,
        ctx: &Ctx<'_>,
        case: &ast::CaseExpr,
        outer: Option<&FromScope<'_>>,
        env: Option<&CteEnv>,
    ) -> Option<Operand> {
        let arg = match &case.arg {
            Some(arg) => Some(self.operand(ctx, Some(arg), outer, true, env)?),
            None => None,
        };
        let mut rels = arg.as_ref().map_or(0, |arg| arg.rels);
        // PG puts the default result first: it weighs most in the choice of
        // the common type.
        let mut values = vec![self.branch_operand(ctx, case.defresult.as_ref(), outer, env)?];
        for when in &case.args {
            let Some(Node::CaseWhen(when)) = when else {
                return None;
            };
            let condition = self.operand(ctx, when.expr.as_ref(), outer, true, env)?;
            match &arg {
                Some(arg) => {
                    let op = ctx
                        .catalog
                        .find_operator("=", Some(arg.typ), condition.typ)?;
                    let proc = op.code.and_then(|code| ctx.catalog.pg_proc.get(&code))?;
                    if proc.provolatile == ProVolatile::Volatile
                        || proc.proretset
                        || proc.prokind != ProKind::Function
                        || op.result_type_oid != oid::BOOL
                        || !op
                            .left_type_oid
                            .is_some_and(|target| coercion_is_nonvolatile(ctx, arg.typ, target))
                        || !coercion_is_nonvolatile(ctx, condition.typ, op.right_type_oid)
                    {
                        return None;
                    }
                }
                None if condition.typ == oid::BOOL => {}
                None => return None,
            }
            rels |= condition.rels;
            values.push(self.branch_operand(ctx, when.result.as_ref(), outer, env)?);
        }
        self.common_operand(ctx, &values, rels)
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
        call: &ast::FuncCall,
        outer: Option<&FromScope<'_>>,
        scalar_sublinks: bool,
        env: Option<&CteEnv>,
    ) -> Option<Operand> {
        let (schema, name, notation) = call_syntax(call)?;
        let mut operands = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            operands.push(self.operand(
                ctx,
                crate::functions::call_arg_value(arg.as_ref()),
                outer,
                scalar_sublinks,
                env,
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
        // A call whose every argument is a plain column of one relation is
        // also what an expression index over that relation may store.
        let call = operands
            .first()
            .and_then(|first| first.column)
            .and_then(|(rel, _)| {
                let args = operands
                    .iter()
                    .map(|operand| match operand.column {
                        Some((r, attnum)) if r == rel => Some(attnum),
                        _ => None,
                    })
                    .collect::<Option<Vec<i16>>>()?;
                Some((
                    rel,
                    IndexCall {
                        proc: resolved.oid,
                        args,
                    },
                ))
            });
        deterministic_type(ctx, resolved.return_type_oid).then_some(Operand {
            typ: resolved.return_type_oid,
            column: None,
            rels: operands.iter().fold(0, |rels, operand| rels | operand.rels),
            call,
        })
    }

    /// An operator expression's one value per evaluation: [`Self::call_operand`]
    /// for the function behind the operator.
    fn operator_operand(
        &self,
        ctx: &Ctx<'_>,
        expr: &ast::AExpr,
        outer: Option<&FromScope<'_>>,
        scalar_sublinks: bool,
        env: Option<&CteEnv>,
    ) -> Option<Operand> {
        let name = string_list(&expr.name).join(".");
        let left = match &expr.lexpr {
            left @ Some(_) => {
                Some(self.operand(ctx, left.as_ref(), outer, scalar_sublinks, env)?)
            }
            None => None,
        };
        let right = self.operand(ctx, expr.rexpr.as_ref(), outer, scalar_sublinks, env)?;
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
            call: None,
        })
    }

    /// The one value a scalar sublink yields per evaluation, when it does
    /// not read a column of this level: it may read outer levels and its
    /// own, and a volatile function inside it runs once with it. Its type is
    /// that of the bare column of its own `FROM` its target list selects, or
    /// of the aggregate it selects; anything else is left to the proof's
    /// give-up answer.
    ///
    /// `visible` is the CTE environment of the level the sublink is written
    /// in, so a `WITH` CTE is judged as it is there (its own `WITH`, if any,
    /// shadows it). The `DISTINCT` proof passes none: every CTE name stays a
    /// reference it gives up on.
    fn scalar_sublink_operand(
        &self,
        ctx: &Ctx<'_>,
        sub: &ast::SubLink,
        outer: Option<&FromScope<'_>>,
        visible: Option<&CteEnv>,
    ) -> Option<Operand> {
        if sub.sub_link_type != ast::SubLinkType::ExprSublink {
            return None;
        }
        self.sublink_value(ctx, sub, outer, visible, false)
    }

    /// The one value the subquery of `x IN (SELECT …)` / `x = ANY (SELECT
    /// …)` compares `x` with, when the subquery yields at most one row per
    /// evaluation ([`super::cardinality::select_at_most_one_row_in`], or its
    /// own `LIMIT 0` / `1`) and is otherwise a value
    /// [`Self::scalar_sublink_operand`] reads. With at most one row, the
    /// membership test is the equality `x = value` (or matches nothing).
    /// Only the cardinality proof asks (`visible` is its CTE environment).
    fn in_sublink_operand(
        &self,
        ctx: &Ctx<'_>,
        sub: &ast::SubLink,
        outer: Option<&FromScope<'_>>,
        visible: &CteEnv,
    ) -> Option<Operand> {
        if sub.sub_link_type != ast::SubLinkType::AnySublink {
            return None;
        }
        self.sublink_value(ctx, sub, outer, Some(visible), true)
    }

    /// [`Self::scalar_sublink_operand`]'s value of a sublink's subquery,
    /// also proving the subquery at most one row when `one_row` is set.
    fn sublink_value(
        &self,
        ctx: &Ctx<'_>,
        sub: &ast::SubLink,
        outer: Option<&FromScope<'_>>,
        visible: Option<&CteEnv>,
        one_row: bool,
    ) -> Option<Operand> {
        let Some(Node::SelectStmt(select)) = sub.subselect.as_ref() else {
            return None;
        };
        if select.op != SetOperation::SetopNone || !select.values_lists.is_empty() {
            return None;
        }
        let [target] = select.target_list.as_slice() else {
            return None;
        };
        let Some(Node::ResTarget(target)) = target else {
            return None;
        };
        let value = target.val.as_ref()?;
        // A sublink is a query level of its own (and may read levels above
        // this one). Its own `WITH` shadows the enclosing level's CTE names.
        // A sublink sees this whole level, then the levels around it.
        let opaque = FromScope::opaque(None);
        let (env, nested) = match visible {
            Some(visible) => {
                let nested = outer.unwrap_or(&opaque);
                let env = super::cardinality::subquery_cte_env(ctx, visible, select, nested);
                (env, nested)
            }
            None => (CteEnv::blocked(&cte_names(ctx.tree)), &opaque),
        };
        let scope = Scope::build_env(ctx, &env, &select.from_clause, nested)?;
        if !self.sublink_uncorrelated(&scope, sub) {
            return None;
        }
        if one_row {
            let own = FromScope::of_select(ctx, select, Some(nested));
            if !(super::cardinality::select_at_most_one_row_in(ctx, select, &own, &env)
                || super::cardinality::limit_bounds_one(select))
            {
                return None;
            }
        }
        let (typ, deterministic_collation) = match value {
            Node::ColumnRef(column) => {
                let Fields::Names(names) = column_ref_fields(column) else {
                    return None;
                };
                let Col::Rel(r, attnum) = scope.resolve(&names) else {
                    return None;
                };
                scope.rels[r].column_info(ctx, attnum)?
            }
            Node::FuncCall(call) => (scope.sublink_aggregate_type(ctx, call, nested, &env)?, true),
            _ => return None,
        };
        deterministic_collation.then_some(Operand {
            typ,
            column: None,
            rels: 0,
            call: None,
        })
    }

    /// The return type of the aggregate call `call` in a scalar sublink's
    /// own level, when the catalog resolves it and its type is not a
    /// polymorphic pseudo-type left unresolved.
    fn sublink_aggregate_type(
        &self,
        ctx: &Ctx<'_>,
        call: &ast::FuncCall,
        outer: &FromScope<'_>,
        env: &CteEnv,
    ) -> Option<PgTypeOid> {
        if call.over.is_some() {
            return None;
        }
        let (schema, name) = function_name(&call.funcname)?;
        let notation = crate::functions::CallNotation::of(call).ok()?;
        let mut types = Vec::with_capacity(call.args.len());
        for arg in &call.args {
            let operand = self.operand(
                ctx,
                crate::functions::call_arg_value(arg.as_ref()),
                Some(outer),
                true,
                Some(env),
            )?;
            types.push(operand.typ);
        }
        let resolved = crate::functions::resolve_function(
            ctx.catalog,
            schema,
            name,
            &types,
            &notation,
            call.agg_star,
            None,
        )
        .ok()?;
        if !resolved.is_aggregate
            || resolved.is_window
            || resolved.is_set_returning
            || crate::polymorphic::is_polymorphic(resolved.return_type_oid)
        {
            return None;
        }
        deterministic_type(ctx, resolved.return_type_oid).then_some(resolved.return_type_oid)
    }

    /// Whether nothing in the sublink's subquery reads a column of this
    /// level, and the subquery is a single plain `SELECT` (no `WITH`, no
    /// `VALUES`, no set operation). `local` is the subquery's own scope.
    ///
    /// PG binds a column reference to the innermost level that has it. A
    /// reference this level could hold ([`Self::may_reference`]) is safe
    /// only where the subquery's own FROM items are all visible and `local`
    /// resolves it: in the subquery's own expressions — its target list,
    /// WHERE, GROUP BY, HAVING, ORDER BY, … — and in any query nested in
    /// them (a nested level that also has the name binds it first, which is
    /// no closer to this one). Inside the subquery's FROM clause a join's ON
    /// condition sees only its own two sides and a FROM subquery or function
    /// only the items to its left (or none), so there a name `local` lists
    /// may still fall through to this level: any reference this level could
    /// hold is refused. `false` when unsure.
    fn sublink_uncorrelated(&self, local: &Scope<'_>, sub: &ast::SubLink) -> bool {
        let Some(root) = sub.subselect.as_ref() else {
            return false;
        };
        let NodeRef::SelectStmt(select) = root.to_ref() else {
            return false;
        };
        if select.op != SetOperation::SetopNone
            || select.with_clause.is_some()
            || !select.values_lists.is_empty()
        {
            return false;
        }
        // (node, whether every item of the subquery's own FROM is visible)
        let mut stack: Vec<(NodeRef<'_>, bool)> = Vec::new();
        for list in [
            &select.target_list,
            &select.group_clause,
            &select.window_clause,
            &select.sort_clause,
            &select.distinct_clause,
        ] {
            stack.extend(list.iter().flatten().map(|node| (node.to_ref(), true)));
        }
        for node in [
            &select.where_clause,
            &select.having_clause,
            &select.limit_count,
            &select.limit_offset,
        ]
        .into_iter()
        .flatten()
        {
            stack.push((node.to_ref(), true));
        }
        stack.extend(
            select
                .from_clause
                .iter()
                .flatten()
                .map(|node| (node.to_ref(), false)),
        );
        while let Some((node, visible)) = stack.pop() {
            if let NodeRef::ColumnRef(column) = node {
                let fields = column_ref_fields(column);
                // A `*` expands the FROM items of the level it is written
                // in, never an outer one; `q.*` reads the relation `q`.
                let (names, star): (Vec<&str>, bool) = match &fields {
                    Fields::Names(names) => (names.clone(), false),
                    Fields::Star(None) => (Vec::new(), true),
                    Fields::Star(Some(qualifier)) => (vec![qualifier.as_str()], true),
                    Fields::Other => return false,
                };
                if !names.is_empty() && self.may_reference(&names) {
                    let resolved_locally = visible
                        && if star {
                            local.rel_named(names[0]).is_some()
                        } else {
                            match local.resolve(&names) {
                                Col::Rel(..) => true,
                                Col::Opaque => false,
                                Col::Other => match names.as_slice() {
                                    [name] => self.local_whole_row(local, name),
                                    _ => false,
                                },
                            }
                        };
                    if !resolved_locally {
                        return false;
                    }
                }
            }
            stack.extend(node.children().map(|child| (child, visible)));
        }
        true
    }

    /// Whether the bare name `name`, written where the sublink's own FROM
    /// items are visible, is a whole-row reference to one of them rather
    /// than anything of this level. PG's `transformColumnRef` reads a
    /// one-field reference first as a column, searching every level from
    /// the innermost out (`colNameToVar`), and only when no level has such
    /// a column as a range variable's whole row (`refnameNamespaceItem`,
    /// innermost first). So the name is the local relation's row when the
    /// subquery has a relation by that refname and neither the subquery's
    /// relations nor this level's have a column by that name (a system
    /// column included); a farther level's column would not be this
    /// level's either. Every relation's columns must be known.
    fn local_whole_row(&self, local: &Scope<'_>, name: &str) -> bool {
        let no_column = |rels: &[Rel]| {
            rels.iter()
                .all(|rel| rel.columns_known && !rel.columns.iter().any(|(n, _)| n == name))
        };
        local.rel_named(name).is_some()
            && !super::proof::is_system_column(name)
            && no_column(&local.rels)
            && no_column(&self.rels)
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
        conjunct: Option<&ast::Node>,
        outer: Option<&FromScope<'_>>,
    ) -> Vec<EqFact> {
        self.facts_from_conjunct(ctx, conjunct, outer, false, None)
    }

    /// [`Self::equality_facts`], the cardinality proof's wider form: a scalar
    /// sublink uncorrelated with this level counts as a value operand, and a
    /// value a volatile cast decides does not. The `DISTINCT` proof keeps
    /// the narrow form. `env` is the level's visible CTE environment, which
    /// the sublink's own body is judged in.
    pub(super) fn cardinality_facts(
        &self,
        ctx: &Ctx<'_>,
        conjunct: Option<&ast::Node>,
        outer: Option<&FromScope<'_>>,
        env: &CteEnv,
    ) -> Vec<EqFact> {
        self.facts_from_conjunct(ctx, conjunct, outer, true, Some(env))
    }

    fn facts_from_conjunct(
        &self,
        ctx: &Ctx<'_>,
        conjunct: Option<&ast::Node>,
        outer: Option<&FromScope<'_>>,
        cardinality: bool,
        env: Option<&CteEnv>,
    ) -> Vec<EqFact> {
        // `x IN (SELECT v …)` / `x = ANY (SELECT v …)` over a subquery of at
        // most one row is `x = v` (see [`Self::in_sublink_operand`]).
        if let Some(Node::SubLink(sub)) = conjunct
            && cardinality
            && let Some(env) = env
        {
            let name = match string_list(&sub.oper_name).as_slice() {
                // `IN` leaves the operator unnamed: it is `=`.
                [] | ["="] => "=",
                ["pg_catalog", "="] => "pg_catalog.=",
                _ => return Vec::new(),
            };
            if matches!(sub.testexpr.as_ref(), Some(Node::RowExpr(_))) {
                return Vec::new();
            }
            let (Some(l), Some(r)) = (
                self.operand(ctx, sub.testexpr.as_ref(), outer, true, Some(env)),
                self.in_sublink_operand(ctx, sub, outer, env),
            ) else {
                return Vec::new();
            };
            return self.facts_of(ctx, name, &l, &r, true);
        }
        let Some(Node::AExpr(e)) = conjunct else {
            return Vec::new();
        };
        let name = match string_list(&e.name).as_slice() {
            ["="] => "=",
            ["pg_catalog", "="] => "pg_catalog.=",
            _ => return Vec::new(),
        };
        let (l, r) = (&e.lexpr, &e.rexpr);
        if l.is_none() || r.is_none() {
            return Vec::new();
        }
        let operand = |expr: Option<&ast::Node>| self.operand(ctx, expr, outer, cardinality, env);
        match e.kind {
            AExprKind::AexprOp => {
                // PG's `make_row_comparison_op` turns a row comparison into
                // the AND of its columns' equalities. Only `=` is read, and
                // both sides must be rows of the same arity.
                if let (Some(Node::RowExpr(left)), Some(Node::RowExpr(right))) =
                    (l.as_ref(), r.as_ref())
                {
                    if left.args.is_empty() || left.args.len() != right.args.len() {
                        return Vec::new();
                    }
                    let mut out = Vec::new();
                    for (left, right) in left.args.iter().zip(&right.args) {
                        let (Some(left), Some(right)) =
                            (operand(left.as_ref()), operand(right.as_ref()))
                        else {
                            return Vec::new();
                        };
                        out.extend(self.facts_of(ctx, name, &left, &right, cardinality));
                    }
                    return out;
                }
                let (Some(l), Some(r)) = (operand(l.as_ref()), operand(r.as_ref())) else {
                    return Vec::new();
                };
                self.facts_of(ctx, name, &l, &r, cardinality)
            }
            // `x IN (v)` with one element and `x = ANY(ARRAY[v])` with a
            // one-element array constructor are the equality `x = v`.
            AExprKind::AexprIn => {
                let Some(Node::List(list)) = r.as_ref() else {
                    return Vec::new();
                };
                let [item] = list.items.as_slice() else {
                    return Vec::new();
                };
                let (Some(l), Some(r)) = (operand(l.as_ref()), operand(item.as_ref())) else {
                    return Vec::new();
                };
                self.facts_of(ctx, name, &l, &r, cardinality)
            }
            AExprKind::AexprOpAny => {
                let Some(Node::AArrayExpr(array)) = r.as_ref() else {
                    return Vec::new();
                };
                let [item] = array.elements.as_slice() else {
                    return Vec::new();
                };
                let (Some(l), Some(r)) = (operand(l.as_ref()), operand(item.as_ref())) else {
                    return Vec::new();
                };
                // The array constructor resolves its element's type first
                // (`ARRAY['1']` is `text[]`), so the operator PG picks is
                // not the one an unknown literal would get on its own.
                if r.typ == oid::UNKNOWN {
                    return Vec::new();
                }
                self.facts_of(ctx, name, &l, &r, cardinality)
            }
            _ => Vec::new(),
        }
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
            let (r, attnum) = self.merged_column(mask, name)?;
            let (typ, deterministic_collation) = self.rels[r].column_info(ctx, attnum)?;
            deterministic_collation.then_some(Operand {
                typ,
                column: Some((r, attnum)),
                rels: 1 << r,
                call: None,
            })
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
        let right_declared = || ctx.catalog.pg_operator.get(&op.oid).map(|o| o.oprright);
        if let Some((rel, attnum)) = l.column
            && r.rels & (1 << rel) == 0
        {
            out.push(EqFact {
                rel,
                attnum,
                op: op.oid,
                declared: op.declared_left_type_oid,
                other: r.rels,
                call: None,
            });
        }
        if let Some((rel, attnum)) = r.column
            && l.rels & (1 << rel) == 0
        {
            out.push(EqFact {
                rel,
                attnum,
                op: op.oid,
                declared: right_declared(),
                other: l.rels,
                call: None,
            });
        }
        // An expression index key: `f(t.a) = value`, attnum `0`.
        if let Some((rel, call)) = &l.call
            && r.rels & (1 << rel) == 0
        {
            out.push(EqFact {
                rel: *rel,
                attnum: 0,
                op: op.oid,
                declared: op.declared_left_type_oid,
                other: r.rels,
                call: Some(call.clone()),
            });
        }
        if let Some((rel, call)) = &r.call
            && l.rels & (1 << rel) == 0
        {
            out.push(EqFact {
                rel: *rel,
                attnum: 0,
                op: op.oid,
                declared: right_declared(),
                other: l.rels,
                call: Some(call.clone()),
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
    call: &ast::FuncCall,
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
pub(super) fn call_spells_every_parameter(ctx: &Ctx<'_>, call: &ast::FuncCall) -> bool {
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
    use crate::coerce::{CoercionContext, CoercionPath, coercion_pathway};
    match coercion_pathway(target, source, CoercionContext::Explicit, ctx.catalog) {
        Some(CoercionPath::Relabel) => true,
        Some(CoercionPath::Func) => ctx
            .catalog
            .cast_by_pair
            .get(&(
                ctx.catalog.unwrap_domain(source),
                ctx.catalog.unwrap_domain(target),
            ))
            .and_then(|oid| ctx.catalog.pg_cast.get(oid))
            .and_then(|cast| cast.castfunc)
            .and_then(|func| ctx.catalog.pg_proc.get(&func))
            .is_some_and(|proc| proc.provolatile != ProVolatile::Volatile),
        // The source type's output function, then the target's input
        // function. The catalog doesn't keep a type's I/O functions, so
        // only types whose functions are PG's own are trusted: a built-in
        // type's (none of PG's I/O functions is volatile) and an enum's
        // (`enum_in` / `enum_out`, stable). A user base type may declare a
        // volatile one, and a user composite, array or range type's reads
        // its elements' through theirs.
        Some(CoercionPath::CoerceViaIo) => {
            io_functions_nonvolatile(ctx, source) && io_functions_nonvolatile(ctx, target)
        }
        Some(CoercionPath::ArrayCoerce) | None => false,
    }
}

/// Whether `typ`'s input and output functions are PG's own non-volatile
/// ones: a built-in type's, or an enum's (see [`coercion_is_nonvolatile`]).
/// A domain reads its base type's.
fn io_functions_nonvolatile(ctx: &Ctx<'_>, typ: PgTypeOid) -> bool {
    let typ = ctx.catalog.unwrap_domain(typ);
    typ.get() < FIRST_NORMAL_OBJECT_ID
        || ctx
            .catalog
            .pg_type
            .get(&typ)
            .is_some_and(|ty| ty.typtype == crate::pg_catalog::TypType::Enum)
}

/// The conjuncts a set of equality facts was read from, for matching a
/// partial index's predicate: WHERE conjuncts for WHERE facts, a join's ON
/// conjuncts for that join's facts.
pub(super) struct PredicateSources<'a> {
    pub(super) conjuncts: &'a [Option<&'a ast::Node>],
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
        let Some(oid) = self.rels[b].oid else {
            return self.derived_key_equated(ctx, b, facts, allowed);
        };
        // Expression keys are matched only by the cardinality proof's call
        // facts; the `DISTINCT` proof never records one.
        let keys = match self.mode {
            KeyScope::Cardinality => unique_keys_with_expressions(ctx, oid),
            KeyScope::Strict => unique_keys(ctx, oid),
        };
        keys.iter().find_map(|key| {
            if key.predicate.is_some() {
                let sources = sources?;
                if !self.predicate_implied(ctx, b, key.predicate.as_ref(), sources) {
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
                        && f.call == c.expr
                        && f.other & !allowed == 0
                        && operand_type_matches(ctx, c.typ, f.declared)
                        && in_btree_equality(ctx, &c.family, f.op)
                })?;
                det |= fact.other;
            }
            Some(det)
        })
    }

    /// [`Self::key_equated_with`] for a relation with no table of its own:
    /// one of its derived keys ([`Rel::derived_keys`]) whose every column
    /// `facts` equate to values read from relations within `allowed`. A
    /// derived key is distinct under each column type's default equality
    /// (what `DISTINCT ON` compares with), so the fact's operator must be
    /// that btree family's equality, on a deterministic collation, and one
    /// that equates no two values the default equality tells apart (see
    /// [`grouping_equality`]).
    fn derived_key_equated(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        facts: &[EqFact],
        allowed: u64,
    ) -> Option<u64> {
        if self.mode != KeyScope::Cardinality {
            return None;
        }
        let rel = &self.rels[b];
        rel.derived_keys.iter().find_map(|key| {
            if key.is_empty() {
                return None;
            }
            let mut det = 0u64;
            for &attnum in key {
                let (typ, deterministic_collation) = rel.column_info(ctx, attnum)?;
                if !deterministic_collation {
                    return None;
                }
                let family = default_btree_family(ctx.catalog, typ)?;
                let fact = facts.iter().find(|f| {
                    f.rel == b
                        && f.attnum == attnum
                        && f.call.is_none()
                        && f.other & !allowed == 0
                        && operand_type_matches(ctx, typ, f.declared)
                        && in_btree_equality(ctx, &family, f.op)
                        && grouping_equality(ctx, f.op)
                })?;
                det |= fact.other;
            }
            Some(det)
        })
    }

    /// Whether relation `rel`'s column `attnum` is equated by `facts` to a
    /// value read only from relations in `known` — a constant, a parameter,
    /// an outer column or a determined relation's column — through the
    /// column type's default btree equality, under a deterministic
    /// collation.
    pub(super) fn column_fixed_by_facts(
        &self,
        ctx: &Ctx<'_>,
        rel: usize,
        attnum: i16,
        facts: &[EqFact],
        known: u64,
    ) -> bool {
        let Some((typ, deterministic_collation)) = self.rels[rel].column_info(ctx, attnum) else {
            return false;
        };
        if !deterministic_collation {
            return false;
        }
        let Some(family) = default_btree_family(ctx.catalog, typ) else {
            return false;
        };
        facts.iter().any(|fact| {
            fact.rel == rel
                && fact.attnum == attnum
                && fact.other & !known == 0
                && operand_type_matches(ctx, typ, fact.declared)
                && in_btree_equality(ctx, &family, fact.op)
                && grouping_equality(ctx, fact.op)
        })
    }

    /// Whether every conjunct of a partial index's `predicate` holds on
    /// every row `sources` describe.
    fn predicate_implied(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        predicate: Option<&ast::Node>,
        sources: &PredicateSources<'_>,
    ) -> bool {
        let conjuncts = conjuncts(predicate);
        !conjuncts.is_empty()
            && conjuncts
                .iter()
                .all(|predicate| self.predicate_conjunct_implied(ctx, b, *predicate, sources))
    }

    fn predicate_conjunct_implied(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        predicate: Option<&ast::Node>,
        sources: &PredicateSources<'_>,
    ) -> bool {
        let Some(predicate) = self.canonical_predicate(ctx, b, predicate) else {
            return false;
        };
        // A bare boolean column's predicate is also implied by the
        // comparisons that force its value: `col = true`, `true = col` and
        // `col IS TRUE` for `col`; `col = false`, `false = col`,
        // `col IS FALSE` and `NOT col` for `NOT col`.
        let boolean_fact = match &predicate {
            Node::Var(var) => i16::try_from(var.varattno)
                .ok()
                .map(|attnum| (attnum, true)),
            Node::BoolExpr(expr)
                if expr.boolop == BoolExprType::NotExpr && expr.args.len() == 1 =>
            {
                canonical_var_attnum(expr.args[0].as_ref()).map(|attnum| (attnum, false))
            }
            _ => None,
        };
        if let Some((attnum, value)) = boolean_fact
            && self.boolean_column(ctx, b, attnum)
            && builtin_boolean_equality(ctx.catalog)
            && sources.conjuncts.iter().any(|source| {
                let source = self.canonical_source(b, *source);
                boolean_conjunct_fixes(source.as_ref(), attnum, value)
            })
        {
            return true;
        }
        // `col IS NOT NULL` holds wherever a strict equality on `col` does:
        // a fact is only recorded for a strict operator, whose result can't
        // be true with a NULL operand.
        if let Node::NullTest(test) = &predicate
            && test.nulltesttype == ast::NullTestType::IsNotNull
            && let Some(Node::Var(var)) = test.arg.as_ref()
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
        // `col IN (c1, …, cn)` holds wherever `col = ci` (or `col IN` a
        // subset of the constants) does.
        if let Some((attnum, allowed)) = membership(&predicate)
            && self.membership_literals_compare_alike(ctx, b, attnum, &allowed)
            && sources.conjuncts.iter().any(|source| {
                let source = self.canonical_source(b, *source);
                source
                    .as_ref()
                    .and_then(membership)
                    .is_some_and(|(column, values)| {
                        column == attnum
                            && self.membership_literals_compare_alike(ctx, b, attnum, &values)
                            && values
                                .iter()
                                .all(|value| allowed.iter().any(|other| other.equal(*value)))
                    })
            })
        {
            return true;
        }
        sources.conjuncts.iter().any(|source| {
            let source = self.canonical_source(b, *source);
            source
                .as_ref()
                .is_some_and(|source| predicate.equal(source))
        })
    }

    /// Whether the constants of a [`membership`] test on relation `b`'s
    /// column `attnum` are compared through the same equality whether they
    /// are written `col = c` or `col IN (…, c, …)`: untyped string
    /// literals, which take the column's type either way (PG's
    /// `select_common_type` ignores them, and `oper` reads one as the other
    /// operand's type), or integer literals on an integer column, where
    /// whichever integer equality PG picks agrees.
    fn membership_literals_compare_alike(
        &self,
        ctx: &Ctx<'_>,
        b: usize,
        attnum: i16,
        values: &[&ast::Node],
    ) -> bool {
        let is = |kind: fn(&AConstValue) -> bool| {
            values.iter().all(|value| {
                matches!(value, Node::AConst(c) if !c.isnull && c.val.as_ref().is_some_and(kind))
            })
        };
        if is(|value| matches!(value, AConstValue::String(_))) {
            return true;
        }
        is(|value| matches!(value, AConstValue::Integer(_)))
            && self.rels[b].oid.is_some_and(|relid| {
                ctx.catalog.attributes_of(relid).iter().any(|attribute| {
                    attribute.attnum == attnum
                        && [oid::INT2, oid::INT4, oid::INT8].contains(&attribute.atttypid)
                })
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
        node: Option<&ast::Node>,
    ) -> Option<ast::Node> {
        // A missing predicate is no shape the proof reads.
        let mut canonical = node.cloned()?;
        let mut resolved = true;
        canonical.rewrite(&mut |_| true, &mut |node| {
            let Node::ColumnRef(column) = &*node else {
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
    fn canonical_source(&self, b: usize, node: Option<&ast::Node>) -> Option<ast::Node> {
        let mut canonical = node.cloned();
        if let Some(canonical) = canonical.as_mut() {
            canonical.rewrite(&mut |_| true, &mut |node| {
                let Node::ColumnRef(column) = &*node else {
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
        }
        canonical
    }
}

/// A column reference resolved to an attnum, in a form no raw parse tree
/// produces: a placeholder `Var`.
fn canonical_column(attnum: i16) -> ast::Node {
    Node::Var(NodeBox::new(ast::Var {
        varattno: i32::from(attnum),
        location: 0,
        ..Default::default()
    }))
}

/// The attnum a canonical [`canonical_column`] `Var` names.
fn canonical_var_attnum(node: Option<&ast::Node>) -> Option<i16> {
    let Some(Node::Var(var)) = node else {
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
fn boolean_conjunct_fixes(node: Option<&ast::Node>, attnum: i16, value: bool) -> bool {
    let is_column = |node: Option<&ast::Node>| canonical_var_attnum(node) == Some(attnum);
    let is_value = |node: Option<&ast::Node>| {
        matches!(
            node,
            Some(Node::AConst(constant))
                if !constant.isnull
                    && matches!(constant.val.as_ref(),
                        Some(AConstValue::Boolean(boolean)) if boolean.boolval == value)
        )
    };
    match node {
        Some(Node::AExpr(expr)) if expr.kind == AExprKind::AexprOp => {
            let name = string_list(&expr.name);
            name.len() == 1
                && name[0] == "="
                && ((is_column(expr.lexpr.as_ref()) && is_value(expr.rexpr.as_ref()))
                    || (is_value(expr.lexpr.as_ref()) && is_column(expr.rexpr.as_ref())))
        }
        Some(Node::BooleanTest(test)) => {
            is_column(test.arg.as_ref())
                && matches!(
                    (value, test.booltesttype),
                    (true, ast::BoolTestType::IsTrue) | (false, ast::BoolTestType::IsFalse)
                )
        }
        Some(Node::BoolExpr(expr))
            if !value && expr.boolop == BoolExprType::NotExpr && expr.args.len() == 1 =>
        {
            canonical_var_attnum(expr.args[0].as_ref()) == Some(attnum)
        }
        _ => false,
    }
}

/// A canonical membership test of one column in non-NULL constants, written
/// with the unqualified `=`: `col = c` or `c = col` (one constant), or
/// `col IN (c1, …)`. Its column's attnum and the constants. (`= ANY
/// (ARRAY[…])` is left out: the array constructor resolves its elements'
/// type on its own, which may pick another equality.)
fn membership(node: &ast::Node) -> Option<(i16, Vec<&ast::Node>)> {
    let Node::AExpr(expr) = node else {
        return None;
    };
    if string_list(&expr.name).as_slice() != ["="] {
        return None;
    }
    fn constant(node: Option<&ast::Node>) -> Option<&ast::Node> {
        node.filter(|node| matches!(node, Node::AConst(c) if !c.isnull))
    }
    match expr.kind {
        AExprKind::AexprOp => {
            match (
                canonical_var_attnum(expr.lexpr.as_ref()),
                canonical_var_attnum(expr.rexpr.as_ref()),
            ) {
                (Some(attnum), None) => Some((attnum, vec![constant(expr.rexpr.as_ref())?])),
                (None, Some(attnum)) => Some((attnum, vec![constant(expr.lexpr.as_ref())?])),
                _ => None,
            }
        }
        AExprKind::AexprIn => {
            let attnum = canonical_var_attnum(expr.lexpr.as_ref())?;
            let Some(Node::List(list)) = expr.rexpr.as_ref() else {
                return None;
            };
            let values = list
                .items
                .iter()
                .map(|item| constant(item.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            (!values.is_empty()).then_some((attnum, values))
        }
        _ => None,
    }
}

/// Whether `node` is one of the predicate shapes a partial index may count
/// with: a bare boolean column, `NOT column`, `column IS [NOT] NULL`,
/// `column = / <> constant`, or `column [NOT] IN (constants)` (also spelled
/// `= ANY` / `<> ALL (ARRAY[...])`).
fn supported_predicate(node: &ast::Node) -> bool {
    let is_var = |node: Option<&ast::Node>| matches!(node, Some(Node::Var(_)));
    match node {
        Node::Var(_) => true,
        Node::NullTest(test) => is_var(test.arg.as_ref()),
        Node::BoolExpr(expr) => {
            expr.boolop == BoolExprType::NotExpr
                && expr.args.len() == 1
                && matches!(expr.args[0].as_ref(), Some(Node::Var(_)))
        }
        Node::AExpr(expr) => match expr.kind {
            AExprKind::AexprOp => {
                matches!(string_list(&expr.name).last(), Some(&"=" | &"<>"))
                    && ((is_var(expr.lexpr.as_ref()) && is_constant(expr.rexpr.as_ref()))
                        || (is_constant(expr.lexpr.as_ref()) && is_var(expr.rexpr.as_ref())))
            }
            AExprKind::AexprIn => {
                matches!(string_list(&expr.name).last(), Some(&"=" | &"<>"))
                    && is_var(expr.lexpr.as_ref())
                    && matches!(
                        expr.rexpr.as_ref(),
                        Some(Node::List(list)) if list.items.iter().all(|n| is_const_node(n.as_ref()))
                    )
            }
            AExprKind::AexprOpAny | AExprKind::AexprOpAll => {
                matches!(string_list(&expr.name).last(), Some(&"=" | &"<>"))
                    && is_var(expr.lexpr.as_ref())
                    && matches!(
                        expr.rexpr.as_ref(),
                        Some(Node::AArrayExpr(array)) if array.elements.iter().all(|n| is_const_node(n.as_ref()))
                    )
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_constant(node: Option<&ast::Node>) -> bool {
    is_const_node(node)
}

fn is_const_node(node: Option<&ast::Node>) -> bool {
    matches!(node, Some(Node::AConst(_)))
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
/// Whether rows equal to one value under `op` are equal to each other
/// under their type's own grouping equality: `op` compares one type with
/// itself, or two integer types (exact, so `int8 = int4` still fixes one
/// value). A cross-type equality in general need not: `timestamp =
/// timestamptz` converts through the session time zone, and two distinct
/// timestamps can both equal one instant.
fn grouping_equality(ctx: &Ctx<'_>, op: PgOperatorOid) -> bool {
    const INTEGERS: [PgTypeOid; 3] = [oid::INT2, oid::INT4, oid::INT8];
    ctx.catalog.pg_operator.get(&op).is_some_and(|declared| {
        declared.oprleft.is_some_and(|left| {
            left == declared.oprright
                || (INTEGERS.contains(&left) && INTEGERS.contains(&declared.oprright))
        })
    })
}

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
                    expr: None,
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

/// Whether the equalities `equated` — each a column of table `relid` (by
/// attnum) compared through operator `op` with a value of the column's own
/// type under a deterministic collation — cover every column of a full
/// unique key of `relid`: a non-partial one (see [`unique_keys`]) whose
/// columns' collations are deterministic, each column compared through the
/// btree equality of its index's operator family on operands the column's
/// type is (binary coercible to). At most one row of `relid` then matches
/// a given value of the other sides.
pub(super) fn equalities_cover_unique_key(
    ctx: &Ctx<'_>,
    relid: PgClassOid,
    equated: &[(i16, PgOperatorOid)],
) -> bool {
    unique_keys(ctx, relid).iter().any(|key| {
        key.predicate.is_none()
            && !key.columns.is_empty()
            && key.columns.iter().all(|column| {
                column.deterministic
                    && equated.iter().any(|&(attnum, op)| {
                        attnum == column.attnum
                            && in_btree_equality(ctx, &column.family, op)
                            && ctx.catalog.pg_operator.get(&op).is_some_and(|declared| {
                                operand_type_matches(ctx, column.typ, declared.oprleft)
                                    && operand_type_matches(
                                        ctx,
                                        column.typ,
                                        Some(declared.oprright),
                                    )
                            })
                    })
            })
    })
}

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
    /// An expression key column (`attnum` is then `0`): the call it stores.
    /// Only [`Scope::key_equated_with`] asks for these (see
    /// [`unique_keys_with_expressions`]); every other reader sees plain
    /// columns only.
    expr: Option<IndexCall>,
}

struct UniqueKey {
    columns: Vec<KeyColumn>,
    nulls_not_distinct: bool,
    /// A partial index's predicate, decoded; `None` for a full index.
    predicate: Option<ast::Node>,
}

/// The unique keys PG enforces at every statement on relation `relid`:
/// unique btree indexes — PRIMARY KEY and UNIQUE constraints' included —
/// that are immediate, valid, not exclusion and on plain columns. A partial
/// index's predicate is kept for the cardinality proof; the `DISTINCT`
/// proof still ignores partial indexes. An index the catalog marks invalid
/// enforces nothing yet — a partitioned parent's `ON ONLY` index until every
/// partition has an attached index — so it is never a key.
fn unique_keys(ctx: &Ctx<'_>, relid: PgClassOid) -> Vec<UniqueKey> {
    unique_keys_in(ctx, relid, false)
}

/// [`unique_keys`], also with the unique indexes whose key columns include
/// expressions of the one shape [`index_expression_call`] reads: a call to
/// a routine over plain columns of the table. Only the cardinality proof
/// matches those, against the [`IndexCall`] facts of a query's own calls.
fn unique_keys_with_expressions(ctx: &Ctx<'_>, relid: PgClassOid) -> Vec<UniqueKey> {
    unique_keys_in(ctx, relid, true)
}

fn unique_keys_in(ctx: &Ctx<'_>, relid: PgClassOid, expressions: bool) -> Vec<UniqueKey> {
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
            Some(stored) => match &stored.ast {
                Some(predicate) => Some(predicate.clone()),
                // An empty stored predicate is never implied: no key.
                None => continue,
            },
        };
        let nkeys = usize::try_from(index.indnkeyatts).unwrap_or(0);
        if nkeys == 0 || index.indkey.len() < nkeys {
            continue;
        }
        let mut columns = Vec::with_capacity(nkeys);
        let mut exprs = index.indexprs.iter();
        for i in 0..nkeys {
            let attnum = index.indkey[i];
            if attnum == 0 {
                // An expression slot, consumed from `indexprs` in order.
                let stored = exprs.next();
                if !expressions {
                    continue 'index;
                }
                let Some((call, typ, collations_deterministic)) = stored
                    .and_then(|stored| index_expression_call(ctx, relid, stored.ast.as_ref()))
                else {
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
                columns.push(KeyColumn {
                    attnum: 0,
                    typ,
                    notnull: false,
                    family: (opclass.opcfamily.clone(), opclass.opcfamilynamespace),
                    default_family: false,
                    deterministic: collations_deterministic
                        && deterministic(catalog, index.indcollation.get(i).copied().flatten()),
                    expr: Some(call),
                });
                continue;
            }
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
                expr: None,
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

/// The call an index expression stores, when it is the one shape a query's
/// call can be matched against: `f(a, b, …)`, every argument a plain column
/// of `relid`, with no named notation, `VARIADIC`, aggregate or window
/// decoration. With the call, the result type and whether every argument
/// column's collation is deterministic (the expression's collation is
/// derived from theirs).
///
/// The stored expression keeps only the name it was written with, resolved
/// when the index was made under that session's `search_path`. It is
/// resolved again here for the columns' types, and an unqualified name is
/// only trusted when every routine of that name lives in the schema of the
/// one it resolves to: no `search_path` could then have bound it to another
/// schema's routine, and within one schema the argument types decide. The
/// routine must be immutable (PG requires it of an index expression) and
/// return one value of a non-polymorphic type.
fn index_expression_call(
    ctx: &Ctx<'_>,
    relid: PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<(IndexCall, PgTypeOid, bool)> {
    let Some(Node::FuncCall(call)) = expr else {
        return None;
    };
    let (schema, name, notation) = call_syntax(call)?;
    let attrs = ctx.catalog.attributes_of(relid);
    let mut args = Vec::with_capacity(call.args.len());
    let mut types = Vec::with_capacity(call.args.len());
    let mut collations_deterministic = true;
    for arg in &call.args {
        let Some(Node::ColumnRef(column)) = arg else {
            return None;
        };
        let Fields::Names(names) = column_ref_fields(column) else {
            return None;
        };
        let [column] = names.as_slice() else {
            return None;
        };
        let attr = attrs
            .iter()
            .find(|a| a.attnum > 0 && a.attname == *column)?;
        args.push(attr.attnum);
        types.push(attr.atttypid);
        collations_deterministic &= deterministic(ctx.catalog, attr.attcollation);
    }
    if args.is_empty() {
        return None;
    }
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
    if resolved.is_aggregate
        || resolved.is_window
        || resolved.is_set_returning
        || resolved.provariadic.is_some()
        || crate::polymorphic::is_polymorphic(resolved.return_type_oid)
    {
        return None;
    }
    let proc = ctx.catalog.pg_proc.get(&resolved.oid)?;
    if proc.provolatile != ProVolatile::Immutable
        || proc.prokind != ProKind::Function
        || proc.proargtypes.len() != args.len()
    {
        return None;
    }
    if schema.is_none()
        && ctx
            .catalog
            .pg_proc
            .values()
            .any(|other| other.proname == name && other.pronamespace != proc.pronamespace)
    {
        return None;
    }
    Some((
        IndexCall {
            proc: resolved.oid,
            args,
        },
        resolved.return_type_oid,
        collations_deterministic,
    ))
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
                        if check_fixes_value(catalog, (**expr).as_ref(), name, ty))
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
    expr: Option<&ast::Node>,
    name: &str,
    ty: PgTypeOid,
) -> bool {
    let is_named =
        |node: Option<&ast::Node>| column_name(node).is_some_and(|column| column == name);
    let Some(expr) = expr else {
        return false;
    };
    match expr {
        Node::BoolExpr(bool) if bool.boolop == BoolExprType::AndExpr => bool
            .args
            .iter()
            .any(|arg| check_fixes_value(catalog, arg.as_ref(), name, ty)),
        Node::ColumnRef(column) => {
            ty == oid::BOOL
                && matches!(column_ref_fields(column), Fields::Names(names)
                    if names.last().is_some_and(|column| *column == name))
        }
        Node::BoolExpr(bool) if bool.boolop == BoolExprType::NotExpr && bool.args.len() == 1 => {
            ty == oid::BOOL
                && matches!(bool.args[0].as_ref(), Some(Node::ColumnRef(column))
                    if matches!(column_ref_fields(column), Fields::Names(names)
                        if names.last().is_some_and(|column| *column == name)))
        }
        Node::AExpr(expr) if expr.kind == AExprKind::AexprOp => {
            let op = string_list(&expr.name);
            op.len() == 1
                && op[0] == "="
                // The CHECK's `=` was bound under the `search_path` in effect
                // when the constraint was created; re-resolving it under
                // today's could find another operator than the one stored.
                // With no user-defined `=` in the catalog there is nothing
                // else it could have bound to.
                && !user_defined_equality_operator_exists(catalog)
                && ((is_named(expr.lexpr.as_ref()) && fixed_literal(catalog, expr.rexpr.as_ref(), ty))
                    || (fixed_literal(catalog, expr.lexpr.as_ref(), ty) && is_named(expr.rexpr.as_ref())))
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
fn column_name(node: Option<&ast::Node>) -> Option<&str> {
    let Node::ColumnRef(column) = node? else {
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
fn fixed_literal(catalog: &PgCatalog, node: Option<&ast::Node>, ty: PgTypeOid) -> bool {
    let Some(Node::AConst(constant)) = node else {
        return false;
    };
    if constant.isnull {
        return false;
    }
    let literal = match constant.val.as_ref() {
        Some(AConstValue::Boolean(_)) if ty == oid::BOOL => oid::BOOL,
        Some(AConstValue::Integer(_)) if ty.get() < 10_000 => oid::INT4,
        Some(AConstValue::String(_)) if ty.get() < 10_000 => oid::UNKNOWN,
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
pub(super) fn type_name_oid(ctx: &Ctx<'_>, type_name: &ast::TypeName) -> Option<PgTypeOid> {
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

/// Whether values of type `typ` carry a collation: the type's own
/// `typcollation`, else its base type's for a domain. An unknown type counts
/// as collatable.
pub(super) fn collatable_type(ctx: &Ctx<'_>, typ: PgTypeOid) -> bool {
    let Some(ty) = ctx.catalog.pg_type.get(&typ) else {
        return true;
    };
    ty.typcollation.is_some()
        || ctx
            .catalog
            .pg_type
            .get(&ctx.catalog.unwrap_domain(typ))
            .is_none_or(|base| base.typcollation.is_some())
}

/// Whether a value of type `typ` is under a deterministic collation: the
/// type's own (`typcollation`), else its base type's for a domain, is none
/// or a bytewise one.
pub(super) fn deterministic_type(ctx: &Ctx<'_>, typ: PgTypeOid) -> bool {
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
fn function_name(nodes: &[Option<ast::Node>]) -> Option<(Option<&str>, &str)> {
    match string_list(nodes).as_slice() {
        [name] => Some((None, *name)),
        [schema, name] => Some((Some(*schema), *name)),
        _ => None,
    }
}

/// Whether `select`'s output (its left arm's, for a set operation) has a
/// `*` or `t.*` target, which expands to a number of columns the target list
/// doesn't show.
fn target_list_has_star(select: &ast::SelectStmt) -> bool {
    if select.op != SetOperation::SetopNone {
        return select.larg.as_deref().is_some_and(target_list_has_star);
    }
    select.target_list.iter().any(|target| {
        let Some(Node::ResTarget(target)) = target else {
            return false;
        };
        match target.val.as_ref() {
            Some(Node::ColumnRef(column)) => matches!(column_ref_fields(column), Fields::Star(_)),
            // `(composite).*` expands to the composite's fields.
            Some(Node::AIndirection(indirection)) => {
                matches!(indirection.indirection.last(), Some(Some(Node::AStar(_))))
            }
            _ => false,
        }
    })
}

/// The names PG gives a subquery's output columns, with whether every one is
/// certain: a bare `VALUES` names them `column1`, `column2`, …, a set
/// operation takes its left arm's, and a plain `SELECT` follows
/// [`target_output_name`]. An uncertain name becomes
/// [`unknown_column_name`]: no reference can spell it.
fn subquery_output_names(select: &ast::SelectStmt) -> Option<(Vec<String>, bool)> {
    if select.op != SetOperation::SetopNone {
        return subquery_output_names(select.larg.as_deref()?);
    }
    if let Some(first) = select.values_lists.first() {
        let Some(Node::List(list)) = first else {
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
        .map(|target| match target {
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
/// written alias, else `FigureColname`'s name for the expression
/// ([`crate::resolve::certain_colname`]: a cast is named after its argument,
/// a call after its function, `1 + 1` is `?column?`); unknown where PG
/// names the entry after a scalar subquery's own first target.
fn target_output_name(target: &ast::ResTarget) -> Option<String> {
    if !target.name.is_empty() {
        return Some(target.name.clone());
    }
    crate::resolve::certain_colname(target.val.as_ref())
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
    s: &'a ast::SelectStmt,
    arms: &mut Vec<&'a ast::SelectStmt>,
    unions: &mut Vec<&'a ast::SelectStmt>,
) -> bool {
    match s.op {
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

fn has_star(s: &ast::SelectStmt) -> bool {
    s.target_list.iter().any(|t| {
        matches!(t, Some(Node::ResTarget(rt))
            if matches!(rt.val.as_ref(),
                Some(Node::ColumnRef(cr)) if matches!(column_ref_fields(cr), Fields::Star(_))))
    })
}

fn target_value(s: &ast::SelectStmt, p: usize) -> Option<&ast::Node> {
    match s.target_list.get(p)?.as_ref() {
        Some(Node::ResTarget(rt)) => rt.val.as_ref(),
        _ => None,
    }
}

/// An arm's value at an output position is of exactly type `typ`, with a
/// deterministic collation: a column of that type, a parameter of it, or a
/// constant that takes it.
fn arm_value_is_exactly(
    ctx: &Ctx<'_>,
    scope: Option<&Scope<'_>>,
    value: Option<&ast::Node>,
    typ: PgTypeOid,
) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        // An untyped literal or NULL takes the other arms' type.
        Node::AConst(c) => match c.val.as_ref() {
            None => c.isnull,
            Some(AConstValue::String(_)) => true,
            Some(AConstValue::Integer(_)) => typ == oid::INT4,
            Some(AConstValue::Boolean(_)) => typ == oid::BOOL,
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
fn is_tag_column(ctx: &Ctx<'_>, arms: &[&ast::SelectStmt], p: usize) -> bool {
    let Some(typ) = type_oid(ctx, &ctx.analyzed.columns[p].pg_type) else {
        return false;
    };
    let mut seen: HashSet<String> = HashSet::new();
    for arm in arms {
        let Some(Node::AConst(c)) = target_value(arm, p) else {
            return false;
        };
        let value = match (c.val.as_ref(), typ) {
            (Some(AConstValue::Integer(i)), oid::INT4) => i.ival.to_string(),
            (Some(AConstValue::String(s)), oid::TEXT) => s.sval.clone(),
            (Some(AConstValue::Boolean(b)), oid::BOOL) => b.boolval.to_string(),
            _ => return false,
        };
        if !seen.insert(value) {
            return false;
        }
    }
    true
}

// ── Text edits ──────────────────────────────────────────────────────────────

fn scan(sql: &str) -> Option<Vec<ast::ScanToken>> {
    typedpg_pg_query::scan(sql).ok().map(|r| r.tokens)
}

fn is_comment(t: &ast::ScanToken) -> bool {
    matches!(t.token, Token::SqlComment | Token::CComment)
}

/// Remove `s`'s `DISTINCT` keyword: the token right before its first
/// target, right after `SELECT`.
fn distinct_keyword_edit(
    sql: &str,
    tokens: &[ast::ScanToken],
    s: &ast::SelectStmt,
) -> Option<Edit> {
    let Some(Node::ResTarget(first)) = s.target_list.first().and_then(|t| t.as_ref()) else {
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
    if distinct.token != Token::Distinct || before.next()?.token != Token::Select {
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
fn union_keyword_edit(tokens: &[ast::ScanToken], s: &ast::SelectStmt) -> Option<Edit> {
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
        .rposition(|t| t.end <= first && t.token == Token::Union)?;
    let between_ok = tokens[union_at + 1..]
        .iter()
        .take_while(|t| t.end <= first)
        .all(|t| {
            is_comment(t)
                || matches!(
                    t.token,
                    Token::Ascii40 | Token::Select | Token::Distinct | Token::All
                )
        });
    if !between_ok {
        return None;
    }
    let union = &tokens[union_at];
    let next = tokens[union_at + 1..].iter().find(|t| !is_comment(t))?;
    match next.token {
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
pub(super) fn cte_names(tree: &ast::ParseResult) -> HashSet<String> {
    tree.nodes()
        .filter_map(|(n, _)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.clone()),
            _ => None,
        })
        .collect()
}

/// The top-level AND conjuncts of a qualification.
pub(super) fn conjuncts(qual: Option<&ast::Node>) -> Vec<Option<&ast::Node>> {
    let mut out = Vec::new();
    // A missing qualification has no conjuncts.
    let mut stack: Vec<Option<&ast::Node>> = Vec::new();
    if qual.is_some() {
        stack.push(qual);
    }
    while let Some(n) = stack.pop() {
        match n {
            Some(Node::BoolExpr(b)) if b.boolop == BoolExprType::AndExpr => {
                stack.extend(b.args.iter().map(Option::as_ref).rev());
            }
            _ => out.push(n),
        }
    }
    out
}

fn string_list(nodes: &[Option<ast::Node>]) -> Vec<&str> {
    nodes
        .iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect()
}

pub(super) fn column_ref_fields(cr: &ast::ColumnRef) -> Fields<'_> {
    let mut names = Vec::with_capacity(cr.fields.len());
    for (i, f) in cr.fields.iter().enumerate() {
        match f {
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

// ── ORDER BY ties ───────────────────────────────────────────────────────────

/// Whether a `SELECT`'s `ORDER BY` keys can tie (see
/// [`crate::order_ties`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderTies {
    /// The keys determine one row: no two rows tie.
    Unique,
    /// Two rows the schema allows may share every key: the FROM items
    /// whose row the keys don't determine, each with its primary key's
    /// columns (empty when it has none).
    Ties(Vec<(String, Vec<String>)>),
    /// The proof can't tell (a FROM item it can't see through, a key that
    /// is an expression index, a set-returning call, …).
    Unknown,
}

/// [`OrderTies`] of `s`'s `ORDER BY` over the rows its FROM, WHERE and
/// grouping make — before `LIMIT` / `OFFSET` / `DISTINCT ON` pick some.
///
/// The keys determine a row when, with the columns `WHERE` equates to
/// constants or parameters, they cover a unique key of every relation, or
/// of the relations the others then determine through equalities to a key
/// (the `DISTINCT` proof's closure, see the module docs). A grouped query
/// (`GROUP BY` of plain columns, or a plain `DISTINCT` of them) yields one
/// row per group: its keys determine a row when they cover, or determine,
/// every grouping column.
///
/// `Ties` is only claimed when the FROM clause is base tables and joins the
/// proof models, none with a partial, expression or deferrable unique index
/// (which may make the keys unique in a way not followed).
///
/// `outer` is the scope of `s`'s own level and the levels around it, when
/// known: a column of an enclosing level (a correlated subquery's, a
/// `LATERAL` item's reference to the items on its left) is one value per
/// evaluation of `s`, so `WHERE` equating a column to it fixes the column
/// like a parameter does. Without it, such a reference fixes nothing.
///
/// A key `col COLLATE c` under a deterministic collation `c` sorts by
/// `col` as far as ties go: a deterministic collation compares only
/// identical strings equal, so values distinct under the column's own
/// (deterministic) collation stay distinct under `c`.
pub(super) fn order_ties(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    s: &ast::SelectStmt,
    outer: Option<&FromScope<'_>>,
) -> OrderTies {
    if s.op != SetOperation::SetopNone
        || s.into_clause.is_some()
        || s.sort_clause.is_empty()
        || s.from_clause.is_empty()
        || !s.values_lists.is_empty()
        || s.group_distinct
        || has_set_returning_call(ctx, s)
    {
        return OrderTies::Unknown;
    }
    let Some(scope) = Scope::build(ctx, ctes, &s.from_clause) else {
        return OrderTies::Unknown;
    };
    let n = scope.rels.len();
    if n == 0 || n > MAX_RELATIONS {
        return OrderTies::Unknown;
    }
    for rel in &scope.rels {
        let Some(oid) = rel.oid else {
            return OrderTies::Unknown;
        };
        let unmodelled_key = ctx.catalog.pg_index.values().any(|i| {
            i.indrelid == oid
                && i.indisunique
                && (i.indpred.is_some()
                    || i.indkey.contains(&0)
                    || ctx.catalog.nonimmediate_indexes.contains(&i.indexrelid))
        });
        if unmodelled_key {
            return OrderTies::Unknown;
        }
    }

    // The ORDER BY keys that are plain columns.
    let mut cols: HashSet<(usize, i16)> = HashSet::new();
    for item in &s.sort_clause {
        let Some(Node::SortBy(sort)) = item else {
            return OrderTies::Unknown;
        };
        let Some(key) = sort_key(s, sort.node.as_ref()) else {
            continue;
        };
        let key = match key {
            Node::CollateClause(collate) if deterministic_collate_clause(ctx, collate) => {
                match collate.arg.as_ref() {
                    Some(arg) => arg,
                    None => continue,
                }
            }
            key => key,
        };
        let Node::ColumnRef(cr) = key else {
            continue;
        };
        let Fields::Names(names) = column_ref_fields(cr) else {
            continue;
        };
        match scope.resolve(&names) {
            Col::Rel(r, attnum) => {
                cols.insert((r, attnum));
            }
            Col::Opaque => return OrderTies::Unknown,
            Col::Other => {}
        }
    }

    // The grouping columns, when the query yields one row per group.
    let grouping: Option<Vec<(usize, i16)>> = if !s.group_clause.is_empty() {
        let mut g = Vec::new();
        for item in &s.group_clause {
            let Some(Node::ColumnRef(cr)) = item.as_ref() else {
                return OrderTies::Unknown;
            };
            let Fields::Names(names) = column_ref_fields(cr) else {
                return OrderTies::Unknown;
            };
            match scope.resolve(&names) {
                Col::Rel(r, attnum) => g.push((r, attnum)),
                _ => return OrderTies::Unknown,
            }
        }
        Some(g)
    } else if is_plain_distinct(s) {
        let mut g = Vec::new();
        for target in &s.target_list {
            let Some(Node::ResTarget(rt)) = target else {
                return OrderTies::Unknown;
            };
            let Some(Node::ColumnRef(cr)) = rt.val.as_ref() else {
                return OrderTies::Unknown;
            };
            let Fields::Names(names) = column_ref_fields(cr) else {
                return OrderTies::Unknown;
            };
            match scope.resolve(&names) {
                Col::Rel(r, attnum) => g.push((r, attnum)),
                _ => return OrderTies::Unknown,
            }
        }
        Some(g)
    } else {
        // An aggregate (or HAVING) without GROUP BY yields one row.
        if s.having_clause.is_some() || has_aggregate(ctx, s) {
            return OrderTies::Unknown;
        }
        None
    };

    let all: u64 = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
    let where_facts = conjuncts(s.where_clause.as_ref())
        .into_iter()
        .flat_map(|c| scope.equality_facts(ctx, c, outer))
        .collect::<Vec<_>>();
    let mut deps: Vec<(u64, usize)> = Vec::new();
    for b in 0..n {
        if let Some(det) = scope.key_equated(ctx, b, &where_facts, all & !(1 << b)) {
            deps.push((det, b));
        }
    }
    for join in &scope.joins {
        let mut facts: Vec<EqFact> = Vec::new();
        for c in conjuncts(join.quals) {
            facts.extend(scope.equality_facts(ctx, c, outer));
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
    let covered = |r: usize, attnum: i16, known: u64| {
        cols.contains(&(r, attnum))
            || scope.column_fixed_by_facts(ctx, r, attnum, &where_facts, known)
    };
    // The columns WHERE and the joins' conditions read.
    let mut read: HashSet<(usize, i16)> = HashSet::new();
    let mut quals: Vec<&Node> = s.where_clause.iter().collect();
    quals.extend(scope.joins.iter().filter_map(|j| j.quals));
    let mut stack: Vec<NodeRef<'_>> = quals.iter().map(|q| q.to_ref()).collect();
    while let Some(n) = stack.pop() {
        if let NodeRef::ColumnRef(cr) = n
            && let Fields::Names(names) = column_ref_fields(cr)
            && let Col::Rel(r, attnum) = scope.resolve(&names)
        {
            read.insert((r, attnum));
        }
        stack.extend(n.children());
    }
    let conditions_read = |r: usize, attnum: i16| read.contains(&(r, attnum));
    let mut uncertain = false;
    let mut known: u64 = 0;
    loop {
        let before = known;
        for (r, rel) in scope.rels.iter().enumerate() {
            if known & (1 << r) != 0 {
                continue;
            }
            let Some(oid) = rel.oid else {
                continue;
            };
            let keys = unique_keys(ctx, oid);
            let covering = |key: &&UniqueKey| {
                key.predicate.is_none()
                    && key
                        .columns
                        .iter()
                        .all(|c| c.deterministic && c.default_family && covered(r, c.attnum, known))
            };
            let determined = singleton_table(ctx, oid)
                || keys.iter().filter(covering).any(|key| {
                    key.columns.iter().all(|c| c.notnull)
                        || (key.nulls_not_distinct && !rel.nullable)
                });
            // A covered key whose NULLs may tie, but whose nullable column
            // a condition reads (`WHERE nick IS NOT NULL`): it may be
            // proven non-NULL there.
            if !determined
                && keys.iter().filter(covering).any(|key| {
                    key.columns
                        .iter()
                        .any(|c| !c.notnull && conditions_read(r, c.attnum))
                })
            {
                uncertain = true;
            }
            if determined {
                known |= 1 << r;
            }
        }
        for &(det, b) in &deps {
            if det & !known == 0 {
                known |= 1 << b;
            }
        }
        if known == before {
            break;
        }
    }
    if known & all == all {
        return OrderTies::Unique;
    }
    if let Some(groups) = &grouping
        && groups
            .iter()
            .all(|&(r, attnum)| known & (1 << r) != 0 || covered(r, attnum, known))
    {
        return OrderTies::Unique;
    }
    if uncertain {
        return OrderTies::Unknown;
    }
    let undetermined = (0..n)
        .filter(|&r| known & (1 << r) == 0)
        .filter(|&r| {
            // Grouped: only a relation with a grouping column left
            // uncovered makes groups tie.
            grouping.as_ref().is_none_or(|groups| {
                groups
                    .iter()
                    .any(|&(g, attnum)| g == r && !covered(g, attnum, known))
            })
        })
        .map(|r| {
            let rel = &scope.rels[r];
            (
                rel.refname.clone(),
                rel.oid
                    .map(|oid| primary_key_columns(ctx, oid))
                    .unwrap_or_default(),
            )
        })
        .collect();
    OrderTies::Ties(undetermined)
}

/// Whether `COLLATE` names a collation the catalog records as deterministic
/// (see [`deterministic`]). An unknown name is refused.
fn deterministic_collate_clause(ctx: &Ctx<'_>, collate: &ast::CollateClause) -> bool {
    let (schema, name) = match string_list(&collate.collname).as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return false,
    };
    ctx.catalog
        .resolve_collation(schema, name)
        .is_some_and(|collation| collation.collisdeterministic)
}

/// The expression an `ORDER BY` item sorts by: an output column's
/// expression for a bare name the select list outputs (SQL92's rule, which
/// PG tries first) or for an ordinal, else the item itself.
fn sort_key<'a>(s: &'a ast::SelectStmt, node: Option<&'a Node>) -> Option<&'a Node> {
    let targets = || {
        s.target_list.iter().filter_map(|t| match t {
            Some(Node::ResTarget(rt)) => Some(rt),
            _ => None,
        })
    };
    match node? {
        Node::AConst(c) => {
            let Some(AConstValue::Integer(i)) = &c.val else {
                return None;
            };
            let index = usize::try_from(i.ival).ok()?.checked_sub(1)?;
            targets().nth(index)?.val.as_ref()
        }
        n @ Node::ColumnRef(cr) => {
            if let Fields::Names(names) = column_ref_fields(cr)
                && let [name] = names.as_slice()
            {
                let mut named = targets().filter(|rt| {
                    rt.name == *name
                        || (rt.name.is_empty()
                            && matches!(rt.val.as_ref(), Some(Node::ColumnRef(c))
                                if matches!(column_ref_fields(c), Fields::Names(ns) if ns.last() == Some(name))))
                });
                if let Some(rt) = named.next() {
                    return if named.next().is_some() {
                        None
                    } else {
                        rt.val.as_ref()
                    };
                }
            }
            Some(n)
        }
        n => Some(n),
    }
}

/// A set-returning function or operator anywhere in `s` (any overload of
/// the name counts).
fn has_set_returning_call(ctx: &Ctx<'_>, s: &ast::SelectStmt) -> bool {
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
                if procs.is_empty() || procs.iter().any(|p| p.proretset) {
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
            // A nested query's own calls don't multiply this level's rows.
            NodeRef::SubLink(_) | NodeRef::RangeSubselect(_) => continue,
            _ => {}
        }
        stack.extend(n.children());
    }
    false
}

/// An aggregate call (without `OVER`) in `s`'s own select list, outside
/// nested queries — any overload of the name being one counts.
fn has_aggregate(ctx: &Ctx<'_>, s: &ast::SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = s.target_list.iter().flatten().map(|n| n.to_ref()).collect();
    while let Some(n) = stack.pop() {
        match n {
            NodeRef::FuncCall(f) if f.over.is_none() => {
                let names = string_list(&f.funcname);
                let name = names.last().copied().unwrap_or_default();
                let schema = (names.len() == 2).then(|| names[0]);
                if f.agg_star
                    || ctx
                        .catalog
                        .find_functions(schema, name)
                        .iter()
                        .any(|p| p.prokind == ProKind::Aggregate)
                {
                    return true;
                }
            }
            NodeRef::SubLink(_) => continue,
            _ => {}
        }
        stack.extend(n.children());
    }
    false
}

/// The names of relation `relid`'s primary key columns, in key order.
fn primary_key_columns(ctx: &Ctx<'_>, relid: PgClassOid) -> Vec<String> {
    let attrs = ctx.catalog.attributes_of(relid);
    ctx.catalog
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && c.contype == ConType::PrimaryKey)
        .map(|c| {
            c.conkey
                .iter()
                .filter_map(|k| attrs.iter().find(|a| a.attnum == *k))
                .map(|a| a.attname.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ── Redundant ORDER BY and GROUP BY keys ────────────────────────────────────

/// One `ORDER BY` item of a `SELECT`, as [`crate::order_keys`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderKey {
    /// The FROM relation (by position among the FROM clause's relations)
    /// and attnum of the plain column the item sorts by — written as a
    /// column, an output column's name or an ordinal — or `None` for any
    /// other key, or one sorted `USING` an operator.
    pub column: Option<(usize, i16)>,
    /// `WHERE` equates that column to a constant, a parameter or an outer
    /// value through the column type's default btree equality, under a
    /// deterministic collation: every row sorts it the same, so the item
    /// can't order any two rows.
    pub pinned: bool,
}

/// The [`OrderKey`]s of `s`'s `ORDER BY`, or `None` when the FROM clause is
/// one the proof can't see through (see [`order_ties`]) or `s` is a set
/// operation.
pub(super) fn order_keys(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    s: &ast::SelectStmt,
) -> Option<Vec<OrderKey>> {
    if s.op != SetOperation::SetopNone
        || s.into_clause.is_some()
        || s.sort_clause.is_empty()
        || s.from_clause.is_empty()
        || !s.values_lists.is_empty()
    {
        return None;
    }
    let scope = Scope::build(ctx, ctes, &s.from_clause)?;
    if scope.rels.is_empty() || scope.rels.len() > MAX_RELATIONS {
        return None;
    }
    let where_facts = conjuncts(s.where_clause.as_ref())
        .into_iter()
        .flat_map(|c| scope.equality_facts(ctx, c, None))
        .collect::<Vec<_>>();
    let mut out = Vec::with_capacity(s.sort_clause.len());
    for item in &s.sort_clause {
        let Some(Node::SortBy(sort)) = item else {
            return None;
        };
        let mut column = None;
        if sort.sortby_dir != ast::SortByDir::SortbyUsing
            && let Some(Node::ColumnRef(cr)) = sort_key(s, sort.node.as_ref())
            && let Fields::Names(names) = column_ref_fields(cr)
        {
            match scope.resolve(&names) {
                Col::Rel(r, attnum) => column = Some((r, attnum)),
                Col::Opaque => return None,
                Col::Other => {}
            }
        }
        let pinned = column.is_some_and(|(r, attnum)| {
            scope.column_fixed_by_facts(ctx, r, attnum, &where_facts, 0)
        });
        out.push(OrderKey { column, pinned });
    }
    Some(out)
}

/// The fewest leading `ORDER BY` items of `s` that already determine a
/// unique row ([`order_ties`] proves them [`OrderTies::Unique`]), when
/// fewer than all of them do: the items after them can't order any two
/// rows.
pub(super) fn unique_order_prefix(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    s: &ast::SelectStmt,
) -> Option<usize> {
    (1..s.sort_clause.len()).find(|&k| {
        let mut prefix = s.clone();
        prefix.sort_clause.truncate(k);
        order_ties(ctx, ctes, &prefix, None) == OrderTies::Unique
    })
}

/// The positions of `s`'s `GROUP BY` items that can be dropped without
/// changing its groups or making it invalid:
///
/// - a plain column of a relation whose other `GROUP BY` columns cover a
///   unique key of it — NOT NULL, or `NULLS NOT DISTINCT` on a relation
///   never null-extended — compared under the column type's default btree
///   equality and a deterministic collation: within a group the relation's
///   row (or its null extension) is one, so the column has one value;
/// - a plain column `WHERE` equates to a constant, a parameter or an outer
///   value (see [`Scope::column_fixed_by_facts`]).
///
/// One item always stays: dropping the last would turn an empty input's
/// no groups into one group.
///
/// PostgreSQL only accepts an ungrouped column in the select list,
/// `HAVING`, `ORDER BY`, `DISTINCT ON` or a window when its table's primary
/// key is grouped (`check_functional_grouping`): an item referenced there
/// — by any column reference spelling its name or its relation's, or a `*`
/// — is only dropped when the remaining items hold every column of its
/// relation's immediate, non-temporal primary key. Items are tried from
/// the last one, against those still kept, so two columns that determine
/// each other never both go.
///
/// Nothing is dropped from `GROUPING SETS`, `ROLLUP`, `CUBE`, `GROUP BY ()`
/// or `GROUP BY DISTINCT`, nor when the FROM clause is one the proof can't
/// see through.
pub(super) fn redundant_group_keys(
    ctx: &Ctx<'_>,
    ctes: &HashSet<String>,
    s: &ast::SelectStmt,
) -> Vec<usize> {
    if s.op != SetOperation::SetopNone
        || s.group_clause.len() < 2
        || s.group_distinct
        || s.from_clause.is_empty()
        || s.into_clause.is_some()
    {
        return Vec::new();
    }
    let Some(scope) = Scope::build(ctx, ctes, &s.from_clause) else {
        return Vec::new();
    };
    if scope.rels.is_empty() || scope.rels.len() > MAX_RELATIONS {
        return Vec::new();
    }
    // Each item's plain column, when it is one.
    let mut items: Vec<Option<(usize, i16)>> = Vec::with_capacity(s.group_clause.len());
    for item in &s.group_clause {
        match item {
            Some(Node::ColumnRef(cr)) => {
                let Fields::Names(names) = column_ref_fields(cr) else {
                    return Vec::new();
                };
                match scope.resolve(&names) {
                    Col::Rel(r, attnum) => items.push(Some((r, attnum))),
                    Col::Opaque => return Vec::new(),
                    // An output column's name, or an outer column.
                    Col::Other => items.push(None),
                }
            }
            // A grouping set, `ROLLUP`, `CUBE` or `()`.
            Some(Node::GroupingSet(_)) | None => return Vec::new(),
            Some(_) => items.push(None),
        }
    }
    let where_facts = conjuncts(s.where_clause.as_ref())
        .into_iter()
        .flat_map(|c| scope.equality_facts(ctx, c, None))
        .collect::<Vec<_>>();
    let referenced = grouped_query_references(s);
    let mut kept: Vec<bool> = vec![true; items.len()];
    let mut removed = Vec::new();
    for i in (0..items.len()).rev() {
        let Some((r, attnum)) = items[i] else {
            continue;
        };
        if !kept.iter().enumerate().any(|(j, &k)| j != i && k) {
            continue;
        }
        let others: Vec<(usize, i16)> = items
            .iter()
            .enumerate()
            .filter(|&(j, _)| j != i && kept[j])
            .filter_map(|(_, item)| *item)
            .collect();
        let rel = &scope.rels[r];
        let Some(oid) = rel.oid else {
            continue;
        };
        let determined = unique_keys(ctx, oid).iter().any(|key| {
            key.predicate.is_none()
                && key.columns.iter().all(|c| {
                    c.deterministic
                        && c.default_family
                        && c.attnum != attnum
                        && others.contains(&(r, c.attnum))
                })
                && (key.columns.iter().all(|c| c.notnull)
                    || (key.nulls_not_distinct && !rel.nullable))
        });
        if !determined && !scope.column_fixed_by_facts(ctx, r, attnum, &where_facts, 0) {
            continue;
        }
        let name = ctx
            .catalog
            .attributes_of(oid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone());
        let is_referenced = name.is_none_or(|name| {
            referenced.stars
                || referenced.names.contains(name.as_str())
                || referenced.names.contains(rel.refname.as_str())
        });
        if is_referenced {
            let pk = primary_key_attnums(ctx, oid);
            let pk_grouped = !pk.is_empty()
                && !pk.contains(&attnum)
                && pk.iter().all(|k| others.contains(&(r, *k)));
            if !pk_grouped {
                continue;
            }
        }
        kept[i] = false;
        removed.push(i);
    }
    removed.reverse();
    removed
}

/// The names a grouped query reads after grouping — in its select list,
/// `HAVING`, `ORDER BY`, `DISTINCT ON` and windows, nested queries
/// included: the last field of each column reference (a column's name, or
/// a whole-row reference's relation), and whether a `*` is among them.
struct GroupedReferences<'a> {
    names: HashSet<&'a str>,
    stars: bool,
}

fn grouped_query_references(s: &ast::SelectStmt) -> GroupedReferences<'_> {
    let mut out = GroupedReferences {
        names: HashSet::new(),
        stars: false,
    };
    let mut stack: Vec<NodeRef<'_>> = Vec::new();
    stack.extend(s.target_list.iter().flatten().map(Node::to_ref));
    stack.extend(s.having_clause.iter().map(Node::to_ref));
    stack.extend(s.sort_clause.iter().flatten().map(Node::to_ref));
    stack.extend(s.distinct_clause.iter().flatten().map(Node::to_ref));
    stack.extend(s.window_clause.iter().flatten().map(Node::to_ref));
    while let Some(n) = stack.pop() {
        if let NodeRef::ColumnRef(cr) = n {
            match cr.fields.last() {
                Some(Some(Node::String(name))) => {
                    out.names.insert(name.sval.as_str());
                }
                _ => out.stars = true,
            }
        }
        stack.extend(n.children());
    }
    out
}

/// The attnums of relation `relid`'s primary key when PostgreSQL's
/// functional-dependency check accepts it (an immediate, non-temporal
/// one), else empty.
fn primary_key_attnums(ctx: &Ctx<'_>, relid: PgClassOid) -> Vec<i16> {
    let Some(constraint) = ctx
        .catalog
        .pg_constraint
        .values()
        .find(|c| c.conrelid == relid && c.contype == ConType::PrimaryKey)
    else {
        return Vec::new();
    };
    let deferrable = ctx.catalog.pg_index.values().any(|i| {
        i.indrelid == relid
            && i.indisprimary
            && ctx.catalog.nonimmediate_indexes.contains(&i.indexrelid)
    });
    if constraint.conperiod || deferrable {
        return Vec::new();
    }
    constraint.conkey.clone()
}
