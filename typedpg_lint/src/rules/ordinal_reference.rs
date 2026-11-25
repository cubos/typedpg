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
//!
//! A `GROUP BY` position whose target is an aliased expression is reported
//! only when the alias provably names no input column of the same `SELECT`:
//! PostgreSQL resolves a bare name in `GROUP BY` to a `FROM` column before an
//! output column, so the alias would group by something else. When it is
//! safe, the help names the alias; an item whose columns can't be enumerated
//! (a CTE, a function without a covering column alias list) leaves the
//! position unreported, as a collision can't be ruled out.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{
    AConstValue, Alias, MinMaxOp, Node, Node as N, RangeFunction, RangeSubselect, ResTarget,
    SelectStmt, SetOperation, SubLinkType,
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
        let input_columns = input_columns(ctx, select);
        for sort in select.sort_clause.iter().flatten() {
            let N::SortBy(sort) = sort else {
                continue;
            };
            let finding = ordinal(
                ctx,
                &targets,
                &names,
                has_distinct_on,
                &input_columns,
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
                &input_columns,
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
                &input_columns,
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
    input_columns: &Option<Vec<String>>,
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
    let help = help(clause, target, index, names, input_columns)?;
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
        help: Some(help),
        fix: fix.into_iter().collect(),
    })
}

/// The help for a position: `None` when the position must be left alone, as
/// naming the target could resolve to something else.
fn help(
    clause: Clause,
    target: &ResTarget,
    index: usize,
    names: &[Option<String>],
    input_columns: &Option<Vec<String>>,
) -> Option<String> {
    if clause != Clause::GroupBy || target.name.is_empty() || column_ref(target).is_some() {
        return Some(clause.help().to_owned());
    }
    // `GROUP BY <alias>` resolves to an input column of that name first.
    // Without every item's input columns, a collision can't be ruled out.
    let input_columns = input_columns.as_ref()?;
    if input_columns.iter().any(|name| name == &target.name) {
        return None;
    }
    // Another output column may carry the same name (or a name this can't
    // tell), which would make the alias ambiguous.
    let contested = names.iter().enumerate().any(|(other, other_name)| {
        other != index
            && (other_name.as_deref() == Some(target.name.as_str()) || other_name.is_none())
    });
    if contested {
        return Some(clause.help().to_owned());
    }
    Some(format!(
        "write the alias `{}` instead of its position",
        typedpg_core::quote_identifier(&target.name)
    ))
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

/// The names the `FROM` items of `select` give its own level — what a bare
/// name in `GROUP BY` resolves to before the select list. `None` when an
/// item's columns can't be enumerated with certainty.
fn input_columns(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for item in &select.from_clause {
        names.extend(from_item_columns(ctx, item.as_ref())?);
    }
    Some(names)
}

/// The input column names one `FROM` item contributes, in column order.
fn from_item_columns(ctx: &Ctx<'_>, node: Option<&Node>) -> Option<Vec<String>> {
    match node? {
        N::RangeVar(range) => range_var_columns(ctx, range),
        N::JoinExpr(join) => {
            // A join's column alias list renames its merged columns.
            if join
                .alias
                .as_ref()
                .is_some_and(|alias| !alias.colnames.is_empty())
            {
                return None;
            }
            let mut names = from_item_columns(ctx, join.larg.as_ref())?;
            names.extend(from_item_columns(ctx, join.rarg.as_ref())?);
            Some(names)
        }
        N::RangeSubselect(subselect) => subselect_columns(subselect),
        N::RangeFunction(function) => range_function_columns(function),
        // A CTE, a table function, a sampled relation, …: what it
        // contributes isn't certain here.
        _ => None,
    }
}

/// A plain table's columns: its `pg_attribute` rows, the system columns, and
/// an alias column list renaming the leading ones.
fn range_var_columns(
    ctx: &Ctx<'_>,
    range: &typedpg_pg_query::ast::RangeVar,
) -> Option<Vec<String>> {
    let Some(typedpg_analyzer::RelationRef::Relation(relation)) = ctx.view.relation_of(range)
    else {
        return None;
    };
    let mut names: Vec<String> = ctx
        .catalog
        .attributes_of(relation)
        .iter()
        .filter(|attribute| attribute.attnum > 0)
        .map(|attribute| attribute.attname.clone())
        .collect();
    // An empty list may as well mean the catalog knows nothing: leave it
    // unknown instead of claiming there is no column to collide with.
    if names.is_empty() {
        return None;
    }
    rename_columns(&mut names, range.alias.as_deref())?;
    names.extend(
        ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"]
            .into_iter()
            .map(str::to_owned),
    );
    Some(names)
}

/// A subquery's output columns, its alias column list renaming the leading
/// ones.
fn subselect_columns(subselect: &RangeSubselect) -> Option<Vec<String>> {
    let N::SelectStmt(select) = subselect.subquery.as_ref()? else {
        return None;
    };
    let arity = select_arity(select)?;
    if let Some(alias) = subselect.alias.as_deref()
        && !alias.colnames.is_empty()
    {
        if alias.colnames.len() == arity {
            // The list names every column; what the subquery would call them
            // doesn't matter.
            return colname_list(alias);
        }
        if alias.colnames.len() > arity {
            return None;
        }
    }
    let mut names = select_output_names(select)?;
    rename_columns(&mut names, subselect.alias.as_deref())?;
    Some(names)
}

/// A function item's columns, when its column definition list or its alias
/// column list names them all.
fn range_function_columns(function: &RangeFunction) -> Option<Vec<String>> {
    if !function.coldeflist.is_empty() {
        return function
            .coldeflist
            .iter()
            .map(|column| match column.as_ref()? {
                N::ColumnDef(column) => Some(column.colname.clone()),
                _ => None,
            })
            .collect();
    }
    let arity = range_function_arity(function)?;
    let alias = function.alias.as_deref()?;
    if alias.colnames.len() != arity {
        return None;
    }
    colname_list(alias)
}

/// The number of columns a `FROM` function yields, when the parse tree tells
/// it. Only `unnest`'s count is known here: one per array argument.
fn range_function_arity(function: &RangeFunction) -> Option<usize> {
    if function.ordinality {
        // `WITH ORDINALITY` appends a column, and the function's own columns
        // may be anything.
        return None;
    }
    let [Some(N::List(pair))] = function.functions.as_slice() else {
        return None;
    };
    if pair.items.get(1).is_some_and(|item| item.is_some()) {
        // A per-function column definition list defines the columns itself.
        return None;
    }
    let Some(Some(N::FuncCall(call))) = pair.items.first() else {
        return None;
    };
    if name_string(call.funcname.last()?.as_ref()?)? != "unnest" {
        return None;
    }
    if call.funcname.len() == 1 {
        // The SQL-standard `unnest(a, b, …)` is rewritten into one
        // single-argument call per array, each yielding one column.
        (!call.args.is_empty()).then_some(call.args.len())
    } else {
        // A qualified `unnest` takes exactly one array or range.
        (call.args.len() == 1).then_some(1)
    }
}

/// The number of output columns of `select`, when the parse tree tells.
fn select_arity(select: &SelectStmt) -> Option<usize> {
    if select.op != SetOperation::SetopNone {
        // A set operation's branches all have the leftmost one's arity.
        return select_arity(select.larg.as_deref()?);
    }
    if let Some(first) = select.values_lists.first() {
        let N::List(row) = first.as_ref()? else {
            return None;
        };
        return (!row.items.is_empty()).then_some(row.items.len());
    }
    if select.target_list.is_empty() {
        return None;
    }
    let mut count = 0;
    for target in select.target_list.iter().flatten() {
        let N::ResTarget(target) = target else {
            return None;
        };
        if is_star(target) {
            return None;
        }
        count += 1;
    }
    (count > 0).then_some(count)
}

/// The names a subquery's output columns take, when every one is certain.
fn select_output_names(select: &SelectStmt) -> Option<Vec<String>> {
    if select.op != SetOperation::SetopNone {
        // A set operation names its columns after the leftmost branch.
        return select_output_names(select.larg.as_deref()?);
    }
    if !select.values_lists.is_empty() {
        let arity = select_arity(select)?;
        return Some(
            (1..=arity)
                .map(|number| format!("column{number}"))
                .collect(),
        );
    }
    select
        .target_list
        .iter()
        .map(|node| {
            let N::ResTarget(target) = node.as_ref()? else {
                return None;
            };
            if is_star(target) {
                return None;
            }
            subquery_output_name(target)
        })
        .collect()
}

/// The name a subquery output column takes with certainty: its alias, or the
/// last field of a plain column reference. Other expressions' derived names
/// are left to the analyzer.
fn subquery_output_name(target: &ResTarget) -> Option<String> {
    if !target.name.is_empty() {
        return Some(target.name.clone());
    }
    if !target.indirection.is_empty() {
        return None;
    }
    let last = column_ref(target)?.fields.last()?.as_ref()?;
    Some(name_string(last)?.to_owned())
}

/// Renames the leading columns after an alias column list (`users AS u(a,
/// b)`); the rest keep their names.
fn rename_columns(names: &mut [String], alias: Option<&Alias>) -> Option<()> {
    let Some(alias) = alias else {
        return Some(());
    };
    if alias.colnames.len() > names.len() {
        return None;
    }
    for (column, name) in names.iter_mut().zip(&alias.colnames) {
        let N::String(name) = name.as_ref()? else {
            return None;
        };
        *column = name.sval.clone();
    }
    Some(())
}

/// The names of an alias column list, when every entry is a plain name.
fn colname_list(alias: &Alias) -> Option<Vec<String>> {
    alias
        .colnames
        .iter()
        .map(|name| match name.as_ref()? {
            N::String(name) => Some(name.sval.clone()),
            _ => None,
        })
        .collect()
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
    fn group_by_positions_naming_an_input_column_are_not_reported() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE cost_rollup (
                bucket timestamptz,
                tenant_id bigint,
                user_role_ids bigint[],
                cost_usd numeric
             );",
        )
        .unwrap();
        // Every alias also names an input column, and repeating the
        // expression is the only alternative to the position.
        let sql = "SELECT CASE WHEN $by_time THEN date_trunc($unit, r.bucket, $tz) END AS bucket,
                          CASE WHEN $g_tenant THEN r.tenant_id END AS tenant_id,
                          SUM(r.cost_usd) AS cost_usd
                   FROM cost_rollup r
                   LEFT JOIN LATERAL unnest(CASE WHEN $g_role THEN r.user_role_ids END) AS gr(user_role_id) ON true
                   GROUP BY 1, 2";
        let findings = lint_rule(&db, sql, RULE);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_group_by_alias_is_named_when_no_input_column_has_it() {
        let db = catalog();
        // `users.email` is an input column: `GROUP BY email` would group by
        // it rather than by the lowercased output column.
        let sql = "SELECT lower(email) AS email, count(*) FROM users GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert!(findings.is_empty(), "{findings:?}");
        // No input column `e`: the alias names the target.
        let sql = "SELECT lower(email) AS e, count(*) FROM users GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].help.as_deref(),
            Some("write the alias `e` instead of its position")
        );
        assert!(findings[0].fix.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_group_by_alias_with_unknown_input_columns_is_not_reported() {
        let db = catalog();
        // `generate_series` names its only column `g` here, but the parse
        // tree doesn't say how many columns it yields: a collision can't be
        // ruled out.
        let sql =
            "SELECT lower(email) AS e, count(*) FROM users, generate_series(1, 3) g GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert!(findings.is_empty(), "{findings:?}");
        // A covering column alias list makes the item's columns certain.
        let sql = "SELECT lower(email) AS e, count(*) \
                   FROM users, unnest(ARRAY['x']) AS u(tag) GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn a_group_by_alias_sees_columns_a_subquery_alias_list_names() {
        let db = catalog();
        let sql = "SELECT lower(v.tag) AS tag, count(*) FROM (VALUES ('a')) AS v(tag) GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert!(findings.is_empty(), "{findings:?}");
        let sql = "SELECT lower(v.tag) AS t, count(*) FROM (VALUES ('a')) AS v(tag) GROUP BY 1";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].help.as_deref(),
            Some("write the alias `t` instead of its position")
        );
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
