//! The `timestamp_timezone_mix` rule: reports an implicit conversion
//! between `timestamp` (without time zone) and `timestamptz`, or between
//! `date` and `timestamptz`. PostgreSQL makes it in the session's
//! `TimeZone`, so the same query gives different results — or matches
//! different rows — depending on a connection setting: a comparison
//! (`created_at > now()` over a `timestamp` column), arithmetic, a
//! function argument, a `CASE` / `COALESCE` / `UNION` type resolution, a
//! value assigned to a column of the other type.
//!
//! The analyzer records each value's conversion to the type its context
//! takes (see [`typedpg_analyzer::QueryView::value_conversion`]) and the
//! types each operator combines: the cross-type comparisons
//! (`timestamp < timestamptz`, `date = timestamptz`) convert inside the
//! operator, with no conversion of their operands.
//!
//! An explicit cast (`x::timestamptz`) or `AT TIME ZONE` says what is meant
//! and is not reported, nor is a parameter or an untyped literal: its type
//! comes from its context, so it converts nothing.

use std::collections::HashSet;

use typedpg_analyzer::PgTypeOid;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AExprKind, Node as N};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding};

const DATE: PgTypeOid = PgTypeOid::from_raw(1082);
const TIMESTAMP: PgTypeOid = PgTypeOid::from_raw(1114);
const TIMESTAMPTZ: PgTypeOid = PgTypeOid::from_raw(1184);

/// Whether converting between `a` and `b` reads the session's `TimeZone`.
fn mixed(a: PgTypeOid, b: PgTypeOid) -> bool {
    let local = |t: PgTypeOid| t == TIMESTAMP || t == DATE;
    (a == TIMESTAMPTZ && local(b)) || (b == TIMESTAMPTZ && local(a))
}

fn type_name(t: PgTypeOid) -> &'static str {
    match t {
        DATE => "date",
        TIMESTAMP => "timestamp without time zone",
        _ => "timestamp with time zone",
    }
}

/// How to write the conversion of a `from` value to `to` explicitly.
fn remedy(from: PgTypeOid, to: PgTypeOid) -> &'static str {
    match (from, to) {
        (DATE, _) => "convert it in an explicit time zone: `value::timestamp AT TIME ZONE 'UTC'`",
        (_, DATE) => "convert it in an explicit time zone: `(value AT TIME ZONE 'UTC')::date`",
        _ => "convert it in an explicit time zone: `value AT TIME ZONE 'UTC'`",
    }
}

fn is_param(node: &N) -> bool {
    matches!(node, N::ParamRef(_))
}

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    let mut reported: HashSet<std::ops::Range<usize>> = HashSet::new();
    // The values converted on their way to the type their context takes.
    for (node, _) in ctx.tree.nodes() {
        if matches!(node, NodeRef::ParamRef(_)) {
            continue;
        }
        let Some(conversion) = ctx.view.value_conversion(node) else {
            continue;
        };
        if conversion.explicit || !mixed(conversion.from, conversion.to) {
            continue;
        }
        let Some(span) = node_span(ctx, node) else {
            continue;
        };
        if !reported.insert(span.clone()) {
            continue;
        }
        out.push(RawFinding {
            span,
            message: format!(
                "{} value implicitly converted to {}: the result depends on the session's \
                 TimeZone setting",
                type_name(conversion.from),
                type_name(conversion.to)
            ),
            help: Some(remedy(conversion.from, conversion.to).to_owned()),
            fix: Vec::new(),
        });
    }
    // The cross-type operators, which convert inside: `timestamp <
    // timestamptz`, `date = timestamptz`.
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::AExpr(expr) = node else {
            continue;
        };
        let between = matches!(
            expr.kind,
            AExprKind::AexprBetween
                | AExprKind::AexprNotBetween
                | AExprKind::AexprBetweenSym
                | AExprKind::AexprNotBetweenSym
        );
        if expr.kind != AExprKind::AexprOp && !between {
            continue;
        }
        let (Some(left), Some(right)) = (expr.lexpr.as_ref(), expr.rexpr.as_ref()) else {
            continue;
        };
        // The right operands: a BETWEEN's two bounds.
        let rights: Vec<&N> = match right {
            N::List(list) if between => list.items.iter().flatten().collect(),
            _ if between => continue,
            other => vec![other],
        };
        if is_param(left) || rights.iter().any(|right| is_param(right)) {
            continue;
        }
        // (A BETWEEN's comparisons share its location: their types agree
        // or no fact is left.)
        let Some(types) = ctx.view.operator_types(expr.location) else {
            continue;
        };
        let Some(left_type) = types.left else {
            continue;
        };
        if !mixed(left_type, types.right) {
            continue;
        }
        // The side the operator converts: the one without a time zone.
        let (locals, local_type) = if left_type == TIMESTAMPTZ {
            (rights, types.right)
        } else {
            (vec![left], left_type)
        };
        let operator = if between {
            "BETWEEN"
        } else {
            operator_name(expr).unwrap_or("operator")
        };
        for local in locals {
            if ctx.view.value_conversion(local.to_ref()).is_some() {
                continue;
            }
            let Some(span) = node_span(ctx, local.to_ref()) else {
                continue;
            };
            if !reported.insert(span.clone()) {
                continue;
            }
            out.push(RawFinding {
                span,
                message: format!(
                    "`{operator}` compares {} with timestamp with time zone, converting the {} \
                 in the session's TimeZone setting",
                    type_name(local_type),
                    if local_type == DATE {
                        "date"
                    } else {
                        "timestamp"
                    }
                ),
                help: Some(remedy(local_type, TIMESTAMPTZ).to_owned()),
                fix: Vec::new(),
            });
        }
    }
    out.sort_by_key(|finding| (finding.span.start, finding.span.end));
    out
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::TimestampTimezoneMix;

    fn db() -> typedpg_analyzer::PgCatalog {
        let mut db = typedpg_analyzer::PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE events (
                id bigint PRIMARY KEY,
                at timestamp NOT NULL,
                at_tz timestamptz NOT NULL,
                day date NOT NULL
             );
             CREATE FUNCTION takes_tz(timestamptz) RETURNS int LANGUAGE sql AS 'SELECT 1';",
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
    fn comparisons_across_time_zones_are_reported() {
        assert_eq!(reported("SELECT id FROM events WHERE at > now()"), ["at"]);
        assert_eq!(reported("SELECT id FROM events WHERE at_tz = at"), ["at"]);
        assert_eq!(reported("SELECT id FROM events WHERE day < at_tz"), ["day"]);
        assert_eq!(
            reported("SELECT id FROM events WHERE at_tz >= current_date"),
            ["current_date"]
        );
        assert_eq!(
            reported("SELECT id FROM events WHERE at BETWEEN now() - interval '1 day' AND now()"),
            ["at"]
        );
        assert_eq!(
            reported("SELECT id FROM events WHERE at_tz BETWEEN at AND localtimestamp"),
            ["at", "localtimestamp"]
        );
    }

    #[test]
    fn arithmetic_and_function_arguments_are_reported() {
        assert_eq!(reported("SELECT at_tz - at AS d FROM events"), ["at"]);
        assert_eq!(reported("SELECT takes_tz(at) AS x FROM events"), ["at"]);
        assert_eq!(reported("SELECT age(at_tz, at) AS x FROM events"), ["at"]);
    }

    #[test]
    fn type_resolution_is_reported() {
        assert_eq!(
            reported("SELECT COALESCE(at, at_tz) AS x FROM events"),
            ["at"]
        );
        assert_eq!(
            reported("SELECT CASE WHEN id > 0 THEN at ELSE at_tz END AS x FROM events"),
            ["at"]
        );
        assert_eq!(
            reported("SELECT at AS x FROM events UNION ALL SELECT at_tz FROM events"),
            ["at"]
        );
    }

    #[test]
    fn assignments_are_reported() {
        assert_eq!(
            reported("UPDATE events SET at = now() WHERE id = $id"),
            ["now()"]
        );
        assert_eq!(
            reported("INSERT INTO events (id, at, at_tz, day) VALUES ($id, now(), $at_tz, $day)"),
            ["now()"]
        );
        assert_eq!(
            reported(
                "INSERT INTO events (id, at, at_tz, day) SELECT id + 1, at_tz, at, day FROM events WHERE id = $id"
            ),
            ["at_tz", "at"]
        );
    }

    #[test]
    fn explicit_conversions_parameters_and_literals_are_fine() {
        for sql in [
            "SELECT id FROM events WHERE at > $since",
            "SELECT id FROM events WHERE at BETWEEN $a AND $b",
            "SELECT id FROM events WHERE at_tz BETWEEN $a AND now()",
            "SELECT id FROM events WHERE at_tz > $since",
            "SELECT id FROM events WHERE at > '2024-01-01'",
            "SELECT id FROM events WHERE at::timestamptz > now()",
            "SELECT id FROM events WHERE at AT TIME ZONE 'UTC' > now()",
            "SELECT id FROM events WHERE at > now() AT TIME ZONE 'UTC'",
            "SELECT id FROM events WHERE at > localtimestamp",
            "SELECT id FROM events WHERE day < at",
            "SELECT id FROM events WHERE at_tz > now() - interval '1 day'",
            "UPDATE events SET at_tz = now() WHERE id = $id",
            "INSERT INTO events (id, at, at_tz, day) VALUES ($id, $at, $at_tz, $day)",
            "SELECT date_trunc('day', at) AS d FROM events",
        ] {
            assert!(reported(sql).is_empty(), "{sql}: {:?}", reported(sql));
        }
    }

    #[test]
    fn stored_expressions_the_query_reads_are_not_its_own() {
        let mut db = db();
        db.apply_sql(
            "CREATE TABLE logs (id bigint PRIMARY KEY, at timestamp NOT NULL DEFAULT now());
             CREATE FUNCTION recent(t timestamp) RETURNS boolean
                 LANGUAGE sql AS 'SELECT t > now() - interval ''1 day''';",
        )
        .unwrap();
        for sql in [
            // The column default converts, not the query.
            "INSERT INTO logs (id) VALUES ($id)",
            // The inlined body converts, not the query.
            "SELECT id FROM events WHERE recent(at)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_domain_is_its_base_type() {
        let mut db = db();
        db.apply_sql(
            "CREATE DOMAIN moment AS timestamp;
             CREATE TABLE moments (id bigint PRIMARY KEY, at moment NOT NULL);",
        )
        .unwrap();
        let sql = "SELECT id FROM moments WHERE at < now()";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "at");
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let sql = "-- typedpg-lint: allow(timestamp_timezone_mix)\nSELECT id FROM events WHERE at > now()";
        assert!(reported(sql).is_empty());
    }
}
