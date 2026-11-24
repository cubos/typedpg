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
//! A `CASE` that is the whole argument (through casts) of a built-in `count`,
//! `sum`, `avg`, `min` or `max` is not reported either: those aggregates skip
//! NULL inputs, so `count(CASE WHEN paid THEN 1 END)` counts the matching
//! rows on purpose.
//!
//! The fix writes the `ELSE NULL` out: an explicit NULL literal is the same
//! untyped input the implicit one is, so the result type and every row
//! stay the same.

use std::collections::HashSet;

use typedpg_analyzer::ProKind;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{CaseExpr, FuncCall, Node};

use super::{location, scan_until};
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut skipped = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::FuncCall(call) = node
            && let [Some(arg)] = call.args.as_slice()
            && skips_nulls(ctx, call)
        {
            let mut arg = arg;
            while let Node::TypeCast(cast) = arg {
                let Some(inner) = cast.arg.as_ref() else {
                    break;
                };
                arg = inner;
            }
            if let Node::CaseExpr(case) = arg {
                skipped.insert(case.location);
            }
        }
    }
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CaseExpr(case) = node
            && case.defresult.is_none()
            && !skipped.contains(&case.location)
            && ctx.view.case_else_reachable(case.location) == Some(true)
        {
            finding(ctx, case, &mut out);
        }
    }
    out
}

/// Whether `call` is PostgreSQL's own `count`, `sum`, `avg`, `min` or `max`
/// aggregate (as an aggregate or a window function), which skip NULL
/// inputs: every routine its name may resolve to is one of `pg_catalog`'s
/// aggregates.
fn skips_nulls(ctx: &Ctx<'_>, call: &FuncCall) -> bool {
    let names: Vec<&str> = call
        .funcname
        .iter()
        .filter_map(|name| match name {
            Some(Node::String(name)) => Some(name.sval.as_str()),
            _ => None,
        })
        .collect();
    let (schema, name) = match names.as_slice() {
        [name] => (None, *name),
        ["pg_catalog", name] => (Some("pg_catalog"), *name),
        _ => return false,
    };
    if !["count", "sum", "avg", "min", "max"].contains(&name) || call.agg_within_group {
        return false;
    }
    let procs = ctx.catalog.find_functions(schema, name);
    !procs.is_empty()
        && procs.iter().all(|proc| {
            proc.prokind == ProKind::Aggregate
                && ctx.catalog.namespace_name(proc.pronamespace) == Some("pg_catalog")
                && proc.oid.get() < 16_384
        })
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

    #[test]
    fn a_case_counted_or_summed_by_an_aggregate_skipping_nulls_is_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT count(CASE WHEN id > 1 THEN 1 END) AS n FROM users",
            "SELECT COUNT(DISTINCT CASE WHEN id > 1 THEN name END) AS n FROM users",
            "SELECT sum(CASE WHEN id > 1 THEN id END) AS s FROM users",
            "SELECT avg((CASE WHEN id > 1 THEN id END)) AS a FROM users",
            "SELECT pg_catalog.min(CASE WHEN id > 1 THEN id END::numeric) AS m FROM users",
            "SELECT max(CASE name WHEN 'a' THEN id END) FILTER (WHERE id > 0) AS m FROM users",
            "SELECT id, sum(CASE WHEN id > 1 THEN id END) OVER () AS s FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_case_inside_another_aggregate_or_expression_is_still_reported() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE FUNCTION public.count(text) RETURNS bigint LANGUAGE sql \
             AS 'SELECT 1::bigint';",
        )
        .unwrap();
        for sql in [
            // Aggregates that keep NULL inputs.
            "SELECT array_agg(CASE WHEN id > 1 THEN id END) AS a FROM users",
            "SELECT json_agg(CASE WHEN id > 1 THEN id END) AS a FROM users",
            "SELECT string_agg(CASE WHEN id > 1 THEN name END, ',') AS a FROM users",
            // Not the whole argument.
            "SELECT sum(CASE WHEN id > 1 THEN id END + 1) AS s FROM users",
            "SELECT sum(coalesce(CASE WHEN id > 1 THEN id END, 0)) AS s FROM users",
            // A `count` a user function of the same name may resolve to.
            "SELECT count(CASE WHEN id > 1 THEN name END) AS n FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        }
    }
}
