//! The `unreachable_case_branch` rule: reports the branches of a searched
//! `CASE` that are never taken — a `WHEN` whose condition the planner
//! certainly folds to FALSE or NULL, and everything after a `WHEN` it
//! certainly folds to TRUE (the later branches and the `ELSE`). See
//! [`typedpg_analyzer::PgCatalog::condition_constant`].
//!
//! No fix is offered: every branch's result takes part in the `CASE`'s
//! result type (`select_common_type`), so dropping one can change it, and
//! what the dead branch was meant to do is for its author to decide.
//! A simple `CASE x WHEN v …` compares `x` with each value and is not
//! judged.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{CaseExpr, Node as N};

use super::{location, scan_until};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CaseExpr(case) = node
            && case.arg.is_none()
        {
            case_expr(ctx, case, &mut out);
        }
    }
    out
}

fn case_expr(ctx: &Ctx<'_>, case: &CaseExpr, out: &mut Vec<RawFinding>) {
    let whens: Vec<_> = case
        .args
        .iter()
        .filter_map(|arg| match arg {
            Some(N::CaseWhen(when)) => Some(when),
            _ => None,
        })
        .collect();
    for (i, when) in whens.iter().enumerate() {
        let Some(start) = location(when.location) else {
            return;
        };
        let Some(value) = when
            .expr
            .as_ref()
            .and_then(|condition| ctx.catalog.condition_constant(condition))
        else {
            continue;
        };
        if !value {
            let end = branch_end(ctx, start, true);
            out.push(RawFinding {
                span: start..end,
                message: "CASE branch is never taken: its condition is never true".to_owned(),
                help: Some("remove the branch, or fix its condition".to_owned()),
                fix: Vec::new(),
            });
            continue;
        }
        // Always true: nothing after it is reached.
        let rest_start = match whens.get(i + 1) {
            Some(next) => location(next.location),
            None if case.defresult.is_some() => {
                let (_, stop) = scan_until(ctx, start + "WHEN".len(), |text| {
                    text.eq_ignore_ascii_case("ELSE")
                });
                stop.map(|index| ctx.tokens[index].start)
            }
            None => None,
        };
        if let Some(rest_start) = rest_start {
            let end = branch_end(ctx, rest_start, false);
            out.push(RawFinding {
                span: rest_start..end,
                message: "CASE branches after an always-true WHEN are never taken".to_owned(),
                help: Some(
                    "remove them, or replace the CASE with the result of its always-true branch"
                        .to_owned(),
                ),
                fix: Vec::new(),
            });
        }
        return;
    }
}

/// The end of the branch starting at `start` (its `WHEN` or `ELSE`
/// keyword): the end of the token before the next `WHEN` / `ELSE` when
/// `one`, else before the `CASE`'s `END`.
fn branch_end(ctx: &Ctx<'_>, start: usize, one: bool) -> usize {
    // Past the keyword the branch starts with.
    let after = ctx
        .tokens
        .iter()
        .find(|token| token.start >= start)
        .map_or(start, |token| token.end);
    let (end, _) = scan_until(ctx, after, |text| {
        one && (text.eq_ignore_ascii_case("WHEN") || text.eq_ignore_ascii_case("ELSE"))
    });
    end.max(after)
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, lint_rule};

    const RULE: Rule = Rule::UnreachableCaseBranch;

    #[test]
    fn branches_never_taken_are_reported() {
        let db = catalog();
        for (sql, reported) in [
            (
                "SELECT CASE WHEN false THEN 1 WHEN id > 1 THEN 2 END FROM users",
                &["WHEN false THEN 1"][..],
            ),
            (
                "SELECT CASE WHEN id > 1 THEN 1 WHEN 1 = 2 THEN 2 ELSE 3 END FROM users",
                &["WHEN 1 = 2 THEN 2"],
            ),
            (
                "SELECT CASE WHEN NULL THEN 1 ELSE 2 END FROM users",
                &["WHEN NULL THEN 1"],
            ),
            (
                "SELECT CASE WHEN true THEN 1 WHEN id > 1 THEN 2 ELSE 3 END FROM users",
                &["WHEN id > 1 THEN 2 ELSE 3"],
            ),
            (
                "SELECT CASE WHEN id > 1 THEN 1 WHEN 1 = 1 THEN 2 ELSE 3 END FROM users",
                &["ELSE 3"],
            ),
            // A nested CASE ends inside the branch.
            (
                "SELECT CASE WHEN false THEN CASE WHEN id > 1 THEN 1 END ELSE 0 END FROM users",
                &["WHEN false THEN CASE WHEN id > 1 THEN 1 END"],
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            let spans: Vec<&str> = findings.iter().map(|f| &sql[f.span.clone()]).collect();
            assert_eq!(spans, reported, "{sql}: {findings:?}");
            assert!(findings.iter().all(|f| f.fix.is_empty()), "{sql}");
        }
    }

    #[test]
    fn branches_that_may_be_taken_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT CASE WHEN id > 1 THEN 1 ELSE 2 END FROM users",
            "SELECT CASE WHEN $1 THEN 1 ELSE 2 END FROM users",
            // The last WHEN always true, with nothing after it.
            "SELECT CASE WHEN id > 1 THEN 1 WHEN true THEN 2 END FROM users",
            // A simple CASE compares values.
            "SELECT CASE 1 WHEN 2 THEN 'a' ELSE 'b' END",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
