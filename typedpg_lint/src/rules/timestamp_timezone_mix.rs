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
//!
//! A `date` formatted by `to_char(<date>, <format>)` is not reported either
//! when the format reads only the date's fields: there is no
//! `to_char(date, text)`, so the date becomes midnight in the session's
//! `TimeZone` — 01:00 of the same day when the midnight doesn't exist — and
//! `to_char(timestamptz, text)` prints it back in the same `TimeZone`, so
//! every date field is the date's own whatever the zone (see
//! [`date_fields_only`]). A `timestamp` is left reported: a wall-clock
//! reading that a zone change maps to another instant need not come back the
//! same.

use std::collections::HashSet;

use typedpg_analyzer::PgTypeOid;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, AExprKind, FuncCall, Node as N};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding};

const TEXT: PgTypeOid = PgTypeOid::from_raw(25);
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

/// The first arguments whose conversion to `timestamptz` only lets
/// `to_char` format a `date` as the date it is: `to_char(<date>, <constant
/// format reading only date fields>)` resolving to
/// `pg_catalog.to_char(timestamptz, text)`.
fn date_only_to_char_arguments(ctx: &Ctx<'_>) -> HashSet<std::ops::Range<usize>> {
    let mut sites = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::FuncCall(call) = node else {
            continue;
        };
        // `to_char(timestamptz, text)` takes the two arguments.
        if call.args.len() != 2 {
            continue;
        }
        let (Some(Some(argument)), Some(Some(format))) = (call.args.first(), call.args.get(1))
        else {
            continue;
        };
        // A named-argument call needn't write the value first.
        if matches!(argument, N::NamedArgExpr(_)) || matches!(format, N::NamedArgExpr(_)) {
            continue;
        }
        let Some(format) = string_constant(format) else {
            continue;
        };
        if !date_fields_only(format) {
            continue;
        }
        let Some(conversion) = ctx.view.value_conversion(argument.to_ref()) else {
            continue;
        };
        // A `timestamp` is a wall-clock reading a zone change may move to
        // another date; only a `date` is printed back as itself.
        if conversion.explicit || conversion.from != DATE || conversion.to != TIMESTAMPTZ {
            continue;
        }
        if !is_builtin_to_char(ctx, call) {
            continue;
        }
        if let Some(span) = node_span(ctx, argument.to_ref()) {
            sites.insert(span);
        }
    }
    sites
}

/// The value of a string literal argument.
fn string_constant(node: &N) -> Option<&str> {
    match node {
        N::AConst(value) => match &value.val {
            Some(AConstValue::String(s)) => Some(s.sval.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `call` is PostgreSQL's own `to_char(timestamptz, text)`: the
/// first routine named `to_char` a call of a `timestamptz` resolves to
/// along the search path is that one (a user routine of the name earlier on
/// the path may read the value otherwise, whatever its kind).
fn is_builtin_to_char(ctx: &Ctx<'_>, call: &FuncCall) -> bool {
    let names: Vec<&str> = call
        .funcname
        .iter()
        .filter_map(|name| match name {
            Some(N::String(name)) => Some(name.sval.as_str()),
            _ => None,
        })
        .collect();
    let schema = match names.as_slice() {
        ["to_char"] => None,
        ["pg_catalog", "to_char"] => Some("pg_catalog"),
        _ => return false,
    };
    let found = ctx
        .catalog
        .find_functions(schema, "to_char")
        .into_iter()
        .find(|proc| proc.proargtypes.first() == Some(&TIMESTAMPTZ));
    found.is_some_and(|proc| {
        proc.proargtypes.as_slice() == [TIMESTAMPTZ, TEXT]
            && ctx.catalog.namespace_name(proc.pronamespace) == Some("pg_catalog")
    })
}

/// One template pattern of `to_char`'s format (`formatting.c`): whether it
/// reads the time of day or the time zone, and its length.
const PATTERNS: &[(bool, &str)] = &[
    // The date fields: the year, the month, the day of the week or year,
    // the week, the quarter, the century, the Julian day, the eras, and
    // the ISO fields; the ordinal suffixes and `FM`/`FX` are not patterns
    // of their own (see [`date_fields_only`]).
    (false, "yyyy"),
    (false, "y,yyy"),
    (false, "iyyy"),
    (false, "month"),
    (false, "iddd"),
    (false, "yyy"),
    (false, "iyy"),
    (false, "mon"),
    (false, "ddd"),
    (false, "day"),
    (false, "dy"),
    (false, "dd"),
    (false, "ww"),
    (false, "iw"),
    (false, "id"),
    (false, "yy"),
    (false, "iy"),
    (false, "rm"),
    (false, "cc"),
    (false, "bc"),
    (false, "b.c."),
    (false, "ad"),
    (false, "a.d."),
    (false, "mm"),
    (false, "q"),
    (false, "w"),
    (false, "d"),
    (false, "j"),
    (false, "y"),
    (false, "i"),
    // The time of day and the time zone.
    (true, "hh24"),
    (true, "hh12"),
    (true, "ssss"),
    (true, "hh"),
    (true, "mi"),
    (true, "ss"),
    (true, "ms"),
    (true, "us"),
    (true, "ff1"),
    (true, "ff2"),
    (true, "ff3"),
    (true, "ff4"),
    (true, "ff5"),
    (true, "ff6"),
    (true, "am"),
    (true, "a.m."),
    (true, "pm"),
    (true, "p.m."),
    (true, "tzm"),
    (true, "tzh"),
    (true, "tz"),
    (true, "of"),
];

/// The pattern at the start of `text`, the longest one, as
/// `formatting.c`'s `parse_format` scans its keyword table.
fn pattern_at(text: &[u8]) -> Option<(bool, usize)> {
    PATTERNS
        .iter()
        .filter(|(_, pattern)| starts_with_ignore_ascii_case(text, pattern))
        .max_by_key(|(_, pattern)| pattern.len())
        .map(|&(time, pattern)| (time, pattern.len()))
}

fn starts_with_ignore_ascii_case(text: &[u8], pattern: &str) -> bool {
    text.len() >= pattern.len() && text[..pattern.len()].eq_ignore_ascii_case(pattern.as_bytes())
}

/// Whether `format` names only date fields: every template pattern in it is
/// one of the year, month, day, week, quarter, century, Julian day or ISO
/// fields — with the `FM`/`FX` prefixes and the `TH` suffix — and the rest
/// is literal text or a quoted string. Any time-of-day or zone pattern
/// (`HH`, `HH12`, `HH24`, `MI`, `SS`, `MS`, `US`, `FF1`–`FF6`, `SSSS`,
/// `AM`, `PM`, `A.M.`, `P.M.`, `TZ`, `tz`, `TZH`, `TZM`, `OF`), in the case
/// PostgreSQL reads it in, is not a date field, and neither are letters
/// that are no pattern this knows: a format it can't prove is reported.
fn date_fields_only(format: &str) -> bool {
    let bytes = format.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            // A quoted string is literal text: its content is never a
            // pattern.
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += 1;
                }
                at += 1;
            }
            // The escaped character is literal.
            b'\\' => at += 2,
            // The fill-mode and fixed-format prefixes.
            b'F' | b'f' if matches!(bytes.get(at + 1).copied(), Some(b'M' | b'X')) => at += 2,
            _ => {
                let Some((time, len)) = pattern_at(&bytes[at..]) else {
                    if bytes[at].is_ascii_alphabetic() {
                        return false;
                    }
                    at += 1;
                    continue;
                };
                if time {
                    return false;
                }
                at += len;
                // The ordinal suffix a numeric pattern may take.
                if starts_with_ignore_ascii_case(&bytes[at..], "th") {
                    at += 2;
                }
            }
        }
    }
    true
}

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    let mut reported: HashSet<std::ops::Range<usize>> = HashSet::new();
    let date_only_to_char = date_only_to_char_arguments(ctx);
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
        if date_only_to_char.contains(&span) {
            continue;
        }
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
    fn a_date_formatted_by_to_char_as_a_date_is_not_reported() {
        for sql in [
            "SELECT to_char(current_date, 'YYYY-MM-DD')",
            "SELECT to_char($d::date, 'FMDD/MM/YYYY')",
            "SELECT to_char(day, 'IYYY-IW') FROM events",
            // Quoted text, the fill mode and the ordinal suffix.
            "SELECT to_char(day, 'FMDDth \"of\" Month, YYYY') FROM events",
            "SELECT to_char(day, 'Q CC J DDD D DD WW W ID IDDD DY Day MON Mon RM') FROM events",
            "SELECT to_char(day, 'YYYY\"-\"MM\"-\"DD') FROM events",
        ] {
            assert!(reported(sql).is_empty(), "{sql}: {:?}", reported(sql));
        }
    }

    #[test]
    fn a_format_reading_the_time_or_the_zone_is_reported() {
        for sql in [
            "SELECT to_char(current_date, 'YYYY-MM-DD HH24:MI')",
            "SELECT to_char(current_date, 'YYYY TZ')",
            "SELECT to_char(current_date, 'yyyy-mm-dd hh24:mi')",
            "SELECT to_char(current_date, 'HH')",
            "SELECT to_char(current_date, 'SS')",
            "SELECT to_char(current_date, 'MS')",
            "SELECT to_char(current_date, 'US')",
            "SELECT to_char(current_date, 'FF3')",
            "SELECT to_char(current_date, 'SSSS')",
            "SELECT to_char(current_date, 'AM')",
            "SELECT to_char(current_date, 'P.M.')",
            "SELECT to_char(current_date, 'TZH')",
            "SELECT to_char(current_date, 'OF')",
        ] {
            assert_eq!(reported(sql), ["current_date"], "{sql}");
        }
        // A `timestamp` argument takes `to_char(timestamp, text)`, which
        // converts nothing: nothing to report either way.
        assert!(reported("SELECT to_char(at, 'YYYY-MM-DD') FROM events").is_empty());
        // And a `timestamp` converted to `timestamptz` elsewhere stays
        // reported, the date fields being no proof for a wall-clock value.
        assert_eq!(reported("SELECT takes_tz(at) FROM events"), ["at"]);
    }

    #[test]
    fn a_non_constant_format_is_reported() {
        assert_eq!(
            reported("SELECT to_char(current_date, $fmt)"),
            ["current_date"]
        );
        assert_eq!(
            reported("SELECT to_char(current_date, 'YYYY' || '-MM')"),
            ["current_date"]
        );
    }

    #[test]
    fn a_user_to_char_of_the_name_is_reported() {
        let mut db = db();
        db.apply_sql(
            "CREATE ROLE app;
             CREATE SCHEMA app;
             CREATE FUNCTION app.to_char(timestamptz, text) RETURNS text
                 LANGUAGE sql AS 'SELECT $2';
             GRANT USAGE ON SCHEMA app TO app;
             ALTER ROLE app SET search_path = app, pg_catalog;",
        )
        .unwrap();
        let scoped = db.with_role_search_path("app");
        let sql = "SELECT to_char(current_date, 'YYYY-MM-DD')";
        let findings = crate::lint_query(&scoped, sql, &crate::LintOptions::default())
            .unwrap()
            .into_iter()
            .filter(|finding| finding.rule == RULE)
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), 1, "{findings:?}");
        // Qualified, it is `pg_catalog`'s again.
        let sql = "SELECT pg_catalog.to_char(current_date, 'YYYY-MM-DD')";
        assert!(lint_rule(&scoped, sql, RULE).is_empty());
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
