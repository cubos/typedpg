//! The lint rules. Both walk every node of the parse tree, so a site inside
//! a subquery, a CTE (data-modifying ones included), a CASE or a JOIN's ON
//! is visited like any other.

use typedpg_analyzer::{CastMethod, PgTypeOid, TypType};
use typedpg_core::{QualifiedName, quote_identifier};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    AExpr, AExprKind, BoolExpr, BoolExprType, CaseWhen, Node, RangeVar, SubLinkType,
    node::Node as N,
};

use crate::{Ctx, RawFinding, Rule};

mod like_unescaped_param;
mod limit_without_order;
mod redundant_distinct;
mod redundant_limit;
mod redundant_null_check;

/// The operators `null_comparison` reports: the ordinary comparisons, not
/// `IS [NOT] DISTINCT FROM` or the pattern operators.
const COMPARISONS: &[&str] = &["=", "<>", "!=", "<", ">", "<=", ">="];

pub(crate) fn findings(rule: Rule, ctx: &Ctx<'_>) -> Vec<RawFinding> {
    match rule {
        Rule::NullComparison => null_comparison(ctx),
        Rule::UnboundedWrite => unbounded_write(ctx),
        Rule::NotInNullable => not_in_nullable(ctx),
        Rule::RedundantNullCheck => redundant_null_check::findings(ctx),
        Rule::LimitWithoutOrder => limit_without_order::findings(ctx),
        Rule::RedundantLimit => redundant_limit::findings(ctx),
        Rule::RedundantDistinct => redundant_distinct::findings(ctx),
        Rule::LikeUnescapedParam => like_unescaped_param::findings(ctx),
        // The migration and schema rules run over a migration's statements,
        // not over a query.
        Rule::CreateIndexBlocking
        | Rule::ConstraintValidating
        | Rule::SetNotNullScan
        | Rule::TableRewrite
        | Rule::FkWithoutIndex
        | Rule::RedundantIndex
        | Rule::FkTypeMismatch
        | Rule::MissingLockTimeout
        | Rule::PreferTimestamptz
        | Rule::PreferIdentity
        | Rule::PreferText
        | Rule::PreferBigintPk
        // The binding and call-site rules run over the Rust source of the
        // invocation, in `typedpg_cli`, which alone sees it.
        | Rule::RedundantBinding
        | Rule::RenamingBinding
        | Rule::FetchMayReturnMany
        | Rule::FetchAllSingleRow
        | Rule::UnusedReturning
        | Rule::InvalidAllow
        | Rule::InternalError => Vec::new(),
    }
}

// ── null_comparison ────────────────────────────────────────────────────────

fn null_comparison(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::AExpr(expr) => comparison(ctx, expr, &mut out),
            NodeRef::CaseWhen(when) => case_when(ctx, when, &mut out),
            _ => {}
        }
    }
    out
}

fn comparison(ctx: &Ctx<'_>, expr: &AExpr, out: &mut Vec<RawFinding>) {
    let kind = AExprKind::try_from(expr.kind).ok();
    match kind {
        Some(AExprKind::AexprOp) => {
            let Some(operator) = operator_name(expr) else {
                return;
            };
            if !COMPARISONS.contains(&operator) || ctx.non_strict_operators.contains(operator) {
                return;
            }
            let left_null = expr
                .lexpr
                .as_deref()
                .is_some_and(|node| is_null_literal(ctx, node));
            let right_null = expr
                .rexpr
                .as_deref()
                .is_some_and(|node| is_null_literal(ctx, node));
            if !left_null && !right_null {
                return;
            }
            let start = expr
                .lexpr
                .as_deref()
                .and_then(operand_start)
                .or_else(|| location(expr.location))
                .unwrap_or(0);
            let end = expr
                .rexpr
                .as_deref()
                .map(|right| {
                    let right_start = operand_start(right).unwrap_or(start);
                    expression_end(ctx, right_start)
                })
                .filter(|end| *end > start)
                .unwrap_or(start + 4);
            out.push(RawFinding {
                span: start..end,
                message: "comparison with NULL is never true".to_owned(),
                help: Some("use IS NULL / IS NOT NULL".to_owned()),
                fix: Vec::new(),
            });
        }
        Some(AExprKind::AexprIn) if operator_name(expr) == Some("<>") => {
            // `NOT IN` compares with `=` internally; a user-defined
            // non-strict `=` may accept NULL.
            if ctx.non_strict_operators.contains("=") {
                return;
            }
            let Some(right) = expr.rexpr.as_deref() else {
                return;
            };
            let Some(N::List(list)) = &right.node else {
                return;
            };
            if !list.items.iter().any(|node| is_null_literal(ctx, node)) {
                return;
            }
            let start = expr
                .lexpr
                .as_deref()
                .and_then(operand_start)
                .or_else(|| location(expr.location))
                .unwrap_or(0);
            let end = usize::try_from(expr.rexpr_list_end)
                .ok()
                .map(|end| end + 1)
                .filter(|end| *end > start)
                .unwrap_or_else(|| expression_end(ctx, start));
            out.push(RawFinding {
                span: start..end,
                message: "NOT IN with a NULL in the list is never true".to_owned(),
                help: Some("remove the NULL from the list".to_owned()),
                fix: Vec::new(),
            });
        }
        _ => {}
    }
}

fn case_when(ctx: &Ctx<'_>, when: &CaseWhen, out: &mut Vec<RawFinding>) {
    let Some(expr) = when.expr.as_deref() else {
        return;
    };
    if !is_null_literal(ctx, expr) || ctx.non_strict_operators.contains("=") {
        return;
    }
    let expr_start = operand_start(expr).unwrap_or(0);
    let end = if expr_start > 0 || location(when.location).is_some() {
        expression_end(ctx, expr_start)
    } else {
        expr_start + 4
    };
    let start = location(when.location)
        .filter(|start| *start <= expr_start)
        .unwrap_or(expr_start);
    out.push(RawFinding {
        span: start..end.max(expr_start + 4),
        message: "CASE ... WHEN NULL never matches".to_owned(),
        help: Some("use CASE WHEN x IS NULL".to_owned()),
        fix: Vec::new(),
    });
}

/// The operator's name without its schema: `OPERATOR(pg_catalog.=)` is
/// `=`.
pub(super) fn operator_name(expr: &AExpr) -> Option<&str> {
    expr.name.iter().rev().find_map(|node| match &node.node {
        Some(N::String(s)) => Some(s.sval.as_str()),
        _ => None,
    })
}

/// Whether `node` is a NULL literal under casts that keep it NULL.
///
/// A cast can turn NULL into a value: a cast function `CALLED ON NULL INPUT`
/// (`proisstrict = false`) may return anything. Only a binary / I/O cast, a
/// strict cast function, or a cast the parser resolves without a function
/// (the `unknown` NULL literal itself, or a built-in I/O conversion) is
/// known to leave NULL NULL; anything whose path cannot be resolved is not
/// treated as a NULL literal.
pub(super) fn is_null_literal(ctx: &Ctx<'_>, node: &Node) -> bool {
    match &node.node {
        Some(N::AConst(value)) => value.isnull,
        Some(N::TypeCast(cast)) => {
            let Some(arg) = cast.arg.as_deref() else {
                return false;
            };
            if !is_null_literal(ctx, arg) {
                return false;
            }
            let Some(target) = cast
                .type_name
                .as_ref()
                .and_then(|name| ctx.catalog.lint_resolve_type_name(name))
            else {
                return false;
            };
            let source = match arg.node.as_ref() {
                // A bare NULL literal is `unknown`: the parser makes a NULL
                // constant of the target type directly, with no function.
                Some(N::AConst(_)) => None,
                Some(N::TypeCast(inner)) => inner
                    .type_name
                    .as_ref()
                    .and_then(|name| ctx.catalog.lint_resolve_type_name(name)),
                _ => return false,
            };
            cast_preserves_null(ctx, source, target)
        }
        _ => false,
    }
}

/// Whether casting a NULL of type `source` (unknown for a bare NULL literal)
/// to `target` yields NULL.
fn cast_preserves_null(ctx: &Ctx<'_>, source: Option<PgTypeOid>, target: PgTypeOid) -> bool {
    let catalog = ctx.catalog;
    let Some(target_type) = catalog.type_row(target) else {
        return false;
    };
    // A domain's NOT NULL / CHECK constraints run on the cast's result and
    // can reject a NULL; the catalog does not expose every check here, so
    // never treat a domain cast as a plain NULL literal.
    if target_type.typtype == TypType::Domain {
        return false;
    }
    let Some(source) = source else {
        // `NULL` written as a literal is `unknown`: the parser makes it a
        // NULL constant of the target type, without calling a function.
        return true;
    };
    if source == target {
        return true;
    }
    if let Some(cast) = catalog
        .iter_casts()
        .find(|cast| cast.castsource == source && cast.casttarget == target)
    {
        return match cast.castmethod {
            CastMethod::Binary | CastMethod::InOut => true,
            CastMethod::Function => cast.castfunc.is_some_and(|function| {
                catalog
                    .pg_proc()
                    .get(&function)
                    .is_some_and(|proc| proc.proisstrict)
            }),
        };
    }
    // No pg_cast row: PostgreSQL may still resolve the cast through the
    // types' I/O functions, which keep NULL. Trust that only between
    // built-in types, where no user-defined input/output function is
    // involved.
    is_builtin_type(ctx, source) && is_builtin_type(ctx, target)
}

fn is_builtin_type(ctx: &Ctx<'_>, oid: PgTypeOid) -> bool {
    ctx.catalog
        .type_row(oid)
        .and_then(|ty| ctx.catalog.namespace_name(ty.typnamespace))
        == Some("pg_catalog")
}

/// The start of the token sequence of `node`, descending through casts so
/// `NULL::int` starts at `NULL` (a `TypeCast`'s own location is its `::`).
pub(super) fn operand_start(node: &Node) -> Option<usize> {
    match &node.node {
        Some(N::AConst(value)) if value.isnull => location(value.location),
        Some(N::TypeCast(cast)) => cast
            .arg
            .as_deref()
            .and_then(operand_start)
            .or_else(|| node_start(node)),
        _ => node_start(node),
    }
}

pub(super) fn node_start(node: &Node) -> Option<usize> {
    let node = node.node.as_ref()?;
    typedpg_pg_query::NodeRef::location(node.to_ref())
        .and_then(|location| usize::try_from(location).ok())
}

pub(super) fn location(location: i32) -> Option<usize> {
    usize::try_from(location).ok()
}

/// The end of the expression starting at `start`, scanning tokens until a
/// token that can't be part of it at paren depth zero.
pub(super) fn expression_end(ctx: &Ctx<'_>, start: usize) -> usize {
    let mut depth = 0usize;
    let mut seen = false;
    let mut end = start;
    for token in ctx.tokens.iter().filter(|token| token.start >= start) {
        let text = &ctx.sql[token.start..token.end];
        if text == ")" {
            if depth == 0 {
                break;
            }
            depth -= 1;
        } else if seen && depth == 0 && is_terminator(text) {
            break;
        } else if text == "(" {
            depth += 1;
        }
        seen = true;
        end = token.end;
    }
    end
}

pub(super) fn is_terminator(text: &str) -> bool {
    if matches!(
        text,
        "," | ";"
            | "="
            | "<>"
            | "!="
            | "<"
            | ">"
            | "<="
            | ">="
            | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "||"
    ) {
        return true;
    }
    [
        "AND",
        "OR",
        "THEN",
        "ELSE",
        "END",
        "WHEN",
        "FROM",
        "WHERE",
        "GROUP",
        "ORDER",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "JOIN",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "CROSS",
        "NATURAL",
        "ON",
        "USING",
        "RETURNING",
        "SET",
        "VALUES",
        "INTO",
        "AS",
        "IS",
        "BETWEEN",
        "IN",
        "LIKE",
        "ILIKE",
        "SIMILAR",
        "COLLATE",
        "FILTER",
        "OVER",
        "WINDOW",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "FOR",
        "FETCH",
    ]
    .iter()
    .any(|keyword| text.eq_ignore_ascii_case(keyword))
}

// ── not_in_nullable ────────────────────────────────────────────────────────

fn not_in_nullable(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::BoolExpr(expr) = node else {
            continue;
        };
        not_in(ctx, expr, &mut out);
    }
    out
}

fn not_in(ctx: &Ctx<'_>, expr: &BoolExpr, out: &mut Vec<RawFinding>) {
    if BoolExprType::try_from(expr.boolop) != Ok(BoolExprType::NotExpr) || expr.args.len() != 1 {
        return;
    }
    let Some(Node {
        node: Some(N::SubLink(sub)),
    }) = expr.args.first()
    else {
        return;
    };
    if SubLinkType::try_from(sub.sub_link_type) != Ok(SubLinkType::AnySublink) {
        return;
    }
    // A row-valued `(a, b) NOT IN (SELECT …)` compares field by field, and
    // one NULL output column only makes the fields it is compared with
    // unknown: `(1, 2) NOT IN (SELECT 2, NULL)` still returns the row
    // because its first fields differ. The scalar null rule does not apply.
    if sub
        .testexpr
        .as_deref()
        .and_then(|node| node.node.as_ref())
        .is_some_and(|node| matches!(node, N::RowExpr(_)))
    {
        return;
    }
    let operator = sub
        .oper_name
        .iter()
        .filter_map(|node| match &node.node {
            Some(N::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(".");
    if !operator.is_empty() && operator != "=" {
        return;
    }
    // The analyzer's facts: without one, we don't know either side's
    // nullability, and never guess.
    let Some(fact) = ctx.view.sublink_nullability(sub.location) else {
        return;
    };
    let message = match (fact.lhs_nullable, fact.subquery_nullable) {
        // A NULL left side against an empty subquery returns the row: the
        // "no rows" claim only holds once the subquery returns one.
        (true, true) => {
            "NOT IN (SELECT …) returns no rows for a NULL left side when the \
                        subquery returns any row, or when the subquery returns a NULL"
        }
        (true, false) => {
            "NOT IN (SELECT …) returns no rows for a NULL left side when the \
                          subquery returns any row"
        }
        (false, true) => "NOT IN (SELECT …) returns no rows when the subquery returns a NULL",
        (false, false) => return,
    };
    let start = sub
        .testexpr
        .as_deref()
        .and_then(operand_start)
        .or_else(|| location(sub.location))
        .or_else(|| location(expr.location))
        .unwrap_or(0);
    // Scan from the subquery's opening parenthesis: the `NOT` and `IN`
    // before it are terminators when scanning the left expression.
    let sub_start = location(sub.location).unwrap_or(start);
    let end = ctx
        .tokens
        .iter()
        .filter(|token| token.start >= sub_start)
        .find(|token| &ctx.sql[token.start..token.end] == "(")
        .map(|token| expression_end(ctx, token.start))
        .unwrap_or_else(|| expression_end(ctx, start));
    out.push(RawFinding {
        span: start..end.max(start + 1),
        message: message.to_owned(),
        help: Some(
            "use NOT EXISTS (SELECT 1 FROM … WHERE …), or make both sides NOT NULL".to_owned(),
        ),
        fix: Vec::new(),
    });
}

// ── unbounded_write ────────────────────────────────────────────────────────

fn unbounded_write(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        match node {
            NodeRef::UpdateStmt(stmt) if stmt.where_clause.is_none() => {
                write_finding(ctx, "UPDATE", "changes", &stmt.relation, &mut out);
            }
            NodeRef::DeleteStmt(stmt) if stmt.where_clause.is_none() => {
                write_finding(ctx, "DELETE", "removes", &stmt.relation, &mut out);
            }
            _ => {}
        }
    }
    out
}

fn write_finding(
    ctx: &Ctx<'_>,
    keyword: &str,
    verb: &str,
    relation: &Option<RangeVar>,
    out: &mut Vec<RawFinding>,
) {
    let Some(relation) = relation else {
        return;
    };
    let Some(relation_start) = location(relation.location) else {
        return;
    };
    let start = ctx
        .tokens
        .iter()
        .filter(|token| token.end <= relation_start)
        .rfind(|token| ctx.sql[token.start..token.end].eq_ignore_ascii_case(keyword))
        .map_or(relation_start, |token| token.start);
    let end = relation_end(ctx, relation).unwrap_or(relation_start + relation.relname.len());
    let table = table_name(relation);
    out.push(RawFinding {
        span: start..end.max(start + keyword.len()),
        message: format!("{keyword} without WHERE {verb} every row of {table}"),
        help: Some(
            "add a WHERE clause, or allow it with -- typedpg-lint: allow(unbounded_write)"
                .to_owned(),
        ),
        fix: Vec::new(),
    });
}

fn relation_end(ctx: &Ctx<'_>, relation: &RangeVar) -> Option<usize> {
    let start = location(relation.location)?;
    let mut tokens = ctx.tokens.iter().filter(|token| token.start >= start);
    let first = tokens.next()?;
    if relation.schemaname.is_empty() {
        return Some(first.end);
    }
    let mut seen_dot = false;
    for token in tokens {
        let text = &ctx.sql[token.start..token.end];
        if seen_dot {
            return Some(token.end);
        }
        if text == "." {
            seen_dot = true;
        }
    }
    Some(first.end)
}

/// The table as the message names it, as SQL writes it: `users`,
/// `public.users`, `"My Users"`.
pub(super) fn table_name(relation: &RangeVar) -> String {
    if relation.schemaname.is_empty() {
        quote_identifier(&relation.relname)
    } else {
        QualifiedName::new(relation.schemaname.clone(), relation.relname.clone()).to_string()
    }
}
