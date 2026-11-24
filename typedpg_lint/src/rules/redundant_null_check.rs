//! The `redundant_null_check` rule: reports an `IS [NOT] NULL` test or a
//! `COALESCE` on a value the analysis proves is never NULL, and an `IS
//! [NOT] NULL` test that is an `AND` argument of a `WHERE`, `JOIN … ON` or
//! `HAVING` whose other arguments already drop the rows where the value is
//! NULL (`WHERE n > 0 AND n IS NOT NULL`: a NULL `n` makes `n > 0` NULL,
//! and the row is dropped either way; see
//! [`typedpg_analyzer::QueryView::null_test_settled`]). Only a condition
//! is read so: in a select list, NULL and FALSE are different values.
//!
//! A `COALESCE` finding comes with a fix dropping the arguments it never
//! uses, when every argument has the call's result type; an `IS [NOT]
//! NULL` test has none (removing it from a `WHERE` changes which rows are
//! kept, or, when the other arguments settle it, leaves what they meant
//! to the author).

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{BoolExprType, CoalesceExpr, Node, Node as N, NullTest, NullTestType};

use super::{location, node_span, operand_start};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    // A `COALESCE` that names an output column (`naming_expressions`: an
    // unaliased item, or under a cast, `COLLATE` or a `CASE`'s `ELSE` there)
    // names it `coalesce`; removing the call renames it, so its fix is
    // withheld. Collect them first: a node
    // visited later needs to know.
    let mut unaliased = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        // A `COALESCE` in FROM (`FROM coalesce(1, 2)`) is a function item:
        // without the call, it is no FROM item at all.
        if let NodeRef::RangeFunction(function) = node {
            for item in function.functions.iter().flatten() {
                if let N::List(list) = item
                    && let Some(Some(N::CoalesceExpr(expr))) = list.items.first()
                {
                    unaliased.insert(expr.location);
                }
            }
        }
    }
    for node in super::naming_expressions(ctx) {
        if let N::CoalesceExpr(expr) = node {
            unaliased.insert(expr.location);
        }
    }
    let conjuncts = condition_conjuncts(ctx);
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::NullTest(test) => {
                null_test(ctx, test, conjuncts.contains(&test.location), &mut out);
            }
            NodeRef::CoalesceExpr(expr) => coalesce(ctx, expr, &unaliased, &mut out),
            _ => {}
        }
    }
    out
}

// ── IS [NOT] NULL ──────────────────────────────────────────────────────────

/// The locations of the `IS [NOT] NULL` tests that are top-level `AND`
/// arguments of a `WHERE` (of a `SELECT`, `UPDATE` or `DELETE`), `JOIN …
/// ON` or `HAVING`: the only places the analyzer's
/// [`typedpg_analyzer::QueryView::null_test_settled`] is about.
fn condition_conjuncts(ctx: &Ctx<'_>) -> HashSet<i32> {
    fn walk(node: Option<&Node>, out: &mut HashSet<i32>) {
        match node {
            Some(N::BoolExpr(b)) if b.boolop == BoolExprType::AndExpr => {
                for arg in &b.args {
                    walk(arg.as_ref(), out);
                }
            }
            Some(N::NullTest(test)) => {
                out.insert(test.location);
            }
            _ => {}
        }
    }
    let mut out = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::SelectStmt(sel) => {
                walk(sel.where_clause.as_ref(), &mut out);
                walk(sel.having_clause.as_ref(), &mut out);
            }
            NodeRef::UpdateStmt(stmt) => walk(stmt.where_clause.as_ref(), &mut out),
            NodeRef::DeleteStmt(stmt) => walk(stmt.where_clause.as_ref(), &mut out),
            NodeRef::JoinExpr(join) => walk(join.quals.as_ref(), &mut out),
            _ => {}
        }
    }
    out
}

fn null_test(ctx: &Ctx<'_>, test: &NullTest, conjunct: bool, out: &mut Vec<RawFinding>) {
    let arg = &test.arg;
    if arg.is_none() {
        return;
    }
    // The analyzer's fact: without one, it didn't prove the argument's
    // nullability (a row / composite value, or a site it never inferred),
    // and the rule never guesses.
    let settled = match ctx.view.null_test_arg_nullable(test.location) {
        Some(false) => false,
        // Nullable, but the other AND arguments of its condition drop the
        // rows where it is NULL. (Checked to be such an argument here too:
        // a fact of another text's node at the same offset — a view or
        // function body the analysis read — is not this test's.)
        Some(true) if conjunct && ctx.view.null_test_settled(test.location) == Some(true) => true,
        _ => return,
    };
    // From the test's first token: a parenthesis opening its argument,
    // `(t.c) IS NULL`, included.
    let start = argument_start(arg.as_ref()).or_else(|| location(test.location));
    let Some(start) = node_span(ctx, typedpg_pg_query::NodeRef::NullTest(test))
        .map(|span| span.start)
        .into_iter()
        .chain(start)
        .min()
    else {
        return;
    };
    let Some(end) = null_keyword_end(ctx, test.location).filter(|end| *end > start) else {
        return;
    };
    let is_null = test.nulltesttype == NullTestType::IsNull;
    let text = argument_text(ctx, arg.as_ref());
    let (message, help) = if settled {
        let value = text.map_or_else(|| "the value".to_owned(), |text| format!("`{text}`"));
        if is_null {
            (
                format!(
                    "{value} IS NULL makes the condition never true: the rest of it is never \
                     true when {value} is NULL"
                ),
                "the condition keeps no row: fix the test or the rest of the condition",
            )
        } else {
            (
                format!(
                    "{value} IS NOT NULL changes nothing: the rest of the condition is never \
                     true when {value} is NULL"
                ),
                "remove the check",
            )
        }
    } else {
        (
            match (text, is_null) {
                (Some(text), true) => format!("`{text}` IS NULL is never true: it is never NULL"),
                (Some(text), false) => {
                    format!("`{text}` IS NOT NULL is always true: it is never NULL")
                }
                (None, true) => "IS NULL is never true here: the value is never NULL".to_owned(),
                (None, false) => {
                    "IS NOT NULL is always true here: the value is never NULL".to_owned()
                }
            },
            "remove the check, or find why the value was expected to be NULL (an outer join?)",
        )
    };
    out.push(RawFinding {
        span: start..end,
        message,
        help: Some(help.to_owned()),
        fix: Vec::new(),
    });
}

/// The end of the `NULL` keyword of the test whose `IS` is at `test_location`.
fn null_keyword_end(ctx: &Ctx<'_>, test_location: i32) -> Option<usize> {
    let start = location(test_location)?;
    ctx.tokens
        .iter()
        .filter(|token| token.start >= start)
        // `IS NULL` is two tokens, `IS NOT NULL` three.
        .take(3)
        .find(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case("NULL"))
        .map(|token| token.end)
}

// ── COALESCE ───────────────────────────────────────────────────────────────

fn coalesce(
    ctx: &Ctx<'_>,
    expr: &CoalesceExpr,
    unaliased: &HashSet<i32>,
    out: &mut Vec<RawFinding>,
) {
    let Some(fact) = ctx.view.coalesce_fact(expr.location) else {
        return;
    };
    // The first argument that is never NULL decides the result: the ones
    // after it are never evaluated (the last one has nothing after it).
    let Some(index) = (0..expr.args.len().saturating_sub(1))
        .find(|&index| fact.args_nullable.get(index) == Some(&false))
    else {
        return;
    };
    let Some(start) = location(expr.location) else {
        return;
    };
    let Some((open, close)) = call_parens(ctx, start) else {
        return;
    };
    let args = call_arguments(ctx, open, close);
    let text = args
        .as_ref()
        .and_then(|args| args.get(index))
        .map(|(start, end)| short_text(ctx, *start, *end))
        .unwrap_or_else(|| argument_text(ctx, expr.args[index].as_ref()));
    let mut fix = Vec::new();
    // The kept argument decides the type once the others are gone: it must
    // already have the result type (`COALESCE('1', int_col)` is integer,
    // `'1'` alone is not).
    //
    // A few dropped arguments make the rewrite unsound or surprising, so the
    // finding stays but the fix is withheld:
    // - a function call outside a sublink is structural (an aggregate, window
    //   or set-returning function shapes the rows even when never evaluated:
    //   `COALESCE(1, max(v))` is one row, `1` is one per row);
    // - a `COLLATE` clause or a column reference can decide the result's
    //   collation (`COALESCE('x'::text, 'y'::text COLLATE "C")`);
    // - an unaliased direct target column is named `coalesce`; any rewrite
    //   risks its output name (dropping the call renames it outright);
    // - a parameter is part of the query's signature: dropping it removes
    //   the parameter, or what typed it (`COALESCE(id, $d::bigint)`).
    let dropped = &expr.args[index + 1..];
    let structural = dropped.iter().any(|n| has_call_outside_sublink(n.as_ref()));
    let collation = dropped.iter().any(|n| has_collation_or_column(n.as_ref()));
    let parameter = dropped.iter().any(|n| has_param(n.as_ref()));
    let unaliased_target = unaliased.contains(&expr.location);
    if fact.args_share_result_type
        && fact.args_typed_as_result.get(index) == Some(&true)
        && let Some(args) = &args
        && args.len() == expr.args.len()
        && !structural
        && !collation
        && !parameter
        && !unaliased_target
    {
        let close_start = ctx.tokens[close].start;
        let arg_end = args[index].1;
        if index > 0 {
            fix.push(SqlEdit {
                range: arg_end..close_start,
                text: removed_separator(ctx, arg_end..close_start),
            });
        } else if is_simple_argument(ctx, expr.args[0].as_ref(), args[0].0, args[0].1) {
            // `COALESCE(x, …)` becomes `x` — the call's closing parenthesis
            // goes too.
            fix.push(SqlEdit {
                range: start..ctx.tokens[open].end,
                text: removed_separator(ctx, start..ctx.tokens[open].end),
            });
            fix.push(SqlEdit {
                range: arg_end..ctx.tokens[close].end,
                text: removed_separator(ctx, arg_end..ctx.tokens[close].end),
            });
        } else {
            // `COALESCE(a + b, 0) * 2` becomes `(a + b) * 2`: keep the
            // grouping the call's parentheses gave the argument.
            fix.push(SqlEdit {
                range: start..ctx.tokens[open].end,
                text: "(".to_owned(),
            });
            fix.push(SqlEdit {
                range: arg_end..close_start,
                text: removed_separator(ctx, arg_end..close_start),
            });
        }
    }
    out.push(RawFinding {
        span: start..ctx.tokens[close].end,
        message: match text {
            Some(text) => {
                format!("COALESCE never uses the arguments after `{text}`: it is never NULL")
            }
            None => format!(
                "COALESCE never uses the arguments after its {} one: it is never NULL",
                ordinal(index)
            ),
        },
        help: None,
        fix,
    });
}

/// The argument tokens the call's parentheses at `open` / `close` hold: each
/// argument's start and end byte offsets in `ctx.sql`, split on the commas at
/// the call's own parenthesis depth (inside a string, a nested call or a
/// subscript it is not a separator).
fn call_arguments(ctx: &Ctx<'_>, open: usize, close: usize) -> Option<Vec<(usize, usize)>> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut first: Option<usize> = None;
    for index in open + 1..close {
        let token = ctx.tokens[index];
        match &ctx.sql[token.start..token.end] {
            "(" | "[" => depth += 1,
            ")" | "]" => depth = depth.checked_sub(1)?,
            "," if depth == 0 => {
                args.push((ctx.tokens[first?].start, ctx.tokens[index - 1].end));
                first = None;
                continue;
            }
            _ => {}
        }
        first.get_or_insert(index);
    }
    let first = first?;
    args.push((ctx.tokens[first].start, ctx.tokens[close - 1].end));
    Some(args)
}

/// The `(` and `)` tokens of the call starting at `start`, by token index.
fn call_parens(ctx: &Ctx<'_>, start: usize) -> Option<(usize, usize)> {
    let open = ctx
        .tokens
        .iter()
        .position(|token| token.start >= start && &ctx.sql[token.start..token.end] == "(")?;
    let mut depth = 0usize;
    for (index, token) in ctx.tokens.iter().enumerate().skip(open) {
        match &ctx.sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((open, index));
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether replacing `COALESCE(...)` by its first argument can leave the
/// argument bare: a column, parameter, constant, function call or an
/// expression whose own parentheses wrap it binds like the call did.
fn is_simple_argument(ctx: &Ctx<'_>, node: Option<&Node>, start: usize, end: usize) -> bool {
    if is_parenthesized(ctx, start, end) {
        return true;
    }
    matches!(
        node,
        Some(N::ColumnRef(_) | N::ParamRef(_) | N::AConst(_) | N::FuncCall(_))
    )
}

/// Whether the argument starting at `start` and ending at `end` is one
/// parenthesized expression: its first token is `(` and the matching `)` is
/// its last token. `((1) + 2)` is, `(1) + 2` is not.
fn is_parenthesized(ctx: &Ctx<'_>, start: usize, end: usize) -> bool {
    let Some(open) = ctx.tokens.iter().position(|token| token.start == start) else {
        return false;
    };
    if &ctx.sql[ctx.tokens[open].start..ctx.tokens[open].end] != "(" {
        return false;
    }
    let mut depth = 0usize;
    for token in &ctx.tokens[open..] {
        match &ctx.sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => {
                let Some(remaining) = depth.checked_sub(1) else {
                    return false;
                };
                depth = remaining;
                if depth == 0 {
                    return token.end == end;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a dropped argument holds a function call outside any sublink:
/// aggregates, window functions and set-returning functions shape the rows
/// the query returns even when `COALESCE` never evaluates them.
fn has_call_outside_sublink(node: Option<&Node>) -> bool {
    node.is_some_and(|node| call_outside_sublink(node.to_ref()))
}

fn call_outside_sublink(node: NodeRef<'_>) -> bool {
    match node {
        // A call inside a sublink belongs to the subquery: dropping the
        // whole sublink cannot change the outer query's rows.
        NodeRef::SubLink(_) => false,
        NodeRef::FuncCall(_) => true,
        other => other.children().into_iter().any(call_outside_sublink),
    }
}

/// Whether a dropped argument holds a `COLLATE` clause or a column reference:
/// either can decide the `COALESCE`'s collation, even though the argument is
/// never evaluated.
/// Whether a dropped argument holds a parameter (`$1`, or a `$name` it was
/// rewritten from).
fn has_param(node: Option<&Node>) -> bool {
    fn walk(node: NodeRef<'_>) -> bool {
        matches!(node, NodeRef::ParamRef(_)) || node.children().into_iter().any(walk)
    }
    node.is_some_and(|node| walk(node.to_ref()))
}

fn has_collation_or_column(node: Option<&Node>) -> bool {
    node.is_some_and(|node| collation_or_column(node.to_ref()))
}

fn collation_or_column(node: NodeRef<'_>) -> bool {
    match node {
        NodeRef::CollateClause(_) | NodeRef::ColumnRef(_) => true,
        other => other.children().into_iter().any(collation_or_column),
    }
}

/// The text replacing a removed range: empty, unless the two tokens it makes
/// adjacent would otherwise merge into one (`1` + `AS` → `1AS`). Then a single
/// space keeps them apart.
fn removed_separator(ctx: &Ctx<'_>, range: std::ops::Range<usize>) -> String {
    let left = ctx.sql[..range.start].chars().next_back();
    let right = ctx.sql[range.end..].chars().next();
    match (left, right) {
        (Some(left), Some(right)) if joins(left) && joins(right) => " ".to_owned(),
        _ => String::new(),
    }
}

/// Whether a token can continue, or start, an identifier, number or quoted
/// token: two of these next to each other lex as one token.
fn joins(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$' | '\'' | '"' | '.')
}

// ── text ───────────────────────────────────────────────────────────────────

/// The start of the token sequence of `node`, descending through casts and a
/// binary expression's left side so `a + b` starts at `a` (an `AExpr`'s own
/// location is its operator).
fn argument_start(node: Option<&Node>) -> Option<usize> {
    let node = node?;
    match node {
        N::TypeCast(cast) => argument_start(cast.arg.as_ref()).or_else(|| operand_start(node)),
        N::AExpr(expr) => argument_start(expr.lexpr.as_ref()).or_else(|| operand_start(node)),
        _ => operand_start(node),
    }
}

/// The expression as the message shows it, on one line; `None` when it is
/// too long to quote.
fn argument_text(ctx: &Ctx<'_>, node: Option<&Node>) -> Option<String> {
    let span = node_span(ctx, node?.to_ref())?;
    short_text(ctx, span.start, span.end)
}

/// The text from `start` to `end` as written (a `$name` parameter stays
/// `$name`), on one line; `None` when it is too long to quote.
fn short_text(ctx: &Ctx<'_>, start: usize, end: usize) -> Option<String> {
    let text = ctx.written(start..end);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.len() <= 40).then_some(text)
}

/// `first`, `second`, … for a zero-based argument index.
fn ordinal(index: usize) -> String {
    match index {
        0 => "first".to_owned(),
        1 => "second".to_owned(),
        2 => "third".to_owned(),
        n => format!("{}th", n + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Finding, LintOptions, Rule, Severity, lint_query};
    use typedpg_analyzer::PgCatalog;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE checks (
                     id integer PRIMARY KEY,
                     nn integer NOT NULL,
                     nullable integer,
                     a integer NOT NULL,
                     b integer NOT NULL,
                     name text NOT NULL,
                     note text
                 );",
            )
            .unwrap();
        catalog
    }

    fn lint(catalog: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint_query(catalog, sql, &LintOptions::default()).unwrap()
    }

    /// Apply a finding's edits to `sql` (their ranges are in the original
    /// text, as the CLI applies them).
    fn fixed(sql: &str, finding: &Finding) -> String {
        let mut edits = finding.fix.clone();
        edits.sort_by_key(|edit| edit.range.start);
        let mut out = String::new();
        let mut position = 0;
        for edit in edits {
            out.push_str(&sql[position..edit.range.start]);
            out.push_str(&edit.text);
            position = edit.range.end;
        }
        out.push_str(&sql[position..]);
        out
    }

    fn only(findings: &[Finding]) -> &Finding {
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule, Rule::RedundantNullCheck);
        assert_eq!(findings[0].severity, Severity::Warning);
        &findings[0]
    }

    #[test]
    fn is_null_and_is_not_null_on_a_not_null_column_are_reported() {
        let db = catalog();
        for (sql, message) in [
            (
                "SELECT nn IS NULL FROM checks",
                "`nn` IS NULL is never true: it is never NULL",
            ),
            (
                "SELECT nn IS NOT NULL FROM checks",
                "`nn` IS NOT NULL is always true: it is never NULL",
            ),
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert_eq!(finding.message, message);
            assert_eq!(
                finding.help.as_deref(),
                Some(
                    "remove the check, or find why the value was expected to be NULL (an outer join?)"
                )
            );
            assert!(finding.fix.is_empty());
        }
    }

    #[test]
    fn the_where_proves_a_select_list_value_non_null() {
        let db = catalog();
        let sql = "SELECT nullable IS NULL AS b FROM checks WHERE nullable IS NOT NULL";
        let findings = lint(&db, sql);
        assert_eq!(
            only(&findings).message,
            "`nullable` IS NULL is never true: it is never NULL"
        );
        let sql = "SELECT COALESCE(nullable, 0) AS v FROM checks WHERE nullable > 0";
        let findings = lint(&db, sql);
        assert_eq!(
            fixed(sql, only(&findings)),
            "SELECT nullable AS v FROM checks WHERE nullable > 0"
        );
    }

    #[test]
    fn messages_quote_named_parameters_as_written() {
        let db = catalog();
        // Only a settled test can quote one: a parameter makes a value
        // nullable, so a test or COALESCE argument holding one is never
        // proven non-NULL.
        let sql = "SELECT id FROM checks WHERE abs(nullable + $n) > 0 \
                   AND abs(nullable + $n) IS NOT NULL";
        let findings = lint(&db, sql);
        assert_eq!(
            only(&findings).message,
            "`abs(nullable + $n)` IS NOT NULL changes nothing: the rest of the condition is \
             never true when `abs(nullable + $n)` is NULL"
        );
        // The whole argument, not its first operand.
        let sql = "SELECT id FROM checks WHERE nullable + $n > 0 AND nullable + $n IS NOT NULL";
        let findings = lint(&db, sql);
        assert_eq!(
            only(&findings).message,
            "`nullable + $n` IS NOT NULL changes nothing: the rest of the condition is never \
             true when `nullable + $n` is NULL"
        );
    }

    #[test]
    fn is_null_span_covers_the_argument_and_the_null_keyword() {
        let db = catalog();
        let sql = "SELECT nn IS NOT NULL FROM checks";
        let findings = lint(&db, sql);
        assert_eq!(&sql[only(&findings).span.clone()], "nn IS NOT NULL");
        // A parenthesized argument: from its opening parenthesis.
        for (sql, span) in [
            ("SELECT (nn) IS NOT NULL FROM checks", "(nn) IS NOT NULL"),
            (
                "SELECT ((nn)::text) IS NULL FROM checks",
                "((nn)::text) IS NULL",
            ),
        ] {
            let findings = lint(&db, sql);
            assert_eq!(&sql[only(&findings).span.clone()], span, "{sql}");
        }
    }

    #[test]
    fn checks_on_nullable_values_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT nullable IS NULL FROM checks",
            "SELECT nullable IS NOT NULL FROM checks",
            "SELECT note IS NULL FROM checks",
            "SELECT (nullable + 1) IS NULL FROM checks",
            "SELECT $1::text IS NULL",
            "SELECT nullif(nullable, 0) IS NOT NULL FROM checks",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::RedundantNullCheck),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn the_where_clause_does_not_narrow_its_own_check() {
        let db = catalog();
        // `nullable` may be NULL even past the check: the analyzer records
        // the argument's own nullability, before the predicate proves
        // anything — the test is not its own proof, nor is a later test of
        // the value (each would settle the other, and removing both changes
        // the rows; the later one is reported, as an earlier AND argument
        // not FALSE proves the value non-NULL there).
        for sql in [
            "SELECT id FROM checks WHERE nullable IS NOT NULL",
            "SELECT id FROM checks WHERE nullable IS NULL",
            "SELECT id FROM checks WHERE nullable IS NOT NULL AND id > 0",
            // Not an AND argument of the condition: under OR or NOT, or in a
            // select list, NULL and FALSE differ.
            "SELECT id FROM checks WHERE nullable > 0 OR nullable IS NOT NULL",
            "SELECT id FROM checks WHERE NOT (nullable > 0 AND nullable IS NOT NULL)",
            "SELECT nullable > 0 AND nullable IS NOT NULL FROM checks",
            "SELECT CASE WHEN nullable > 0 AND nullable IS NOT NULL THEN 1 END FROM checks",
            // `IS DISTINCT FROM` and `coalesce` are not NULL for a NULL
            // operand: they don't drop the row.
            "SELECT id FROM checks WHERE nullable IS DISTINCT FROM 0 AND nullable IS NOT NULL",
            "SELECT id FROM checks WHERE coalesce(nullable, 0) = 0 AND nullable IS NOT NULL",
            // A parameter may be NULL: `$1 > 0` drops nothing of `nullable`.
            "SELECT id FROM checks WHERE $1::int > 0 AND nullable IS NOT NULL",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::RedundantNullCheck),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn a_test_the_other_and_arguments_settle_is_reported() {
        let db = catalog();
        // The second of two tests: the first, not FALSE, proves it.
        let sql = "SELECT id FROM checks WHERE nullable IS NOT NULL AND nullable IS NOT NULL";
        let findings: Vec<Finding> = lint(&db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::RedundantNullCheck)
            .collect();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].span.start, sql.rfind("nullable IS").unwrap());
        for (sql, span, message) in [
            (
                "SELECT id FROM checks WHERE nullable > 0 AND nullable IS NOT NULL",
                "nullable IS NOT NULL",
                "`nullable` IS NOT NULL changes nothing: the rest of the condition is never \
                 true when `nullable` is NULL",
            ),
            (
                "SELECT id FROM checks WHERE nullable IS NOT NULL AND id = 1 AND nullable < 3",
                "nullable IS NOT NULL",
                "`nullable` IS NOT NULL changes nothing",
            ),
            (
                "SELECT id FROM checks WHERE (id > 0 AND note = 'x') AND note IS NOT NULL",
                "note IS NOT NULL",
                "`note` IS NOT NULL changes nothing",
            ),
            (
                "SELECT id FROM checks WHERE nullable = 1 AND nullable IS NULL",
                "nullable IS NULL",
                "`nullable` IS NULL makes the condition never true: the rest of it is never \
                 true when `nullable` is NULL",
            ),
            (
                "SELECT l.id FROM checks l JOIN checks r ON r.nullable = l.id \
                 AND r.nullable IS NOT NULL",
                "r.nullable IS NOT NULL",
                "`r.nullable` IS NOT NULL changes nothing",
            ),
            (
                "SELECT nullable FROM checks GROUP BY nullable \
                 HAVING nullable > 0 AND nullable IS NOT NULL",
                "nullable IS NOT NULL",
                "`nullable` IS NOT NULL changes nothing",
            ),
            (
                "UPDATE checks SET nn = 1 WHERE nullable <> 2 AND nullable IS NOT NULL",
                "nullable IS NOT NULL",
                "`nullable` IS NOT NULL changes nothing",
            ),
            (
                "DELETE FROM checks WHERE nullable IS NOT NULL AND lower(note) = 'x' \
                 AND note IS NOT NULL AND nullable IN (1, 2)",
                "nullable IS NOT NULL",
                "`nullable` IS NOT NULL changes nothing",
            ),
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::RedundantNullCheck)
                .collect();
            assert!(!findings.is_empty(), "{sql}");
            let finding = &findings[0];
            assert_eq!(&sql[finding.span.clone()], span, "{sql}");
            assert!(
                finding.message.contains(message),
                "{sql}: {}",
                finding.message
            );
            assert!(finding.fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn a_not_null_column_of_an_outer_joins_nullable_side_is_nullable() {
        let mut db = catalog();
        // A different (not self-joined) table: nothing proves the left row
        // has a match, so the right side is null-extended.
        db.apply_sql("CREATE TABLE others (id integer PRIMARY KEY, nn integer NOT NULL);")
            .unwrap();
        for sql in [
            "SELECT l.id FROM checks l LEFT JOIN others r ON r.id = l.id WHERE r.nn IS NOT NULL",
            "SELECT l.id FROM checks l LEFT JOIN others r ON r.id = l.id WHERE r.nn IS NULL",
            "SELECT r.nn IS NULL FROM checks l LEFT JOIN others r ON r.id = l.id",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::RedundantNullCheck),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn row_and_composite_checks_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT ROW(nn, nullable) IS NULL FROM checks",
            "SELECT ROW(nn, nullable) IS NOT NULL FROM checks",
            "SELECT checks IS NULL FROM checks",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::RedundantNullCheck),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn coalesce_drops_the_arguments_after_a_not_null_one() {
        let db = catalog();
        let cases = [
            (
                "SELECT COALESCE(nn, 0) AS result FROM checks",
                "SELECT nn AS result FROM checks",
            ),
            (
                "SELECT COALESCE(a + b, 0) * 2 FROM checks",
                "SELECT (a + b) * 2 FROM checks",
            ),
            (
                "SELECT COALESCE((a + b), 0) * 2 FROM checks",
                "SELECT (a + b) * 2 FROM checks",
            ),
            (
                "SELECT COALESCE(nullable, nn, 0) AS result FROM checks",
                "SELECT COALESCE(nullable, nn) AS result FROM checks",
            ),
            (
                "SELECT COALESCE(nullable, nn, 5, 6, 0) AS result FROM checks",
                "SELECT COALESCE(nullable, nn) AS result FROM checks",
            ),
        ];
        for (sql, expected) in cases {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert!(!finding.fix.is_empty(), "{sql}: {finding:?}");
            assert_eq!(fixed(sql, finding), expected);
        }
        let sql = "SELECT COALESCE(nullable, nn, 0) FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert_eq!(
            finding.message,
            "COALESCE never uses the arguments after `nn`: it is never NULL"
        );
        assert_eq!(&sql[finding.span.clone()], "COALESCE(nullable, nn, 0)");
    }

    #[test]
    fn coalesce_without_a_shared_type_is_reported_without_a_fix() {
        let db = catalog();
        let sql = "SELECT COALESCE(nn, 0::int8) FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert!(finding.fix.is_empty(), "{finding:?}");
    }

    #[test]
    fn a_long_argument_is_named_by_its_position() {
        let db = catalog();
        let sql = "SELECT COALESCE((SELECT count(*) FROM checks c2 WHERE c2.nullable = checks.id), 0) FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert_eq!(
            finding.message,
            "COALESCE never uses the arguments after its first one: it is never NULL"
        );
    }

    #[test]
    fn coalesce_dropping_an_untyped_literal_keeps_the_fix() {
        let db = catalog();
        let sql = "SELECT COALESCE(name, '') AS n FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert_eq!(fixed(sql, finding), "SELECT name AS n FROM checks");
    }

    #[test]
    fn coalesce_keeping_an_untyped_literal_has_no_fix() {
        // `COALESCE('1', nn)` is integer; `'1'` alone would be text.
        let db = catalog();
        let sql = "SELECT COALESCE('1', nn) AS x FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert!(finding.fix.is_empty(), "{finding:?}");
    }

    #[test]
    fn coalesce_dropping_a_structural_function_call_has_no_fix() {
        let db = catalog();
        // Aggregates and window functions shape the rows even though
        // `COALESCE` never evaluates them: `COALESCE(1, max(v))` is one row,
        // while `1` alone is one per row.
        for sql in [
            "SELECT COALESCE(1, max(nn)) AS x FROM checks",
            "SELECT COALESCE(nn, max(nn) OVER ()) AS x FROM checks",
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert!(finding.fix.is_empty(), "{sql}: {finding:?}");
        }
        // A call inside a sublink belongs to the subquery, so dropping the
        // whole sublink is safe.
        let sql = "SELECT COALESCE(1, (SELECT max(1))) AS x FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert_eq!(fixed(sql, finding), "SELECT 1 AS x FROM checks");
    }

    #[test]
    fn coalesce_parenthesization_requires_the_whole_argument() {
        let db = catalog();
        // Only `((1) + 2)` is wholly parenthesized; `(1) + 2` is not, so the
        // call's grouping must be re-added.
        let cases = [
            ("SELECT COALESCE((1) + 2, 0) * 3", "SELECT ((1) + 2) * 3"),
            ("SELECT COALESCE(((1) + 2), 0) * 3", "SELECT ((1) + 2) * 3"),
        ];
        for (sql, expected) in cases {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert_eq!(fixed(sql, finding), expected, "{sql}");
        }
    }

    #[test]
    fn coalesce_dropping_a_collated_or_column_argument_has_no_fix() {
        let db = catalog();
        // A dropped argument's collation can still decide the result's; a
        // column reference brings its own.
        for sql in [
            "SELECT COALESCE(name, 'y'::text COLLATE \"C\") AS x FROM checks",
            "SELECT COALESCE(name, note) AS x FROM checks",
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert!(finding.fix.is_empty(), "{sql}: {finding:?}");
        }
    }

    #[test]
    fn coalesce_dropping_a_parameter_has_no_fix() {
        let db = catalog();
        // Dropping the argument would remove the parameter (or what typed
        // it) from the query.
        for sql in [
            "SELECT COALESCE(nn, $d::int) AS x FROM checks",
            "SELECT COALESCE(nn, $1::int) AS x FROM checks",
            "SELECT COALESCE(nn, $d::int + 1) AS x FROM checks WHERE id = $d",
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert!(finding.fix.is_empty(), "{sql}: {finding:?}");
        }
    }

    #[test]
    fn coalesce_whose_type_modifier_the_kept_argument_lacks_has_no_fix() {
        let mut db = catalog();
        db.apply_sql("CREATE TABLE vcs (vc varchar(10) NOT NULL)")
            .unwrap();
        // `COALESCE(vc, NULL)` is a plain varchar (its arguments' modifiers
        // differ); `vc` alone a varchar(10). So is `COALESCE(vc, 'x')`.
        for sql in [
            "SELECT COALESCE(vc, NULL) AS v FROM vcs",
            "SELECT COALESCE(vc, 'x') AS v FROM vcs",
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert!(finding.fix.is_empty(), "{sql}: {finding:?}");
        }
    }

    #[test]
    fn coalesce_naming_an_unaliased_target_has_no_fix() {
        let db = catalog();
        // The unnamed target column would be renamed from `coalesce` to `nn`,
        // and any rewrite of it is withheld to keep the output name stable.
        for sql in [
            "SELECT COALESCE(nn, 0) FROM checks",
            "SELECT COALESCE(nullable, nn, 0) FROM checks",
            "SELECT * FROM (SELECT COALESCE(nn, 0) FROM checks) s",
            // A FROM item.
            "SELECT * FROM coalesce(1, 2)",
            // A CASE takes its ELSE's name: `coalesce`.
            "SELECT CASE WHEN id > 0 THEN 1 ELSE COALESCE(nn, 0) END FROM checks",
            // A cast or COLLATE takes its operand's name: still `coalesce`.
            "SELECT COALESCE(nn, 0)::bigint FROM checks",
            "SELECT (COALESCE(name, 'x'))::varchar FROM checks",
            "SELECT COALESCE(name, 'x') COLLATE \"C\" FROM checks",
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert!(finding.fix.is_empty(), "{sql}: {finding:?}");
        }
        // An alias keeps the output name, and a larger expression is not the
        // target entry's name.
        for (sql, expected) in [
            (
                "SELECT COALESCE(nn, 0) AS nn FROM checks",
                "SELECT nn AS nn FROM checks",
            ),
            (
                "SELECT COALESCE(nn, 0) + 1 FROM checks",
                "SELECT nn + 1 FROM checks",
            ),
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert_eq!(fixed(sql, finding), expected, "{sql}");
        }
    }

    #[test]
    fn coalesce_removal_keeps_adjacent_tokens_apart() {
        let db = catalog();
        for (sql, expected) in [
            ("SELECT COALESCE(1,0)AS result", "SELECT 1 AS result"),
            (
                "SELECT COALESCE(\"name\", 'y'::text)AS result FROM checks",
                "SELECT \"name\" AS result FROM checks",
            ),
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert_eq!(fixed(sql, finding), expected, "{sql}");
        }
    }

    #[test]
    fn never_null_inference_handles_empty_inputs_and_srf_padding() {
        let db = catalog();
        // A scalar subquery returns NULL when it selects no row; an aggregate
        // over no rows is NULL; a CASE without ELSE is NULL on the else path;
        // and target-list set-returning functions of different lengths pad
        // the shorter ones with NULL.
        for sql in [
            "SELECT COALESCE((SELECT nn FROM checks WHERE false), 0) AS x FROM checks",
            "SELECT COALESCE(max(nn), 0) AS x FROM checks",
            "SELECT COALESCE(CASE WHEN nn > 0 THEN nn END, 0) AS x FROM checks",
            "SELECT generate_series(1, 2), unnest(ARRAY[1, 2, 3]) IS NULL",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::RedundantNullCheck),
                "{sql}: {findings:?}"
            );
        }
        // `count(*)` is 0, never NULL, even over an empty input.
        let sql = "SELECT count(*) IS NULL FROM checks";
        let findings = lint(&db, sql);
        assert_eq!(only(&findings).rule, Rule::RedundantNullCheck);
    }

    #[test]
    fn coalesce_that_may_use_every_argument_is_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT COALESCE(note, 'x') FROM checks",
            "SELECT COALESCE(note, nullable::text) FROM checks",
            "SELECT COALESCE(nullable, nn + nullable, 0) FROM checks",
            "SELECT COALESCE($1, 0)",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::RedundantNullCheck),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn allow_comment_suppresses_the_rule() {
        let db = catalog();
        for sql in [
            "-- typedpg-lint: allow(redundant_null_check)\nSELECT nn IS NULL FROM checks",
            "/* typedpg-lint: allow(redundant_null_check) */ SELECT COALESCE(nn, 0) FROM checks",
        ] {
            let findings = lint(&db, sql);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn query_view_collects_the_null_check_facts_without_leaking_from_analyze() {
        let db = catalog();
        db.analyze("SELECT nn IS NULL, COALESCE(nn, 0) FROM checks")
            .unwrap();
        let view = db
            .query_view("SELECT nn IS NULL, COALESCE(nn, 0) FROM checks")
            .unwrap();
        let mut null_test = None;
        let mut coalesce = None;
        for (node, _) in view.tree.nodes() {
            match node {
                NodeRef::NullTest(test) => null_test = Some(test.location),
                NodeRef::CoalesceExpr(expr) => coalesce = Some(expr.location),
                _ => {}
            }
        }
        assert_eq!(
            view.null_test_arg_nullable(null_test.expect("a NullTest")),
            Some(false)
        );
        let fact = view
            .coalesce_fact(coalesce.expect("a CoalesceExpr"))
            .unwrap();
        assert_eq!(fact.args_nullable, vec![false, false]);
        assert!(fact.args_share_result_type);

        let view = db
            .query_view("SELECT nullable IS NULL FROM checks")
            .unwrap();
        let location = view
            .tree
            .nodes()
            .find_map(|(node, _)| match node {
                NodeRef::NullTest(test) => Some(test.location),
                _ => None,
            })
            .unwrap();
        assert_eq!(view.null_test_arg_nullable(location), Some(true));

        // A plain analyze records nothing: a view without such a check finds
        // none.
        let view = db.query_view("SELECT id FROM checks").unwrap();
        assert!(view.null_test_arg_nullable(0).is_none());
        assert!(view.coalesce_fact(0).is_none());
    }
}
