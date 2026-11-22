//! The `integer_division` rule: reports an integer division (`/` of two
//! `smallint` / `integer` / `bigint` values, which truncates) whose result
//! is then used as a `numeric` or floating-point value — cast to one
//! (`(done / total)::numeric`), combined with one (`done / total * 1.0`,
//! `done / total > 0.5`), passed where one is expected (`round(done /
//! total, 2)`) or assigned to such a column. The fraction was already lost:
//! `1 / 2 * 100.0` is `0`, not `50`. As a function argument it is only
//! reported for the rounding functions (`round`, `trunc`, `ceil`,
//! `ceiling`, `floor`), which have nothing to round in an integer: another
//! function (`power(2, level / 10)`) may take the steps on purpose.
//!
//! The division may also be an integer factor of the converted value:
//! `(done / total * 100)::numeric` truncates the same way. An integer
//! result that stays an integer (`done / total`, `avg(done / total)`,
//! `sum(...)`) is not reported, nor is a division by `1` or of two integer
//! literals that divide exactly (`10 / 2`).
//!
//! The analyzer records the operand types of each operator and how each
//! value is converted to the type its context takes (see
//! [`typedpg_analyzer::QueryView::value_conversion`]).

use std::collections::HashSet;

use typedpg_analyzer::PgTypeOid;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, AExpr, AExprKind, Node as N};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding};

const INT2: PgTypeOid = PgTypeOid::from_raw(21);
const INT4: PgTypeOid = PgTypeOid::from_raw(23);
const INT8: PgTypeOid = PgTypeOid::from_raw(20);
const FLOAT4: PgTypeOid = PgTypeOid::from_raw(700);
const FLOAT8: PgTypeOid = PgTypeOid::from_raw(701);
const NUMERIC: PgTypeOid = PgTypeOid::from_raw(1700);

fn is_integer(t: PgTypeOid) -> bool {
    matches!(t, INT2 | INT4 | INT8)
}

fn fractional_name(t: PgTypeOid) -> Option<&'static str> {
    match t {
        NUMERIC => Some("numeric"),
        FLOAT4 => Some("real"),
        FLOAT8 => Some("double precision"),
        _ => None,
    }
}

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // The nodes from the root to the current one.
    let mut path: Vec<NodeRef<'_>> = Vec::new();
    for (node, depth) in ctx.tree.nodes() {
        path.truncate(depth);
        let parent = path.last().copied();
        path.push(node);
        let NodeRef::AExpr(converted) = node else {
            continue;
        };
        // A function argument: only a rounding function says the fraction
        // was expected (`power(2, level / 10)` may step on purpose).
        if let Some(parent) = parent
            && !matches!(parent, NodeRef::FuncCall(call) if rounds(call))
            && matches!(parent, NodeRef::FuncCall(_) | NodeRef::NamedArgExpr(_))
        {
            continue;
        }
        let Some(conversion) = ctx.view.value_conversion(node) else {
            continue;
        };
        let Some(target) = fractional_name(conversion.to) else {
            continue;
        };
        if !is_integer(conversion.from) {
            continue;
        }
        let mut divisions = Vec::new();
        truncating_divisions(ctx, converted, &mut divisions);
        for division in divisions {
            if !seen.insert(division.location) {
                continue;
            }
            let Some(span) = node_span(ctx, NodeRef::AExpr(division)) else {
                continue;
            };
            out.push(RawFinding {
                span,
                message: format!(
                    "integer division truncates before its result is used as {target}: the \
                     fraction is already lost"
                ),
                help: Some(format!(
                    "convert an operand before dividing: `x::{} / y`",
                    if conversion.to == NUMERIC {
                        "numeric"
                    } else {
                        target
                    }
                )),
                fix: Vec::new(),
            });
        }
    }
    out.sort_by_key(|finding| (finding.span.start, finding.span.end));
    out
}

/// Whether `call` is one of the built-in rounding functions, which have
/// nothing to round in an integer.
fn rounds(call: &typedpg_pg_query::ast::FuncCall) -> bool {
    let names: Vec<&str> = call
        .funcname
        .iter()
        .filter_map(|part| match part {
            Some(N::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    matches!(
        names.as_slice(),
        [name] | ["pg_catalog", name] if matches!(*name, "round" | "trunc" | "ceil" | "ceiling" | "floor")
    )
}

/// The integer divisions whose truncated result `expr`, an integer
/// expression, carries: `expr` itself when it is one, else the divisions
/// among the factors of a product or a negation (not of a sum: `a + b / 2`
/// may mean to halve `b`).
fn truncating_divisions<'a>(ctx: &Ctx<'_>, expr: &'a AExpr, out: &mut Vec<&'a AExpr>) {
    if expr.kind != AExprKind::AexprOp {
        return;
    }
    let Some(types) = ctx.view.operator_types(expr.location) else {
        return;
    };
    if !is_integer(types.result) {
        return;
    }
    match (operator_name(expr), types.left) {
        (Some("/"), Some(left)) if is_integer(left) && is_integer(types.right) => {
            if !exact(expr) {
                out.push(expr);
            }
        }
        (Some("*"), Some(_)) => {
            for operand in [expr.lexpr.as_ref(), expr.rexpr.as_ref()]
                .into_iter()
                .flatten()
            {
                if let N::AExpr(inner) = operand {
                    truncating_divisions(ctx, inner, out);
                }
            }
        }
        (Some("-"), None) => {
            if let Some(N::AExpr(inner)) = expr.rexpr.as_ref() {
                truncating_divisions(ctx, inner, out);
            }
        }
        _ => {}
    }
}

/// Whether the division loses nothing: by the literal `1` or `-1`, or of
/// two integer literals that divide exactly.
fn exact(expr: &AExpr) -> bool {
    let literal = |node: Option<&N>| match node {
        Some(N::AConst(value)) if !value.isnull => match value.val {
            Some(AConstValue::Integer(ref integer)) => Some(i64::from(integer.ival)),
            _ => None,
        },
        _ => None,
    };
    match (literal(expr.lexpr.as_ref()), literal(expr.rexpr.as_ref())) {
        (_, Some(1 | -1)) => true,
        (Some(left), Some(right)) => right != 0 && left % right == 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::IntegerDivision;

    fn db() -> typedpg_analyzer::PgCatalog {
        let mut db = typedpg_analyzer::PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE stats (
                id bigint PRIMARY KEY,
                done integer NOT NULL,
                total integer NOT NULL,
                big bigint NOT NULL,
                small smallint NOT NULL,
                ratio numeric,
                score double precision,
                whole integer
             );",
        )
        .unwrap();
        db
    }

    fn reported(sql: &str) -> Vec<String> {
        let db = db();
        lint_rule(&db, sql, RULE)
            .into_iter()
            .map(|finding| sql[finding.span].to_owned())
            .collect()
    }

    #[test]
    fn a_truncated_quotient_used_as_a_fraction_is_reported() {
        assert_eq!(
            reported("SELECT (done / total)::numeric AS r FROM stats"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT done / total * 1.0 AS r FROM stats"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT id FROM stats WHERE done / total > 0.5"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT round(done / total, 2) AS r FROM stats"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT ceil(done / total) AS r FROM stats"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT CAST(big / small AS double precision) AS r FROM stats"),
            ["big / small"]
        );
        assert_eq!(
            reported("UPDATE stats SET ratio = done / total WHERE id = $id"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT COALESCE(done / total, 0.5) AS r FROM stats"),
            ["done / total"]
        );
    }

    #[test]
    fn a_percentage_built_from_a_truncated_quotient_is_reported() {
        assert_eq!(
            reported("SELECT (done / total * 100)::numeric AS pct FROM stats"),
            ["done / total"]
        );
        assert_eq!(
            reported("SELECT (100 * done / total)::float8 AS pct FROM stats"),
            ["100 * done / total"]
        );
        assert_eq!(
            reported(
                "SELECT (count(*) FILTER (WHERE done > 0) / count(*))::numeric AS r FROM stats"
            ),
            ["count(*) FILTER (WHERE done > 0) / count(*)"]
        );
    }

    #[test]
    fn integer_results_and_exact_divisions_are_fine() {
        for sql in [
            "SELECT done / total AS r FROM stats",
            "SELECT avg(done / total) AS r FROM stats",
            "SELECT sum(done / total) AS r FROM stats",
            "SELECT done::numeric / total AS r FROM stats",
            "SELECT done * 1.0 / total AS r FROM stats",
            "SELECT 100.0 * done / total AS r FROM stats",
            "SELECT (10 / 2)::numeric AS r",
            "SELECT (done / 1)::numeric AS r FROM stats",
            "SELECT id FROM stats WHERE done / total > 1",
            "UPDATE stats SET whole = done / total WHERE id = $id",
            "SELECT ((done + total / 2) / 1)::numeric AS r FROM stats",
            "SELECT (done + total / 2)::numeric AS r FROM stats",
            "SELECT score / 2 AS r FROM stats",
            // Another function's argument may step on purpose.
            "SELECT power(2, done / 10) AS r FROM stats",
            "SELECT sqrt(done / 10) AS r FROM stats",
        ] {
            assert!(reported(sql).is_empty(), "{sql}: {:?}", reported(sql));
        }
    }

    #[test]
    fn a_domain_over_an_integer_divides_as_one() {
        let mut db = db();
        db.apply_sql(
            "CREATE DOMAIN amount AS integer;
             CREATE TABLE parts (id bigint PRIMARY KEY, part amount NOT NULL, whole amount NOT NULL);",
        )
        .unwrap();
        let sql = "SELECT (part / whole)::numeric AS r FROM parts";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "part / whole");
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let sql = "-- typedpg-lint: allow(integer_division)\nSELECT (done / total)::numeric AS r FROM stats";
        assert!(reported(sql).is_empty());
    }
}
