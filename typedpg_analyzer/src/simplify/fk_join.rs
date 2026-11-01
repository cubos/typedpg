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
//! - the key is enforced, validated and NOT DEFERRABLE (a deferred key may
//!   be violated inside a transaction), and so is each partition's clone;
//! - the migrations never disabled the triggers enforcing it (`DISABLE
//!   TRIGGER ALL` on either table, even if enabled again later: rows
//!   written meanwhile were never checked) nor set
//!   `session_replication_role`, which turns them off too;
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
use typedpg_pg_query::protobuf::{
    self, AExprKind, BoolExprType, JoinType, SetOperation, node::Node,
};

use super::proof::{column_not_null, has_volatile};
use super::{Candidate, Ctx, Edit, Rule};
use crate::ddl::tables::inherit::{all_inheritors, children_of};
use crate::oid::PgClassOid;
use crate::pg_catalog::{ConType, PgCatalog, RelKind};
use crate::resolve::row_guarantees::{FkEquality, fk_equalities_hold, fk_parent_ok};

pub(crate) fn candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let [raw] = ctx.tree.stmts.as_slice() else {
        return Vec::new();
    };
    let Some(root) = raw.stmt.as_deref().and_then(|n| n.node.as_ref()) else {
        return Vec::new();
    };
    let Node::SelectStmt(sel) = root else {
        return Vec::new();
    };
    if SetOperation::try_from(sel.op) != Ok(SetOperation::SetopNone)
        || sel.with_clause.is_some()
        || sel.into_clause.is_some()
        || !sel.locking_clause.is_empty()
        || !sel.values_lists.is_empty()
        || ctx.catalog.fk_checks_bypassed
    {
        return Vec::new();
    }
    let Some(stmt) = raw.stmt.as_deref() else {
        return Vec::new();
    };
    // Views and row locking count as volatile there too.
    if has_volatile(ctx, stmt) || has_using_or_natural(root) {
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
        site.join_sites(item, &mut out);
    }
    if let Some(quals) = sel.where_clause.as_deref() {
        for (i, item) in sel.from_clause.iter().enumerate() {
            if let Some(Node::RangeVar(rv)) = item.node.as_ref()
                && let Some(edits) = site.comma_site(sel, i, rv, quals)
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

// ─── Text ─────────────────────────────────────────────────────────────────

/// The SQL's lexical tokens, as `(start, end)` byte ranges.
struct Tokens<'a> {
    sql: &'a str,
    spans: Vec<(usize, usize)>,
}

impl<'a> Tokens<'a> {
    fn scan(sql: &'a str) -> Option<Self> {
        let scanned = typedpg_pg_query::scan(sql).ok()?;
        let spans = scanned
            .tokens
            .iter()
            .map(|t| Some((usize::try_from(t.start).ok()?, usize::try_from(t.end).ok()?)))
            .collect::<Option<Vec<_>>>()?;
        Some(Tokens { sql, spans })
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
    fn column_ref(&self, cr: &protobuf::ColumnRef) -> Option<(usize, usize)> {
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
fn qualified(n: &protobuf::Node) -> Option<(&protobuf::ColumnRef, &str, &str)> {
    let Some(Node::ColumnRef(cr)) = n.node.as_ref() else {
        return None;
    };
    match cr.fields.as_slice() {
        [q, c] => match (q.node.as_ref()?, c.node.as_ref()?) {
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
    left: &'a protobuf::ColumnRef,
    right: &'a protobuf::ColumnRef,
    parent_col: &'a str,
    child: &'a str,
    child_col: &'a str,
    parent_left: bool,
}

/// `node` as an equality between a column of `parent` (by its range
/// name) and a column of another range name.
fn key_equality<'a>(node: &'a protobuf::Node, parent: &str) -> Option<KeyEquality<'a>> {
    let Some(Node::AExpr(e)) = node.node.as_ref() else {
        return None;
    };
    if AExprKind::try_from(e.kind) != Ok(AExprKind::AexprOp) {
        return None;
    }
    match e.name.as_slice() {
        [op] if matches!(op.node.as_ref(), Some(Node::String(s)) if s.sval == "=") => {}
        _ => return None,
    }
    let (left, lq, lc) = qualified(e.lexpr.as_deref()?)?;
    let (right, rq, rc) = qualified(e.rexpr.as_deref()?)?;
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
fn conjuncts(quals: &protobuf::Node) -> Vec<&protobuf::Node> {
    match quals.node.as_ref() {
        Some(Node::BoolExpr(b))
            if BoolExprType::try_from(b.boolop) == Ok(BoolExprType::AndExpr) =>
        {
            b.args.iter().collect()
        }
        _ => vec![quals],
    }
}

/// A table the query reads by name, as the rule can reason about it.
struct Rel<'a> {
    rv: &'a protobuf::RangeVar,
    oid: PgClassOid,
    relkind: RelKind,
}

fn refname(rv: &protobuf::RangeVar) -> &str {
    rv.alias
        .as_ref()
        .map_or(rv.relname.as_str(), |a| a.aliasname.as_str())
}

/// The plain or partitioned table `rv` names. A column alias list renames
/// the columns the names below are matched against, so it isn't followed.
fn relation<'a>(ctx: &Ctx<'_>, rv: &'a protobuf::RangeVar) -> Option<Rel<'a>> {
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
    Here(&'a protobuf::RangeVar),
    Absent,
    /// Present where its rows may be NULL-extended, or behind something
    /// the rule doesn't follow.
    Unusable,
}

/// The range named `name` in the join tree `node`, when every row of the
/// tree has one of its rows (it's never on the nullable side of an outer
/// join).
fn find_range<'a>(node: &'a protobuf::Node, name: &str) -> Found<'a> {
    match node.node.as_ref() {
        Some(Node::RangeVar(rv)) if refname(rv) == name => Found::Here(rv),
        Some(Node::RangeVar(_)) => Found::Absent,
        Some(Node::JoinExpr(j)) => {
            if j.alias.is_some() {
                // The alias hides the names inside.
                return Found::Unusable;
            }
            let kind = JoinType::try_from(j.jointype);
            let sides = [
                (
                    j.larg.as_deref(),
                    matches!(kind, Ok(JoinType::JoinInner | JoinType::JoinLeft)),
                ),
                (
                    j.rarg.as_deref(),
                    matches!(kind, Ok(JoinType::JoinInner | JoinType::JoinRight)),
                ),
            ];
            for (side, preserved) in sides {
                match side.map_or(Found::Absent, |s| find_range(s, name)) {
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
    root.nodes().iter().any(
        |(n, _)| matches!(n, NodeRef::JoinExpr(j) if j.is_natural || !j.using_clause.is_empty()),
    )
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
                    match f.node.as_ref() {
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
        quals: &[&'n protobuf::Node],
        parent: &str,
    ) -> Option<Vec<KeyEquality<'n>>> {
        let eqs: Vec<KeyEquality<'n>> = quals
            .iter()
            .map(|q| key_equality(q, parent))
            .collect::<Option<_>>()?;
        let child = eqs.first()?.child;
        eqs.iter().all(|e| e.child == child).then_some(eqs)
    }

    /// The JOIN form at every INNER JoinExpr of the tree `node` whose
    /// right-hand side is the parent.
    fn join_sites(&self, node: &protobuf::Node, out: &mut Vec<Candidate>) {
        let Some(Node::JoinExpr(j)) = node.node.as_ref() else {
            return;
        };
        for side in [j.larg.as_deref(), j.rarg.as_deref()].into_iter().flatten() {
            self.join_sites(side, out);
        }
        if let Some(edits) = self.join_site(j) {
            out.push(Candidate {
                rule: Rule::FkJoin,
                edits,
            });
        }
    }

    fn join_site(&self, j: &protobuf::JoinExpr) -> Option<Vec<Edit>> {
        if JoinType::try_from(j.jointype) != Ok(JoinType::JoinInner)
            || j.is_natural
            || !j.using_clause.is_empty()
            || j.alias.is_some()
            || j.join_using_alias.is_some()
        {
            return None;
        }
        let Some(Node::RangeVar(prv)) = j.rarg.as_deref()?.node.as_ref() else {
            return None;
        };
        let parent = relation(self.ctx, prv)?;
        let quals = conjuncts(j.quals.as_deref()?);
        let equalities = self.all_key_equalities(&quals, refname(prv))?;
        let Found::Here(crv) = find_range(j.larg.as_deref()?, equalities[0].child) else {
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
        // Cut from the end of the left-hand side to the end of the ON.
        let t = &self.tokens;
        let mut first = self.relation_first_token(prv)?;
        if !t.is(first.checked_sub(1)?, "join") {
            return None;
        }
        first -= 1;
        // `INNER`, unless it is the left-hand side's alias (`AS inner`).
        if t.is(first.checked_sub(1)?, "inner") && !t.is(first.checked_sub(2)?, "as") {
            first -= 1;
        }
        let start = t.end(first.checked_sub(1)?)?;
        let on = (first..t.spans.len()).find(|&i| t.is(i, "on"))?;
        let end = self.conjuncts_end(on, &equalities)?;
        Some(vec![Edit {
            range: start..end,
            text: String::new(),
        }])
    }

    /// The comma form: from item `i` the parent, and key equalities among
    /// the WHERE's top-level conjuncts.
    fn comma_site(
        &self,
        sel: &protobuf::SelectStmt,
        i: usize,
        prv: &protobuf::RangeVar,
        where_clause: &protobuf::Node,
    ) -> Option<Vec<Edit>> {
        let parent = relation(self.ctx, prv)?;
        let pname = refname(prv);
        let all = conjuncts(where_clause);
        let keyed: Vec<(usize, KeyEquality<'_>)> = all
            .iter()
            .enumerate()
            .filter_map(|(k, q)| Some((k, key_equality(q, pname)?)))
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
            match find_range(item, child_name) {
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
    fn relation_first_token(&self, rv: &protobuf::RangeVar) -> Option<usize> {
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
    fn cut_from_item(
        &self,
        sel: &protobuf::SelectStmt,
        i: usize,
        rv: &protobuf::RangeVar,
    ) -> Option<Edit> {
        let t = &self.tokens;
        let first = self.relation_first_token(rv)?;
        // The relation's last token: the one before the `,` or the WHERE
        // that follows it (a relation holds neither).
        let after = (first..t.spans.len()).find(|&k| t.is(k, ",") || t.is(k, "where"))?;
        let last = after.checked_sub(1)?;
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
