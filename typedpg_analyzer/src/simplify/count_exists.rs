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
//! - Anything that may call a volatile function, or reads a relation
//!   other than a plain table (a view or a CTE may hide one; a foreign
//!   table may do anything): `EXISTS` evaluates fewer rows.
//! - Row security on a read table (its policies run per row, too).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, AExprKind, Node, ResTarget, SelectStmt, SetOperation, SubLinkType, Token, node,
};

use super::proof::{FromScope, has_volatile, strictly_not_null};
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
    fn keyword(self) -> &'static str {
        match self {
            Test::Any => "EXISTS",
            Test::None => "NOT EXISTS",
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

    /// Where the unnamed target whose value is a comparison starts at token
    /// `at` ends: past every token at its own parenthesis depth (the
    /// comparison's operator and literal, and the groups around them).
    fn comparison_end(&self, at: i32) -> Option<usize> {
        let first = self.at(at)?;
        let mut depth = 0usize;
        let mut end = None;
        for t in &self.toks[first..] {
            if t.is(Token::Ascii40) {
                depth += 1;
            } else if t.is(Token::Ascii41) {
                let Some(d) = depth.checked_sub(1) else { break };
                depth = d;
            } else if depth == 0 && !is_comparison_token(*t) {
                break;
            }
            end = Some(t.end);
        }
        (depth == 0).then_some(end).flatten()
    }
}

fn is_comparison_token(t: Tok) -> bool {
    [
        Token::Iconst,
        Token::Ascii60,
        Token::Ascii61,
        Token::Ascii62,
        Token::LessEquals,
        Token::GreaterEquals,
        Token::NotEquals,
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
    // The comparisons that are an unnamed output column: the rewrite names
    // them as PG did, since `EXISTS` would be named `exists`.
    let unnamed: Vec<(i32, &ResTarget)> = nodes
        .iter()
        .filter_map(|(n, _)| match n {
            NodeRef::ResTarget(rt) if rt.name.is_empty() && rt.indirection.is_empty() => {
                match rt.val.as_deref().and_then(|v| v.node.as_ref()) {
                    Some(node::Node::AExpr(e)) => Some((e.location, *rt)),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect();

    let mut out = Vec::new();
    for (n, _) in &nodes {
        let NodeRef::AExpr(e) = n else { continue };
        if let Some(edits) = sublink_form(ctx, &text, &cte_names, &unnamed, e) {
            out.push(Candidate {
                rule: Rule::CountExists,
                edits,
            });
        }
    }
    if let Some(edits) = top_level_form(ctx, &text, &cte_names, stmt) {
        out.push(Candidate {
            rule: Rule::CountExists,
            edits,
        });
    }
    out
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
        if !strictly_not_null(ctx, arg, &scope) {
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
    unnamed: &[(i32, &ResTarget)],
    e: &protobuf::AExpr,
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
    if !pure(ctx, cte_names, cmp.count) {
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
                b' ' | b'\t' | b'\n' | b'\r' | b'('
            );
        format!("{}{} ", if spaced { " " } else { "" }, cmp.test.keyword())
    };
    // The comparison's name, when it is an unnamed output column.
    let alias = unnamed
        .iter()
        .find(|(at, _)| *at == e.location)
        .map(|(_, rt)| {
            text.comparison_end(rt.location).map(|end| {
                (
                    end,
                    format!(" AS {}", typedpg_core::quote_identifier("?column?")),
                )
            })
        });
    let alias = match alias {
        Some(None) => return None,
        Some(Some(a)) => Some(a),
        None => None,
    };
    if cmp.count_left {
        // `(…) > 0`: drop the operator and the literal, from the end of
        // the token before the operator.
        let from = text.toks[op_tok.checked_sub(1)?].end;
        let to = text.toks[lit_tok].end;
        edits.push(Edit {
            range: sub_at..sub_at,
            text: keyword(sub_at),
        });
        match alias {
            Some((end, name)) if end == to => edits.push(Edit {
                range: from..to,
                text: name,
            }),
            Some((end, name)) => {
                edits.push(Edit {
                    range: from..to,
                    text: String::new(),
                });
                edits.push(Edit {
                    range: end..end,
                    text: name,
                });
            }
            None => edits.push(Edit {
                range: from..to,
                text: String::new(),
            }),
        }
    } else {
        // `0 < (…)`: drop the literal and the operator, up to the token
        // after the operator.
        let from = text.toks[lit_tok].start;
        let to = text.toks.get(op_tok + 1)?.start;
        if to == sub_at {
            edits.push(Edit {
                range: from..to,
                text: format!("{} ", cmp.test.keyword()),
            });
        } else {
            edits.push(Edit {
                range: from..to,
                text: String::new(),
            });
            edits.push(Edit {
                range: sub_at..sub_at,
                text: keyword(sub_at),
            });
        }
        if let Some((end, name)) = alias {
            edits.push(Edit {
                range: end..end,
                text: name,
            });
        }
    }
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    Some(edits)
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
    if !pure(ctx, cte_names, stmt_node) {
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
            text: format!(") AS {}", typedpg_core::quote_identifier(&column.name)),
        },
    ])
}
