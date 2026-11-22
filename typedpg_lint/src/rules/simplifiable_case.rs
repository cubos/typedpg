//! The `simplifiable_case` rule: reports a `CASE` that a simpler
//! expression says the same as.
//!
//! - `CASE WHEN x IS NULL THEN y ELSE x END` is `COALESCE(x, y)`, for `x` a
//!   column or a parameter (read twice by the `CASE`, once by
//!   `COALESCE`) that isn't a row: a row of NULL fields `IS NULL`, while
//!   `COALESCE` tests the value itself. PostgreSQL resolves both result
//!   types over `x` then `y` (the `CASE` puts its `ELSE` first), and
//!   evaluates `y` only when `x` is NULL in both.
//! - `CASE WHEN c THEN true ELSE false END` is `c` when `c` is a `boolean`
//!   that is never NULL; when it may be NULL, the `CASE` turns NULL into
//!   false, which `c IS TRUE` says — the finding then has no fix.
//!
//! A `CASE` that names its output column (`case`) keeps it: the fix would
//! rename the column, so it is withheld there. A `CASE … ELSE NULL END` is
//! not read (`case_without_else` asks for that `ELSE`).

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, CaseExpr, Node, Node as N, NullTestType};

use super::{leftmost_start, location, scan_until};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    // The CASEs that are an output column's whole expression with no
    // alias: they name the column.
    let mut naming = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::ResTarget(target) = node
            && target.name.is_empty()
            && let Some(N::CaseExpr(case)) = target.val.as_ref()
        {
            naming.insert(case.location);
        }
    }
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CaseExpr(case) = node {
            simplifiable(ctx, case, naming.contains(&case.location), &mut out);
        }
    }
    out
}

fn simplifiable(ctx: &Ctx<'_>, case: &CaseExpr, names_column: bool, out: &mut Vec<RawFinding>) {
    if case.arg.is_some() {
        return;
    }
    let [Some(N::CaseWhen(when))] = case.args.as_slice() else {
        return;
    };
    let (Some(condition), Some(result), Some(default)) = (
        when.expr.as_ref(),
        when.result.as_ref(),
        case.defresult.as_ref(),
    ) else {
        return;
    };
    let Some(start) = location(case.location) else {
        return;
    };
    // The whole CASE … END.
    let Some(case_index) = ctx.tokens.iter().position(|token| token.start == start) else {
        return;
    };
    let (_, end_index) = scan_until(ctx, ctx.tokens[case_index].end, |_| false);
    let Some(end_index) = end_index.filter(|&index| {
        let token = ctx.tokens[index];
        ctx.sql[token.start..token.end].eq_ignore_ascii_case("END")
    }) else {
        return;
    };
    let end = ctx.tokens[end_index].end;
    if let N::NullTest(test) = condition
        && test.nulltesttype == NullTestType::IsNull
        && let Some(tested) = test.arg.as_ref()
    {
        let case = CaseParts {
            start,
            end,
            names_column,
        };
        coalesce(ctx, case, test.location, tested, result, default, out);
        return;
    }
    if is_boolean(result, true) && is_boolean(default, false) {
        let Some(cond_start) = leftmost_start(condition) else {
            return;
        };
        let (cond_end, _) = scan_until(ctx, cond_start, |text| text.eq_ignore_ascii_case("THEN"));
        let written = ctx.written(cond_start..cond_end);
        let not_null = ctx.view.case_condition_not_null(when.location) == Some(true);
        let fix = if not_null && !names_column && cond_end > cond_start {
            let text = if matches!(condition, N::ColumnRef(_) | N::FuncCall(_) | N::AConst(_)) {
                written.to_owned()
            } else {
                format!("({written})")
            };
            vec![SqlEdit {
                range: start..end,
                text,
            }]
        } else {
            Vec::new()
        };
        let help = if not_null {
            format!("write {written}")
        } else {
            format!("write ({written}) IS TRUE: the CASE is false where the condition is NULL")
        };
        out.push(RawFinding {
            span: start..end,
            message: "CASE WHEN … THEN true ELSE false END is its condition".to_owned(),
            help: Some(help),
            fix,
        });
    }
}

/// Where a reported `CASE` is.
struct CaseParts {
    start: usize,
    end: usize,
    names_column: bool,
}

/// `CASE WHEN x IS NULL THEN y ELSE x END`.
fn coalesce(
    ctx: &Ctx<'_>,
    case: CaseParts,
    test_location: i32,
    tested: &Node,
    result: &Node,
    default: &Node,
    out: &mut Vec<RawFinding>,
) {
    // A column or a parameter (under a cast), the same in both places.
    let plain = match tested {
        N::ColumnRef(_) | N::ParamRef(_) => true,
        N::TypeCast(cast) => matches!(cast.arg.as_ref(), Some(N::ParamRef(_))),
        _ => false,
    };
    if !plain {
        return;
    }
    let (Some(tested_start), Some(default_start), Some(result_start)) = (
        leftmost_start(tested),
        leftmost_start(default),
        leftmost_start(result),
    ) else {
        return;
    };
    let tested_text = text_until(ctx, tested_start, "IS");
    let default_text = text_until(ctx, default_start, "END");
    if tokens_of(ctx, tested_text.clone()) != tokens_of(ctx, default_text.clone()) {
        return;
    }
    // Whether the test reads a row: the analyzer records the argument of a
    // test on a scalar only. A parameter and a column are the same value at
    // both places only if both resolved the same way.
    if ctx.view.null_test_arg_nullable(test_location).is_none() {
        return;
    }
    if let (N::ColumnRef(a), N::ColumnRef(b)) = (tested, default) {
        let (Some(a), Some(b)) = (
            ctx.view.column_ref_binding(a.location),
            ctx.view.column_ref_binding(b.location),
        ) else {
            return;
        };
        if a.source.is_none() || a != b {
            return;
        }
    }
    let result_text = text_until(ctx, result_start, "ELSE");
    let x = ctx.written(tested_text);
    let y = ctx.written(result_text);
    let fix = if case.names_column {
        Vec::new()
    } else {
        vec![SqlEdit {
            range: case.start..case.end,
            text: format!("COALESCE({x}, {y})"),
        }]
    };
    out.push(RawFinding {
        span: case.start..case.end,
        message: format!("this CASE is COALESCE({x}, {y})"),
        help: Some(format!("write COALESCE({x}, {y})")),
        fix,
    });
}

/// The bytes from `start` to the token `keyword` at nesting depth zero.
fn text_until(ctx: &Ctx<'_>, start: usize, keyword: &str) -> std::ops::Range<usize> {
    let (end, _) = scan_until(ctx, start, |text| text.eq_ignore_ascii_case(keyword));
    start..end.max(start)
}

/// The token texts in `range`, for comparing two spellings.
fn tokens_of<'a>(ctx: &Ctx<'a>, range: std::ops::Range<usize>) -> Vec<&'a str> {
    ctx.tokens
        .iter()
        .filter(|token| token.start >= range.start && token.end <= range.end)
        .map(|token| &ctx.sql[token.start..token.end])
        .collect()
}

/// Whether `node` is the boolean literal `value`.
fn is_boolean(node: &Node, value: bool) -> bool {
    matches!(node, N::AConst(c) if !c.isnull
        && matches!(&c.val, Some(AConstValue::Boolean(b)) if b.boolval == value))
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::SimplifiableCase;

    #[test]
    fn cases_that_are_a_coalesce_are_rewritten() {
        let db = catalog();
        for (sql, expected) in [
            (
                "SELECT CASE WHEN name IS NULL THEN 'none' ELSE name END AS n FROM users",
                "SELECT COALESCE(name, 'none') AS n FROM users",
            ),
            (
                "SELECT id FROM users u WHERE CASE WHEN u.name IS NULL THEN email ELSE u.name END \
                 = 'x'",
                "SELECT id FROM users u WHERE COALESCE(u.name, email) = 'x'",
            ),
            (
                "SELECT CASE WHEN $1::text IS NULL THEN 'a' ELSE $1::text END AS v",
                "SELECT COALESCE($1::text, 'a') AS v",
            ),
            // Named parameters are written back as named.
            (
                "SELECT CASE WHEN $v::text IS NULL THEN $d::text ELSE $v::text END AS v",
                "SELECT COALESCE($v::text, $d::text) AS v",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn cases_that_are_their_condition_are_rewritten() {
        let db = catalog();
        for (sql, expected) in [
            (
                "SELECT CASE WHEN id > 1 THEN true ELSE false END AS big FROM users",
                "SELECT (id > 1) AS big FROM users",
            ),
            (
                "SELECT id FROM users WHERE CASE WHEN email = 'x' THEN true ELSE false END",
                "SELECT id FROM users WHERE (email = 'x')",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn what_has_no_fix() {
        let db = catalog();
        for sql in [
            // `name = 'x'` may be NULL.
            "SELECT CASE WHEN name = 'x' THEN true ELSE false END AS b FROM users",
            "SELECT CASE WHEN $1::bool THEN true ELSE false END AS b",
            // The CASE names the column.
            "SELECT CASE WHEN id > 1 THEN true ELSE false END FROM users",
            "SELECT CASE WHEN name IS NULL THEN 'none' ELSE name END FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn other_cases_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT CASE WHEN name IS NULL THEN 'none' ELSE email END AS n FROM users",
            "SELECT CASE WHEN name IS NULL THEN 'none' ELSE NULL END AS n FROM users",
            "SELECT CASE WHEN name IS NOT NULL THEN name ELSE 'x' END AS n FROM users",
            "SELECT CASE WHEN lower(name) IS NULL THEN 'x' ELSE lower(name) END AS n FROM users",
            "SELECT CASE WHEN id > 1 THEN false ELSE true END AS b FROM users",
            "SELECT CASE WHEN id > 1 THEN true WHEN id < 0 THEN true ELSE false END AS b \
             FROM users",
            "SELECT CASE WHEN u IS NULL THEN NULL ELSE u END AS r FROM users u",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
