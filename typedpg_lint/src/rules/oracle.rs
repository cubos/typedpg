//! The lint oracle (the `pg_sanity` feature): every finding's fix and
//! claim, checked against a real PostgreSQL over adversarial rows.
//!
//! [`crate::lint_query_called_as`] hands each query's findings here, with
//! their spans and fixes still in the analyzed text. Each becomes
//! [`LintProbe`]s the analyzer's `pg_sanity` mirror runs next to the query
//! (see `typedpg_analyzer`'s `pg_sanity/soundness/lint.rs`), over the
//! nullability oracle's seeded scenarios:
//!
//! - a fix: the fixed query must return the same rows, row count and table
//!   contents as the original, or fail with the same SQLSTATE;
//! - a claim that a value or condition is something wherever it is
//!   evaluated (`redundant_null_check`, `constant_condition`,
//!   `unreachable_case_branch`, `count_not_null`, `null_comparison`): the
//!   expression is wrapped in a `pg_temp.typedpg_lint_*` assertion that
//!   raises when the claim is broken;
//! - a claim that a clause changes nothing (`redundant_distinct`,
//!   `redundant_limit`, `redundant_order_by`, `redundant_group_by`,
//!   `redundant_join`, `left_join_nullified`, a settled `IS [NOT] NULL`,
//!   `simplifiable_case`'s `IS TRUE`): the query without it (or with the
//!   join it behaves as) must return the same rows;
//! - a claim that the query returns at most one row (`fetch_all_single_row`,
//!   a top-level `redundant_limit`): it never returns two;
//! - `check_violation`: the statement never writes a row.
//!
//! A broken claim panics with the query, the probe and the data, as the
//! analyzer's mirror does — unless [`collect`] is collecting them (the
//! corpus run in `tests/lint_oracle.rs`). `TYPEDPG_LINT_ORACLE_STATS=<file>`
//! appends per-rule counters to a file (`scripts/run-lint-oracle.sh`
//! prints their totals).
//!
//! What is not checked: `scalar_subquery_may_return_many`,
//! `case_without_else`'s reachability and `not_in_nullable` only say a
//! thing *may* happen, which no execution refutes; the advisory rules
//! (`cartesian_join`, `float_equality`, …) claim nothing about results.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::ops::Range;

use typedpg_analyzer::{LintProbe, LintProbeResult};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, JoinType, Node as N};

use super::node_span as rule_node_span;
use crate::{Ctx, Rule, SqlEdit};

/// One finding, as the rules reported it (analyzed text).
pub(crate) struct Record {
    pub rule: Rule,
    pub span: Range<usize>,
    pub message: String,
    /// The fix as the rule offered it, in the analyzed text (its
    /// replacement text may name parameters as written).
    pub fix: Vec<SqlEdit>,
    /// The fix in the text as written, empty when `fix_keeps_shape`
    /// withheld it.
    pub written_fix: Vec<SqlEdit>,
}

thread_local! {
    static COLLECTED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    static STATS: RefCell<BTreeMap<(String, String), u64>> = const { RefCell::new(BTreeMap::new()) };
}

/// Run `f`, collecting the oracle's broken claims instead of panicking.
pub fn collect<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let previous = COLLECTED.with(|c| c.borrow_mut().replace(Vec::new()));
    let result = f();
    let collected = COLLECTED.with(|c| std::mem::replace(&mut *c.borrow_mut(), previous));
    (result, collected.unwrap_or_default())
}

/// This thread's counters since the last call: `(rule, outcome)` → count.
pub fn take_stats() -> BTreeMap<(String, String), u64> {
    STATS.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

fn count(stats: &mut Vec<(String, String)>, rule: Rule, what: &str) {
    stats.push((rule.name().to_owned(), what.to_owned()));
}

/// Check `records` (the findings of the query `ctx` analyzed).
pub(crate) fn check(ctx: &Ctx<'_>, records: &[Record]) {
    let mut probes: Vec<(Rule, &'static str, LintProbe)> = Vec::new();
    for record in records {
        if !record.written_fix.is_empty()
            && let Some(sql) = fixed_sql(ctx, &record.written_fix)
        {
            let probe = if record.rule == Rule::UnusedReturning {
                LintProbe::FixDropsReturning { sql }
            } else {
                LintProbe::Fix { sql }
            };
            probes.push((record.rule, "fix", probe));
        }
        for probe in claims(ctx, record) {
            probes.push((record.rule, "claim", probe));
        }
    }
    if probes.is_empty() {
        return;
    }
    let list: Vec<LintProbe> = probes.iter().map(|(_, _, p)| p.clone()).collect();
    let results = ctx
        .catalog
        .pg_sanity_lint_probes(ctx.sql, &ctx.view.analyzed, &list);
    let mut stats = Vec::new();
    let mut broken = Vec::new();
    for ((rule, what, probe), result) in probes.iter().zip(results) {
        match result {
            LintProbeResult::Held { .. } => count(&mut stats, *rule, &format!("{what} held")),
            LintProbeResult::Inconclusive(why) => {
                count(&mut stats, *rule, &format!("{what} inconclusive"));
                if std::env::var_os("TYPEDPG_LINT_ORACLE_VERBOSE").is_some() {
                    eprintln!(
                        "lint oracle: {rule} {what} inconclusive: {why}\nquery:\n---\n{}\n---\n\
                         probe: {probe:?}",
                        ctx.sql
                    );
                }
            }
            LintProbeResult::Broken(how) => {
                count(&mut stats, *rule, &format!("{what} broken"));
                broken.push(format!(
                    "lint oracle: the {rule} {what} is wrong: {how}\nquery:\n---\n{}\n---",
                    ctx.sql
                ));
            }
        }
    }
    record_stats(&stats);
    if broken.is_empty() {
        return;
    }
    let collecting = COLLECTED.with(|c| {
        if let Some(out) = c.borrow_mut().as_mut() {
            out.extend(broken.iter().cloned());
            true
        } else {
            false
        }
    });
    if !collecting {
        panic!("{}", broken.join("\n\n"));
    }
}

fn record_stats(stats: &[(String, String)]) {
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        for key in stats {
            *s.entry(key.clone()).or_default() += 1;
        }
    });
    let Ok(path) = std::env::var("TYPEDPG_LINT_ORACLE_STATS") else {
        return;
    };
    let mut out = String::new();
    for (rule, what) in stats {
        out.push_str(&format!("{rule}\t{what}\t1\n"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(out.as_bytes());
    }
}

/// The analyzed text of the query as written with `fix` applied, its
/// parameters numbered as the original's are (a fix may drop the first
/// mention of one, which the lexer would number differently).
fn fixed_sql(ctx: &Ctx<'_>, fix: &[SqlEdit]) -> Option<String> {
    let written = crate::apply_edits(ctx.original, fix)?;
    let view = ctx.catalog.query_view(&written).ok()?;
    let names: Vec<&str> = ctx
        .view
        .analyzed
        .params
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    let renumber: Vec<usize> = view
        .analyzed
        .params
        .iter()
        .map(|p| names.iter().position(|n| *n == p.name).map(|i| i + 1))
        .collect::<Option<_>>()?;
    let scanned = typedpg_pg_query::scan(&view.sql).ok()?;
    let mut edits = Vec::new();
    for token in &scanned.tokens {
        if token.token != typedpg_pg_query::ast::Token::Param {
            continue;
        }
        let range = usize::try_from(token.start).ok()?..usize::try_from(token.end).ok()?;
        let n: usize = view.sql[range.clone()].strip_prefix('$')?.parse().ok()?;
        let to = *renumber.get(n.checked_sub(1)?)?;
        if to != n {
            edits.push((range, format!("${to}")));
        }
    }
    replace_all(&view.sql, edits)
}

/// The bytes `node` covers ([`super::node_span`]), with the keyword ending
/// an `IS [NOT] NULL` / `IS [NOT] TRUE` test, which no node locates.
fn node_span(ctx: &Ctx<'_>, node: NodeRef<'_>) -> Option<Range<usize>> {
    let mut range = rule_node_span(ctx, node)?;
    loop {
        let upper = ctx.sql[range.clone()].trim_end().to_ascii_uppercase();
        if !(upper.ends_with(" IS") || (upper.ends_with(" NOT") && upper.contains(" IS "))) {
            return Some(range);
        }
        let next = ctx.tokens.iter().find(|t| t.start >= range.end)?;
        range.end = next.end;
    }
}

/// `sql` with `range` replaced by `text`.
fn replace(sql: &str, range: Range<usize>, text: &str) -> String {
    format!("{}{text}{}", &sql[..range.start], &sql[range.end..])
}

/// `sql` with each range (not overlapping) replaced.
fn replace_all(sql: &str, mut edits: Vec<(Range<usize>, String)>) -> Option<String> {
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::new();
    let mut at = 0;
    for (range, text) in edits {
        if range.start < at {
            return None;
        }
        out.push_str(&sql[at..range.start]);
        out.push_str(&text);
        at = range.end;
    }
    out.push_str(&sql[at..]);
    Some(out)
}

fn is_expression(node: NodeRef<'_>) -> bool {
    matches!(
        node,
        NodeRef::AExpr(_)
            | NodeRef::BoolExpr(_)
            | NodeRef::NullTest(_)
            | NodeRef::BooleanTest(_)
            | NodeRef::SubLink(_)
            | NodeRef::AConst(_)
            | NodeRef::ColumnRef(_)
            | NodeRef::FuncCall(_)
            | NodeRef::TypeCast(_)
            | NodeRef::ParamRef(_)
            | NodeRef::CaseExpr(_)
            | NodeRef::CoalesceExpr(_)
            | NodeRef::MinMaxExpr(_)
            | NodeRef::RowExpr(_)
            | NodeRef::AArrayExpr(_)
            | NodeRef::AIndirection(_)
            | NodeRef::CollateClause(_)
            | NodeRef::SqlValueFunction(_)
    )
}

/// The largest expression node inside `span`, and its own span.
fn expression_in<'a>(ctx: &Ctx<'a>, span: &Range<usize>) -> Option<(NodeRef<'a>, Range<usize>)> {
    let mut best: Option<(NodeRef<'a>, Range<usize>)> = None;
    for (node, _) in ctx.tree.nodes() {
        if !is_expression(node) {
            continue;
        }
        let Some(s) = node_span(ctx, node) else {
            continue;
        };
        if s.start < span.start || s.end > span.end || s.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| s.len() > b.len()) {
            best = Some((node, s));
        }
    }
    best
}

fn assert_call(function: &str, text: &str, rule: Rule) -> String {
    format!(
        "pg_temp.typedpg_lint_{function}(({text}), '{}')",
        rule.name()
    )
}

/// Whether `range` is inside a `WHERE`, `HAVING` or `JOIN … ON`: the
/// planner may evaluate a condition there below the joins and other
/// conditions it is written after (a qual pushed down to its table's
/// scan), on rows the claim is not about. There a claim runs as the
/// equivalent query instead of as an assertion. (Only the start is
/// compared: a qual's span leaves out a trailing `IS NULL`'s `NULL`.)
fn in_qual(ctx: &Ctx<'_>, range: &Range<usize>) -> bool {
    let inside = |qual: Option<&N>| {
        qual.and_then(|q| node_span(ctx, q.to_ref()))
            .is_some_and(|q| q.start <= range.start && range.start < q.end)
    };
    ctx.tree.nodes().any(|(n, _)| match n {
        NodeRef::SelectStmt(s) => {
            inside(s.where_clause.as_ref()) || inside(s.having_clause.as_ref())
        }
        NodeRef::UpdateStmt(s) => inside(s.where_clause.as_ref()),
        NodeRef::DeleteStmt(s) => inside(s.where_clause.as_ref()),
        NodeRef::JoinExpr(j) => inside(j.quals.as_ref()),
        NodeRef::MergeStmt(s) => inside(s.join_condition.as_ref()),
        _ => false,
    })
}

/// The text standing for the expression at `range` under the claim
/// `function` makes of it: the assertion call, or (`qual`) a value the
/// query gives the same rows with when the claim holds — `true` for a
/// condition always true; `(c AND NULL)`, which is `c` unless `c` is
/// true, for one never true; a NULL of its type for a value never
/// evaluated.
fn claim_text(
    ctx: &Ctx<'_>,
    range: &Range<usize>,
    function: &str,
    rule: Rule,
    qual: bool,
) -> String {
    let text = &ctx.sql[range.clone()];
    if !qual {
        return assert_call(function, text, rule);
    }
    match function {
        "true" => "true".to_owned(),
        "not_true" => format!("(({text}) AND NULL)"),
        "unreached" => format!("(CASE WHEN false THEN ({text}) END)"),
        _ => assert_call(function, text, rule),
    }
}

/// The claim `function` makes of the expression at `range` as a probe.
fn wrap(ctx: &Ctx<'_>, range: Range<usize>, function: &str, rule: Rule) -> LintProbe {
    let qual = in_qual(ctx, &range) && function != "nonnull";
    let text = claim_text(ctx, &range, function, rule, qual);
    claim_probe(replace(ctx.sql, range, &text), qual)
}

fn claim_probe(sql: String, qual: bool) -> LintProbe {
    if qual {
        LintProbe::Equivalent { sql }
    } else {
        LintProbe::Assert { sql }
    }
}

/// The claims of `record` as probes.
fn claims(ctx: &Ctx<'_>, record: &Record) -> Vec<LintProbe> {
    let message = record.message.as_str();
    let span = &record.span;
    let rule = record.rule;
    match rule {
        Rule::NullComparison => {
            if message.starts_with("CASE ... WHEN NULL") {
                case_when_at(ctx, span.start)
                    .and_then(|(when, _)| when.result.as_ref())
                    .and_then(|r| node_span(ctx, r.to_ref()))
                    .map(|r| wrap(ctx, r, "unreached", rule))
                    .into_iter()
                    .collect()
            } else {
                condition_assert(ctx, span, false, rule)
            }
        }
        Rule::ConstantCondition => {
            let value = message.contains("always true");
            condition_assert(ctx, span, value, rule)
        }
        Rule::RedundantNullCheck => redundant_null_check(ctx, record),
        Rule::UnreachableCaseBranch => unreachable_case_branch(ctx, record),
        Rule::RedundantLimit => {
            let removal = record
                .fix
                .first()
                .map_or(span.clone(), |edit| edit.range.clone());
            let sql = replace(ctx.sql, removal, "");
            let top = matches!(
                ctx.tree.stmts.first().and_then(|s| s.stmt.as_ref()),
                Some(N::SelectStmt(select)) if select.limit_count.as_ref()
                    .and_then(|c| c.to_ref().location())
                    .and_then(|l| usize::try_from(l).ok())
                    .is_some_and(|l| span.contains(&l))
            );
            vec![if top {
                LintProbe::AtMostOneRow { sql }
            } else {
                LintProbe::Equivalent { sql }
            }]
        }
        Rule::RedundantDistinct | Rule::RedundantJoin if record.fix.is_empty() => {
            vec![LintProbe::Equivalent {
                sql: replace(ctx.sql, span.clone(), ""),
            }]
        }
        Rule::RedundantOrderBy | Rule::RedundantGroupBy if record.fix.is_empty() => {
            list_removals(ctx, span)
        }
        Rule::CountNotNull => count_not_null(ctx, record),
        Rule::CheckViolation => check_violation(ctx, span),
        Rule::LeftJoinNullified => left_join_nullified(ctx, record),
        Rule::SimplifiableCase if record.fix.is_empty() => simplifiable_case(ctx, span),
        Rule::FetchAllSingleRow => vec![LintProbe::AtMostOneRow {
            sql: ctx.sql.to_owned(),
        }],
        _ => Vec::new(),
    }
}

/// A condition (the largest expression in `span`) asserted always true
/// (`value`) or never true.
fn condition_assert(ctx: &Ctx<'_>, span: &Range<usize>, value: bool, rule: Rule) -> Vec<LintProbe> {
    let Some((_, range)) = expression_in(ctx, span) else {
        return Vec::new();
    };
    let function = if value { "true" } else { "not_true" };
    vec![wrap(ctx, range, function, rule)]
}

fn redundant_null_check(ctx: &Ctx<'_>, record: &Record) -> Vec<LintProbe> {
    let message = record.message.as_str();
    let span = record.span.clone();
    let rule = record.rule;
    if message.starts_with("COALESCE") {
        // The arguments after the first one never NULL are never evaluated.
        let Some(expr) = ctx.tree.nodes().find_map(|(n, _)| match n {
            NodeRef::CoalesceExpr(c) if usize::try_from(c.location).ok() == Some(span.start) => {
                Some(c)
            }
            _ => None,
        }) else {
            return Vec::new();
        };
        let Some(fact) = ctx.view.coalesce_fact(expr.location) else {
            return Vec::new();
        };
        let Some(index) = (0..expr.args.len().saturating_sub(1))
            .find(|&index| fact.args_nullable.get(index) == Some(&false))
        else {
            return Vec::new();
        };
        let qual = in_qual(ctx, &span);
        let mut edits = Vec::new();
        for arg in expr.args[index + 1..].iter().flatten() {
            let Some(range) = node_span(ctx, arg.to_ref()) else {
                return Vec::new();
            };
            let text = claim_text(ctx, &range, "unreached", rule, qual);
            edits.push((range, text));
        }
        return replace_all(ctx.sql, edits)
            .map(|sql| claim_probe(sql, qual))
            .into_iter()
            .collect();
    }
    if message.contains("IS NOT NULL is always true") {
        return vec![wrap(ctx, span, "true", rule)];
    }
    if message.contains("IS NULL is never true") {
        return vec![wrap(ctx, span, "not_true", rule)];
    }
    // Settled by the rest of the condition: with the test replaced by the
    // value it changes nothing to, the rows are the same.
    if message.contains("IS NOT NULL changes nothing") {
        return vec![LintProbe::Equivalent {
            sql: replace(ctx.sql, span, "true"),
        }];
    }
    if message.contains("IS NULL makes the condition never true") {
        return vec![LintProbe::Equivalent {
            sql: replace(ctx.sql, span, "false"),
        }];
    }
    Vec::new()
}

/// The `WHEN` at byte `at`, and its `CASE`.
fn case_when_at<'a>(
    ctx: &Ctx<'a>,
    at: usize,
) -> Option<(
    &'a typedpg_pg_query::ast::CaseWhen,
    &'a typedpg_pg_query::ast::CaseExpr,
)> {
    ctx.tree.nodes().find_map(|(n, _)| match n {
        NodeRef::CaseExpr(case) => case.args.iter().find_map(|a| match a {
            Some(N::CaseWhen(when)) if usize::try_from(when.location).ok() == Some(at) => {
                Some((&**when, case))
            }
            _ => None,
        }),
        _ => None,
    })
}

fn unreachable_case_branch(ctx: &Ctx<'_>, record: &Record) -> Vec<LintProbe> {
    let span = &record.span;
    let rule = record.rule;
    if record.message.starts_with("CASE branch is never taken") {
        let Some((when, case)) = case_when_at(ctx, span.start) else {
            return Vec::new();
        };
        let target = if case.arg.is_none() {
            when.expr.as_ref().map(|e| (e, "not_true"))
        } else {
            when.result.as_ref().map(|r| (r, "unreached"))
        };
        return target
            .and_then(|(node, function)| {
                Some(wrap(ctx, node_span(ctx, node.to_ref())?, function, rule))
            })
            .into_iter()
            .collect();
    }
    // Everything from `span.start` on is never reached: the WHEN before it
    // is always true where it is evaluated.
    // The innermost CASE around the span: a CASE nested in a WHEN's
    // condition contains it too.
    let Some((case, _)) = ctx
        .tree
        .nodes()
        .filter_map(|(n, _)| match n {
            NodeRef::CaseExpr(case) => {
                let range = node_span(ctx, n)?;
                (range.start < span.start && span.end <= range.end).then_some((case, range.len()))
            }
            _ => None,
        })
        .min_by_key(|(_, len)| *len)
    else {
        return Vec::new();
    };
    let whens: Vec<&typedpg_pg_query::ast::CaseWhen> = case
        .args
        .iter()
        .filter_map(|a| match a {
            Some(N::CaseWhen(w)) => Some(&**w),
            _ => None,
        })
        .collect();
    let before = whens
        .iter()
        .rev()
        .find(|w| usize::try_from(w.location).is_ok_and(|l| l < span.start));
    let Some(before) = before else {
        return Vec::new();
    };
    let target = if case.arg.is_none() {
        before.expr.as_ref()
    } else {
        None
    };
    if let Some(cond) = target {
        return node_span(ctx, cond.to_ref())
            .map(|r| wrap(ctx, r, "true", rule))
            .into_iter()
            .collect();
    }
    // A simple CASE: the later results are never evaluated.
    let qual = in_qual(ctx, span);
    let mut edits = Vec::new();
    let later = whens
        .iter()
        .filter(|w| usize::try_from(w.location).is_ok_and(|l| l >= span.start))
        .filter_map(|w| w.result.as_ref())
        .chain(case.defresult.as_ref());
    for result in later {
        if matches!(result, N::AConst(c) if c.isnull) {
            continue;
        }
        let Some(range) = node_span(ctx, result.to_ref()) else {
            return Vec::new();
        };
        let text = claim_text(ctx, &range, "unreached", rule, qual);
        edits.push((range, text));
    }
    if edits.is_empty() {
        return Vec::new();
    }
    replace_all(ctx.sql, edits)
        .map(|sql| claim_probe(sql, qual))
        .into_iter()
        .collect()
}

/// An `ORDER BY` / `GROUP BY` key (`span`) removed, with the comma or
/// keyword around it: each candidate that parses is a probe.
fn list_removals(ctx: &Ctx<'_>, span: &Range<usize>) -> Vec<LintProbe> {
    let sql = ctx.sql;
    let before = sql[..span.start].trim_end();
    let after = sql[span.end..].trim_start();
    let mut out = Vec::new();
    if let Some(stripped) = before.strip_suffix(',') {
        out.push(format!("{stripped} {after}"));
    }
    if let Some(stripped) = after.strip_prefix(',') {
        out.push(format!("{before} {stripped}"));
    }
    let upper = before.to_ascii_uppercase();
    for keyword in ["ORDER BY", "GROUP BY"] {
        if upper.ends_with(keyword) && !after.starts_with(',') {
            out.push(format!(
                "{} {after}",
                &before[..before.len() - keyword.len()]
            ));
        }
    }
    out.into_iter()
        .filter(|candidate| typedpg_pg_query::parse(candidate).is_ok())
        .map(|sql| LintProbe::Equivalent { sql })
        .collect()
}

fn count_not_null(ctx: &Ctx<'_>, record: &Record) -> Vec<LintProbe> {
    let Some(call) = ctx.tree.nodes().find_map(|(n, _)| match n {
        NodeRef::FuncCall(c) if usize::try_from(c.location).ok() == Some(record.span.start) => {
            Some(c)
        }
        _ => None,
    }) else {
        return Vec::new();
    };
    let Some(arg) = call.args.first().and_then(Option::as_ref) else {
        return Vec::new();
    };
    node_span(ctx, arg.to_ref())
        .map(|r| wrap(ctx, r, "nonnull", record.rule))
        .into_iter()
        .collect()
}

/// The statement never writes a row: only read for a top-level `INSERT` or
/// `UPDATE` without `WITH`, and not for a value in an `ON CONFLICT DO
/// UPDATE`, whose statement may insert instead.
fn check_violation(ctx: &Ctx<'_>, span: &Range<usize>) -> Vec<LintProbe> {
    let checkable = match ctx.tree.stmts.first().and_then(|s| s.stmt.as_ref()) {
        Some(N::InsertStmt(insert)) => {
            insert.with_clause.is_none()
                && insert
                    .on_conflict_clause
                    .as_ref()
                    .is_none_or(|c| usize::try_from(c.location).is_ok_and(|l| span.start < l))
        }
        Some(N::UpdateStmt(update)) => update.with_clause.is_none(),
        _ => false,
    };
    if checkable {
        vec![LintProbe::NeverWrites]
    } else {
        Vec::new()
    }
}

/// The names `node` (a FROM item) exposes as a relation.
fn exposes(node: Option<&N>, name: &str) -> bool {
    match node {
        Some(N::RangeVar(rv)) => match &rv.alias {
            Some(alias) => alias.aliasname == name,
            None => rv.relname == name,
        },
        Some(N::RangeSubselect(sub)) => sub.alias.as_ref().is_some_and(|a| a.aliasname == name),
        Some(N::RangeFunction(f)) => f.alias.as_ref().is_some_and(|a| a.aliasname == name),
        Some(N::JoinExpr(join)) => match &join.alias {
            Some(alias) => alias.aliasname == name,
            None => exposes(join.larg.as_ref(), name) || exposes(join.rarg.as_ref(), name),
        },
        _ => false,
    }
}

fn left_join_nullified(ctx: &Ctx<'_>, record: &Record) -> Vec<LintProbe> {
    let message = record.message.as_str();
    // "… of <entry>, on the nullable side of a <written> JOIN…: the join
    // behaves as a(n) <reduced> JOIN".
    let Some(entry) = message
        .strip_prefix("this condition is never true for the NULL-extended rows of ")
        .and_then(|rest| rest.split_once(", on the nullable side"))
        .map(|(entry, _)| entry.trim_matches('"').replace("\"\"", "\""))
    else {
        return Vec::new();
    };
    let Some(reduced) = message
        .rsplit_once("the join behaves as ")
        .and_then(|(_, rest)| rest.split_whitespace().nth(1))
    else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::JoinExpr(join) = node else {
            continue;
        };
        let nullable = match join.jointype {
            JoinType::JoinLeft => exposes(join.rarg.as_ref(), &entry),
            JoinType::JoinRight => exposes(join.larg.as_ref(), &entry),
            JoinType::JoinFull => {
                exposes(join.larg.as_ref(), &entry) || exposes(join.rarg.as_ref(), &entry)
            }
            _ => false,
        };
        if nullable && let Some(range) = join_keyword(ctx, join) {
            candidates.push(range);
        }
    }
    let [range] = candidates.as_slice() else {
        return Vec::new();
    };
    vec![LintProbe::Equivalent {
        sql: replace(ctx.sql, range.clone(), &format!("{reduced} JOIN")),
    }]
}

/// The `LEFT [OUTER] JOIN` / `RIGHT …` / `FULL …` keywords of `join`: the
/// tokens before its right-hand item.
fn join_keyword(ctx: &Ctx<'_>, join: &typedpg_pg_query::ast::JoinExpr) -> Option<Range<usize>> {
    let rarg = node_span(ctx, join.rarg.as_ref()?.to_ref())?;
    let text = |i: usize| &ctx.sql[ctx.tokens[i].start..ctx.tokens[i].end];
    // The `JOIN` keyword, a few tokens before the right-hand item at most
    // (`LATERAL`, an opening parenthesis).
    let before = ctx.tokens.iter().rposition(|t| t.end <= rarg.start)?;
    let join_at = (before.saturating_sub(3)..=before)
        .rev()
        .find(|&i| text(i).eq_ignore_ascii_case("JOIN"))?;
    let mut start = join_at;
    while start > 0
        && ["LEFT", "RIGHT", "FULL", "OUTER"]
            .iter()
            .any(|k| text(start - 1).eq_ignore_ascii_case(k))
    {
        start -= 1;
    }
    (start < join_at).then(|| ctx.tokens[start].start..ctx.tokens[join_at].end)
}

fn simplifiable_case(ctx: &Ctx<'_>, span: &Range<usize>) -> Vec<LintProbe> {
    let Some(case) = ctx.tree.nodes().find_map(|(n, _)| match n {
        NodeRef::CaseExpr(c) if usize::try_from(c.location).ok() == Some(span.start) => Some(c),
        _ => None,
    }) else {
        return Vec::new();
    };
    let [Some(N::CaseWhen(when))] = case.args.as_slice() else {
        return Vec::new();
    };
    let is_false = matches!(&case.defresult, Some(N::AConst(c))
        if matches!(c.val, Some(AConstValue::Boolean(ref b)) if !b.boolval));
    if !is_false {
        return Vec::new();
    }
    let (Some(cond), Some(range)) = (
        when.expr.as_ref().and_then(|e| node_span(ctx, e.to_ref())),
        node_span(ctx, NodeRef::CaseExpr(case)),
    ) else {
        return Vec::new();
    };
    let text = format!("(({}) IS TRUE)", &ctx.sql[cond]);
    vec![LintProbe::Equivalent {
        sql: replace(ctx.sql, range, &text),
    }]
}
