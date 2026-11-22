//! The lint rules. Both walk every node of the parse tree, so a site inside
//! a subquery, a CTE (data-modifying ones included), a CASE or a JOIN's ON
//! is visited like any other.

use typedpg_analyzer::{CastMethod, PgTypeOid, TypType};
use typedpg_core::{QualifiedName, quote_identifier};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{
    AExpr, AExprKind, BoolExpr, BoolExprType, CaseWhen, Node, Node as N, RangeVar, SubLinkType,
};

use crate::{Ctx, RawFinding, Rule};

mod cartesian_join;
mod case_without_else;
mod constant_condition;
mod insert_without_columns;
mod integer_division;
mod like_unescaped_param;
mod limit_without_order;
mod outer_reference_in_subquery;
mod redundant_alias;
mod redundant_cast;
mod redundant_distinct;
mod redundant_join;
mod redundant_limit;
mod redundant_null_check;
mod scalar_subquery_may_return_many;
mod self_assignment;
mod simplifiable_case;
mod timestamp_timezone_mix;
mod unreachable_case_branch;
mod unused_cte;

pub(crate) use insert_without_columns::width as insert_width;

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
        Rule::ConstantCondition => constant_condition::findings(ctx),
        Rule::UnreachableCaseBranch => unreachable_case_branch::findings(ctx),
        Rule::CaseWithoutElse => case_without_else::findings(ctx),
        Rule::RedundantCast => redundant_cast::findings(ctx),
        Rule::RedundantAlias => redundant_alias::findings(ctx),
        Rule::OuterReferenceInSubquery => outer_reference_in_subquery::findings(ctx),
        Rule::CartesianJoin => cartesian_join::findings(ctx),
        Rule::InsertWithoutColumns => insert_without_columns::findings(ctx),
        Rule::SelfAssignment => self_assignment::findings(ctx),
        Rule::UnusedCte => unused_cte::findings(ctx),
        Rule::SimplifiableCase => simplifiable_case::findings(ctx),
        Rule::LikeUnescapedParam => like_unescaped_param::findings(ctx),
        Rule::RedundantJoin => redundant_join::findings(ctx),
        Rule::ScalarSubqueryMayReturnMany => scalar_subquery_may_return_many::findings(ctx),
        Rule::TimestampTimezoneMix => timestamp_timezone_mix::findings(ctx),
        Rule::IntegerDivision => integer_division::findings(ctx),
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
        | Rule::DropIndexBlocking
        | Rule::BlockingRebuild
        | Rule::DetachPartitionBlocking
        | Rule::ValidateInSameTransaction
        | Rule::AttachPartitionScan
        | Rule::MissingPrimaryKey
        | Rule::RlsPolicyNotEnforced
        | Rule::RlsNoPolicy
        | Rule::SecurityDefinerSearchPath
        | Rule::PreferTimestamptz
        | Rule::PreferIdentity
        | Rule::PreferText
        | Rule::PreferBigintPk
        | Rule::PreferJsonb
        // The binding and call-site rules run over the Rust source of the
        // invocation, in `typedpg_cli`, which alone sees it.
        | Rule::RedundantBinding
        | Rule::RenamingBinding
        | Rule::FetchMayReturnMany
        | Rule::FetchAllSingleRow
        | Rule::UnusedReturning
        // It compares the deployed code's queries across two schemas, in
        // `deployed`.
        | Rule::BreaksDeployedQueries
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
    match expr.kind {
        AExprKind::AexprOp => {
            let Some(operator) = operator_name(expr) else {
                return;
            };
            if !COMPARISONS.contains(&operator) || ctx.non_strict_operators.contains(operator) {
                return;
            }
            let left_null = expr
                .lexpr
                .as_ref()
                .is_some_and(|node| is_null_literal(ctx, node));
            let right_null = expr
                .rexpr
                .as_ref()
                .is_some_and(|node| is_null_literal(ctx, node));
            if !left_null && !right_null {
                return;
            }
            let start = expr
                .lexpr
                .as_ref()
                .and_then(operand_start)
                .or_else(|| location(expr.location))
                .unwrap_or(0);
            let end = expr
                .rexpr
                .as_ref()
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
        AExprKind::AexprIn if operator_name(expr) == Some("<>") => {
            // `NOT IN` compares with `=` internally; a user-defined
            // non-strict `=` may accept NULL.
            if ctx.non_strict_operators.contains("=") {
                return;
            }
            let Some(right) = expr.rexpr.as_ref() else {
                return;
            };
            let N::List(list) = right else {
                return;
            };
            if !list
                .items
                .iter()
                .any(|node| node.as_ref().is_some_and(|node| is_null_literal(ctx, node)))
            {
                return;
            }
            let start = expr
                .lexpr
                .as_ref()
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
    let Some(expr) = when.expr.as_ref() else {
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
    expr.name.iter().rev().find_map(|node| match node {
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
    match node {
        N::AConst(value) => value.isnull,
        N::TypeCast(cast) => {
            let Some(arg) = cast.arg.as_ref() else {
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
            let source = match arg {
                // A bare NULL literal is `unknown`: the parser makes a NULL
                // constant of the target type directly, with no function.
                N::AConst(_) => None,
                N::TypeCast(inner) => inner
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
    match node {
        N::AConst(value) if value.isnull => location(value.location),
        N::TypeCast(cast) => cast
            .arg
            .as_ref()
            .and_then(operand_start)
            .or_else(|| node_start(node)),
        _ => node_start(node),
    }
}

pub(super) fn node_start(node: &Node) -> Option<usize> {
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

/// The start of `node`'s first token: an operator expression starts at its
/// left operand (`A_Expr`'s own location is its operator), a boolean
/// expression at its first argument, a cast or test at its operand.
pub(super) fn leftmost_start(node: &Node) -> Option<usize> {
    match node {
        N::AExpr(expr) => expr
            .lexpr
            .as_ref()
            .and_then(leftmost_start)
            .or_else(|| location(expr.location)),
        N::BoolExpr(expr) if expr.boolop != BoolExprType::NotExpr => expr
            .args
            .first()
            .and_then(Option::as_ref)
            .and_then(leftmost_start)
            .or_else(|| location(expr.location)),
        N::TypeCast(cast) => cast
            .arg
            .as_ref()
            .and_then(leftmost_start)
            .or_else(|| node_start(node)),
        N::NullTest(test) => test
            .arg
            .as_ref()
            .and_then(leftmost_start)
            .or_else(|| location(test.location)),
        N::BooleanTest(test) => test
            .arg
            .as_ref()
            .and_then(leftmost_start)
            .or_else(|| location(test.location)),
        _ => node_start(node),
    }
}

/// The bytes `node` covers: from the first to the last token its subtree
/// has a location for, widened so the parentheses, brackets and `CASE …
/// END` opened or closed in between are balanced (`f(x)` ends at its `)`,
/// the `(a + b) * 2` of a product starts at its `(`). A trailing keyword no
/// node locates (`IS NULL`'s `NULL`) is not included.
pub(super) fn node_span(ctx: &Ctx<'_>, node: NodeRef<'_>) -> Option<std::ops::Range<usize>> {
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    let mut stack = vec![node];
    while let Some(inner) = stack.pop() {
        if let Some(at) = inner.location().and_then(|at| usize::try_from(at).ok()) {
            first = Some(first.map_or(at, |first| first.min(at)));
            last = Some(last.map_or(at, |last| last.max(at)));
        }
        stack.extend(inner.children());
    }
    let (first, last) = (first?, last?);
    let opens = |text: &str| text == "(" || text == "[" || text.eq_ignore_ascii_case("CASE");
    let closes = |text: &str| text == ")" || text == "]" || text.eq_ignore_ascii_case("END");
    let start_index = ctx.tokens.iter().position(|token| token.start >= first)?;
    let mut depth = 0isize;
    let mut lowest = 0isize;
    let mut end = first;
    let mut index = start_index;
    while let Some(token) = ctx.tokens.get(index) {
        let text = &ctx.sql[token.start..token.end];
        if token.start > last && depth <= lowest {
            // A name no node locates past the last one: the rest of a
            // qualified function name, a call's parenthesized arguments
            // (`count(*)`'s star has no location).
            if text == "." && index + 1 < ctx.tokens.len() {
                end = ctx.tokens[index + 1].end;
                index += 2;
                continue;
            }
            if text != "(" {
                break;
            }
        }
        if opens(text) {
            depth += 1;
        } else if closes(text) {
            depth -= 1;
            lowest = lowest.min(depth);
        }
        end = token.end;
        index += 1;
    }
    // Parentheses closed after `first` that opened before it.
    let mut start = first;
    let mut missing = -lowest;
    let mut index = start_index;
    while missing > 0 && index > 0 {
        index -= 1;
        let token = ctx.tokens[index];
        let text = &ctx.sql[token.start..token.end];
        if closes(text) {
            missing += 1;
        } else if opens(text) {
            missing -= 1;
        }
        start = token.start;
    }
    Some(start..end.max(start))
}

/// Scan the tokens starting at byte `from`, keeping track of parentheses,
/// brackets and `CASE … END`: stop at the first token at nesting depth zero
/// that `stop` accepts, or that closes a level opened before `from`.
/// Returns the end of the last token before it (`from` when none) and its
/// index in [`Ctx::tokens`] (`None` at the end of the text).
pub(super) fn scan_until(
    ctx: &Ctx<'_>,
    from: usize,
    mut stop: impl FnMut(&str) -> bool,
) -> (usize, Option<usize>) {
    let mut depth = 0usize;
    let mut end = from;
    for (index, token) in ctx.tokens.iter().enumerate() {
        if token.start < from {
            continue;
        }
        let text = &ctx.sql[token.start..token.end];
        let opens = text == "(" || text == "[" || text.eq_ignore_ascii_case("CASE");
        let closes = text == ")" || text == "]" || text.eq_ignore_ascii_case("END");
        if depth == 0 && (closes || stop(text)) {
            return (end, Some(index));
        }
        if opens {
            depth += 1;
        } else if closes {
            depth -= 1;
        }
        end = token.end;
    }
    (end, None)
}

/// Whether `text` is a keyword that starts a clause after a select list, a
/// condition or a `RETURNING` list.
pub(super) fn starts_clause(text: &str) -> bool {
    [
        "FROM",
        "INTO",
        "WHERE",
        "GROUP",
        "HAVING",
        "WINDOW",
        "ORDER",
        "LIMIT",
        "OFFSET",
        "FETCH",
        "FOR",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "RETURNING",
        "ON",
        "USING",
        "JOIN",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "CROSS",
        "NATURAL",
        "WHEN",
        "THEN",
    ]
    .iter()
    .any(|keyword| text.eq_ignore_ascii_case(keyword))
        || text == ";"
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
    if expr.boolop != BoolExprType::NotExpr || expr.args.len() != 1 {
        return;
    }
    let Some(Some(N::SubLink(sub))) = expr.args.first() else {
        return;
    };
    if sub.sub_link_type != SubLinkType::AnySublink {
        return;
    }
    // A row-valued `(a, b) NOT IN (SELECT …)` compares field by field, and
    // one NULL output column only makes the fields it is compared with
    // unknown: `(1, 2) NOT IN (SELECT 2, NULL)` still returns the row
    // because its first fields differ. The scalar null rule does not apply.
    if sub
        .testexpr
        .as_ref()
        .is_some_and(|node| matches!(node, N::RowExpr(_)))
    {
        return;
    }
    let operator = sub
        .oper_name
        .iter()
        .filter_map(|node| match node {
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
        .as_ref()
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
                write_finding(ctx, "UPDATE", "changes", stmt.relation.as_deref(), &mut out);
            }
            NodeRef::DeleteStmt(stmt) if stmt.where_clause.is_none() => {
                write_finding(ctx, "DELETE", "removes", stmt.relation.as_deref(), &mut out);
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
    relation: Option<&RangeVar>,
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

pub(super) fn relation_end(ctx: &Ctx<'_>, relation: &RangeVar) -> Option<usize> {
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
pub(crate) fn table_name(relation: &RangeVar) -> String {
    if relation.schemaname.is_empty() {
        quote_identifier(&relation.relname)
    } else {
        QualifiedName::new(relation.schemaname.clone(), relation.relname.clone()).to_string()
    }
}

/// What the rules' own tests share.
#[cfg(test)]
pub(crate) mod test_support {
    use typedpg_analyzer::PgCatalog;

    use crate::{Finding, LintOptions, Rule, SqlEdit, lint_query};

    /// A small schema: `users` and `posts`.
    pub(crate) fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    name text,
                    email varchar(100) NOT NULL UNIQUE,
                    code char(3)
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users,
                    title text NOT NULL
                 );",
            )
            .unwrap();
        catalog
    }

    /// The findings of `rule` alone (every rule at its default level).
    pub(crate) fn lint_rule(catalog: &PgCatalog, sql: &str, rule: Rule) -> Vec<Finding> {
        lint_query(catalog, sql, &LintOptions::default())
            .unwrap_or_else(|e| panic!("{sql}: {}", e.error))
            .into_iter()
            .filter(|finding| finding.rule == rule)
            .collect()
    }

    /// `sql` with every finding's fix applied, in the original text.
    pub(crate) fn fixed(sql: &str, findings: &[Finding]) -> String {
        let mut edits: Vec<SqlEdit> = findings
            .iter()
            .flat_map(|finding| finding.fix.clone())
            .collect();
        edits.sort_by_key(|edit| edit.range.start);
        let mut out = String::with_capacity(sql.len());
        let mut last = 0;
        for edit in edits {
            out.push_str(&sql[last..edit.range.start]);
            out.push_str(&edit.text);
            last = edit.range.end;
        }
        out.push_str(&sql[last..]);
        out
    }
}
