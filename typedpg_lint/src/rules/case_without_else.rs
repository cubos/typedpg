//! The `case_without_else` rule: reports a `CASE` without `ELSE` whose
//! `WHEN`s may all fail for some row, where PostgreSQL's implicit `ELSE
//! NULL` makes it NULL — a NULL easy to miss when reading the query. The
//! analyzer decides (see
//! [`typedpg_analyzer::QueryView::case_else_reachable`]): a `WHEN` that
//! certainly folds to TRUE, or `WHEN`s the constraints and the conditions
//! around them prove cover every row (`WHEN active … WHEN NOT active …` over
//! a NOT NULL column, every value a CHECK allows), leave no row to the
//! `ELSE`, and the `CASE` is not reported.
//!
//! The fix writes the `ELSE NULL` out: an explicit NULL literal is the same
//! untyped input the implicit one is, so the result type and every row
//! stay the same.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::CaseExpr;

use super::{location, scan_until};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CaseExpr(case) = node
            && case.defresult.is_none()
            && ctx.view.case_else_reachable(case.location) == Some(true)
        {
            finding(ctx, case, &mut out);
        }
    }
    out
}

fn finding(ctx: &Ctx<'_>, case: &CaseExpr, out: &mut Vec<RawFinding>) {
    let Some(start) = location(case.location) else {
        return;
    };
    // The CASE's END: the token closing the level its CASE keyword opens.
    let (_, end) = scan_until(ctx, start + "CASE".len(), |_| false);
    let Some(end_token) = end.map(|index| ctx.tokens[index]) else {
        return;
    };
    if !ctx.sql[end_token.start..end_token.end].eq_ignore_ascii_case("END") {
        return;
    }
    out.push(RawFinding {
        span: start..end_token.end,
        message: "CASE without ELSE is NULL when no WHEN matches".to_owned(),
        help: Some("write ELSE NULL if that is intended, or add the missing case".to_owned()),
        fix: vec![SqlEdit {
            range: end_token.start..end_token.start,
            text: "ELSE NULL ".to_owned(),
        }],
    });
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::CaseWithoutElse;

    #[test]
    fn a_case_that_may_fall_through_gets_an_explicit_else() {
        let db = catalog();
        for (sql, expected) in [
            (
                "SELECT CASE WHEN id > 1 THEN 'big' END AS size FROM users",
                "SELECT CASE WHEN id > 1 THEN 'big' ELSE NULL END AS size FROM users",
            ),
            (
                "SELECT CASE name WHEN 'a' THEN 1 WHEN 'b' THEN 2 END AS n FROM users",
                "SELECT CASE name WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE NULL END AS n FROM users",
            ),
            // The inner CASE only.
            (
                "SELECT CASE WHEN id > 1 THEN CASE WHEN name = 'x' THEN 1 END ELSE 0 END AS n \
                 FROM users",
                "SELECT CASE WHEN id > 1 THEN CASE WHEN name = 'x' THEN 1 ELSE NULL END ELSE 0 \
                 END AS n FROM users",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn a_case_that_covers_every_row_is_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT CASE WHEN id > 1 THEN 1 ELSE 2 END AS n FROM users",
            "SELECT CASE WHEN id > 1 THEN 1 ELSE NULL END AS n FROM users",
            // A NOT NULL column is one or the other.
            "SELECT CASE WHEN id > 1 THEN 1 WHEN id <= 1 THEN 2 END AS n FROM users",
            "SELECT CASE WHEN email IS NULL THEN 1 WHEN email IS NOT NULL THEN 2 END AS n \
             FROM users",
            // An always-true WHEN.
            "SELECT CASE WHEN id > 1 THEN 1 WHEN true THEN 2 END AS n FROM users",
            // The WHERE makes a WHEN true for every row.
            "SELECT CASE WHEN name = 'a' THEN 1 END AS n FROM users WHERE name = 'a'",
            "SELECT CASE WHEN name = 'a' THEN 1 WHEN name = 'b' THEN 2 END AS n FROM users \
             WHERE name IN ('a', 'b')",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
