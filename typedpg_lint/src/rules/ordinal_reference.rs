//! The `ordinal_reference` rule: reports `ORDER BY 1`, `GROUP BY 1` and
//! `DISTINCT ON (1)`, which pick an output column by its position. A column
//! added to, or removed from, the select list silently changes what they
//! mean; their name or expression doesn't.
//!
//! Only a plain integer constant is reported: `ORDER BY 1 + 0` is an
//! expression, not a position. The position must name a target (a query
//! whose position is out of range doesn't analyze). A set operation's
//! `ORDER BY 1` names the union's output column, and a `VALUES` list's
//! position its own column; neither has a select list to name, so neither is
//! reported. A `SELECT *` expands to columns the parse tree doesn't list: the
//! positions after it map to no target, and one of its columns may take the
//! name a target has, so a select list with a star is skipped.
//!
//! The fix names the target:
//! - `ORDER BY`: the target's alias, or — without one — its column
//!   reference. An unqualified column is written qualified (`ORDER BY
//!   users.id`) when another output column may carry the same name:
//!   `ORDER BY` resolves a bare name to an output column first. Any other
//!   expression is left alone: writing it a second time would evaluate a
//!   volatile function twice.
//! - `GROUP BY` and `DISTINCT ON`: the target's expression when that is a
//!   plain column reference, never its alias — PostgreSQL resolves a name in
//!   `GROUP BY` to an input column before an output column, so an alias could
//!   name a different column there.
//!
//! When the name a fix would write is ambiguous among the output columns, or
//! the query has a `DISTINCT ON` the rewritten key must keep matching, no fix
//! is offered.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{
    AConstValue, MinMaxOp, Node, Node as N, ResTarget, SetOperation, SubLinkType,
};

use super::node_span;
use crate::{Ctx, RawFinding, SqlEdit};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        // A set operation's or a VALUES list's position names a column this
        // node has no select list for.
        if select.op != SetOperation::SetopNone || !select.values_lists.is_empty() {
            continue;
        }
        let targets: Vec<&ResTarget> = select
            .target_list
            .iter()
            .flatten()
            .filter_map(|target| match target {
                N::ResTarget(target) => Some(&**target),
                _ => None,
            })
            .collect();
        if targets.is_empty() || targets.iter().any(|target| is_star(target)) {
            continue;
        }
        let names = target_names(&targets);
        let has_distinct_on = select.distinct_clause.iter().flatten().next().is_some();
        for sort in select.sort_clause.iter().flatten() {
            let N::SortBy(sort) = sort else {
                continue;
            };
            let finding = ordinal(
                ctx,
                &targets,
                &names,
                has_distinct_on,
                sort.node.as_ref(),
                Clause::OrderBy,
            );
            out.extend(finding);
        }
        for key in select.group_clause.iter().flatten() {
            out.extend(ordinal(
                ctx,
                &targets,
                &names,
                has_distinct_on,
                Some(key),
                Clause::GroupBy,
            ));
        }
        for key in select.distinct_clause.iter().flatten() {
            out.extend(ordinal(
                ctx,
                &targets,
                &names,
                has_distinct_on,
                Some(key),
                Clause::DistinctOn,
            ));
        }
    }
    out
}

/// The clause a position appears in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    OrderBy,
    GroupBy,
    DistinctOn,
}

impl Clause {
    fn name(self) -> &'static str {
        match self {
            Clause::OrderBy => "ORDER BY",
            Clause::GroupBy => "GROUP BY",
            Clause::DistinctOn => "DISTINCT ON",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Clause::OrderBy => "write the column or its alias instead of its position",
            Clause::GroupBy => {
                "write the column or expression instead of its position (a name in GROUP BY \
                 resolves to an input column first)"
            }
            Clause::DistinctOn => "write the column or expression instead of its position",
        }
    }
}

fn ordinal(
    ctx: &Ctx<'_>,
    targets: &[&ResTarget],
    names: &[Option<String>],
    has_distinct_on: bool,
    node: Option<&Node>,
    clause: Clause,
) -> Option<RawFinding> {
    let Some(N::AConst(constant)) = node else {
        return None;
    };
    if constant.isnull {
        return None;
    }
    let Some(AConstValue::Integer(position)) = &constant.val else {
        return None;
    };
    let position = usize::try_from(position.ival).ok().filter(|&n| n >= 1)?;
    let index = position - 1;
    let target = targets.get(index)?;
    let description = target_description(ctx, target, position);
    let span = node_span(ctx, NodeRef::AConst(constant))?;
    let message = format!(
        "{} {position} refers to {description} by its position: name it instead",
        clause.name()
    );
    let fix = fix(
        ctx,
        target,
        index,
        names,
        has_distinct_on,
        span.clone(),
        clause,
    );
    Some(RawFinding {
        span,
        message,
        help: Some(clause.help().to_owned()),
        fix: fix.into_iter().collect(),
    })
}

/// How the message names the target: its alias, its plain column reference,
/// or — for anything else — its position.
fn target_description(ctx: &Ctx<'_>, target: &ResTarget, position: usize) -> String {
    if !target.name.is_empty() {
        return format!(
            "the output column `{}`",
            typedpg_core::quote_identifier(&target.name)
        );
    }
    if let Some(text) = column_text(ctx, target) {
        return format!("the output column `{text}`");
    }
    format!("the {} output column", ordinal_word(position))
}

/// The target expression as a plain column reference, as written.
fn column_text(ctx: &Ctx<'_>, target: &ResTarget) -> Option<String> {
    let column = column_ref(target)?;
    if column
        .fields
        .iter()
        .flatten()
        .any(|field| matches!(field, N::AStar(_)))
    {
        return None;
    }
    Some(
        ctx.written(node_span(ctx, target.val.as_ref()?.to_ref())?)
            .to_owned(),
    )
}

fn column_ref(target: &ResTarget) -> Option<&typedpg_pg_query::ast::ColumnRef> {
    match target.val.as_ref()? {
        N::ColumnRef(column) => Some(column),
        _ => None,
    }
}

/// Whether the target is a `*` (or `t.*`), which expands to columns this
/// rule can't name.
fn is_star(target: &ResTarget) -> bool {
    column_ref(target).is_some_and(|column| {
        column
            .fields
            .iter()
            .flatten()
            .any(|field| matches!(field, N::AStar(_)))
    })
}

/// The name each target's output column takes, as closely as the parse tree
/// tells: the alias, or the name PostgreSQL derives from the expression.
/// `None` for an expression whose derived name this can't tell.
fn target_names(targets: &[&ResTarget]) -> Vec<Option<String>> {
    targets.iter().map(|target| target_name(target)).collect()
}

fn target_name(target: &ResTarget) -> Option<String> {
    if !target.name.is_empty() {
        return Some(target.name.clone());
    }
    derived_name(target.val.as_ref()?)
}

fn derived_name(node: &Node) -> Option<String> {
    Some(match node {
        N::ColumnRef(column) => name_string(column.fields.last()?.as_ref()?)?.to_owned(),
        N::FuncCall(call) => name_string(call.funcname.last()?.as_ref()?)?.to_owned(),
        // A cast keeps its argument's name.
        N::TypeCast(cast) => derived_name(cast.arg.as_ref()?)?,
        N::CollateClause(collate) => derived_name(collate.arg.as_ref()?)?,
        N::AIndirection(indirection) => derived_name(indirection.arg.as_ref()?)?,
        N::CoalesceExpr(_) => "coalesce".to_owned(),
        N::CaseExpr(_) => "case".to_owned(),
        N::MinMaxExpr(minmax) => match minmax.op {
            MinMaxOp::IsGreatest => "greatest".to_owned(),
            MinMaxOp::IsLeast => "least".to_owned(),
        },
        N::AArrayExpr(_) => "array".to_owned(),
        N::RowExpr(_) => "row".to_owned(),
        N::SubLink(sublink) if sublink.sub_link_type == SubLinkType::ExistsSublink => {
            "exists".to_owned()
        }
        // A literal, parameter, operator expression, NULL / boolean test,
        // scalar subquery: PostgreSQL names it `?column?`.
        N::AConst(_)
        | N::ParamRef(_)
        | N::AExpr(_)
        | N::BoolExpr(_)
        | N::NullTest(_)
        | N::BooleanTest(_)
        | N::SubLink(_) => "?column?".to_owned(),
        _ => return None,
    })
}

fn name_string(node: &Node) -> Option<&str> {
    match node {
        N::String(name) => Some(name.sval.as_str()),
        _ => None,
    }
}

/// The edit naming the target, when the replacement is provably the same
/// expression (see the module docs).
fn fix(
    ctx: &Ctx<'_>,
    target: &ResTarget,
    index: usize,
    names: &[Option<String>],
    has_distinct_on: bool,
    span: std::ops::Range<usize>,
    clause: Clause,
) -> Option<SqlEdit> {
    if !target.indirection.is_empty() {
        return None;
    }
    let text = match clause {
        Clause::OrderBy => order_key(ctx, target, index, names, has_distinct_on)?,
        Clause::GroupBy | Clause::DistinctOn => column_text(ctx, target)?,
    };
    Some(SqlEdit { range: span, text })
}

/// The `ORDER BY` key naming the target without changing what it sorts by.
fn order_key(
    ctx: &Ctx<'_>,
    target: &ResTarget,
    index: usize,
    names: &[Option<String>],
    has_distinct_on: bool,
) -> Option<String> {
    let name = target_name(target)?;
    // Another output column may take the same name — or a name this can't
    // tell, which may be this one. `ORDER BY` resolves a bare name to an
    // output column first, and two columns of one name make it ambiguous.
    let contested = names.iter().enumerate().any(|(other, other_name)| {
        other != index && (other_name.as_deref() == Some(name.as_str()) || other_name.is_none())
    });
    if !target.name.is_empty() {
        // The alias names exactly this target, unless it is contested.
        return (!contested).then(|| typedpg_core::quote_identifier(&name));
    }
    let column = column_ref(target)?;
    let fields: Vec<&str> = column
        .fields
        .iter()
        .map(|field| name_string(field.as_ref()?))
        .collect::<Option<_>>()?;
    match fields.as_slice() {
        [] => None,
        // Already qualified: `ORDER BY` resolves it as an expression, never
        // as a bare output-column name.
        [_, _, ..] => column_text(ctx, target),
        [field] if !contested => Some(typedpg_core::quote_identifier(field)),
        // The bare name would resolve to another output column: qualify the
        // input column instead. With a `DISTINCT ON`, the key must keep
        // matching its expression, so leave the position alone.
        [field] if !has_distinct_on => {
            let binding = ctx.view.column_ref_binding(column.location)?;
            let source = binding.source.as_deref()?;
            // A whole-row reference (or a column named like its entry) has
            // no `entry.column` form to write.
            if source == *field {
                return None;
            }
            Some(format!(
                "{}.{}",
                typedpg_core::quote_identifier(source),
                typedpg_core::quote_identifier(field)
            ))
        }
        _ => None,
    }
}

/// `1st`, `2nd`, `3rd`, `4th`, …
fn ordinal_word(position: usize) -> String {
    let suffix = match (position % 10, position % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{position}{suffix}")
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::OrdinalReference;

    #[test]
    fn order_by_positions_are_named() {
        let db = catalog();
        for (sql, message, fixed_sql) in [
            (
                "SELECT id, name FROM users ORDER BY 1",
                "ORDER BY 1 refers to the output column `id` by its position: name it instead",
                "SELECT id, name FROM users ORDER BY id",
            ),
            (
                "SELECT id, name FROM users ORDER BY 2 DESC NULLS LAST",
                "ORDER BY 2 refers to the output column `name` by its position: name it instead",
                "SELECT id, name FROM users ORDER BY name DESC NULLS LAST",
            ),
            (
                "SELECT id AS user_id FROM users ORDER BY 1",
                "ORDER BY 1 refers to the output column `user_id` by its position: name it \
                 instead",
                "SELECT id AS user_id FROM users ORDER BY user_id",
            ),
            (
                "SELECT u.id FROM users u ORDER BY 1",
                "ORDER BY 1 refers to the output column `u.id` by its position: name it instead",
                "SELECT u.id FROM users u ORDER BY u.id",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].message, message, "{sql}");
            assert!(findings[0].help.is_some(), "{sql}");
            assert_eq!(fixed(sql, &findings), fixed_sql, "{sql}");
        }
    }

    #[test]
    fn a_contested_order_by_name_is_qualified() {
        let db = catalog();
        // `ORDER BY email` would resolve to the second output column.
        let sql = "SELECT email, name AS email FROM users ORDER BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            fixed(sql, &findings),
            "SELECT email, name AS email FROM users ORDER BY users.email"
        );
        // A duplicated alias can't be written at all: reported without a fix.
        let sql = "SELECT id AS x, name AS x FROM users ORDER BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty(), "{findings:?}");
        // Here the second `code` is the third target's derived name.
        let sql = "SELECT u.id AS code, u.name, u.code FROM users u ORDER BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].fix.is_empty(), "{findings:?}");
    }

    #[test]
    fn group_by_and_distinct_on_positions_are_named_by_their_expression() {
        let db = catalog();
        for (sql, fixed_sql) in [
            (
                "SELECT id AS user_id, count(*) FROM posts GROUP BY 1",
                "SELECT id AS user_id, count(*) FROM posts GROUP BY id",
            ),
            (
                "SELECT DISTINCT ON (1) id, name FROM users ORDER BY 1, name",
                "SELECT DISTINCT ON (id) id, name FROM users ORDER BY id, name",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(!findings.is_empty(), "{sql}");
            assert!(findings.iter().all(|finding| finding.help.is_some()));
            assert_eq!(fixed(sql, &findings), fixed_sql, "{sql}");
        }
        // An alias is not used in GROUP BY: it may name an input column.
        let sql = "SELECT id AS name, count(*) FROM posts GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(
            fixed(sql, &findings),
            "SELECT id AS name, count(*) FROM posts GROUP BY id"
        );
    }

    #[test]
    fn expressions_name_their_position_without_a_fix() {
        let db = catalog();
        for (sql, message) in [
            (
                "SELECT lower(name) FROM users ORDER BY 1",
                "ORDER BY 1 refers to the 1st output column by its position: name it instead",
            ),
            (
                "SELECT 1 FROM users ORDER BY 1",
                "ORDER BY 1 refers to the 1st output column by its position: name it instead",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(findings[0].message, message, "{sql}");
            assert!(findings[0].fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn a_select_star_is_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT * FROM users ORDER BY 1",
            "SELECT *, name FROM users ORDER BY 2",
            "SELECT u.*, name FROM users u ORDER BY 2",
            "SELECT name, * FROM users ORDER BY 1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn positions_of_set_operations_and_values_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users UNION SELECT id FROM posts ORDER BY 1",
            "VALUES (1), (2) ORDER BY 1",
            "SELECT id FROM users ORDER BY id",
            "SELECT id FROM users ORDER BY 1 + 0",
            "SELECT id FROM users ORDER BY (SELECT 1)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
