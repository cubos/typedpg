//! The `boolean_comparison` rule: reports `x = true`, `x = false` and their
//! `<>` / `!=` forms over a `boolean` expression, with the literal on either
//! side. They spell out what the boolean already says:
//!
//! | written | same as |
//! |---|---|
//! | `x = true`, `x <> false` | `x` |
//! | `x = false`, `x <> true` | `NOT x` |
//!
//! Both forms treat NULL alike (`NULL = true` is NULL, as `NULL` is; `NOT
//! NULL` is NULL), so the rewrite changes neither the rows nor the result
//! type. `IS TRUE`, `IS NOT TRUE` and their `FALSE` forms are different
//! tests — they turn NULL into a value — and are not reported.
//!
//! The operands must be `boolean` itself, not a domain over it: the rewrite
//! would change the result's type. A comparison of two literals is left to
//! `constant_condition`.
//!
//! The fix writes the expression, wrapped in parentheses when it is an `AND`
//! / `OR` / `NOT` expression, or `NOT` it. In a select list whose column has
//! no alias the comparison's name (`?column?`) would become the expression's
//! own name, and in a `CASE`'s `WHEN` condition the analyzer proves a value
//! set of the result it wouldn't for a bare boolean, so both are reported
//! without a fix: a fix never changes an output column.

use typedpg_analyzer::PgTypeOid;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, AExpr, AExprKind, Node, Node as N};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding, SqlEdit};

const BOOL: PgTypeOid = PgTypeOid::from_raw(16);

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::AExpr(expr) = node else {
            continue;
        };
        if let Some(finding) = comparison(ctx, expr) {
            out.push(finding);
        }
    }
    out
}

fn comparison(ctx: &Ctx<'_>, expr: &AExpr) -> Option<RawFinding> {
    if expr.kind != AExprKind::AexprOp {
        return None;
    }
    let operator = operator_name(expr)?;
    let positive = match operator {
        "=" => true,
        "<>" | "!=" => false,
        _ => return None,
    };
    // `OPERATOR(myschema.=)` may be anything.
    if expr.name.len() > 1
        && !matches!(expr.name.first(), Some(Some(Node::String(s))) if s.sval == "pg_catalog")
    {
        return None;
    }
    let (left, right) = (expr.lexpr.as_ref()?, expr.rexpr.as_ref()?);
    // The boolean, and the literal compared with it (either side).
    let (value, literal) = match (bool_literal(left), bool_literal(right)) {
        (Some(_), Some(_)) | (None, None) => return None,
        (Some(literal), None) => (right, literal),
        (None, Some(literal)) => (left, literal),
    };
    if is_literal(value) {
        // `NULL = true`, `'yes' = true`: not a boolean value to stand alone.
        return None;
    }
    let types = ctx.view.operator_types(expr.location)?;
    // Exactly `boolean = boolean`: a domain over it would change the
    // rewrite's result type.
    if types.result != BOOL || types.left != Some(BOOL) || types.right != BOOL {
        return None;
    }
    let same = positive == literal;
    let text = operand_text(ctx, value)?;
    let replacement = if same {
        text.clone()
    } else {
        format!("NOT {text}")
    };
    let no_fix = unaliased_target(ctx, expr.location) || in_case_when(ctx, expr.location);
    let span = node_span(ctx, NodeRef::AExpr(expr))?;
    Some(RawFinding {
        span: span.clone(),
        message: if same {
            format!("comparison with the literal {literal}: it is the value itself")
        } else {
            format!("comparison with the literal {literal}: it is the value's negation")
        },
        help: Some(if same {
            "write the boolean expression itself".to_owned()
        } else {
            "write NOT the boolean expression".to_owned()
        }),
        fix: (!no_fix)
            .then_some(SqlEdit {
                range: span,
                text: replacement,
            })
            .into_iter()
            .collect(),
    })
}

/// Whether `node` is the literal `true` or `false`.
fn bool_literal(node: &Node) -> Option<bool> {
    let N::AConst(constant) = node else {
        return None;
    };
    if constant.isnull {
        return None;
    }
    match &constant.val {
        Some(AConstValue::Boolean(value)) => Some(value.boolval),
        _ => None,
    }
}

/// Whether `node` is a literal (possibly under casts): `NULL`, a number, a
/// string.
fn is_literal(node: &Node) -> bool {
    match node {
        N::AConst(_) => true,
        N::TypeCast(cast) => cast.arg.as_ref().is_some_and(is_literal),
        _ => false,
    }
}

/// The expression as written, wrapped in parentheses when it is an `AND` /
/// `OR` / `NOT`, whose precedence would otherwise swallow the code around
/// it.
fn operand_text(ctx: &Ctx<'_>, node: &Node) -> Option<String> {
    let mut span = node_span(ctx, node.to_ref())?;
    // `IS NULL` and `IS [NOT] TRUE` end in keywords no node locates.
    if matches!(node, N::NullTest(_) | N::BooleanTest(_)) {
        let start = ctx
            .tokens
            .iter()
            .position(|token| token.start >= span.end)?;
        for token in &ctx.tokens[start..] {
            let text = &ctx.sql[token.start..token.end];
            if !matches!(
                text.to_ascii_uppercase().as_str(),
                "IS" | "NOT" | "NULL" | "TRUE" | "FALSE" | "UNKNOWN"
            ) {
                break;
            }
            span.end = token.end;
        }
    }
    let text = ctx.written(span).to_owned();
    Some(match node {
        N::BoolExpr(expr) if expr.boolop != typedpg_pg_query::ast::BoolExprType::NotExpr => {
            format!("({text})")
        }
        N::NullTest(_) | N::BooleanTest(_) => format!("({text})"),
        _ => text,
    })
}

/// Whether the `A_Expr` at `location` is a select or `RETURNING` list's own
/// column with no alias: rewriting it would change the column's name.
fn unaliased_target(ctx: &Ctx<'_>, location: i32) -> bool {
    ctx.tree.nodes().any(|(node, _)| match node {
        NodeRef::ResTarget(target) => {
            target.name.is_empty()
                && matches!(target.val.as_ref(), Some(N::AExpr(expr)) if expr.location == location)
        }
        _ => false,
    })
}

/// Whether the `A_Expr` at `location` is (part of) a `CASE`'s `WHEN`
/// condition: the analyzer proves a value set of the result through
/// `x = true` that it doesn't through a bare `x`, and a fix may not change an
/// output column's refinement.
fn in_case_when(ctx: &Ctx<'_>, location: i32) -> bool {
    ctx.tree.nodes().any(|(node, _)| match node {
        NodeRef::CaseWhen(when) => when.expr.as_ref().is_some_and(|expr| {
            expr.nodes()
                .any(|(node, _)| matches!(node, NodeRef::AExpr(expr) if expr.location == location))
        }),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::{fixed, lint_rule};

    const RULE: Rule = Rule::BooleanComparison;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE flags (
                    id integer PRIMARY KEY,
                    active boolean,
                    verified boolean NOT NULL
                 );",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn comparisons_with_a_boolean_literal_are_rewritten() {
        let db = catalog();
        for (sql, fixed_sql) in [
            (
                "SELECT id FROM flags WHERE active = true",
                "SELECT id FROM flags WHERE active",
            ),
            (
                "SELECT id FROM flags WHERE active = false",
                "SELECT id FROM flags WHERE NOT active",
            ),
            (
                "SELECT id FROM flags WHERE active <> true",
                "SELECT id FROM flags WHERE NOT active",
            ),
            (
                "SELECT id FROM flags WHERE active != false",
                "SELECT id FROM flags WHERE active",
            ),
            (
                "SELECT id FROM flags WHERE true = active",
                "SELECT id FROM flags WHERE active",
            ),
            (
                "SELECT id FROM flags WHERE false <> verified",
                "SELECT id FROM flags WHERE verified",
            ),
            (
                "SELECT id FROM flags WHERE active = true AND id = 1",
                "SELECT id FROM flags WHERE active AND id = 1",
            ),
            (
                "SELECT id FROM flags WHERE (active IS NULL) = true",
                "SELECT id FROM flags WHERE (active IS NULL)",
            ),
            (
                "SELECT id FROM flags WHERE (verified IS NOT TRUE) = false",
                "SELECT id FROM flags WHERE NOT (verified IS NOT TRUE)",
            ),
            (
                // The parentheses the comparison sits in are the author's,
                // and stay.
                "SELECT id FROM flags WHERE NOT (active = true)",
                "SELECT id FROM flags WHERE NOT (active)",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].help.is_some(), "{sql}");
            assert_eq!(fixed(sql, &findings), fixed_sql, "{sql}");
        }
    }

    #[test]
    fn a_compound_operand_is_parenthesized() {
        let db = catalog();
        let sql = "SELECT id FROM flags WHERE (active OR verified) = true";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            &sql[findings[0].span.clone()],
            "(active OR verified) = true"
        );
        assert_eq!(
            fixed(sql, &findings),
            "SELECT id FROM flags WHERE (active OR verified)"
        );
    }

    #[test]
    fn a_case_when_condition_is_reported_without_a_fix() {
        let db = catalog();
        let sql = "SELECT CASE WHEN (active = true) THEN true ELSE false END AS b \
                   FROM flags WHERE NOT active";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_select_lists_own_column_is_reported_without_a_fix() {
        let db = catalog();
        let sql = "SELECT active = true FROM flags";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty(), "{findings:?}");
        // An aliased column keeps its name.
        let sql = "SELECT active = true AS is_active FROM flags";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(
            fixed(sql, &findings),
            "SELECT active AS is_active FROM flags"
        );
    }

    #[test]
    fn other_boolean_tests_and_types_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM flags WHERE active IS TRUE",
            "SELECT id FROM flags WHERE active IS NOT FALSE",
            "SELECT id FROM flags WHERE active = NULL",
            "SELECT id FROM flags WHERE active",
            "SELECT id FROM flags WHERE NOT active",
            "SELECT id FROM flags WHERE active::integer = 1",
            "SELECT id FROM flags WHERE id = 1",
            "SELECT id FROM flags WHERE true = false",
            "SELECT id FROM flags WHERE active = 'true'",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
