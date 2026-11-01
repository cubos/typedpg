//! [`Rule::CountExists`]: a count compared with zero becomes `EXISTS`.
//!
//! "Is there any row?" written as a count makes PG count every matching
//! row before comparing; `EXISTS` stops at the first one. The forms:
//!
//! ```sql
//! (SELECT count(*) FROM t WHERE p) > 0    →  EXISTS (SELECT 1 FROM t WHERE p)
//! (SELECT count(*) FROM t WHERE p) >= 1   →  EXISTS (SELECT 1 FROM t WHERE p)
//! (SELECT count(*) FROM t WHERE p) <> 0   →  EXISTS (SELECT 1 FROM t WHERE p)
//! (SELECT count(*) FROM t WHERE p) = 0    →  NOT EXISTS (SELECT 1 FROM t WHERE p)
//! SELECT count(*) > 0 FROM t WHERE p      →  SELECT EXISTS (SELECT 1 FROM t WHERE p) AS "?column?"
//! ```
//!
//! and their mirrors (`0 < (…)`, `1 <= (…)`, `0 <> (…)`, `0 = (…)`).
//!
//! Why the result is the same: a counting query with no `GROUP BY`,
//! `HAVING`, window, `DISTINCT`, `LIMIT`/`OFFSET` (nor anything else that
//! changes its row count) returns exactly one row, the number of rows its
//! FROM and WHERE produce — never NULL. Compared with 0 or 1 through the
//! built-in `int8`/`int4` operators, that is true exactly when there is a
//! row, which is what `EXISTS` says (also never NULL). `count(x)` counts
//! the rows where `x` is not NULL: the same rows when `x` provably can't
//! be NULL, and `count(DISTINCT x)` is nonzero exactly when `count(x)` is.
//!
//! What is not rewritten, and why:
//! - `count(…) FILTER (WHERE f)`: the equivalent `EXISTS` moves `f` into
//!   the WHERE, where the planner may evaluate it on rows the original
//!   never evaluates it on (rows `p` rejects) — an error the original
//!   doesn't raise. A rewrite must never make a succeeding query fail.
//! - `count(x ORDER BY …)`, `WITHIN GROUP`, `VARIADIC`, an outer-level
//!   `count(outer.col)`: not the plain aggregate of this level.
//! - A `count` or comparison operator some user object could resolve to:
//!   only `pg_catalog.count` and the built-in `int8`/`int4` comparisons.
//! - A counting query anything in could raise an error — a cast, an
//!   arithmetic operator, a function call, a comparison across types or
//!   collations ([`super::proof::error_free_select`]). `EXISTS` doesn't
//!   just evaluate fewer rows: the planner may pull it up into a semi- or
//!   anti-join and evaluate the subquery's conditions over rows the
//!   correlated count never reached (`1 / s.z > 0` pushed down to every
//!   row of `s`, not only those with `s.k = o.k`), and a top-level
//!   `EXISTS` gets a plan of its own. A rewrite must never make a
//!   succeeding query fail.
//! - Anything that may call a volatile function, or reads a relation
//!   other than a plain table (a view or a CTE may hide one; a foreign
//!   table may do anything): how many times it runs may change.
//! - Row security on a read table (its policies run per row, too).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, AExprKind, Node, ResTarget, SelectStmt, SetOperation, SubLinkType, Token, node,
};

use super::proof::{
    FromScope, cannot_error, cheap_siblings, error_free_select, has_volatile, strictly_not_null,
};
use super::{Candidate, Ctx, Edit, Rule};
use crate::oid::{PgOperatorOid, PgProcOid};
use crate::pg_catalog::{RelKind, oid};

/// `pg_catalog.count(*)` and `pg_catalog.count("any")`.
const COUNT_STAR: PgProcOid = PgProcOid::from_raw(2803);
const COUNT_ANY: PgProcOid = PgProcOid::from_raw(2147);

/// What a count compared with a literal says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Test {
    /// There is a row: `EXISTS`.
    Any,
    /// There is none: `NOT EXISTS`.
    None,
}

impl Test {
    /// What closes [`Test::keyword`] after the subquery.
    fn close(self) -> &'static str {
        match self {
            Test::Any => "",
            Test::None => ")",
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Test::Any => "EXISTS",
            // Parenthesized, with a `)` after the subquery (see
            // `sublink_form`): `NOT` binds looser than `IS [NOT] NULL`,
            // `IS [NOT] DISTINCT FROM` and the like, so a bare `NOT EXISTS
            // (…) IS NOT NULL` would test the EXISTS, not the comparison.
            Test::None => "(NOT EXISTS",
        }
    }
}

/// The test `count <op> lit` (count on the left) makes, and the built-in
/// `int8 <op> int4` operator it must resolve to.
fn count_left(op: &str, lit: i32) -> Option<(Test, u32)> {
    // pg_operator: int84gt 419, int84ge 430, int84ne 417, int84eq 416.
    Some(match (op, lit) {
        (">", 0) => (Test::Any, 419),
        (">=", 1) => (Test::Any, 430),
        ("<>", 0) => (Test::Any, 417),
        ("=", 0) => (Test::None, 416),
        _ => return None,
    })
}

/// The test `lit <op> count` (count on the right) makes, and the built-in
/// `int4 <op> int8` operator it must resolve to.
fn count_right(op: &str, lit: i32) -> Option<(Test, u32)> {
    // pg_operator: int48lt 37, int48le 80, int48ne 36, int48eq 15.
    Some(match (op, lit) {
        ("<", 0) => (Test::Any, 37),
        ("<=", 1) => (Test::Any, 80),
        ("<>", 0) => (Test::Any, 36),
        ("=", 0) => (Test::None, 15),
        _ => return None,
    })
}

/// A scanned token: its byte range and kind. Comments are dropped.
#[derive(Clone, Copy)]
struct Tok {
    start: usize,
    end: usize,
    kind: i32,
}

impl Tok {
    fn is(self, t: Token) -> bool {
        self.kind == t as i32
    }
}

struct Text<'a> {
    sql: &'a str,
    toks: Vec<Tok>,
}

impl<'a> Text<'a> {
    fn new(sql: &'a str) -> Option<Self> {
        let scan = typedpg_pg_query::scan(sql).ok()?;
        let toks = scan
            .tokens
            .iter()
            .filter(|t| t.token != Token::SqlComment as i32 && t.token != Token::CComment as i32)
            .map(|t| {
                Some(Tok {
                    start: usize::try_from(t.start).ok()?,
                    end: usize::try_from(t.end).ok()?,
                    kind: t.token,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Text { sql, toks })
    }

    /// The index of the `)` closing the parenthesis token `open`.
    fn matching_close(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, tok) in self.toks.iter().enumerate().skip(open) {
            if tok.is(Token::Ascii40) {
                depth += 1;
            } else if tok.is(Token::Ascii41) {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    }

    /// The index of the token starting at `at`.
    fn at(&self, at: i32) -> Option<usize> {
        let at = usize::try_from(at).ok()?;
        self.toks.iter().position(|t| t.start == at)
    }

    /// The index of the `)` closing the `(` at index `open`.
    fn close(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, t) in self.toks.iter().enumerate().skip(open) {
            if t.is(Token::Ascii40) {
                depth += 1;
            } else if t.is(Token::Ascii41) {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    }

    /// The byte range of the call starting at token `at`: its name through
    /// its closing parenthesis.
    fn call(&self, at: i32) -> Option<std::ops::Range<usize>> {
        let first = self.at(at)?;
        let open = (first..self.toks.len()).find(|&i| self.toks[i].is(Token::Ascii40))?;
        let close = self.close(open)?;
        Some(self.toks[first].start..self.toks[close].end)
    }

    /// Whether the token at `at` is the integer literal `0` or `1`, written
    /// so, and which.
    fn digit(&self, at: i32) -> Option<i32> {
        let t = self.toks[self.at(at)?];
        if !t.is(Token::Iconst) {
            return None;
        }
        match &self.sql[t.start..t.end] {
            "0" => Some(0),
            "1" => Some(1),
            _ => None,
        }
    }

    /// Where the output column (target-list or RETURNING entry) starting at
    /// token `at` ends: before the first token, outside every parenthesis
    /// and bracket it opens, that ends a target-list entry — a `,`, a `)`
    /// closing a group around the list, or a clause keyword (a `FROM` that
    /// is not `IS DISTINCT FROM`'s) — or at the statement's end.
    fn target_end(&self, at: i32) -> Option<usize> {
        let first = self.at(at)?;
        let mut depth = 0usize;
        let mut end = None;
        for (i, t) in self.toks.iter().enumerate().skip(first) {
            if t.is(Token::Ascii40) || t.is(Token::Ascii91) {
                depth += 1;
            } else if t.is(Token::Ascii41) || t.is(Token::Ascii93) {
                let Some(d) = depth.checked_sub(1) else { break };
                depth = d;
            } else if depth == 0 && ends_target(*t, self.toks.get(i.wrapping_sub(1)).copied()) {
                break;
            }
            end = Some(t.end);
        }
        (depth == 0).then_some(end).flatten()
    }
}

/// Whether `t`, outside any group of the entry, ends a target-list entry
/// (`prev`: the token before it).
fn ends_target(t: Tok, prev: Option<Tok>) -> bool {
    if t.is(Token::From) {
        return !prev.is_some_and(|p| p.is(Token::Distinct));
    }
    [
        Token::Ascii44,
        Token::Ascii59,
        Token::Into,
        Token::Where,
        Token::GroupP,
        Token::Having,
        Token::Window,
        Token::Order,
        Token::Limit,
        Token::Offset,
        Token::Fetch,
        Token::For,
        Token::Union,
        Token::Intersect,
        Token::Except,
        Token::On,
        Token::Returning,
    ]
    .iter()
    .any(|&k| t.is(k))
}

/// The rewrites of every count compared with zero in the query.
pub(crate) fn candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
    let [stmt] = ctx.tree.stmts.as_slice() else {
        return Vec::new();
    };
    let Some(text) = Text::new(ctx.sql) else {
        return Vec::new();
    };
    let nodes = ctx.tree.nodes();
    // A reference to one of these may read a CTE rather than a table.
    let cte_names: Vec<&str> = nodes
        .iter()
        .filter_map(|(n, _)| match n {
            NodeRef::CommonTableExpr(c) => Some(c.ctename.as_str()),
            _ => None,
        })
        .collect();
    // Every unnamed output column, at any level (target lists, RETURNING):
    // PG names one after its expression (FigureColname), and a rewrite
    // inside it may change that name — `(x > 0)::int` is `int4`, `(EXISTS
    // …)::int` is `exists` — even where the driver can't see it (a FROM
    // subquery's column names reach `row_to_json`).
    let unnamed: Vec<&ResTarget> = nodes
        .iter()
        .flat_map(|(n, _)| match n {
            NodeRef::SelectStmt(s) => s.target_list.iter().collect::<Vec<_>>(),
            NodeRef::InsertStmt(s) => returning(s.returning_clause.as_ref()),
            NodeRef::UpdateStmt(s) => returning(s.returning_clause.as_ref()),
            NodeRef::DeleteStmt(s) => returning(s.returning_clause.as_ref()),
            NodeRef::MergeStmt(s) => returning(s.returning_clause.as_ref()),
            _ => Vec::new(),
        })
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::ResTarget(rt)) if rt.name.is_empty() && rt.val.is_some() => {
                Some(&**rt)
            }
            _ => None,
        })
        .collect();

    let mut walker = Walker {
        ctx,
        text: &text,
        cte_names: &cte_names,
        unnamed: &unnamed,
        out: Vec::new(),
    };
    if let Some(node) = stmt.stmt.as_deref().and_then(|n| n.node.as_ref()) {
        walker.statement(node.to_ref(), None);
    }
    let mut out = walker.out;
    if let Some(edits) = top_level_form(ctx, &text, &cte_names, stmt) {
        out.push(Candidate {
            rule: Rule::CountExists,
            edits,
        });
    }
    out
}

/// Where an expression is evaluated, for the sibling-conjunct guard.
#[derive(Clone, Copy)]
enum Clause<'n> {
    /// A target list, ORDER BY, VALUES, function arguments…: evaluated as
    /// written, never pulled up into a join.
    Free,
    /// The WHERE, HAVING or JOIN ... ON condition rooted at this node.
    In(&'n Node),
    /// A position the walker doesn't model (MERGE, ON CONFLICT): no
    /// rewrite.
    Unknown,
}

/// Walks the statement keeping the FROM scope each expression is evaluated
/// in, which the error-freedom proof of a counted subquery resolves its
/// outer references in. Where the scope isn't known, an opaque one stands
/// in: nothing resolves, nothing is proved.
struct Walker<'c, 'a> {
    ctx: &'c Ctx<'a>,
    text: &'c Text<'c>,
    cte_names: &'c [&'c str],
    unnamed: &'c [&'c ResTarget],
    out: Vec<Candidate>,
}

impl Walker<'_, '_> {
    /// A statement nested at `parent`'s level.
    fn statement(&mut self, node: NodeRef<'_>, parent: Option<&FromScope<'_>>) {
        let ctx = self.ctx;
        match node {
            NodeRef::SelectStmt(s) => self.select(s, parent),
            NodeRef::InsertStmt(i) => {
                self.ctes(i.with_clause.as_ref(), parent);
                if let Some(s) = i.select_stmt.as_deref().and_then(|s| s.node.as_ref()) {
                    self.statement(s.to_ref(), parent);
                }
                let scope = FromScope::of_dml(ctx, i.relation.as_ref(), &[], parent);
                let opaque = FromScope::opaque(parent);
                if let Some(c) = i.on_conflict_clause.as_deref() {
                    self.children(NodeRef::OnConflictClause(c), &opaque, Clause::Unknown);
                }
                self.returning(i.returning_clause.as_ref(), &scope);
            }
            NodeRef::UpdateStmt(u) => {
                self.ctes(u.with_clause.as_ref(), parent);
                self.walk_from_subqueries(&u.from_clause, parent);
                let scope = FromScope::of_dml(ctx, u.relation.as_ref(), &u.from_clause, parent);
                self.list(&u.target_list, &scope, Clause::Free);
                self.clause(u.where_clause.as_deref(), &scope);
                self.returning(u.returning_clause.as_ref(), &scope);
            }
            NodeRef::DeleteStmt(d) => {
                self.ctes(d.with_clause.as_ref(), parent);
                self.walk_from_subqueries(&d.using_clause, parent);
                let scope = FromScope::of_dml(ctx, d.relation.as_ref(), &d.using_clause, parent);
                self.clause(d.where_clause.as_deref(), &scope);
                self.returning(d.returning_clause.as_ref(), &scope);
            }
            // Anything else (MERGE, utility statements): no known scope.
            other => {
                let opaque = FromScope::opaque(parent);
                self.children(other, &opaque, Clause::Unknown);
            }
        }
    }

    fn ctes(&mut self, with: Option<&protobuf::WithClause>, parent: Option<&FromScope<'_>>) {
        for cte in with.map(|w| w.ctes.as_slice()).unwrap_or_default() {
            if let Some(node::Node::CommonTableExpr(c)) = cte.node.as_ref()
                && let Some(q) = c.ctequery.as_deref().and_then(|q| q.node.as_ref())
            {
                self.statement(q.to_ref(), parent);
            }
        }
    }

    /// The non-LATERAL subqueries of a FROM list (in joins too), which see
    /// the levels around the FROM's, not its own.
    fn walk_from_subqueries(&mut self, from: &[Node], parent: Option<&FromScope<'_>>) {
        for item in from {
            match item.node.as_ref() {
                Some(node::Node::RangeSubselect(s)) if !s.lateral => {
                    if let Some(q) = s.subquery.as_deref().and_then(|q| q.node.as_ref()) {
                        self.statement(q.to_ref(), parent);
                    }
                }
                Some(node::Node::JoinExpr(j)) => {
                    for side in [&j.larg, &j.rarg].into_iter().flatten() {
                        self.walk_from_subqueries(std::slice::from_ref(&**side), parent);
                    }
                }
                _ => {}
            }
        }
    }

    /// The rest of a FROM list: join conditions, LATERAL subqueries and
    /// FROM functions, each in the scope PG gives it — an ON condition sees
    /// its own join's two sides, a LATERAL item (a FROM function is one)
    /// the items to its left; both see the levels around (`parent`), never
    /// the rest of the FROM, whose names may shadow an outer one.
    fn walk_from_rest(&mut self, from: &[Node], parent: Option<&FromScope<'_>>) {
        let mut left: Vec<Node> = Vec::new();
        for item in from {
            self.walk_from_item(item, &mut left, parent);
        }
    }

    /// One FROM item, `left` holding the items before it (and getting it).
    fn walk_from_item(
        &mut self,
        item: &Node,
        left: &mut Vec<Node>,
        parent: Option<&FromScope<'_>>,
    ) {
        match item.node.as_ref() {
            Some(node::Node::RangeSubselect(s)) => {
                if s.lateral
                    && let Some(q) = s.subquery.as_deref().and_then(|q| q.node.as_ref())
                {
                    let scope = FromScope::of_items(self.ctx, left, parent);
                    self.statement(q.to_ref(), Some(&scope));
                }
            }
            Some(node::Node::JoinExpr(j)) => {
                let mark = left.len();
                // The right side sees the left one (and what precedes the
                // join): a LATERAL there may read it.
                for side in [&j.larg, &j.rarg].into_iter().flatten() {
                    self.walk_from_item(side, left, parent);
                }
                let sides: Vec<Node> = [&j.larg, &j.rarg]
                    .into_iter()
                    .flatten()
                    .map(|s| (**s).clone())
                    .collect();
                let scope = FromScope::of_items(self.ctx, &sides, parent);
                self.clause(j.quals.as_deref(), &scope);
                left.truncate(mark);
            }
            Some(node::Node::RangeVar(_)) => {}
            Some(n) => {
                let scope = FromScope::of_items(self.ctx, left, parent);
                self.children(n.to_ref(), &scope, Clause::Free);
            }
            None => {}
        }
        left.push(item.clone());
    }

    fn select(&mut self, s: &SelectStmt, parent: Option<&FromScope<'_>>) {
        self.ctes(s.with_clause.as_ref(), parent);
        if s.op != SetOperation::SetopNone as i32 {
            for side in [&s.larg, &s.rarg].into_iter().flatten() {
                self.select(side, parent);
            }
            return;
        }
        self.walk_from_subqueries(&s.from_clause, parent);
        let scope = FromScope::of_select(self.ctx, s, parent);
        self.walk_from_rest(&s.from_clause, parent);
        for list in [
            &s.target_list,
            &s.group_clause,
            &s.window_clause,
            &s.sort_clause,
            &s.distinct_clause,
            &s.values_lists,
        ] {
            self.list(list, &scope, Clause::Free);
        }
        for n in [&s.limit_count, &s.limit_offset] {
            self.opt(n.as_deref(), &scope, Clause::Free);
        }
        self.clause(s.where_clause.as_deref(), &scope);
        self.clause(s.having_clause.as_deref(), &scope);
    }

    fn returning(&mut self, clause: Option<&protobuf::ReturningClause>, scope: &FromScope<'_>) {
        if let Some(c) = clause {
            self.list(&c.exprs, scope, Clause::Free);
        }
    }

    /// A WHERE, HAVING or JOIN ... ON condition.
    fn clause(&mut self, node: Option<&Node>, scope: &FromScope<'_>) {
        if let Some(n) = node {
            self.opt(Some(n), scope, Clause::In(n));
        }
    }

    fn opt(&mut self, node: Option<&Node>, scope: &FromScope<'_>, clause: Clause<'_>) {
        if let Some(n) = node.and_then(|n| n.node.as_ref()) {
            self.expr(n.to_ref(), scope, clause);
        }
    }

    fn list(&mut self, nodes: &[Node], scope: &FromScope<'_>, clause: Clause<'_>) {
        for n in nodes {
            self.opt(Some(n), scope, clause);
        }
    }

    fn children(&mut self, node: NodeRef<'_>, scope: &FromScope<'_>, clause: Clause<'_>) {
        for child in node.children() {
            self.expr(child, scope, clause);
        }
    }

    /// Look for count comparisons in an expression evaluated in `scope`,
    /// within `clause`.
    fn expr(&mut self, node: NodeRef<'_>, scope: &FromScope<'_>, clause: Clause<'_>) {
        match node {
            NodeRef::SelectStmt(_)
            | NodeRef::InsertStmt(_)
            | NodeRef::UpdateStmt(_)
            | NodeRef::DeleteStmt(_)
            | NodeRef::MergeStmt(_) => self.statement(node, Some(scope)),
            NodeRef::AExpr(e) => {
                // In a WHERE, HAVING or ON, the count's SubPlan may be what
                // keeps a costlier sibling conjunct from running on a row:
                // pulled up into a join, it no longer does.
                let guarded = match clause {
                    Clause::Free => true,
                    Clause::In(root) => cheap_siblings(self.ctx, root, e.location),
                    Clause::Unknown => false,
                };
                if guarded
                    && let Some(edits) =
                        sublink_form(self.ctx, self.text, self.cte_names, self.unnamed, e, scope)
                {
                    self.out.push(Candidate {
                        rule: Rule::CountExists,
                        edits,
                    });
                }
                self.children(node, scope, clause);
            }
            _ => self.children(node, scope, clause),
        }
    }
}

/// Whether `select`, a counting query, can be run over rows it never ran
/// on before without raising an error ([`error_free_select`]) — its count
/// target aside, which the rewrite drops (`count(x)`'s `x` must be
/// error-free too).
fn counted_error_free(ctx: &Ctx<'_>, select: &SelectStmt, scope: &FromScope<'_>) -> bool {
    let mut without_count = select.clone();
    without_count.target_list = vec![Node {
        node: Some(node::Node::ResTarget(Box::new(ResTarget {
            val: Some(Box::new(Node {
                node: Some(node::Node::AConst(protobuf::AConst {
                    isnull: false,
                    location: -1,
                    val: Some(protobuf::a_const::Val::Ival(protobuf::Integer { ival: 1 })),
                })),
            })),
            ..Default::default()
        }))),
    }];
    error_free_select(ctx, &without_count, scope)
}

/// The comparison `e`, split into its operator, its count side and its
/// literal (`0` or `1`), with the test it makes — when the operator
/// resolves to the built-in `int8`/`int4` comparison the test needs.
struct Comparison<'a> {
    test: Test,
    /// The count side is the left operand.
    count_left: bool,
    count: &'a Node,
    /// The literal's location.
    lit_at: i32,
}

fn comparison<'a>(
    ctx: &Ctx<'_>,
    text: &Text<'_>,
    e: &'a protobuf::AExpr,
) -> Option<Comparison<'a>> {
    if e.kind != AExprKind::AexprOp as i32 {
        return None;
    }
    // A qualified `OPERATOR(schema.op)` is left alone.
    let [name] = e.name.as_slice() else {
        return None;
    };
    let Some(node::Node::String(op)) = name.node.as_ref() else {
        return None;
    };
    let op = op.sval.as_str();
    let (l, r) = (e.lexpr.as_deref()?, e.rexpr.as_deref()?);
    let literal = |n: &Node| match n.node.as_ref() {
        Some(node::Node::AConst(c))
            if !c.isnull && matches!(c.val, Some(protobuf::a_const::Val::Ival(_))) =>
        {
            text.digit(c.location).map(|d| (d, c.location))
        }
        _ => None,
    };
    let (count_left, count, (_, lit_at), (test, builtin)) = if let Some(l_lit) = literal(r) {
        (true, l, l_lit, count_left(op, l_lit.0)?)
    } else {
        let r_lit = literal(l)?;
        (false, r, r_lit, count_right(op, r_lit.0)?)
    };
    // The operator the comparison resolves to: count is `bigint`, the
    // literal `integer`.
    let (left, right) = if count_left {
        (oid::INT8, oid::INT4)
    } else {
        (oid::INT4, oid::INT8)
    };
    let resolved = ctx.catalog.find_operator(op, Some(left), right)?;
    if resolved.oid != PgOperatorOid::from_raw(builtin) {
        return None;
    }
    Some(Comparison {
        test,
        count_left,
        count,
        lit_at,
    })
}

/// The count call `n` of the query `select`, when it is `pg_catalog.count`
/// of this level counting every row its FROM and WHERE produce (`count(*)`,
/// `count(x)` or `count(DISTINCT x)` of an `x` that can't be NULL): the
/// byte range of the call.
fn plain_count(
    ctx: &Ctx<'_>,
    text: &Text<'_>,
    select: &SelectStmt,
    n: &Node,
) -> Option<std::ops::Range<usize>> {
    let Some(node::Node::FuncCall(fc)) = n.node.as_ref() else {
        return None;
    };
    let names: Option<Vec<&str>> = fc
        .funcname
        .iter()
        .map(|f| match f.node.as_ref() {
            Some(node::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    let schema = match names?.as_slice() {
        ["count"] => None,
        ["pg_catalog", "count"] => Some("pg_catalog"),
        _ => return None,
    };
    // Every `count` the name may resolve to is the built-in aggregate.
    let procs = ctx.catalog.find_functions(schema, "count");
    if procs.is_empty()
        || procs
            .iter()
            .any(|p| p.oid != COUNT_STAR && p.oid != COUNT_ANY)
    {
        return None;
    }
    if fc.agg_filter.is_some()
        || fc.over.is_some()
        || !fc.agg_order.is_empty()
        || fc.agg_within_group
        || fc.func_variadic
    {
        return None;
    }
    if fc.agg_star {
        if !fc.args.is_empty() || fc.agg_distinct {
            return None;
        }
    } else {
        // `count(x)`: the rows where `x` isn't NULL — all of them, when
        // `x` can't be. `x` must be of this level (an outer column makes
        // the aggregate the outer query's).
        let [arg] = fc.args.as_slice() else {
            return None;
        };
        let scope = FromScope::of_select(ctx, select, None);
        if !strictly_not_null(ctx, arg, &scope) || !cannot_error(ctx, arg, &scope) {
            return None;
        }
    }
    text.call(fc.location)
}

/// Whether `select` is a plain query of one row per evaluation: no
/// clause that changes how many rows an aggregate-only target list gives.
fn one_row_aggregate(select: &SelectStmt) -> bool {
    select.op == SetOperation::SetopNone as i32
        && select.larg.is_none()
        && select.rarg.is_none()
        && select.distinct_clause.is_empty()
        && select.into_clause.is_none()
        && select.group_clause.is_empty()
        && select.having_clause.is_none()
        && select.window_clause.is_empty()
        && select.values_lists.is_empty()
        && select.sort_clause.is_empty()
        && select.limit_offset.is_none()
        && select.limit_count.is_none()
        && select.locking_clause.is_empty()
        && select.with_clause.is_none()
}

/// Whether evaluating `node` fewer times can't be told apart: it calls
/// nothing volatile and reads only plain tables without row security
/// (whose policies run per row). A CTE read from inside `node` is refused
/// too: [`has_volatile`] doesn't look into one declared outside it, and
/// `EXISTS` reads a materialized CTE's rows only as far as it needs them.
fn pure(ctx: &Ctx<'_>, cte_names: &[&str], node: &Node) -> bool {
    if has_volatile(ctx, node) {
        return false;
    }
    let Some(inner) = node.node.as_ref() else {
        return true;
    };
    inner.nodes().iter().all(|(n, _)| match n {
        NodeRef::RangeVar(rv) => {
            if rv.schemaname.is_empty() && cte_names.contains(&rv.relname.as_str()) {
                return false;
            }
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            ctx.catalog
                .resolve_table(schema, &rv.relname)
                .is_some_and(|c| {
                    matches!(
                        c.relkind,
                        RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
                    ) && !ctx.catalog.row_security.contains(&c.oid)
                })
        }
        _ => true,
    })
}

/// `(SELECT count(*) …) > 0` and its other forms.
fn sublink_form(
    ctx: &Ctx<'_>,
    text: &Text<'_>,
    cte_names: &[&str],
    unnamed: &[&ResTarget],
    e: &protobuf::AExpr,
    scope: &FromScope<'_>,
) -> Option<Vec<Edit>> {
    let cmp = comparison(ctx, text, e)?;
    let Some(node::Node::SubLink(sub)) = cmp.count.node.as_ref() else {
        return None;
    };
    if sub.sub_link_type != SubLinkType::ExprSublink as i32 || sub.testexpr.is_some() {
        return None;
    }
    let Some(node::Node::SelectStmt(select)) = sub.subselect.as_deref()?.node.as_ref() else {
        return None;
    };
    if !one_row_aggregate(select) {
        return None;
    }
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(node::Node::ResTarget(target)) = target.node.as_ref() else {
        return None;
    };
    if !target.indirection.is_empty() {
        return None;
    }
    let call = plain_count(ctx, text, select, target.val.as_deref()?)?;
    // The planner may pull `EXISTS` up into a semi- or anti-join and run
    // the subquery's conditions over rows the correlated count never
    // reached (`1 / s.z > 0` on every row of `s`, not just those with
    // `s.k = o.k`): only a subquery nothing in can fail is rewritten.
    if !pure(ctx, cte_names, cmp.count)
        || !counted_error_free(ctx, select, &FromScope::of_select(ctx, select, Some(scope)))
    {
        return None;
    }

    let sub_at = usize::try_from(sub.location).ok()?;
    let sub_tok = text.at(sub.location)?;
    if !text.toks[sub_tok].is(Token::Ascii40) {
        return None;
    }
    let op_tok = text.at(e.location)?;
    let lit_tok = text.at(cmp.lit_at)?;
    let mut edits = vec![Edit {
        range: call,
        text: "1".to_owned(),
    }];
    // `EXISTS` where the subquery opens; a space keeps it apart from a
    // keyword written against the parenthesis (`AND(SELECT …`).
    let keyword = |at: usize| {
        let spaced = at > 0
            && !matches!(
                ctx.sql.as_bytes()[at - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b','
            );
        format!("{}{} ", if spaced { " " } else { "" }, cmp.test.keyword())
    };
    let lit_start = text.toks[lit_tok].start;
    if cmp.count_left {
        // `(…) > 0`: drop the operator and the literal, from the end of
        // the token before the operator.
        let from = text.toks[op_tok.checked_sub(1)?].end;
        let to = text.toks[lit_tok].end;
        edits.push(Edit {
            range: sub_at..sub_at,
            text: keyword(sub_at),
        });
        edits.push(Edit {
            range: from..to,
            text: cmp.test.close().to_owned(),
        });
    } else {
        // `0 < (…)`: drop the literal and the operator, up to the token
        // after the operator.
        let to = text.toks.get(op_tok + 1)?.start;
        if cmp.test == Test::None {
            let close = text.toks[text.matching_close(sub_tok)?].end;
            edits.push(Edit {
                range: close..close,
                text: cmp.test.close().to_owned(),
            });
        }
        if to == sub_at {
            edits.push(Edit {
                range: lit_start..to,
                text: format!("{} ", cmp.test.keyword()),
            });
        } else {
            edits.push(Edit {
                range: lit_start..to,
                text: String::new(),
            });
            edits.push(Edit {
                range: sub_at..sub_at,
                text: keyword(sub_at),
            });
        }
    }
    // Every unnamed output column the comparison is in keeps the name PG
    // gave it.
    let site = sub_at.min(lit_start);
    let mut names = Vec::new();
    for rt in unnamed {
        let start = usize::try_from(rt.location).ok()?;
        if site < start {
            continue;
        }
        let end = text.target_end(rt.location)?;
        if site >= end {
            continue;
        }
        let val = rt.val.as_deref()?;
        // A `*` FigureColname would name after a column of what it expands
        // to: not a name this level alone tells.
        if val.node.as_ref()?.nodes().iter().any(|(n, _)| {
            matches!(n, NodeRef::ColumnRef(c)
                if c.fields.iter().any(|f| matches!(f.node, Some(node::Node::AStar(_)))))
        }) {
            return None;
        }
        let name = crate::resolve::target_resname(rt);
        edits.push(Edit {
            range: end..end,
            text: format!(" AS {}", typedpg_core::quote_identifier(&name)),
        });
        names.push((start, name));
    }
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    named_as_before(ctx.sql, &edits, &names).then_some(edits)
}

/// Whether `sql` edited by `edits` parses with an output column named
/// `name` where each `(start, name)` of `names` started: the aliases landed
/// on the columns they are for.
fn named_as_before(sql: &str, edits: &[Edit], names: &[(usize, String)]) -> bool {
    if names.is_empty() {
        return true;
    }
    let mut out = String::with_capacity(sql.len() + 64);
    let mut last = 0;
    for e in edits {
        if e.range.start < last || e.range.end > sql.len() {
            return false;
        }
        out.push_str(&sql[last..e.range.start]);
        out.push_str(&e.text);
        last = e.range.end;
    }
    out.push_str(&sql[last..]);
    // Where an offset of `sql` moved to.
    let moved = |at: usize| {
        edits
            .iter()
            .filter(|e| e.range.end <= at && !(e.range.is_empty() && e.range.start == at))
            .fold(at as isize, |pos, e| {
                pos + e.text.len() as isize - e.range.len() as isize
            })
    };
    let Ok(tree) = typedpg_pg_query::parse(&out) else {
        return false;
    };
    let targets: Vec<(i32, &str)> = tree
        .protobuf
        .nodes()
        .into_iter()
        .filter_map(|(n, _)| match n {
            NodeRef::ResTarget(rt) => Some((rt.location, rt.name.as_str())),
            _ => None,
        })
        .collect();
    names.iter().all(|(start, name)| {
        let at = moved(*start);
        targets
            .iter()
            .any(|(loc, n)| *loc as isize == at && n == name)
    })
}

/// `SELECT count(*) > 0 FROM t WHERE p` → `SELECT EXISTS (SELECT 1 FROM t
/// WHERE p) AS "?column?"`, the column named as before.
fn top_level_form(
    ctx: &Ctx<'_>,
    text: &Text<'_>,
    cte_names: &[&str],
    stmt: &protobuf::RawStmt,
) -> Option<Vec<Edit>> {
    let stmt_node = stmt.stmt.as_deref()?;
    let Some(node::Node::SelectStmt(select)) = stmt_node.node.as_ref() else {
        return None;
    };
    if !one_row_aggregate(select) || select.from_clause.is_empty() {
        return None;
    }
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(node::Node::ResTarget(target)) = target.node.as_ref() else {
        return None;
    };
    let Some(node::Node::AExpr(e)) = target.val.as_deref()?.node.as_ref() else {
        return None;
    };
    let cmp = comparison(ctx, text, e)?;
    plain_count(ctx, text, select, cmp.count)?;
    // `EXISTS` gets its own plan (one that stops early), which may
    // evaluate a join's conditions over other row pairs. No sibling
    // conjunct guard ([`cheap_siblings`]) is needed: the count isn't a
    // conjunct of anything — the WHERE is the counted query's own, moved
    // whole into the EXISTS, and `error_free_select` already keeps every
    // conjunct of it free of sublinks, functions and errors.
    if !pure(ctx, cte_names, stmt_node)
        || !counted_error_free(ctx, select, &FromScope::of_select(ctx, select, None))
    {
        return None;
    }
    let [column] = ctx.analyzed.columns.as_slice() else {
        return None;
    };
    // The statement is `SELECT <target> FROM …`, nothing around it.
    let start = usize::try_from(stmt.stmt_location).ok()?;
    let end = match usize::try_from(stmt.stmt_len).ok()? {
        0 => ctx.sql.len(),
        len => start + len,
    };
    let in_stmt: Vec<usize> = (0..text.toks.len())
        .filter(|&i| text.toks[i].start >= start && text.toks[i].end <= end)
        .filter(|&i| !text.toks[i].is(Token::Ascii59))
        .collect();
    let (&first, &last) = (in_stmt.first()?, in_stmt.last()?);
    let target_tok = text.at(target.location)?;
    if !text.toks[first].is(Token::Select) || target_tok != first + 1 {
        return None;
    }
    // The FROM ending the target list.
    let mut depth = 0usize;
    let from = text.toks[target_tok..=last].iter().find(|t| {
        if t.is(Token::Ascii40) {
            depth += 1;
        } else if t.is(Token::Ascii41) {
            depth = depth.saturating_sub(1);
        }
        depth == 0 && t.is(Token::From)
    })?;
    let stmt_end = text.toks[last].end;
    Some(vec![
        Edit {
            range: text.toks[target_tok].start..from.start,
            text: format!("{} (SELECT 1 ", cmp.test.keyword()),
        },
        Edit {
            range: stmt_end..stmt_end,
            text: format!(
                "){} AS {}",
                cmp.test.close(),
                typedpg_core::quote_identifier(&column.name)
            ),
        },
    ])
}

/// A DML statement's RETURNING entries.
fn returning(clause: Option<&protobuf::ReturningClause>) -> Vec<&Node> {
    clause.map_or_else(Vec::new, |c| c.exprs.iter().collect())
}
