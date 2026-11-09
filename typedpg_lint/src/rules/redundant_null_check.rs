//! The `redundant_null_check` rule: reports an `IS [NOT] NULL` test or a
//! `COALESCE` on a value the analysis proves is never NULL. A `COALESCE`
//! finding comes with a fix dropping the arguments it never uses, when every
//! argument has the call's result type; an `IS [NOT] NULL` test has none
//! (removing it from a `WHERE` changes which rows are kept).

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{CoalesceExpr, Node, NullTest, NullTestType, node::Node as N};

use super::{expression_end, location, operand_start};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::NullTest(test) => null_test(ctx, test, &mut out),
            NodeRef::CoalesceExpr(expr) => coalesce(ctx, expr, &mut out),
            _ => {}
        }
    }
    out
}

// ── IS [NOT] NULL ──────────────────────────────────────────────────────────

fn null_test(ctx: &Ctx<'_>, test: &NullTest, out: &mut Vec<RawFinding>) {
    let Some(arg) = test.arg.as_deref() else {
        return;
    };
    // The analyzer's fact: without one, it didn't prove the argument's
    // nullability (a row / composite value, or a site it never inferred),
    // and the rule never guesses.
    if ctx.view.null_test_arg_nullable(test.location) != Some(false) {
        return;
    }
    let Some(start) = argument_start(arg).or_else(|| location(test.location)) else {
        return;
    };
    let Some(end) = null_keyword_end(ctx, test.location).filter(|end| *end > start) else {
        return;
    };
    let is_null = NullTestType::try_from(test.nulltesttype) == Ok(NullTestType::IsNull);
    let message = match (argument_text(ctx, arg), is_null) {
        (Some(text), true) => format!("`{text}` IS NULL is never true: it is never NULL"),
        (Some(text), false) => format!("`{text}` IS NOT NULL is always true: it is never NULL"),
        (None, true) => "IS NULL is never true here: the value is never NULL".to_owned(),
        (None, false) => "IS NOT NULL is always true here: the value is never NULL".to_owned(),
    };
    out.push(RawFinding {
        span: start..end,
        message,
        help: Some(
            "remove the check, or find why the value was expected to be NULL (an outer join?)"
                .to_owned(),
        ),
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

fn coalesce(ctx: &Ctx<'_>, expr: &CoalesceExpr, out: &mut Vec<RawFinding>) {
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
        .unwrap_or_else(|| argument_text(ctx, &expr.args[index]));
    let mut fix = Vec::new();
    // The kept argument decides the type once the others are gone: it must
    // already have the result type (`COALESCE('1', int_col)` is integer,
    // `'1'` alone is not).
    if fact.args_share_result_type
        && fact.args_typed_as_result.get(index) == Some(&true)
        && let Some(args) = &args
        && args.len() == expr.args.len()
    {
        let close_start = ctx.tokens[close].start;
        let arg_end = args[index].1;
        if index > 0 {
            fix.push(SqlEdit {
                range: arg_end..close_start,
                text: String::new(),
            });
        } else if is_simple_argument(ctx, &expr.args[0], args[0].0) {
            // `COALESCE(x, …)` becomes `x` — the call's closing parenthesis
            // goes too.
            fix.push(SqlEdit {
                range: start..ctx.tokens[open].end,
                text: String::new(),
            });
            fix.push(SqlEdit {
                range: arg_end..ctx.tokens[close].end,
                text: String::new(),
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
                text: String::new(),
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
/// argument bare: a column, parameter, constant, function call or an already
/// parenthesized expression binds like the call did.
fn is_simple_argument(ctx: &Ctx<'_>, node: &Node, start: usize) -> bool {
    if ctx.sql[start..].starts_with('(') {
        return true;
    }
    matches!(
        node.node.as_ref(),
        Some(N::ColumnRef(_) | N::ParamRef(_) | N::AConst(_) | N::FuncCall(_))
    )
}

// ── text ───────────────────────────────────────────────────────────────────

/// The start of the token sequence of `node`, descending through casts and a
/// binary expression's left side so `a + b` starts at `a` (an `AExpr`'s own
/// location is its operator).
fn argument_start(node: &Node) -> Option<usize> {
    match node.node.as_ref()? {
        N::TypeCast(cast) => cast
            .arg
            .as_deref()
            .and_then(argument_start)
            .or_else(|| operand_start(node)),
        N::AExpr(expr) => expr
            .lexpr
            .as_deref()
            .and_then(argument_start)
            .or_else(|| operand_start(node)),
        _ => operand_start(node),
    }
}

/// The expression as the message shows it, on one line; `None` when it is
/// too long to quote.
fn argument_text(ctx: &Ctx<'_>, node: &Node) -> Option<String> {
    let start = argument_start(node)?;
    short_text(ctx, start, expression_end(ctx, start))
}

fn short_text(ctx: &Ctx<'_>, start: usize, end: usize) -> Option<String> {
    let text = ctx.sql.get(start..end).unwrap_or_default();
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
    fn is_null_span_covers_the_argument_and_the_null_keyword() {
        let db = catalog();
        let sql = "SELECT nn IS NOT NULL FROM checks";
        let findings = lint(&db, sql);
        assert_eq!(&sql[only(&findings).span.clone()], "nn IS NOT NULL");
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
        // anything.
        for sql in [
            "SELECT id FROM checks WHERE nullable IS NOT NULL",
            "SELECT id FROM checks WHERE nullable IS NULL",
            "SELECT id FROM checks WHERE nullable > 0 AND nullable IS NOT NULL",
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
                "SELECT COALESCE(nn, 0) FROM checks",
                "SELECT nn FROM checks",
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
                "SELECT COALESCE(nullable, nn, 0) FROM checks",
                "SELECT COALESCE(nullable, nn) FROM checks",
            ),
            (
                "SELECT COALESCE(nullable, nn, a, b, 0) FROM checks",
                "SELECT COALESCE(nullable, nn) FROM checks",
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
        let sql = "SELECT COALESCE(name, '') FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert_eq!(fixed(sql, finding), "SELECT name FROM checks");
    }

    #[test]
    fn coalesce_keeping_an_untyped_literal_has_no_fix() {
        // `COALESCE('1', nn)` is integer; `'1'` alone would be text.
        let db = catalog();
        let sql = "SELECT COALESCE('1', nn) FROM checks";
        let findings = lint(&db, sql);
        let finding = only(&findings);
        assert!(finding.fix.is_empty(), "{finding:?}");
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
            .into_iter()
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
