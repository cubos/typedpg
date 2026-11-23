//! [`Rule::FkJoin`]: drop an unused INNER JOIN to a parent table through a
//! foreign key (#174).
//!
//! ```sql
//! SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id
//! -- becomes
//! SELECT o.id FROM orders o
//! ```
//!
//! PostgreSQL removes an unused LEFT JOIN to a unique inner side, never an
//! unused INNER one. Through a foreign key, an inner join keeps every child
//! row exactly once — when each of these holds, as PG enforces it at every
//! statement:
//!
//! - the join condition is exactly the key's column pairs, compared with
//!   the key's own equality operators ([`fk_equalities_hold`]);
//! - the referencing columns are NOT NULL (a validated not-null
//!   constraint): a NULL matches no parent;
//! - the key is enforced, validated — by checking the rows, not by an
//!   `ALTER CONSTRAINT` rewriting its row as valid — and NOT DEFERRABLE (a
//!   deferred key may be violated inside a transaction), and so is each
//!   partition's clone;
//! - the migrations never disabled the triggers enforcing it (`DISABLE
//!   TRIGGER ALL` on either table, even if enabled again later: rows
//!   written meanwhile were never checked) nor did anything else that may
//!   turn them off (`crate::ddl::fk_bypass`: `session_replication_role`,
//!   catalog writes, subscriptions, any routine defined that might);
//! - the parent has no row security, which could hide the referenced row;
//! - the scan of the parent is the relation the key looks rows up in — not
//!   one with inheritance children, whose rows may repeat the key, nor
//!   `ONLY` a partitioned table (always empty) — and the scan of the child
//!   holds only rows the key covers (inheritance children don't carry it);
//! - no column of the parent is used anywhere else, `p.*` and a whole-row
//!   `p` included;
//! - the query is a plain SELECT — no WITH, set operation, `FOR UPDATE`/
//!   `FOR SHARE` (the join's lock on the parent is observable) — without
//!   volatile functions.
//!
//! Only two textual shapes are rewritten, those whose rewrite is a clean
//! cut: `child [INNER] JOIN parent ON <key equalities>`, the parent being
//! the right-hand side, and the comma form `FROM child, parent WHERE <key
//! equalities> [AND ...]`, the equalities being top-level conjuncts of the
//! WHERE. Views are never inlined: the sent SQL would no longer go through
//! the view's owner privileges. Anything else is left alone.

use std::collections::BTreeSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{self, AExprKind, BoolExprType, JoinType, Node, SetOperation, Token};

use super::dedup;
use super::proof::{FromScope, column_not_null, has_volatile};
use super::{Candidate, Ctx, Edit, Rule};
use crate::ddl::tables::inherit::{all_inheritors, children_of};
use crate::oid::PgClassOid;
use crate::pg_catalog::{ConType, PgCatalog, QueryView, RelKind};
use crate::resolve::row_guarantees::{FkEquality, fk_equalities_hold, fk_parent_ok};

pub(crate) fn candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let Some((root, sel)) = plain_select(ctx) else {
        return Vec::new();
    };
    if ctx.catalog.fk_checks_bypassed {
        return Vec::new();
    }
    let Some(tokens) = Tokens::scan(ctx.sql) else {
        return Vec::new();
    };
    let site = Site {
        ctx,
        root: root.to_ref(),
        tokens,
    };
    let mut out = Vec::new();
    for item in &sel.from_clause {
        site.join_sites(item.as_ref(), &mut out);
    }
    if sel.where_clause.is_some() {
        let quals = &sel.where_clause;
        for (i, item) in sel.from_clause.iter().enumerate() {
            if let Some(Node::RangeVar(rv)) = item.as_ref()
                && let Some(edits) = site.comma_site(sel, i, rv, quals.as_ref())
                && site.tokens.clear_of_comments(&edits)
            {
                out.push(Candidate {
                    rule: Rule::FkJoin,
                    edits,
                });
            }
        }
    }
    out
}

/// What proves a [`RedundantJoin`] keeps every row exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedundantJoinProof {
    /// An INNER join to a parent through a foreign key every child row
    /// has (the [`Rule::FkJoin`] proof).
    ForeignKey,
    /// A LEFT join on a full unique key of the joined table: each row
    /// matches at most one of its rows.
    UniqueKey,
}

/// A join of a plain SELECT that can't change the rows — it neither drops
/// nor repeats one — and whose table nothing else reads: what the
/// `redundant_join` lint rule reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedundantJoin {
    /// From the join's first keyword to the end of its ON condition, in
    /// [`QueryView::sql`].
    pub span: std::ops::Range<usize>,
    /// The name the joined table is referenced by (its alias, or its name).
    pub relation: String,
    pub proof: RedundantJoinProof,
    /// The range deleting the join (from the end of its left-hand side),
    /// or `None` when a comment is in the way.
    pub fix: Option<std::ops::Range<usize>>,
}

impl QueryView {
    /// The joins of the statement that keep every row exactly once and
    /// whose table nothing else reads: an INNER join through a foreign key
    /// (exactly the joins the `fk_join` simplification removes, in their
    /// JOIN form), or a LEFT join equating every column of a full unique
    /// key of the joined table. Only a plain SELECT's FROM is read, as by
    /// the simplification; anything the proofs can't see through is left
    /// out.
    pub fn redundant_joins(&self, catalog: &PgCatalog) -> Vec<RedundantJoin> {
        if !self.analyzed.spreads.is_empty() {
            return Vec::new();
        }
        let ctx = Ctx {
            sql: &self.sql,
            tree: &self.tree,
            catalog,
            analyzed: &self.analyzed,
        };
        let Some((root, sel)) = plain_select(&ctx) else {
            return Vec::new();
        };
        let Some(tokens) = Tokens::scan(ctx.sql) else {
            return Vec::new();
        };
        let site = Site {
            ctx: &ctx,
            root: root.to_ref(),
            tokens,
        };
        let mut found = Vec::new();
        for item in &sel.from_clause {
            site.redundant_joins(sel, item.as_ref(), &mut found);
        }
        if found.is_empty() {
            return Vec::new();
        }
        let Ok(original) = crate::resolve::analyze_static(catalog, &self.sql, &[]) else {
            return Vec::new();
        };
        found
            .into_iter()
            .filter(|(_, cut)| {
                let mut edited = self.sql.clone();
                edited.replace_range(cut.clone(), "");
                proves_the_same(catalog, &original, &edited)
            })
            .map(|(join, _)| join)
            .collect()
    }
}

/// Whether the analysis of `edited` proves the same of its result as
/// `original`, the analysis of the query before the join was cut: the same
/// columns — name, type, nullability and refinement — and parameters. A
/// join through a foreign key can refine a column (the referencing column
/// takes the values a CHECK allows the parent's key): the join then isn't
/// dead text to typedpg, and the rule leaves it alone.
fn proves_the_same(
    catalog: &PgCatalog,
    original: &(
        Vec<crate::AnalyzedColumn>,
        Vec<crate::resolve::ParamInfo>,
        bool,
    ),
    edited: &str,
) -> bool {
    let Ok((columns, params, can_run_as_subquery)) =
        crate::resolve::analyze_static(catalog, edited, &[])
    else {
        return false;
    };
    columns.len() == original.0.len()
        && columns.iter().zip(&original.0).all(|(a, b)| {
            a.name == b.name
                && a.pg_type == b.pg_type
                && a.nullable == b.nullable
                && a.refinement == b.refinement
                && a.elem_refinement == b.elem_refinement
        })
        && params.len() == original.1.len()
        && params
            .iter()
            .zip(&original.1)
            .all(|(a, b)| a.pg_type == b.pg_type && a.nullable == b.nullable)
        && can_run_as_subquery == original.2
}

/// The statement, when it is a plain SELECT the rewrites can reason about:
/// one statement, no WITH, set operation, `INTO`, `VALUES`, `FOR UPDATE` /
/// `FOR SHARE` (the join's lock on the parent is observable), volatile
/// function (views count) nor `USING` / `NATURAL` join.
fn plain_select<'a>(ctx: &Ctx<'a>) -> Option<(&'a Node, &'a ast::SelectStmt)> {
    let [raw] = ctx.tree.stmts.as_slice() else {
        return None;
    };
    let root = raw.stmt.as_ref()?;
    let Node::SelectStmt(sel) = root else {
        return None;
    };
    if sel.op != SetOperation::SetopNone
        || sel.with_clause.is_some()
        || sel.into_clause.is_some()
        || !sel.locking_clause.is_empty()
        || !sel.values_lists.is_empty()
    {
        return None;
    }
    // Views and row locking count as volatile there too.
    if has_volatile(ctx, Some(root)) || has_using_or_natural(root) {
        return None;
    }
    Some((root, sel))
}

// ─── Text ─────────────────────────────────────────────────────────────────

/// The SQL's lexical tokens, as `(start, end)` byte ranges — comments
/// included, so a comment between the tokens a shape expects makes it not
/// match.
struct Tokens<'a> {
    sql: &'a str,
    spans: Vec<(usize, usize)>,
    /// The comments among them (`--` and `/* */`).
    comments: Vec<(usize, usize)>,
}

impl<'a> Tokens<'a> {
    fn scan(sql: &'a str) -> Option<Self> {
        let scanned = typedpg_pg_query::scan(sql).ok()?;
        let span = |t: &ast::ScanToken| {
            Some((usize::try_from(t.start).ok()?, usize::try_from(t.end).ok()?))
        };
        let spans = scanned
            .tokens
            .iter()
            .map(span)
            .collect::<Option<Vec<_>>>()?;
        let comments = scanned
            .tokens
            .iter()
            .filter(|t| matches!(t.token, Token::SqlComment | Token::CComment))
            .map(span)
            .collect::<Option<Vec<_>>>()?;
        Some(Tokens {
            sql,
            spans,
            comments,
        })
    }

    /// Token `i` is a comment.
    fn is_comment(&self, i: usize) -> bool {
        self.spans
            .get(i)
            .is_some_and(|s| self.comments.binary_search(s).is_ok())
    }

    /// No edit touches a comment: none starts or ends against one, nor
    /// holds one. A cut starting right after a `--` comment would take the
    /// newline ending it, and comment out what follows.
    fn clear_of_comments(&self, edits: &[Edit]) -> bool {
        edits.iter().all(|e| {
            self.comments
                .iter()
                .all(|&(start, end)| start > e.range.end || end < e.range.start)
        })
    }

    /// The index of the token starting at byte `at`.
    fn at(&self, at: i32) -> Option<usize> {
        let at = usize::try_from(at).ok()?;
        self.spans.binary_search_by_key(&at, |s| s.0).ok()
    }

    fn text(&self, i: usize) -> Option<&'a str> {
        let (s, e) = *self.spans.get(i)?;
        self.sql.get(s..e)
    }

    /// Token `i` is `word` (a keyword or a one-character operator).
    fn is(&self, i: usize, word: &str) -> bool {
        self.text(i).is_some_and(|t| t.eq_ignore_ascii_case(word))
    }

    fn start(&self, i: usize) -> Option<usize> {
        self.spans.get(i).map(|s| s.0)
    }

    fn end(&self, i: usize) -> Option<usize> {
        self.spans.get(i).map(|s| s.1)
    }

    /// The first and last token of a two-name `ColumnRef` (`q.col`).
    fn column_ref(&self, cr: &ast::ColumnRef) -> Option<(usize, usize)> {
        let first = self.at(cr.location)?;
        self.is(first + 1, ".").then_some((first, first + 2))
    }

    /// The tokens of one key equality `q.a = r.b`, with the parentheses
    /// wrapped around it alone.
    fn equality(&self, eq: &KeyEquality<'_>) -> Option<(usize, usize)> {
        let (mut first, _) = self.column_ref(eq.left)?;
        let (_, mut last) = self.column_ref(eq.right)?;
        if first + 6 != last || !self.is(first + 3, "=") {
            return None;
        }
        while first > 0 && self.is(first - 1, "(") && self.is(last + 1, ")") {
            first -= 1;
            last += 1;
        }
        Some((first, last))
    }
}

// ─── Shapes ───────────────────────────────────────────────────────────────

/// A `q.col` column reference's qualifier and column.
fn qualified(n: Option<&ast::Node>) -> Option<(&ast::ColumnRef, &str, &str)> {
    let Some(Node::ColumnRef(cr)) = n else {
        return None;
    };
    match cr.fields.as_slice() {
        [q, c] => match (q.as_ref()?, c.as_ref()?) {
            (Node::String(q), Node::String(c)) => Some((cr, q.sval.as_str(), c.sval.as_str())),
            _ => None,
        },
        _ => None,
    }
}

/// One `parent.k = child.fk` (or the other way round) of the join
/// condition.
struct KeyEquality<'a> {
    /// The `=`'s location: the node the reference check skips.
    location: i32,
    left: &'a ast::ColumnRef,
    right: &'a ast::ColumnRef,
    parent_col: &'a str,
    child: &'a str,
    child_col: &'a str,
    parent_left: bool,
}

/// `node` as an equality between a column of `parent` (by its range
/// name) and a column of another range name.
fn key_equality<'a>(node: Option<&'a ast::Node>, parent: &str) -> Option<KeyEquality<'a>> {
    let Some(Node::AExpr(e)) = node else {
        return None;
    };
    if e.kind != AExprKind::AexprOp {
        return None;
    }
    match e.name.as_slice() {
        [op] if matches!(op.as_ref(), Some(Node::String(s)) if s.sval == "=") => {}
        _ => return None,
    }
    let (left, lq, lc) = qualified(e.lexpr.as_ref())?;
    let (right, rq, rc) = qualified(e.rexpr.as_ref())?;
    let (parent_left, parent_col, child, child_col) = if lq == parent && rq != parent {
        (true, lc, rq, rc)
    } else if rq == parent && lq != parent {
        (false, rc, lq, lc)
    } else {
        return None;
    };
    Some(KeyEquality {
        location: e.location,
        left,
        right,
        parent_col,
        child,
        child_col,
        parent_left,
    })
}

/// The conjuncts of `quals`: the arguments of a top-level AND, or `quals`
/// itself.
fn conjuncts(quals: Option<&ast::Node>) -> Vec<Option<&ast::Node>> {
    match quals {
        Some(Node::BoolExpr(b)) if b.boolop == BoolExprType::AndExpr => {
            b.args.iter().map(Option::as_ref).collect()
        }
        _ => vec![quals],
    }
}

/// A table the query reads by name, as the rule can reason about it.
struct Rel<'a> {
    rv: &'a ast::RangeVar,
    oid: PgClassOid,
    relkind: RelKind,
}

fn refname(rv: &ast::RangeVar) -> &str {
    rv.alias
        .as_ref()
        .map_or(rv.relname.as_str(), |a| a.aliasname.as_str())
}

/// The plain or partitioned table `rv` names. A column alias list renames
/// the columns the names below are matched against, so it isn't followed.
fn relation<'a>(ctx: &Ctx<'_>, rv: &'a ast::RangeVar) -> Option<Rel<'a>> {
    if !rv.catalogname.is_empty() || rv.alias.as_ref().is_some_and(|a| !a.colnames.is_empty()) {
        return None;
    }
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    let class = ctx.catalog.resolve_table(schema, &rv.relname)?;
    matches!(class.relkind, RelKind::Table | RelKind::Partitioned).then_some(Rel {
        rv,
        oid: class.oid,
        relkind: class.relkind,
    })
}

enum Found<'a> {
    Here(&'a ast::RangeVar),
    Absent,
    /// Present where its rows may be NULL-extended, or behind something
    /// the rule doesn't follow.
    Unusable,
}

/// The range named `name` in the join tree `node`, when every row of the
/// tree has one of its rows (it's never on the nullable side of an outer
/// join).
fn find_range<'a>(node: Option<&'a ast::Node>, name: &str) -> Found<'a> {
    match node {
        Some(Node::RangeVar(rv)) if refname(rv) == name => Found::Here(rv),
        Some(Node::RangeVar(_)) => Found::Absent,
        Some(Node::JoinExpr(j)) => {
            if j.alias.is_some() {
                // The alias hides the names inside.
                return Found::Unusable;
            }
            let kind = j.jointype;
            let sides = [
                (
                    &j.larg,
                    matches!(kind, JoinType::JoinInner | JoinType::JoinLeft),
                ),
                (
                    &j.rarg,
                    matches!(kind, JoinType::JoinInner | JoinType::JoinRight),
                ),
            ];
            for (side, preserved) in sides {
                let found = match side {
                    None => Found::Absent,
                    Some(_) => find_range(side.as_ref(), name),
                };
                match found {
                    Found::Here(rv) if preserved => return Found::Here(rv),
                    Found::Absent => {}
                    _ => return Found::Unusable,
                }
            }
            Found::Absent
        }
        Some(Node::RangeSubselect(s)) => match &s.alias {
            Some(a) if a.aliasname != name => Found::Absent,
            _ => Found::Unusable,
        },
        Some(Node::RangeFunction(f)) => match &f.alias {
            Some(a) if a.aliasname != name => Found::Absent,
            _ => Found::Unusable,
        },
        _ => Found::Unusable,
    }
}

/// A JOIN USING or NATURAL JOIN anywhere: its merged columns may come from
/// the parent without naming it.
fn has_using_or_natural(root: &Node) -> bool {
    root.nodes().any(
        |(n, _)| matches!(n, NodeRef::JoinExpr(j) if j.is_natural || !j.using_clause.is_empty()),
    )
}

/// A join without `USING`, `NATURAL` or an alias (which hides the names
/// inside it).
fn plain_join(j: &ast::JoinExpr) -> bool {
    !j.is_natural && j.using_clause.is_empty() && j.alias.is_none() && j.join_using_alias.is_none()
}

/// System columns, which every table has.
const SYSTEM_COLUMNS: &[&str] = &["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// Whether anything in the statement but the key equalities (by their
/// `=`'s location) may read the parent: a name qualified by its range
/// name, a whole-row reference to it, an unqualified `*`, or an
/// unqualified name one of its columns has.
fn parent_used(ctx: &Ctx<'_>, root: NodeRef<'_>, parent: &Rel<'_>, skip: &[i32]) -> bool {
    let name = refname(parent.rv);
    let columns: BTreeSet<&str> = ctx
        .catalog
        .attributes_of(parent.oid)
        .iter()
        .map(|a| a.attname.as_str())
        .chain(SYSTEM_COLUMNS.iter().copied())
        .collect();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        match n {
            NodeRef::AExpr(e) if skip.contains(&e.location) => continue,
            NodeRef::ColumnRef(cr) => {
                let unqualified = cr.fields.len() == 1;
                for f in &cr.fields {
                    match f.as_ref() {
                        Some(Node::AStar(_)) if unqualified => return true,
                        Some(Node::String(s))
                            if s.sval == name || (unqualified && columns.contains(&*s.sval)) =>
                        {
                            return true;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        stack.extend(n.children());
    }
    false
}

// ─── The proof ────────────────────────────────────────────────────────────

/// The scan of `rel` returns `rel`'s rows and its partitions' — the rows
/// its constraints cover — and nothing else: not a plain inheritance
/// child's rows (they don't carry its foreign keys, and may repeat its
/// keys), nor nothing at all (`ONLY` a partitioned table).
fn scans_own_rows(ctx: &Ctx<'_>, rel: &Rel<'_>) -> bool {
    match rel.relkind {
        RelKind::Partitioned => rel.rv.inh,
        _ => !rel.rv.inh || children_of(ctx.catalog, rel.oid).is_empty(),
    }
}

/// The triggers enforcing foreign keys were never off for a row a scan of
/// `rel` returns: no `DISABLE TRIGGER ALL` on it, its descendants or its
/// ancestors, at any point of the migrations.
fn triggers_never_disabled(catalog: &PgCatalog, rel: PgClassOid) -> bool {
    let ever = &catalog.ri_triggers_ever_disabled;
    if ever.is_empty() {
        return true;
    }
    let mut related = all_inheritors(catalog, rel);
    let mut i = 0;
    while i < related.len() && i < 256 {
        let rel = related[i];
        for h in catalog.pg_inherits.iter().filter(|h| h.inhrelid == rel) {
            if !related.contains(&h.inhparent) {
                related.push(h.inhparent);
            }
        }
        i += 1;
    }
    !related.iter().any(|r| ever.contains(r))
}

/// Whether the join of `child` to `parent` on `equalities` keeps each row
/// of `child` exactly once.
fn join_is_identity(
    ctx: &Ctx<'_>,
    child: &Rel<'_>,
    parent: &Rel<'_>,
    equalities: &[KeyEquality<'_>],
) -> bool {
    let catalog = ctx.catalog;
    if child.oid == parent.oid && refname(child.rv) == refname(parent.rv) {
        return false;
    }
    if !fk_parent_ok(catalog, parent.oid)
        || !scans_own_rows(ctx, parent)
        || !scans_own_rows(ctx, child)
        || !triggers_never_disabled(catalog, child.oid)
        || !triggers_never_disabled(catalog, parent.oid)
    {
        return false;
    }
    // Every referencing column NOT NULL (validated), in every relation the
    // child's scan reads.
    let not_null = equalities.iter().all(|e| {
        catalog
            .attribute_by_name(child.oid, e.child_col)
            .is_some_and(|a| column_not_null(catalog, child.oid, a.attnum))
    });
    if !not_null {
        return false;
    }
    let wanted: BTreeSet<(&str, &str)> = equalities
        .iter()
        .map(|e| (e.child_col, e.parent_col))
        .collect();
    if !enforced_key(catalog, child.oid, parent.oid, &wanted) {
        return false;
    }
    let (Some(child_types), Some(parent_types)) = (
        equalities
            .iter()
            .map(|e| {
                catalog
                    .attribute_by_name(child.oid, e.child_col)
                    .map(|a| a.atttypid)
            })
            .collect::<Option<Vec<_>>>(),
        equalities
            .iter()
            .map(|e| {
                catalog
                    .attribute_by_name(parent.oid, e.parent_col)
                    .map(|a| a.atttypid)
            })
            .collect::<Option<Vec<_>>>(),
    ) else {
        return false;
    };
    let checked: Vec<FkEquality<'_>> = equalities
        .iter()
        .zip(child_types.into_iter().zip(parent_types))
        .map(|(e, (child_type, parent_type))| FkEquality {
            parent_col: e.parent_col,
            child_col: e.child_col,
            parent_type,
            child_type,
            parent_left: e.parent_left,
        })
        .collect();
    fk_equalities_hold(catalog, child.oid, parent.oid, &checked)
}

/// Whether each row of the left-hand side of `… LEFT JOIN joined ON
/// <equalities>` matches at most one row of `joined`: its scan holds only
/// the rows its unique indexes cover, and the equalities — every conjunct
/// of the condition, each `joined.k = other.c` with `other.c` a column of
/// the same type as `k` under a deterministic collation, compared through
/// a btree equality — equate every column of a full unique key of it
/// through that key's own operator family
/// ([`dedup::equalities_cover_unique_key`]). A NULL never equals anything,
/// so the key's NULLs don't matter, and row security only hides rows.
fn left_join_matches_at_most_one(
    ctx: &Ctx<'_>,
    sel: &ast::SelectStmt,
    joined: &Rel<'_>,
    equalities: &[KeyEquality<'_>],
) -> bool {
    if !scans_own_rows(ctx, joined) {
        return false;
    }
    let catalog = ctx.catalog;
    let scope = FromScope::of_select(ctx, sel, None);
    let mut equated = Vec::with_capacity(equalities.len());
    for e in equalities {
        let Some(attr) = catalog.attribute_by_name(joined.oid, e.parent_col) else {
            return false;
        };
        let other = if e.parent_left { e.right } else { e.left };
        let Some((other_type, deterministic)) = scope.resolve_value(ctx, other) else {
            return false;
        };
        if other_type != attr.atttypid || !deterministic {
            return false;
        }
        let Some(op) = catalog.find_operator("=", Some(attr.atttypid), attr.atttypid) else {
            return false;
        };
        let btree_equality = catalog
            .pg_amop
            .iter()
            .any(|a| a.amopmethod == "btree" && a.amopstrategy == 3 && a.amopopr == op.oid);
        if !btree_equality {
            return false;
        }
        equated.push((attr.attnum, op.oid));
    }
    dedup::equalities_cover_unique_key(ctx, joined.oid, &equated)
}

/// Whether `child` has a foreign key into `parent` over exactly the
/// `(child column, parent column)` pairs `wanted` that PG enforces at every
/// statement: enforced, validated, NOT DEFERRABLE, not temporal — and so is
/// each clone of it on a partition, on either side.
fn enforced_key(
    catalog: &PgCatalog,
    child: PgClassOid,
    parent: PgClassOid,
    wanted: &BTreeSet<(&str, &str)>,
) -> bool {
    let attname = |rel, attnum: i16| {
        catalog
            .attributes_of(rel)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.as_str())
    };
    let strict = |con: &crate::pg_catalog::PgConstraint| {
        con.conenforced
            && con.convalidated
            && !con.conperiod
            && catalog
                .fk_details
                .get(&con.oid)
                .is_some_and(|d| !d.deferrable())
    };
    catalog.pg_constraint.values().any(|con| {
        con.contype == ConType::ForeignKey
            && con.conrelid == child
            && con.confrelid == Some(parent)
            && catalog
                .fk_details
                .get(&con.oid)
                .is_some_and(|d| d.parent.is_none())
            && strict(con)
            && con.conkey.len() == wanted.len()
            && con
                .conkey
                .iter()
                .zip(&con.confkey)
                .map(|(&c, &p)| Some((attname(child, c)?, attname(parent, p)?)))
                .collect::<Option<BTreeSet<_>>>()
                .is_some_and(|pairs| pairs == *wanted)
            && {
                // The clones, at any depth.
                let mut family = vec![con.oid];
                let mut i = 0;
                let mut all_strict = true;
                while i < family.len() {
                    for (oid, d) in catalog.fk_details.iter() {
                        if d.parent == Some(family[i]) && !family.contains(oid) {
                            family.push(*oid);
                            all_strict &= catalog.pg_constraint.get(oid).is_some_and(strict);
                        }
                    }
                    i += 1;
                }
                all_strict
            }
    })
}

// ─── Sites ────────────────────────────────────────────────────────────────

struct Site<'c, 'a> {
    ctx: &'c Ctx<'a>,
    root: NodeRef<'a>,
    tokens: Tokens<'a>,
}

impl Site<'_, '_> {
    /// The key equalities `quals` consists of, when every conjunct is one,
    /// all against the same child range name.
    fn all_key_equalities<'n>(
        &self,
        quals: &[Option<&'n ast::Node>],
        parent: &str,
    ) -> Option<Vec<KeyEquality<'n>>> {
        let eqs: Vec<KeyEquality<'n>> = quals
            .iter()
            .map(|q| key_equality(*q, parent))
            .collect::<Option<_>>()?;
        let child = eqs.first()?.child;
        eqs.iter().all(|e| e.child == child).then_some(eqs)
    }

    /// The JOIN form at every INNER JoinExpr of the tree `node` whose
    /// right-hand side is the parent.
    fn join_sites(&self, node: Option<&ast::Node>, out: &mut Vec<Candidate>) {
        let Some(Node::JoinExpr(j)) = node else {
            return;
        };
        for side in [&j.larg, &j.rarg] {
            self.join_sites(side.as_ref(), out);
        }
        if let Some(edits) = self.join_site(j)
            && self.tokens.clear_of_comments(&edits)
        {
            out.push(Candidate {
                rule: Rule::FkJoin,
                edits,
            });
        }
    }

    /// The [`RedundantJoin`]s of every JoinExpr of the tree `node`.
    fn redundant_joins(
        &self,
        sel: &ast::SelectStmt,
        node: Option<&ast::Node>,
        out: &mut Vec<(RedundantJoin, std::ops::Range<usize>)>,
    ) {
        let Some(Node::JoinExpr(j)) = node else {
            return;
        };
        for side in [&j.larg, &j.rarg] {
            self.redundant_joins(sel, side.as_ref(), out);
        }
        let fk = if self.ctx.catalog.fk_checks_bypassed {
            None
        } else {
            self.fk_join(j)
                .map(|(rv, eqs)| (rv, eqs, RedundantJoinProof::ForeignKey))
        };
        let Some((rv, equalities, proof)) = fk.or_else(|| {
            self.unique_left_join(sel, j)
                .map(|(rv, eqs)| (rv, eqs, RedundantJoinProof::UniqueKey))
        }) else {
            return;
        };
        let left = proof == RedundantJoinProof::UniqueKey;
        let Some((first, edit)) = self.join_cut(rv, &equalities, left) else {
            return;
        };
        let Some(start) = self.tokens.start(first) else {
            return;
        };
        let fix = self
            .tokens
            .clear_of_comments(std::slice::from_ref(&edit))
            .then(|| edit.range.clone());
        out.push((
            RedundantJoin {
                span: start..edit.range.end,
                relation: refname(rv).to_owned(),
                proof,
                fix,
            },
            edit.range,
        ));
    }

    fn join_site(&self, j: &ast::JoinExpr) -> Option<Vec<Edit>> {
        let (prv, equalities) = self.fk_join(j)?;
        let (_, edit) = self.join_cut(prv, &equalities, false)?;
        Some(vec![edit])
    }

    /// The parent and the key equalities of an INNER JoinExpr whose
    /// right-hand side is a parent its left-hand side has a foreign key to,
    /// when the join keeps every child row exactly once
    /// ([`join_is_identity`]) and nothing else reads the parent.
    fn fk_join<'j>(
        &self,
        j: &'j ast::JoinExpr,
    ) -> Option<(&'j ast::RangeVar, Vec<KeyEquality<'j>>)> {
        if j.jointype != JoinType::JoinInner || !plain_join(j) {
            return None;
        }
        let Some(Node::RangeVar(prv)) = j.rarg.as_ref() else {
            return None;
        };
        let parent = relation(self.ctx, prv)?;
        j.quals.as_ref()?;
        let quals = conjuncts(j.quals.as_ref());
        let equalities = self.all_key_equalities(&quals, refname(prv))?;
        let Found::Here(crv) = find_range(j.larg.as_ref(), equalities[0].child) else {
            return None;
        };
        let child = relation(self.ctx, crv)?;
        if !join_is_identity(self.ctx, &child, &parent, &equalities) {
            return None;
        }
        let skip: Vec<i32> = equalities.iter().map(|e| e.location).collect();
        if parent_used(self.ctx, self.root, &parent, &skip) {
            return None;
        }
        Some((prv, equalities))
    }

    /// The joined table and the equalities of a LEFT JoinExpr whose
    /// right-hand side is a table each left-hand row matches at most one
    /// row of ([`left_join_matches_at_most_one`]), when nothing else reads
    /// it: the join then keeps every left-hand row exactly once, and adds
    /// nothing to it.
    fn unique_left_join<'j>(
        &self,
        sel: &ast::SelectStmt,
        j: &'j ast::JoinExpr,
    ) -> Option<(&'j ast::RangeVar, Vec<KeyEquality<'j>>)> {
        if j.jointype != JoinType::JoinLeft || !plain_join(j) {
            return None;
        }
        let Some(Node::RangeVar(prv)) = j.rarg.as_ref() else {
            return None;
        };
        let joined = relation(self.ctx, prv)?;
        j.quals.as_ref()?;
        let equalities = conjuncts(j.quals.as_ref())
            .into_iter()
            .map(|q| key_equality(q, refname(prv)))
            .collect::<Option<Vec<_>>>()?;
        if equalities.is_empty()
            || !left_join_matches_at_most_one(self.ctx, sel, &joined, &equalities)
        {
            return None;
        }
        let skip: Vec<i32> = equalities.iter().map(|e| e.location).collect();
        if parent_used(self.ctx, self.root, &joined, &skip) {
            return None;
        }
        Some((prv, equalities))
    }

    /// The edit cutting the join of `prv` on `equalities`, from the end of
    /// the left-hand side through the end of the ON, with the index of the
    /// join's first keyword: `[INNER] JOIN`, or `LEFT [OUTER] JOIN` when
    /// `left`.
    fn join_cut(
        &self,
        prv: &ast::RangeVar,
        equalities: &[KeyEquality<'_>],
        left: bool,
    ) -> Option<(usize, Edit)> {
        let t = &self.tokens;
        let mut first = self.relation_first_token(prv)?;
        if !t.is(first.checked_sub(1)?, "join") {
            return None;
        }
        first -= 1;
        // A keyword before it, unless it is the left-hand side's alias (`AS
        // inner`).
        let keyword = |first: usize, word: &str| {
            Some(t.is(first.checked_sub(1)?, word) && !t.is(first.checked_sub(2)?, "as"))
        };
        if left {
            if keyword(first, "outer")? {
                first -= 1;
            }
            if !keyword(first, "left")? {
                return None;
            }
            first -= 1;
        } else if keyword(first, "inner")? {
            first -= 1;
        }
        let start = t.end(first.checked_sub(1)?)?;
        let on = (first..t.spans.len()).find(|&i| t.is(i, "on"))?;
        let end = self.conjuncts_end(on, equalities)?;
        Some((
            first,
            Edit {
                range: start..end,
                text: String::new(),
            },
        ))
    }

    /// The comma form: from item `i` the parent, and key equalities among
    /// the WHERE's top-level conjuncts.
    fn comma_site(
        &self,
        sel: &ast::SelectStmt,
        i: usize,
        prv: &ast::RangeVar,
        where_clause: Option<&ast::Node>,
    ) -> Option<Vec<Edit>> {
        let parent = relation(self.ctx, prv)?;
        let pname = refname(prv);
        let all = conjuncts(where_clause);
        let keyed: Vec<(usize, KeyEquality<'_>)> = all
            .iter()
            .enumerate()
            .filter_map(|(k, q)| Some((k, key_equality(*q, pname)?)))
            .collect();
        let child_name = keyed.first()?.1.child;
        if keyed.iter().any(|(_, e)| e.child != child_name) {
            return None;
        }
        // The child: in another FROM item, never NULL-extended.
        let mut found = None;
        for (k, item) in sel.from_clause.iter().enumerate() {
            if k == i {
                continue;
            }
            match find_range(item.as_ref(), child_name) {
                Found::Here(rv) if found.is_none() => found = Some(rv),
                Found::Absent => {}
                _ => return None,
            }
        }
        let child = relation(self.ctx, found?)?;
        let removed: Vec<usize> = keyed.iter().map(|(k, _)| *k).collect();
        let equalities: Vec<KeyEquality<'_>> = keyed.into_iter().map(|(_, e)| e).collect();
        if !join_is_identity(self.ctx, &child, &parent, &equalities) {
            return None;
        }
        let skip: Vec<i32> = equalities.iter().map(|e| e.location).collect();
        if parent_used(self.ctx, self.root, &parent, &skip) {
            return None;
        }
        let mut edits = vec![self.cut_from_item(sel, i, prv)?];
        edits.extend(self.where_cuts(all.len(), &removed, &equalities)?);
        Some(edits)
    }

    /// The first token of the relation `rv`: its name, or the `ONLY` and
    /// parenthesis before it.
    fn relation_first_token(&self, rv: &ast::RangeVar) -> Option<usize> {
        let t = &self.tokens;
        let mut first = t.at(rv.location)?;
        if !rv.inh {
            if first > 0 && t.is(first - 1, "(") {
                first -= 1;
            }
            if !t.is(first.checked_sub(1)?, "only") {
                return None;
            }
            first -= 1;
        }
        Some(first)
    }

    /// The edit removing FROM item `i` (a relation) and the comma next to
    /// it.
    fn cut_from_item(&self, sel: &ast::SelectStmt, i: usize, rv: &ast::RangeVar) -> Option<Edit> {
        let t = &self.tokens;
        let first = self.relation_first_token(rv)?;
        // The relation's last token: the one before the `,` or the WHERE
        // that follows it (a relation holds neither), comments aside.
        let after = (first..t.spans.len()).find(|&k| t.is(k, ",") || t.is(k, "where"))?;
        let mut last = after.checked_sub(1)?;
        while last > first && t.is_comment(last) {
            last -= 1;
        }
        let range = if i > 0 {
            if !t.is(first.checked_sub(1)?, ",") {
                return None;
            }
            t.end(first.checked_sub(2)?)?..t.end(last)?
        } else {
            if sel.from_clause.len() < 2 || !t.is(after, ",") {
                return None;
            }
            t.start(first)?..t.start(after + 1)?
        };
        Some(Edit {
            range,
            text: String::new(),
        })
    }

    /// The end of a condition after keyword token `kw` (`ON`) whose
    /// conjuncts are exactly `equalities`, in order: the last one's end,
    /// with the parentheses opened since `kw` closed.
    fn conjuncts_end(&self, kw: usize, equalities: &[KeyEquality<'_>]) -> Option<usize> {
        let t = &self.tokens;
        let spans = equalities
            .iter()
            .map(|e| t.equality(e))
            .collect::<Option<Vec<_>>>()?;
        let (first, _) = *spans.iter().min()?;
        let (_, mut last) = *spans.iter().max()?;
        let open = first.checked_sub(kw + 1)?;
        if !(kw + 1..first).all(|k| t.is(k, "(")) {
            return None;
        }
        for _ in 0..open {
            last += 1;
            if !t.is(last, ")") {
                return None;
            }
        }
        // Nothing of the condition after it.
        if t.is(last + 1, "and") || t.is(last + 1, "or") {
            return None;
        }
        t.end(last)
    }

    /// The edits removing the conjuncts `removed` (indices into the
    /// WHERE's `count` top-level conjuncts, whose key equalities are
    /// `equalities`, in the same order) — the whole WHERE when it's all of
    /// them.
    fn where_cuts(
        &self,
        count: usize,
        removed: &[usize],
        equalities: &[KeyEquality<'_>],
    ) -> Option<Vec<Edit>> {
        let t = &self.tokens;
        let spans = equalities
            .iter()
            .map(|e| t.equality(e))
            .collect::<Option<Vec<_>>>()?;
        // The conjuncts must be in text order, each one's span its own.
        if spans.windows(2).any(|w| w[0].1 >= w[1].0) {
            return None;
        }
        let cut = |range| Edit {
            range,
            text: String::new(),
        };
        if removed.len() == count {
            // `WHERE` and the whole condition.
            let (mut first, _) = *spans.first()?;
            while first > 0 && t.is(first - 1, "(") {
                first -= 1;
            }
            let kw = first.checked_sub(1)?;
            if !t.is(kw, "where") {
                return None;
            }
            let end = self.conjuncts_end(kw, equalities)?;
            return Some(vec![cut(t.end(kw.checked_sub(1)?)?..end)]);
        }
        // Group the removed conjuncts into runs of consecutive ones.
        let mut edits = Vec::new();
        let mut k = 0;
        while k < removed.len() {
            let mut run_end = k;
            while run_end + 1 < removed.len() && removed[run_end + 1] == removed[run_end] + 1 {
                run_end += 1;
            }
            let (first, _) = spans[k];
            let (_, last) = spans[run_end];
            if removed[run_end] + 1 < count {
                // `a AND ` up to the next conjunct.
                if !t.is(last + 1, "and") {
                    return None;
                }
                edits.push(cut(t.start(first)?..t.start(last + 2)?));
            } else {
                // The trailing run: ` AND a` after the previous conjunct.
                if !t.is(first.checked_sub(1)?, "and") {
                    return None;
                }
                edits.push(cut(t.end(first.checked_sub(2)?)?..t.end(last)?));
            }
            k = run_end + 1;
        }
        Some(edits)
    }
}
