//! The `unreachable_case_branch` rule: reports the branches of a searched
//! `CASE` that are never taken — a `WHEN` whose condition is never true
//! (FALSE or NULL), and everything after a `WHEN` that is always true (the
//! later branches and the `ELSE`). A condition is decided when the planner
//! certainly folds it to a constant (see
//! [`typedpg_analyzer::PgCatalog::condition_constant`]), or when what the
//! analyzer knows of its operands decides it — a column's CHECK
//! constraints, domain or enum, the conditions around it and the `WHEN`s
//! before it (see `constant_condition`, which reports the decided parts of
//! a condition not decided as a whole); the message then says why.
//!
//! No fix is offered: every branch's result takes part in the `CASE`'s
//! result type (`select_common_type`), so dropping one can change it, and
//! what the dead branch was meant to do is for its author to decide.
//!
//! A simple `CASE x WHEN v …` compares `x = v` — the operator resolved as
//! the written comparison would be, after the same coercions — and its
//! `WHEN`s are judged by what the analyzer knows there, as a searched
//! `CASE WHEN x = v …` is; its values are no conditions, so the planner's
//! folding of a condition is not asked.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{CaseExpr, Node as N};

use super::constant_condition::condition_value;
use super::{location, scan_until};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CaseExpr(case) = node {
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
    let simple = case.arg.is_some();
    let never_matches = if simple {
        "CASE branch is never taken: with the WHENs before it not taken, the CASE operand \
         never equals its value"
    } else {
        "CASE branch is never taken: with the WHENs before it not taken, its condition is never \
         true"
    };
    for (i, when) in whens.iter().enumerate() {
        let Some(start) = location(when.location) else {
            return;
        };
        let decided = when
            .expr
            .as_ref()
            .filter(|_| !simple)
            .and_then(|condition| condition_value(ctx, condition));
        let (value, never, after) = match decided {
            Some((value, reason)) => {
                let because = reason.map_or_else(String::new, |r| format!(" ({r})"));
                (
                    value,
                    format!("CASE branch is never taken: its condition is never true{because}"),
                    format!("CASE branches after an always-true WHEN are never taken{because}"),
                )
            }
            // What the analyzer knows where the CASE is evaluated, with the
            // WHENs before this one not taken.
            None if ctx.view.case_when_reached(when.location) == Some(false) => {
                (false, never_matches.to_owned(), String::new())
            }
            None if ctx.view.case_when_passed(when.location) == Some(false) => (
                true,
                String::new(),
                "CASE branches after this WHEN are never taken: every row takes it or a branch \
                 before it"
                    .to_owned(),
            ),
            None => continue,
        };
        if !value {
            let end = branch_end(ctx, start, true);
            out.push(RawFinding {
                span: start..end,
                message: never,
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
                message: after,
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
    fn a_null_operand_of_distinct_from_or_in_is_no_constant() {
        let db = catalog();
        for sql in [
            "SELECT CASE WHEN name IS DISTINCT FROM NULL THEN 1 ELSE 0 END AS v FROM users",
            "SELECT CASE WHEN name IS NOT DISTINCT FROM NULL THEN 1 ELSE 0 END AS v FROM users",
            // TRUE when another item matches.
            "SELECT CASE WHEN name IN ('a', NULL) THEN 1 ELSE 0 END AS v FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

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
    fn the_where_decides_a_select_list_branch() {
        let db = catalog();
        for (sql, reported, because) in [
            (
                "SELECT CASE WHEN name = 'b' THEN 1 ELSE 2 END AS v FROM users WHERE name = 'a'",
                "WHEN name = 'b' THEN 1",
                "never taken: its condition is never true (`name` is always 'a' where this is \
                 evaluated)",
            ),
            (
                "SELECT CASE WHEN name = 'a' THEN 1 ELSE 2 END AS v FROM users WHERE name = 'a'",
                "ELSE 2",
                "after an always-true WHEN",
            ),
            (
                "SELECT CASE WHEN name = 'a' THEN 1 WHEN name <> 'b' THEN 2 ELSE 3 END AS v \
                 FROM users WHERE name IN ('a', 'b')",
                "WHEN name <> 'b' THEN 2",
                "with the WHENs before it not taken",
            ),
            (
                "SELECT CASE WHEN name = 'a' THEN 1 WHEN name = 'b' THEN 2 ELSE 3 END AS v \
                 FROM users WHERE name IN ('a', 'b')",
                "ELSE 3",
                "after this WHEN are never taken",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert!(
                findings[0].message.contains(because),
                "{sql}: {}",
                findings[0].message
            );
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

    #[test]
    fn what_the_analyzer_knows_decides_a_branch() {
        let mut db = typedpg_analyzer::PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE t (id int PRIMARY KEY, s text NOT NULL CHECK (s IN ('a', 'b')));",
        )
        .unwrap();
        for (sql, reported, because) in [
            (
                "SELECT CASE WHEN s = 'c' THEN 1 ELSE 2 END FROM t",
                "WHEN s = 'c' THEN 1",
                "never taken: its condition is never true (`s` is always one of 'a', 'b' \
                 (CHECK constraint \"t_s_check\")",
            ),
            // The reason quotes a named parameter as written.
            (
                "SELECT CASE WHEN abs($d::int) < 0 THEN 1 ELSE 2 END FROM t",
                "WHEN abs($d::int) < 0 THEN 1",
                "(`abs($d::int)` is always at least 0 where this is evaluated)",
            ),
            (
                "SELECT CASE WHEN s IN ('a', 'b') THEN 1 ELSE 2 END FROM t",
                "ELSE 2",
                "after an always-true WHEN",
            ),
            // The WHENs before it are known not to be taken.
            (
                "SELECT CASE WHEN s = 'a' THEN 1 WHEN s = 'a' THEN 2 ELSE 3 END FROM t",
                "WHEN s = 'a' THEN 2",
                "never taken",
            ),
            (
                "SELECT CASE WHEN s = 'a' THEN 1 WHEN s = 'b' THEN 2 ELSE 3 END FROM t",
                "ELSE 3",
                "after this WHEN are never taken",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert!(
                findings[0].message.contains(because),
                "{sql}: {}",
                findings[0].message
            );
        }
        for sql in [
            "SELECT CASE WHEN s = 'a' THEN 1 ELSE 2 END FROM t",
            "SELECT CASE WHEN id > 3 THEN 1 ELSE 2 END FROM t",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_simple_case_value_the_operand_never_equals_is_reported() {
        let mut db = typedpg_analyzer::PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE t (
                id int PRIMARY KEY,
                s text NOT NULL CHECK (s IN ('a', 'b')),
                n int CHECK (n BETWEEN 1 AND 3)
             );
             CREATE COLLATION nd (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
             CREATE TABLE u (s text COLLATE nd NOT NULL CHECK (s IN ('a', 'b')));",
        )
        .unwrap();
        for (sql, reported, message) in [
            (
                "SELECT CASE s WHEN 'c' THEN 1 ELSE 2 END FROM t",
                "WHEN 'c' THEN 1",
                "CASE branch is never taken: with the WHENs before it not taken, the CASE \
                 operand never equals its value",
            ),
            (
                "SELECT CASE n WHEN 1 THEN 'x' WHEN 7 THEN 'y' END FROM t",
                "WHEN 7 THEN 'y'",
                "never equals its value",
            ),
            // The WHENs before it took the value.
            (
                "SELECT CASE s WHEN 'a' THEN 1 WHEN 'a' THEN 2 ELSE 3 END FROM t",
                "WHEN 'a' THEN 2",
                "never equals its value",
            ),
            (
                "SELECT CASE s WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 3 END FROM t",
                "ELSE 3",
                "CASE branches after this WHEN are never taken",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert!(
                findings[0].message.contains(message),
                "{sql}: {}",
                findings[0].message
            );
            assert!(findings[0].fix.is_empty());
        }
        for sql in [
            "SELECT CASE s WHEN 'a' THEN 1 ELSE 2 END FROM t",
            "SELECT CASE s WHEN 'a' THEN 1 WHEN 'b' THEN 2 END FROM t",
            // A NULL n takes the ELSE.
            "SELECT CASE n WHEN 1 THEN 'x' WHEN 2 THEN 'y' WHEN 3 THEN 'z' ELSE 'w' END FROM t",
            // Compared under another collation, or as another type: the
            // CHECK's values say nothing of that comparison.
            "SELECT CASE s COLLATE \"C\" WHEN 'c' THEN 1 ELSE 2 END FROM t",
            "SELECT CASE s WHEN 'c' COLLATE \"C\" THEN 1 ELSE 2 END FROM t",
            "SELECT CASE s::varchar(1) WHEN 'c' THEN 1 ELSE 2 END FROM t",
            // Under a nondeterministic collation `'A'` equals `'a'`.
            "SELECT CASE s WHEN 'A' THEN 1 ELSE 2 END FROM u",
            // Not a column.
            "SELECT CASE id + 1 WHEN 0 THEN 1 ELSE 2 END FROM t",
            "SELECT CASE $1::text WHEN 'c' THEN 1 ELSE 2 END FROM t",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
