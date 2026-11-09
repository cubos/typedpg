//! The `like_unescaped_param` rule: reports a `LIKE` / `ILIKE` pattern built
//! from a parameter whose `%` and `_` are not escaped, so user input acts as
//! wildcards. A parameter wrapped in a function call (`replace`, an escaping
//! helper) is assumed escaped and not counted.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{AExpr, AExprKind, FuncCall, Node, a_const, node::Node as N};

use super::{expression_end, operand_start, operator_name};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::AExpr(expr) = node else {
            continue;
        };
        let Some((start, end)) = pattern(ctx, expr) else {
            continue;
        };
        out.push(RawFinding {
            span: start..end,
            message: "the pattern concatenates a parameter unescaped: a `%` or `_` in it matches \
                      any characters"
                .to_owned(),
            help: Some(
                "escape \\, % and _ in the parameter (replace(replace(replace($q, '\\', '\\\\'), \
                 '%', '\\%'), '_', '\\_')), or use starts_with / strpos"
                    .to_owned(),
            ),
            fix: Vec::new(),
        });
    }
    out
}

/// The pattern's byte range, when `expr` is a `LIKE` / `ILIKE` whose pattern
/// concatenates an unescaped parameter and a wildcard.
fn pattern(ctx: &Ctx<'_>, expr: &AExpr) -> Option<(usize, usize)> {
    if !is_like(expr) {
        return None;
    }
    let pattern = expr.rexpr.as_deref()?;
    let mut bare_param = false;
    let mut wildcard = false;
    classify(pattern, &mut bare_param, &mut wildcard);
    if !bare_param || !wildcard {
        return None;
    }
    let start = pattern_start(pattern)?;
    let end = pattern_end(ctx, pattern)?.max(start + 1);
    Some((start, end))
}

fn is_like(expr: &AExpr) -> bool {
    match AExprKind::try_from(expr.kind) {
        // `LIKE` / `ILIKE`, and `NOT LIKE` / `NOT ILIKE` (PG's `NOT` wraps
        // one of these, with the negated operator's name).
        Ok(AExprKind::AexprLike | AExprKind::AexprIlike) => true,
        Ok(AExprKind::AexprOp) => {
            matches!(operator_name(expr), Some("~~" | "~~*" | "!~~" | "!~~*"))
        }
        _ => false,
    }
}

/// What the pattern concatenates: a bare parameter (possibly cast), a string
/// constant with a wildcard, or a nested `||` / `concat(...)`. Anything else
/// (a function call's arguments above all) is assumed to escape the value.
fn classify(node: &Node, bare_param: &mut bool, wildcard: &mut bool) {
    match node.node.as_ref() {
        Some(N::ParamRef(_)) => *bare_param = true,
        Some(N::TypeCast(cast)) => {
            // A cast chain through a type that cannot hold `%` or `_`
            // (`$n::integer::text`, `$id::uuid::text`) validates or converts
            // the value: no wildcard survives it.
            if cast.type_name.as_ref().is_none_or(type_carries_wildcards)
                && let Some(arg) = &cast.arg
            {
                classify(arg, bare_param, wildcard);
            }
        }
        Some(N::AExpr(expr)) if is_concat(expr) => {
            if let Some(left) = &expr.lexpr {
                classify(left, bare_param, wildcard);
            }
            if let Some(right) = &expr.rexpr {
                classify(right, bare_param, wildcard);
            }
        }
        Some(N::FuncCall(call)) if is_concat_call(call) => {
            for arg in &call.args {
                classify(arg, bare_param, wildcard);
            }
        }
        Some(N::AConst(value)) if !value.isnull => {
            if let Some(a_const::Val::Sval(s)) = &value.val
                && (s.sval.contains('%') || s.sval.contains('_'))
            {
                *wildcard = true;
            }
        }
        _ => {}
    }
}

fn is_concat(expr: &AExpr) -> bool {
    AExprKind::try_from(expr.kind) == Ok(AExprKind::AexprOp) && operator_name(expr) == Some("||")
}

/// Whether a cast target type keeps a string's `%` and `_`: only the string
/// and identifier types PG has. A cast to anything else parses the value, so
/// the wildcard characters cannot reach the pattern.
fn type_carries_wildcards(type_name: &typedpg_pg_query::protobuf::TypeName) -> bool {
    let name = type_name.names.last().and_then(|node| match &node.node {
        Some(N::String(s)) => Some(s.sval.as_str()),
        _ => None,
    });
    matches!(
        name,
        Some("text" | "varchar" | "bpchar" | "name" | "citext" | "unknown")
    )
}

fn is_concat_call(call: &FuncCall) -> bool {
    matches!(
        call.funcname.last().and_then(|node| match &node.node {
            Some(N::String(s)) => Some(s.sval.as_str()),
            _ => None,
        }),
        Some("concat")
    )
}

/// The start of the pattern's text: the leftmost leaf of a `||` chain.
fn pattern_start(node: &Node) -> Option<usize> {
    match node.node.as_ref()? {
        N::TypeCast(cast) => cast
            .arg
            .as_deref()
            .and_then(pattern_start)
            .or_else(|| operand_start(node)),
        N::AExpr(expr) if is_concat(expr) => expr.lexpr.as_deref().and_then(pattern_start),
        _ => operand_start(node),
    }
}

/// The end of the pattern's text: the rightmost leaf of a `||` chain.
fn pattern_end(ctx: &Ctx<'_>, node: &Node) -> Option<usize> {
    match node.node.as_ref()? {
        N::AExpr(expr) if is_concat(expr) => {
            let right = expr.rexpr.as_deref()?;
            pattern_end(ctx, right)
                .or_else(|| operand_start(right).map(|start| expression_end(ctx, start)))
        }
        _ => {
            let start = operand_start(node)?;
            Some(expression_end(ctx, start))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Finding, LintOptions, Rule, Severity, lint_query};
    use typedpg_analyzer::PgCatalog;
    use typedpg_core::config::{LintConfig, LintLevel};

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql("CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL);")
            .unwrap();
        catalog
    }

    /// The default options, with `like_unescaped_param` enabled at info.
    fn options() -> LintOptions {
        let mut config = LintConfig::default();
        config
            .levels
            .insert("like_unescaped_param".to_owned(), LintLevel::Info);
        LintOptions::from_config(&config)
    }

    fn lint(catalog: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint_query(catalog, sql, &options()).unwrap()
    }

    fn only(findings: &[Finding]) -> &Finding {
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule, Rule::LikeUnescapedParam);
        assert_eq!(findings[0].severity, Severity::Info);
        &findings[0]
    }

    #[test]
    fn an_unescaped_parameter_in_a_like_pattern_is_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE name LIKE '%' || $q || '%'",
            "SELECT id FROM users WHERE name ILIKE $q || '%'",
            "SELECT id FROM users WHERE name LIKE concat('%', $q::text, '%')",
            "SELECT id FROM users WHERE name <> $q AND name LIKE concat('%', $q, '%')",
            "SELECT id FROM users WHERE name NOT LIKE '%' || $q || '%'",
            "SELECT id FROM users WHERE name ~~ ('%' || $q || '%')",
            "SELECT id FROM users WHERE name LIKE '%' || $q::text || '%'",
        ] {
            let findings = lint(&db, sql);
            let finding = only(&findings);
            assert_eq!(
                finding.message,
                "the pattern concatenates a parameter unescaped: a `%` or `_` in it matches any \
                 characters"
            );
            assert_eq!(
                finding.help.as_deref(),
                Some(
                    "escape \\, % and _ in the parameter (replace(replace(replace($q, '\\', \
                     '\\\\'), '%', '\\%'), '_', '\\_')), or use starts_with / strpos"
                )
            );
            assert!(finding.fix.is_empty());
        }
    }

    #[test]
    fn the_span_is_the_pattern_expression() {
        let db = catalog();
        let sql = "SELECT id FROM users WHERE name LIKE '%' || $q || '%'";
        let findings = lint(&db, sql);
        assert_eq!(&sql[only(&findings).span.clone()], "'%' || $q || '%'");

        let sql = "SELECT id FROM users WHERE name LIKE concat('%', $q::text, '%')";
        let findings = lint(&db, sql);
        assert_eq!(
            &sql[only(&findings).span.clone()],
            "concat('%', $q::text, '%')"
        );
    }

    #[test]
    fn patterns_without_a_bare_parameter_or_a_wildcard_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE name LIKE $q",
            "SELECT id FROM users WHERE name LIKE $q || 'abc'",
            "SELECT id FROM users WHERE name LIKE 'abc' || $q",
            "SELECT id FROM users WHERE name LIKE '%' || replace($q, '%', '\\%') || '%'",
            "SELECT id FROM users WHERE name LIKE '%' || lower($q) || '%'",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::LikeUnescapedParam),
                "{sql}: {findings:?}"
            );
        }
    }

    #[test]
    fn a_param_cast_through_a_type_without_wildcards_is_not_reported() {
        let db = catalog();
        // The intermediate cast parses the value, so `%` and `_` cannot reach
        // the pattern.
        for sql in [
            "SELECT id FROM users WHERE name LIKE '%' || $n::integer::text || '%'",
            "SELECT id FROM users WHERE name LIKE '%' || $id::uuid::text || '%'",
            "SELECT id FROM users WHERE name LIKE concat('%', $n::integer::text, '%')",
        ] {
            let findings = lint(&db, sql);
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.rule != Rule::LikeUnescapedParam),
                "{sql}: {findings:?}"
            );
        }
        // A text cast keeps the parameter's wildcards.
        let sql = "SELECT id FROM users WHERE name LIKE '%' || $q::text || '%'";
        assert_eq!(only(&lint(&db, sql)).rule, Rule::LikeUnescapedParam);
    }

    #[test]
    fn the_rule_is_off_by_default() {
        let db = catalog();
        let sql = "SELECT id FROM users WHERE name LIKE '%' || $q || '%'";
        let findings = lint_query(&db, sql, &LintOptions::default()).unwrap();
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_not_like_operator_is_reported_too() {
        let db = catalog();
        let sql = "SELECT id FROM users WHERE name !~~ ('%' || $q || '%')";
        let findings = lint(&db, sql);
        assert_eq!(only(&findings).rule, Rule::LikeUnescapedParam);
    }

    #[test]
    fn allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(like_unescaped_param)\n\
                   SELECT id FROM users WHERE name LIKE '%' || $q || '%'";
        assert!(lint(&db, sql).is_empty());
    }
}
