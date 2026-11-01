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
//!   not hide a second row: every column `attnotnull`, or the index `NULLS
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
//! the ones behind PRIMARY KEY and UNIQUE constraints included. Relations
//! the proof can't see through — views, subqueries, functions, CTEs,
//! inheritance parents, partitioned tables, tables under row security —
//! make it give up, and so do set-returning, aggregate and volatile calls.

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, AExprKind, BoolExprType, JoinType, SetOperation, Token, a_const::Val, node::Node,
};

use super::{Candidate, Ctx, Edit, Rule};
use crate::Type;
use crate::oid::{PgClassOid, PgCollationOid, PgNamespaceOid, PgOperatorOid, PgTypeOid};
use crate::pg_catalog::{ProKind, ProVolatile, RelKind, oid};

/// The most relations one proof follows (a bit each in a `u64`).
const MAX_RELATIONS: usize = 64;

/// [`Rule::Distinct`]: every plain `SELECT DISTINCT` — at the top level or
/// nested anywhere — whose rows are provably distinct without it.
pub(crate) fn distinct_candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
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
            out.push(Candidate {
                rule: Rule::Distinct,
                edits: vec![edit],
            });
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
                    for a in ctx.catalog.attributes_of(scope.rels[r].oid) {
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
                if let Col::Rel(r, attnum) = scope.resolve(ctx, &names) {
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
        let determined = unique_keys(ctx, rel.oid).iter().any(|key| {
            key.columns
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
        .flat_map(|c| scope.equality_facts(ctx, c))
        .collect::<Vec<_>>();
    for b in 0..n {
        if let Some(det) = scope.key_equated(ctx, b, &where_facts, all & !(1 << b)) {
            deps.push((det, b));
        }
    }
    for join in &scope.joins {
        let mut facts: Vec<EqFact> = Vec::new();
        for c in conjuncts(join.quals) {
            facts.extend(scope.equality_facts(ctx, c));
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
fn is_plain_distinct(s: &protobuf::SelectStmt) -> bool {
    matches!(s.distinct_clause.as_slice(), [d] if d.node.is_none())
}

/// A call that may multiply rows (a set-returning function or operator),
/// collapse them (an aggregate without OVER) or have effects (anything
/// [`super::proof::has_volatile`] flags), anywhere in `s` — or a function
/// this can't resolve. Every overload of the name counts, not just the one
/// PG picks.
fn has_unsafe_call(ctx: &Ctx<'_>, s: &protobuf::SelectStmt) -> bool {
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

/// A base table in a FROM clause.
struct Rel {
    oid: PgClassOid,
    /// The alias, or the table's name.
    refname: String,
    aliased: bool,
    schema: String,
    relname: String,
    /// The relation may be null-extended by an outer join above it.
    nullable: bool,
}

/// A join in a FROM clause, with its sides' relations as bitmasks.
struct Join<'a> {
    kind: JoinType,
    left: u64,
    right: u64,
    /// The side, when it is a single relation.
    left_rel: Option<usize>,
    right_rel: Option<usize>,
    quals: Option<&'a protobuf::Node>,
    using: Vec<&'a str>,
    /// The join may be null-extended by an outer join above it.
    nullable: bool,
}

impl Join<'_> {
    fn rels(&self) -> u64 {
        self.left | self.right
    }
}

/// The base tables and joins of one FROM clause.
struct Scope<'a> {
    rels: Vec<Rel>,
    joins: Vec<Join<'a>>,
}

/// What a column reference names.
enum Col {
    /// A column of one of the scope's relations.
    Rel(usize, i16),
    /// Anything else: an outer query's column, an ambiguous name, a whole
    /// row, a name this doesn't resolve.
    Other,
}

enum Fields<'a> {
    Names(Vec<&'a str>),
    /// `*` or `qualifier.*`.
    Star(Option<String>),
    Other,
}

/// One proven fact `rel.attnum = <value>` with a strict equality operator,
/// where the value is read from the relations of `other` (constants and
/// parameters read none).
struct EqFact {
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
    /// base tables the proof can trust and plain joins of them.
    fn build(
        ctx: &Ctx<'_>,
        ctes: &HashSet<String>,
        from: &'a [protobuf::Node],
    ) -> Option<Scope<'a>> {
        let mut scope = Scope {
            rels: Vec::new(),
            joins: Vec::new(),
        };
        for item in from {
            scope.add(ctx, ctes, item, false)?;
        }
        Some(scope)
    }

    /// Add one FROM item; its relations as a bitmask.
    fn add(
        &mut self,
        ctx: &Ctx<'_>,
        ctes: &HashSet<String>,
        item: &'a protobuf::Node,
        nullable: bool,
    ) -> Option<u64> {
        match item.node.as_ref()? {
            Node::RangeVar(rv) => {
                if self.rels.len() == MAX_RELATIONS
                    || !rv.catalogname.is_empty()
                    || (rv.schemaname.is_empty() && ctes.contains(&rv.relname))
                {
                    return None;
                }
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                let class = ctx.catalog.resolve_table(schema, &rv.relname)?;
                if class.relkind != RelKind::Table
                    || ctx
                        .catalog
                        .pg_inherits
                        .iter()
                        .any(|i| i.inhparent == class.oid)
                    || ctx.catalog.row_security.contains(&class.oid)
                {
                    return None;
                }
                let alias = rv.alias.as_ref().map(|a| a.aliasname.clone());
                self.rels.push(Rel {
                    oid: class.oid,
                    refname: alias.clone().unwrap_or_else(|| rv.relname.clone()),
                    aliased: alias.is_some(),
                    schema: ctx
                        .catalog
                        .namespace_name(class.relnamespace)
                        .unwrap_or_default()
                        .to_owned(),
                    relname: rv.relname.clone(),
                    nullable,
                });
                Some(1 << (self.rels.len() - 1))
            }
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
                let left = self.add(ctx, ctes, j.larg.as_deref()?, left_nullable)?;
                let middle = self.rels.len();
                let right = self.add(ctx, ctes, j.rarg.as_deref()?, right_nullable)?;
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

    /// The relation a qualifier names.
    fn rel_named(&self, qualifier: &str) -> Option<usize> {
        self.rels.iter().position(|r| r.refname == qualifier)
    }

    /// Resolve a column reference against this scope alone.
    fn resolve(&self, ctx: &Ctx<'_>, names: &[&str]) -> Col {
        let attnum = |r: usize, name: &str| {
            ctx.catalog
                .attribute_by_name(self.rels[r].oid, name)
                .filter(|a| a.attnum > 0)
                .map_or(Col::Other, |a| Col::Rel(r, a.attnum))
        };
        match names {
            [name] => {
                let mut found = self
                    .rels
                    .iter()
                    .enumerate()
                    .filter(|(_, rel)| ctx.catalog.attribute_by_name(rel.oid, name).is_some());
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
    /// scope, a parameter or a constant.
    fn operand(&self, ctx: &Ctx<'_>, expr: &protobuf::Node) -> Option<Operand> {
        match expr.node.as_ref()? {
            Node::ColumnRef(cr) => {
                let Fields::Names(names) = column_ref_fields(cr) else {
                    return None;
                };
                let Col::Rel(r, attnum) = self.resolve(ctx, &names) else {
                    return None;
                };
                let attr = ctx
                    .catalog
                    .attributes_of(self.rels[r].oid)
                    .iter()
                    .find(|a| a.attnum == attnum)?;
                deterministic(attr.attcollation).then_some(Operand {
                    typ: attr.atttypid,
                    column: Some((r, attnum)),
                    rels: 1 << r,
                })
            }
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
            _ => None,
        }
    }

    /// The facts `column = value` an `=` conjunct proves.
    fn equality_facts(&self, ctx: &Ctx<'_>, conjunct: &protobuf::Node) -> Vec<EqFact> {
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
        let (Some(l), Some(r)) = (self.operand(ctx, l), self.operand(ctx, r)) else {
            return Vec::new();
        };
        self.facts_of(ctx, name, &l, &r)
    }

    /// The facts `USING (name)` proves between the join's sides.
    fn using_facts(&self, ctx: &Ctx<'_>, name: &str, left: u64, right: u64) -> Vec<EqFact> {
        let side = |mask: u64| {
            let mut found = (0..self.rels.len())
                .filter(|&r| mask & (1 << r) != 0)
                .filter_map(|r| {
                    let a = ctx.catalog.attribute_by_name(self.rels[r].oid, name)?;
                    Some((r, a))
                });
            match (found.next(), found.next()) {
                (Some((r, a)), None) if a.attnum > 0 && deterministic(a.attcollation) => {
                    Some(Operand {
                        typ: a.atttypid,
                        column: Some((r, a.attnum)),
                        rels: 1 << r,
                    })
                }
                _ => None,
            }
        };
        match (side(left), side(right)) {
            (Some(l), Some(r)) => self.facts_of(ctx, "=", &l, &r),
            _ => Vec::new(),
        }
    }

    fn facts_of(&self, ctx: &Ctx<'_>, name: &str, l: &Operand, r: &Operand) -> Vec<EqFact> {
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

    /// When `facts` equate every column of a unique key of relation `b` to
    /// values read from relations within `allowed`: the relations read.
    fn key_equated(&self, ctx: &Ctx<'_>, b: usize, facts: &[EqFact], allowed: u64) -> Option<u64> {
        let oid = self.rels[b].oid;
        unique_keys(ctx, oid).iter().find_map(|key| {
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
}

/// The equality operator reads the column as its own type: no coercion
/// that could change its value on the way.
fn operand_type_matches(ctx: &Ctx<'_>, column: PgTypeOid, declared: Option<PgTypeOid>) -> bool {
    declared.is_some_and(|d| {
        d == column
            || d == ctx.catalog.unwrap_domain(column)
            || ctx.catalog.is_binary_coercible(column, d)
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

// ── Unique keys ─────────────────────────────────────────────────────────────

struct KeyColumn {
    attnum: i16,
    typ: PgTypeOid,
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
}

/// The unique keys PG enforces at every statement on relation `relid`:
/// unique btree indexes — PRIMARY KEY and UNIQUE constraints' included —
/// that are immediate, not partial, not exclusion and on plain columns.
fn unique_keys(ctx: &Ctx<'_>, relid: PgClassOid) -> Vec<UniqueKey> {
    let catalog = ctx.catalog;
    let attrs = catalog.attributes_of(relid);
    let mut out = Vec::new();
    'index: for index in catalog.pg_index.values() {
        if index.indrelid != relid
            || !index.indisunique
            || index.indisexclusion
            || index.indpred.is_some()
            || catalog.nonimmediate_indexes.contains(&index.indexrelid)
        {
            continue;
        }
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
                notnull: attr.attnotnull,
                family,
                default_family,
                deterministic: deterministic(index.indcollation.get(i).copied().flatten())
                    && deterministic(attr.attcollation),
            });
        }
        out.push(UniqueKey {
            columns,
            nulls_not_distinct: catalog
                .index_keys
                .get(&index.indexrelid)
                .is_some_and(|k| k.nulls_not_distinct),
        });
    }
    out
}

/// A collation whose equality is bytewise: none, the database default
/// (PG doesn't allow a nondeterministic one), `C` or `POSIX`. Any other is
/// treated as possibly nondeterministic.
fn deterministic(collation: Option<PgCollationOid>) -> bool {
    collation.is_none_or(|c| matches!(c.get(), 100 | 950 | 951))
}

/// [`deterministic`] for a type as the analysis reports it (the collation
/// is named only when not the database default).
fn deterministic_type_collation(t: &Type) -> bool {
    match t {
        Type::Basic { collation, .. } | Type::Domain { collation, .. } => collation
            .as_deref()
            .is_none_or(|c| matches!(c, "C" | "POSIX")),
        _ => true,
    }
}

fn type_oid(ctx: &Ctx<'_>, t: &Type) -> Option<PgTypeOid> {
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
            let Col::Rel(r, attnum) = scope.resolve(ctx, &names) else {
                return false;
            };
            ctx.catalog
                .attributes_of(scope.rels[r].oid)
                .iter()
                .find(|a| a.attnum == attnum)
                .is_some_and(|a| a.atttypid == typ && deterministic(a.attcollation))
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
fn cte_names(tree: &protobuf::ParseResult) -> HashSet<String> {
    tree.nodes()
        .into_iter()
        .filter_map(|(n, _)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.clone()),
            _ => None,
        })
        .collect()
}

/// The top-level AND conjuncts of a qualification.
fn conjuncts(qual: Option<&protobuf::Node>) -> Vec<&protobuf::Node> {
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

fn column_ref_fields(cr: &protobuf::ColumnRef) -> Fields<'_> {
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
