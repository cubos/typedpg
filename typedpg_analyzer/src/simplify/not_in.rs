//! `x NOT IN (SELECT y …)` → `NOT EXISTS (SELECT 1 … WHERE x = y)`.
//!
//! PG plans a `NOT EXISTS` as an anti-join, but keeps a `NOT IN` a SubPlan
//! (`pull_up_sublinks` converts an ANY sublink only when it isn't under a
//! NOT): hashed while the subquery's rows fit in `work_mem`, rescanned for
//! every outer row past it — quadratic. The two differ only on NULLs (a
//! NULL `x`, or a NULL `y` among the rows, makes `NOT IN` NULL or false
//! where `NOT EXISTS` is true), so the rewrite is exact when neither side
//! can be NULL:
//!
//! - every left-hand expression and every subquery output is a column
//!   reference [`strictly_not_null`] proves never NULL: a validated NOT
//!   NULL on a base table, not made nullable by an outer join or a
//!   grouping set. Being plain columns, they also can't fail where the
//!   original didn't evaluate them (the rewrite may evaluate `x` for an
//!   outer row the original skipped over an empty subquery, and `y` before
//!   the subquery's own WHERE).
//! - `NOT IN` is `NOT (x = ANY (sub))`: the `=` written in the rewrite,
//!   between operands of the same types and collations, resolves to the
//!   same operator — a built-in one, never NULL between non-NULL
//!   operands of built-in types. Row-wise `(a, b) NOT IN (SELECT x, y …)` compares
//!   column by column with `=`, as PG's row comparison does.
//! - nothing in it is volatile (the subquery may run a different number
//!   of times), and no table read has row security.
//! - nothing the rewrite may newly evaluate can fail: an anti-join filters
//!   the subquery's rows by its WHERE before (or without) looking at the
//!   outer row, where the SubPlan evaluated it per outer row, in cost
//!   order, or not at all over an empty outer side. So the subquery reads
//!   only stored tables, and every expression in it is a column,
//!   constant, parameter, boolean combination, NULL / boolean test or a
//!   comparison through a built-in operator needing no cast
//!   ([`super::proof::error_free_select`]). A subquery with a cast,
//!   arithmetic, a function call or a pattern match is left as written,
//!   even where it couldn't fail on the data at hand.
//! - the NOT IN is in a SELECT's target list, WHERE or HAVING, or in an
//!   UPDATE / DELETE's WHERE (or SET) — where [`FromScope`] knows what
//!   its columns are — and not repeated in a GROUP BY, ORDER BY or
//!   DISTINCT ON that must match it.
//!
//! The subquery is kept as written. A plain one (a FROM, optionally a
//! WHERE, nothing after) is inlined — `NOT EXISTS (SELECT 1 FROM … WHERE
//! (<where>) AND x = y)` — so PG can make an anti-join even when it is
//! correlated; any other (LIMIT, DISTINCT, GROUP BY, …) is wrapped whole —
//! `NOT EXISTS (SELECT 1 FROM (<sub>) AS __typedpg_s1 (__typedpg_c1, …)
//! WHERE x = __typedpg_s1.__typedpg_c1)` — so its clauses still apply to
//! it. Inlining places `x` inside the subquery's scope, so it is done only
//! when none of the subquery's FROM items can capture `x`'s columns.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    BoolExprType, ColumnRef, Node, SelectStmt, SetOperation, SubLinkType, Token, node::Node as N,
};

use super::proof::{
    FromScope, builtin_strict_equality, column_type, embed_source, error_free_select, has_volatile,
    qualified_column, strictly_not_null, written_names_use,
};
use super::{Candidate, Ctx, Edit, Rule};

/// The names the rewrite introduces: `__typedpg_s<n>` (a new one for each
/// wrapped subquery) and its columns `__typedpg_c<i>`. A query whose
/// written SQL already uses the prefix is left alone, so they can't
/// collide with its own names.
const RESERVED_PREFIX: &str = "__typedpg";

pub(crate) fn candidates(ctx: &Ctx<'_>) -> Vec<Candidate> {
    // Most queries have no `IN (SELECT …)` at all: nothing to scan for.
    let has_site = ctx.tree.nodes().iter().any(|(n, _)| {
        matches!(n, NodeRef::SubLink(s)
            if s.sub_link_type() == SubLinkType::AnySublink && s.oper_name.is_empty())
    });
    // The guard is on the names of the SQL as written (decoded: a
    // `U&"…"` identifier too): the current SQL may hold the aliases of
    // earlier rewrites.
    if !has_site || written_names_use(ctx, RESERVED_PREFIX) {
        return Vec::new();
    }
    let Ok(scanned) = typedpg_pg_query::scan(ctx.sql) else {
        return Vec::new();
    };
    // Comments aren't tokens of the grammar: leaving them out, a slice
    // from one token to another never ends in one.
    let tokens: Vec<Tok> = scanned
        .tokens
        .iter()
        .filter(|t| !matches!(t.token(), Token::SqlComment | Token::CComment))
        .filter_map(|t| {
            Some(Tok {
                start: usize::try_from(t.start).ok()?,
                end: usize::try_from(t.end).ok()?,
                token: t.token(),
            })
        })
        .collect();
    let mut walker = Walker {
        ctx,
        tokens: &tokens,
        out: Vec::new(),
        next_alias: std::cell::Cell::new(next_alias(ctx.sql)),
    };
    for stmt in &ctx.tree.stmts {
        if let Some(node) = stmt.stmt.as_deref() {
            walker.statement(node, None);
        }
    }
    walker.out
}

#[derive(Clone, Copy)]
struct Tok {
    start: usize,
    end: usize,
    token: Token,
}

struct Walker<'c, 'a> {
    ctx: &'c Ctx<'a>,
    tokens: &'c [Tok],
    out: Vec<Candidate>,
    /// The number of the next wrapped subquery's alias.
    next_alias: std::cell::Cell<usize>,
}

/// The first `n` no `__typedpg_s<n>` of earlier rewrites in `sql` uses.
fn next_alias(sql: &str) -> usize {
    let prefix = format!("{RESERVED_PREFIX}_s");
    sql.match_indices(&prefix)
        .filter_map(|(at, _)| {
            let digits: String = sql[at + prefix.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<usize>().ok()
        })
        .max()
        .map_or(1, |n| n + 1)
}

impl Walker<'_, '_> {
    /// A statement nested at `parent`'s level: a SELECT, or a DML whose
    /// WHERE has a known scope.
    fn statement(&mut self, node: &Node, parent: Option<&FromScope<'_>>) {
        match node.node.as_ref() {
            Some(N::SelectStmt(s)) => self.select(s, parent),
            Some(N::InsertStmt(i)) => {
                self.ctes(i.with_clause.as_ref(), parent);
                if let Some(s) = i.select_stmt.as_deref() {
                    self.statement(s, parent);
                }
            }
            Some(N::UpdateStmt(u)) => {
                self.ctes(u.with_clause.as_ref(), parent);
                self.walk_from_subqueries(&u.from_clause, parent);
                let scope =
                    FromScope::of_dml(self.ctx, u.relation.as_ref(), &u.from_clause, parent);
                self.expr_list(&u.target_list, &scope);
                self.expr_opt(u.where_clause.as_deref(), &scope);
            }
            Some(N::DeleteStmt(d)) => {
                self.ctes(d.with_clause.as_ref(), parent);
                self.walk_from_subqueries(&d.using_clause, parent);
                let scope =
                    FromScope::of_dml(self.ctx, d.relation.as_ref(), &d.using_clause, parent);
                self.expr_opt(d.where_clause.as_deref(), &scope);
            }
            _ => {}
        }
    }

    fn ctes(
        &mut self,
        with: Option<&typedpg_pg_query::protobuf::WithClause>,
        parent: Option<&FromScope<'_>>,
    ) {
        for cte in with.map(|w| w.ctes.as_slice()).unwrap_or_default() {
            if let Some(N::CommonTableExpr(c)) = cte.node.as_ref()
                && let Some(q) = c.ctequery.as_deref()
            {
                self.statement(q, parent);
            }
        }
    }

    /// The non-LATERAL subqueries of a FROM list (in joins too), which see
    /// the levels around the FROM's, not its own.
    fn walk_from_subqueries(&mut self, from: &[Node], parent: Option<&FromScope<'_>>) {
        for item in from {
            match item.node.as_ref() {
                Some(N::RangeSubselect(s)) if !s.lateral => {
                    if let Some(q) = s.subquery.as_deref() {
                        self.statement(q, parent);
                    }
                }
                Some(N::JoinExpr(j)) => {
                    for side in [&j.larg, &j.rarg].into_iter().flatten() {
                        self.walk_from_subqueries(std::slice::from_ref(&**side), parent);
                    }
                }
                _ => {}
            }
        }
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
        self.expr_opt(s.where_clause.as_deref(), &scope);
        // A GROUP BY, ORDER BY or DISTINCT ON expression is matched against
        // the target list's: rewriting one side only would break that.
        let matched = [&s.group_clause, &s.sort_clause, &s.distinct_clause]
            .into_iter()
            .flatten()
            .any(contains_sublink);
        if !matched {
            self.expr_list(&s.target_list, &scope);
            self.expr_opt(s.having_clause.as_deref(), &scope);
        }
    }

    fn expr_opt(&mut self, node: Option<&Node>, scope: &FromScope<'_>) {
        if let Some(n) = node.and_then(|n| n.node.as_ref()) {
            self.expr(n.to_ref(), scope);
        }
    }

    fn expr_list(&mut self, nodes: &[Node], scope: &FromScope<'_>) {
        for n in nodes {
            self.expr_opt(Some(n), scope);
        }
    }

    /// Look for NOT IN sites in an expression evaluated in `scope`.
    fn expr(&mut self, node: NodeRef<'_>, scope: &FromScope<'_>) {
        match node {
            NodeRef::SubLink(s) => {
                self.expr_opt(s.testexpr.as_deref(), scope);
                if let Some(q) = s.subselect.as_deref() {
                    self.statement(q, Some(scope));
                }
            }
            NodeRef::BoolExpr(b) => {
                if b.boolop() == BoolExprType::NotExpr
                    && let [arg] = b.args.as_slice()
                    && let Some(N::SubLink(s)) = arg.node.as_ref()
                    && let Some(edit) = self.site(b.location, s, scope)
                {
                    self.out.push(Candidate {
                        rule: Rule::NotIn,
                        edits: vec![edit],
                    });
                }
                self.expr_list(&b.args, scope);
            }
            // Nested statements are reached through their SubLink.
            NodeRef::SelectStmt(_) => {}
            _ => {
                for child in node.children() {
                    self.expr(child, scope);
                }
            }
        }
    }

    /// The rewrite of `NOT <sublink>` (the NOT at `not_at`), if it is a
    /// provable `x NOT IN (SELECT y …)`.
    fn site(
        &self,
        not_at: i32,
        s: &typedpg_pg_query::protobuf::SubLink,
        scope: &FromScope<'_>,
    ) -> Option<Edit> {
        let ctx = self.ctx;
        // `IN` syntax: an ANY sublink with no operator written, which PG
        // compares with `=`.
        if s.sub_link_type() != SubLinkType::AnySublink || !s.oper_name.is_empty() {
            return None;
        }
        let testexpr = s.testexpr.as_deref()?;
        let (lhs, lhs_at): (Vec<&ColumnRef>, i32) = match testexpr.node.as_ref()? {
            N::ColumnRef(c) => (vec![c], c.location),
            N::RowExpr(r) => (
                r.args
                    .iter()
                    .map(|a| match a.node.as_ref() {
                        Some(N::ColumnRef(c)) => Some(c),
                        _ => None,
                    })
                    .collect::<Option<_>>()?,
                r.location,
            ),
            _ => return None,
        };
        let Some(N::SelectStmt(sub)) = s.subselect.as_deref()?.node.as_ref() else {
            return None;
        };
        if sub.op != SetOperation::SetopNone as i32
            || !sub.values_lists.is_empty()
            || !sub.locking_clause.is_empty()
            || sub.into_clause.is_some()
        {
            return None;
        }
        let rhs: Vec<&ColumnRef> = sub
            .target_list
            .iter()
            .map(|t| match t.node.as_ref() {
                Some(N::ResTarget(r)) if r.indirection.is_empty() => {
                    match r.val.as_deref()?.node.as_ref() {
                        Some(N::ColumnRef(c)) => Some(c),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect::<Option<_>>()?;
        if rhs.len() != lhs.len() || lhs.is_empty() {
            return None;
        }
        // Neither side can be NULL.
        let sub_scope = FromScope::of_select(ctx, sub, Some(scope));
        let col_node = |c: &ColumnRef| Node {
            node: Some(N::ColumnRef(c.clone())),
        };
        if !lhs
            .iter()
            .all(|c| strictly_not_null(ctx, &col_node(c), scope))
            || !rhs
                .iter()
                .all(|c| strictly_not_null(ctx, &col_node(c), &sub_scope))
        {
            return None;
        }
        // Nothing the rewrite may newly evaluate can fail.
        if !error_free_select(ctx, sub, &sub_scope) {
            return None;
        }
        // Nothing volatile, no row security.
        let sub_node = s.subselect.as_deref()?;
        if has_volatile(ctx, sub_node)
            || has_volatile(ctx, testexpr)
            || reads_row_security(ctx, sub_node)
        {
            return None;
        }
        for (l, r) in lhs.iter().zip(&rhs) {
            let l = scope.resolve(ctx, l)?;
            let r = sub_scope.resolve(ctx, r)?;
            if ctx.catalog.row_security.contains(&l.rel) {
                return None;
            }
            // `=` must never be NULL between non-NULL operands.
            let (lt, rt) = (
                column_type(ctx.catalog, l.rel, l.attnum)?,
                column_type(ctx.catalog, r.rel, r.attnum)?,
            );
            if !builtin_strict_equality(ctx.catalog, lt, rt) {
                return None;
            }
        }

        // The text: from the NOT or the left-hand side, whichever comes
        // first, to the subquery's closing parenthesis.
        let start = usize::try_from(not_at.min(lhs_at)).ok()?;
        let in_kw = self.token_at_or_after(usize::try_from(s.location).ok()?)?;
        let in_idx = (in_kw..self.tokens.len()).find(|&i| self.tokens[i].token == Token::InP)?;
        let open = in_idx + 1;
        if self.tokens.get(open)?.token != Token::Ascii40 {
            return None;
        }
        let close = self.matching_paren(open)?;
        let end = self.tokens[close].end;
        let first = self.token_at_or_after(start)?;
        if self.tokens[first].start != start || !self.balanced(first, close + 1) {
            return None;
        }
        // The subquery as written, without the comments around it.
        let (inner_first, inner_last) = (open + 1, close.checked_sub(1)?);
        if inner_last < inner_first {
            return None;
        }
        let inner_start = self.tokens[inner_first].start;
        let inner_end = self.tokens[inner_last].end;

        // The left-hand columns qualified by the table they read: moved
        // into the subquery (or next to the wrapper), they can't be
        // captured by a name there.
        let lhs: Vec<ColumnRef> = lhs
            .iter()
            .map(|c| qualified_column(ctx, c, scope))
            .collect::<Option<_>>()?;
        let lhs: Vec<&ColumnRef> = lhs.iter().collect();
        let lhs_text: Vec<String> = lhs.iter().map(|c| render(c)).collect::<Option<_>>()?;
        let inline = self.inline_parts(sub, &sub_scope, &lhs, inner_first, inner_last);
        let text = match inline {
            Some((from_start, where_end)) => {
                let rhs_text: Vec<String> = rhs.iter().map(|c| render(c)).collect::<Option<_>>()?;
                let eqs = equalities(&lhs_text, &rhs_text);
                match where_end {
                    Some(where_end) => format!(
                        "(NOT EXISTS (SELECT 1 {} ({}) AND {eqs}))",
                        embed_source(ctx.sql, from_start..where_end),
                        embed_source(ctx.sql, where_end..inner_end).trim_start()
                    ),
                    None => format!(
                        "(NOT EXISTS (SELECT 1 {} WHERE {eqs}))",
                        embed_source(ctx.sql, from_start..inner_end)
                    ),
                }
            }
            None => {
                let n = self.next_alias.get();
                self.next_alias.set(n + 1);
                let alias = format!("{RESERVED_PREFIX}_s{n}");
                let columns: Vec<String> = (1..=rhs.len())
                    .map(|i| format!("{RESERVED_PREFIX}_c{i}"))
                    .collect();
                let rhs_text: Vec<String> =
                    columns.iter().map(|c| format!("{alias}.{c}")).collect();
                format!(
                    "(NOT EXISTS (SELECT 1 FROM ({}) AS {alias} ({}) WHERE {}))",
                    embed_source(ctx.sql, inner_start..inner_end),
                    columns.join(", "),
                    equalities(&lhs_text, &rhs_text)
                )
            }
        };
        Some(Edit {
            range: start..end,
            text,
        })
    }

    /// Whether the subquery can be inlined — `SELECT <columns> FROM …
    /// [WHERE …]` and nothing else, `x`'s columns not capturable by its
    /// FROM items — and if so where its FROM starts and its WHERE keyword
    /// ends (`None`: no WHERE).
    fn inline_parts(
        &self,
        sub: &SelectStmt,
        sub_scope: &FromScope<'_>,
        lhs: &[&ColumnRef],
        first: usize,
        last: usize,
    ) -> Option<(usize, Option<usize>)> {
        if sub.from_clause.is_empty()
            || !sub.distinct_clause.is_empty()
            || !sub.group_clause.is_empty()
            || sub.having_clause.is_some()
            || !sub.window_clause.is_empty()
            || !sub.sort_clause.is_empty()
            || sub.limit_count.is_some()
            || sub.limit_offset.is_some()
            || sub.with_clause.is_some()
            || self.tokens[first].token != Token::Select
            || lhs.iter().any(|c| sub_scope.may_bind_here(self.ctx, c))
        {
            return None;
        }
        // The FROM and WHERE keywords at the subquery's own level.
        let mut depth = 0usize;
        let (mut from, mut where_end) = (None, None);
        for t in &self.tokens[first..=last] {
            match t.token {
                Token::Ascii40 => depth += 1,
                Token::Ascii41 => depth = depth.checked_sub(1)?,
                Token::From if depth == 0 && from.is_none() => from = Some(t.start),
                Token::Where if depth == 0 => {
                    if where_end.is_some() {
                        return None;
                    }
                    where_end = Some(t.end);
                }
                _ => {}
            }
        }
        if sub.where_clause.is_some() != where_end.is_some() {
            return None;
        }
        Some((from?, where_end))
    }

    /// The index of the first token starting at or after `at`.
    fn token_at_or_after(&self, at: usize) -> Option<usize> {
        let i = self.tokens.partition_point(|t| t.start < at);
        (i < self.tokens.len()).then_some(i)
    }

    /// The index of the `)` matching the `(` at `open`.
    fn matching_paren(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, t) in self.tokens.iter().enumerate().skip(open) {
            match t.token {
                Token::Ascii40 => depth += 1,
                Token::Ascii41 => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Whether the tokens `from..to` open as many parentheses as they
    /// close, never closing one opened before.
    fn balanced(&self, from: usize, to: usize) -> bool {
        let mut depth = 0usize;
        for t in &self.tokens[from..to] {
            match t.token {
                Token::Ascii40 => depth += 1,
                Token::Ascii41 => match depth.checked_sub(1) {
                    Some(d) => depth = d,
                    None => return false,
                },
                _ => {}
            }
        }
        depth == 0
    }
}

/// `l1 = r1 AND l2 = r2 …`, the left-hand side first as in `x = ANY`.
fn equalities(lhs: &[String], rhs: &[String]) -> String {
    lhs.iter()
        .zip(rhs)
        .map(|(l, r)| format!("{l} = {r}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// A column reference as SQL, each name quoted as needed.
fn render(c: &ColumnRef) -> Option<String> {
    let parts: Vec<String> = c
        .fields
        .iter()
        .map(|f| match f.node.as_ref() {
            Some(N::String(s)) => Some(typedpg_core::quote_identifier(&s.sval)),
            _ => None,
        })
        .collect::<Option<_>>()?;
    Some(parts.join("."))
}

/// Whether `node` holds a sublink.
fn contains_sublink(node: &Node) -> bool {
    node.node.as_ref().is_some_and(|n| {
        n.nodes()
            .iter()
            .any(|(n, _)| matches!(n, NodeRef::SubLink(_)))
    })
}

/// Whether a relation `node` reads (by name) has row security.
fn reads_row_security(ctx: &Ctx<'_>, node: &Node) -> bool {
    node.node.as_ref().is_some_and(|n| {
        n.nodes().iter().any(|(n, _)| match n {
            NodeRef::RangeVar(rv) => {
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                ctx.catalog
                    .resolve_table(schema, &rv.relname)
                    .is_some_and(|c| ctx.catalog.row_security.contains(&c.oid))
            }
            _ => false,
        })
    })
}
