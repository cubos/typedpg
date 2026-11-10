//! The `limit_without_order` rule: reports a `LIMIT`, an `OFFSET` or a
//! `DISTINCT ON` without an `ORDER BY`, which picks arbitrary rows (and, for
//! `DISTINCT ON`, an arbitrary row per group).
//!
//! Not reported when the choice doesn't matter: a query proven to return at
//! most one row ([`typedpg_analyzer::select_at_most_one_row`]), a subquery
//! of `EXISTS` (existence doesn't care which rows), an `IN` / `= ANY` /
//! `ALL` in the `WHERE` of an `UPDATE` or `DELETE` (the batching idiom), a
//! `LIMIT 0`, or a locking clause with `SKIP LOCKED` (the queue pattern
//! wants any unlocked row).

use std::collections::HashSet;

use typedpg_analyzer::{ProKind, ProVolatile, Type, select_at_most_one_row};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, AExprKind, BoolExprType, LockWaitPolicy, SelectStmt, SubLinkType, a_const::Val,
    node::Node,
};

use crate::{Ctx, RawFinding};

const HELP: &str = "add an ORDER BY that makes the choice deterministic, or allow it with \
                    -- typedpg-lint: allow(limit_without_order)";

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let batched = batched_subqueries(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SelectStmt(select) = node else {
            continue;
        };
        if !select.sort_clause.is_empty() {
            continue;
        }
        let pointer: *const SelectStmt = select;
        if batched.contains(&pointer) {
            continue;
        }
        if has_skip_locked(select) || limit_is_zero(select) {
            continue;
        }
        if constant_target(ctx, select) || invariant_target(ctx, select) {
            continue;
        }
        if select_at_most_one_row(ctx.catalog, ctx.view, select) {
            continue;
        }
        if let Some(span) = distinct_on_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "DISTINCT ON without ORDER BY keeps an arbitrary row of each group"
                    .to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
        if let Some(span) = limit_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "LIMIT without ORDER BY returns arbitrary rows".to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
        if let Some(span) = offset_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "OFFSET without ORDER BY skips arbitrary rows".to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
    }
    out
}

/// The selects whose arbitrary row choice doesn't matter: the subquery of
/// an `EXISTS` (existence doesn't care which rows), or an `IN` / `= ANY` /
/// `ALL` in the `WHERE` of an `UPDATE` or `DELETE` — the batching idiom
/// (`DELETE … WHERE id IN (SELECT … LIMIT 1000)`) deliberately takes any
/// batch. An `IN` elsewhere (a `SELECT`'s WHERE, a projection) does care:
/// the limit changes which values the outer query sees.
fn batched_subqueries(ctx: &Ctx<'_>) -> HashSet<*const SelectStmt> {
    let mut batched = HashSet::new();
    // The EXISTS exemption propagates through FROM subqueries an
    // existence-only select reads; the DML batching exemption does not (the
    // IN list's values matter there).
    let mut existence = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SubLink(sublink) = node else {
            continue;
        };
        match sublink.sub_link_type() {
            SubLinkType::ExistsSublink => {
                insert_sublink_select(&mut existence, sublink);
            }
            SubLinkType::AnySublink | SubLinkType::AllSublink if in_dml_where(ctx, sublink) => {
                insert_sublink_select(&mut batched, sublink);
            }
            _ => {}
        }
    }
    batched.extend(existence.iter().copied());
    // A select whose rows are only tested for existence (and which only
    // passes them through: a single FROM item, no filter, grouping,
    // distinct, limit or aggregate) observes its FROM subquery the same
    // way, so the subquery's LIMIT still picks an arbitrary row.
    loop {
        let mut added = false;
        for (node, _) in ctx.tree.nodes() {
            let NodeRef::SelectStmt(select) = node else {
                continue;
            };
            let pointer: *const SelectStmt = select;
            if !existence.contains(&pointer) || !observes_only_existence(ctx, select) {
                continue;
            }
            let Some(Node::RangeSubselect(range)) = select
                .from_clause
                .first()
                .and_then(|node| node.node.as_ref())
            else {
                continue;
            };
            let Some(Node::SelectStmt(subquery)) = range
                .subquery
                .as_deref()
                .and_then(|node| node.node.as_ref())
            else {
                continue;
            };
            if existence.insert(&**subquery) {
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    batched.extend(existence);
    batched
}

fn insert_sublink_select(batched: &mut HashSet<*const SelectStmt>, sublink: &protobuf::SubLink) {
    if let Some(Node::SelectStmt(select)) = sublink
        .subselect
        .as_deref()
        .and_then(|node| node.node.as_ref())
    {
        let pointer: *const SelectStmt = &**select;
        batched.insert(pointer);
    }
}

/// Whether `select` only observes its single FROM item's rows: it adds no
/// filter, grouping, distinct, limit or aggregate, so whether it returns a
/// row is whether the FROM item does. A set-returning function in its own
/// target list breaks that: the function can return no rows even when its
/// input does (and how many it returns depends on the chosen row), so the
/// LIMIT inside the FROM subquery still matters.
fn observes_only_existence(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    select.op() == typedpg_pg_query::protobuf::SetOperation::SetopNone
        && select.from_clause.len() == 1
        && select.where_clause.is_none()
        && select.group_clause.is_empty()
        && select.having_clause.is_none()
        && select.window_clause.is_empty()
        && select.distinct_clause.is_empty()
        && select.limit_count.is_none()
        && select.limit_offset.is_none()
        && !target_has_aggregate(ctx, select)
        && !has_set_returning_function(ctx, select)
}

/// Whether an expression of `select` at its own query level (its target
/// list, its `FROM` items' expressions, … — not a nested `SELECT`) calls a
/// set-returning function. Every overload counts; an unresolved call is
/// treated as one, conservatively.
fn has_set_returning_function(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = NodeRef::SelectStmt(select).children();
    while let Some(node) = stack.pop() {
        // A nested `SELECT` is another query level: its own target list is
        // checked when it is walked as a select of its own.
        if matches!(node, NodeRef::SelectStmt(_)) {
            continue;
        }
        if let NodeRef::FuncCall(call) = node {
            let names: Vec<&str> = call
                .funcname
                .iter()
                .filter_map(|node| match &node.node {
                    Some(Node::String(string)) => Some(string.sval.as_str()),
                    _ => None,
                })
                .collect();
            let (schema, name) = match names.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => return true,
            };
            let procs = ctx.catalog.find_functions(schema, name);
            if procs.is_empty() || procs.iter().any(|proc| proc.proretset) {
                return true;
            }
        }
        stack.extend(node.children());
    }
    false
}

/// Whether the target list calls an aggregate, which returns a row even
/// when its input is empty (so it changes what EXISTS observes). A function
/// that cannot be resolved is treated as one, conservatively.
fn target_has_aggregate(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = select
        .target_list
        .iter()
        .filter_map(|node| node.node.as_ref().map(|inner| inner.to_ref()))
        .collect();
    while let Some(node) = stack.pop() {
        if let NodeRef::FuncCall(call) = node {
            let names: Vec<&str> = call
                .funcname
                .iter()
                .filter_map(|node| match &node.node {
                    Some(Node::String(string)) => Some(string.sval.as_str()),
                    _ => None,
                })
                .collect();
            let (schema, name) = match names.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => return true,
            };
            let procs = ctx.catalog.find_functions(schema, name);
            if procs.is_empty() || procs.iter().any(|proc| proc.prokind == ProKind::Aggregate) {
                return true;
            }
        }
        stack.extend(node.children());
    }
    false
}

/// Whether this `IN` / `ANY` / `ALL` sublink sits in the `WHERE` of an
/// `UPDATE` or `DELETE` at its own query level. Nested `SELECT`s inside the
/// WHERE are other levels, whose sublinks aren't exempt.
fn in_dml_where(ctx: &Ctx<'_>, target: &protobuf::SubLink) -> bool {
    let target: *const protobuf::SubLink = target;
    for (node, _) in ctx.tree.nodes() {
        let where_clause = match node {
            NodeRef::UpdateStmt(update) => update.where_clause.as_deref(),
            NodeRef::DeleteStmt(delete) => delete.where_clause.as_deref(),
            _ => continue,
        };
        let Some(where_clause) = where_clause.and_then(|node| node.node.as_ref()) else {
            continue;
        };
        if where_contains(where_clause.to_ref(), target) {
            return true;
        }
    }
    false
}

/// Whether `target` is a sublink of `node` at this query level (not inside
/// a nested `SELECT`, which is another level).
fn where_contains(node: NodeRef<'_>, target: *const protobuf::SubLink) -> bool {
    match node {
        NodeRef::SelectStmt(_) => false,
        NodeRef::SubLink(sublink) => {
            if std::ptr::eq(sublink, target) {
                return true;
            }
            false
        }
        _ => node
            .children()
            .into_iter()
            .any(|child| where_contains(child, target)),
    }
}

fn has_skip_locked(select: &SelectStmt) -> bool {
    select.locking_clause.iter().any(|node| {
        matches!(&node.node, Some(Node::LockingClause(locking))
            if locking.wait_policy() == LockWaitPolicy::LockWaitSkip)
    })
}

/// Whether the clause is a literal `LIMIT 0` / `FETCH FIRST 0 ROWS`.
fn limit_is_zero(select: &SelectStmt) -> bool {
    matches!(
        select.limit_count.as_deref().and_then(|node| node.node.as_ref()),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(Val::Ival(ref count)) if count.ival == 0)
    )
}

/// Whether every output row carries the same values: the target list reads
/// no column (anywhere, sublinks included) and calls nothing that may vary
/// per row. Whichever rows the plan picks, the output is then the same.
fn constant_target(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    // A set operation's rows come from its arms; there is no target list.
    if select.op() != typedpg_pg_query::protobuf::SetOperation::SetopNone
        || select.target_list.is_empty()
    {
        return false;
    }
    select.target_list.iter().all(|node| {
        node.node
            .as_ref()
            .is_some_and(|_| constant_expression(ctx, node))
    })
}

/// Whether every output row carries the same values: the expression reads
/// no column (anywhere, sublinks included) and calls nothing that may vary
/// per row.
fn constant_expression(ctx: &Ctx<'_>, expression: &protobuf::Node) -> bool {
    let Some(root) = expression.node.as_ref() else {
        return false;
    };
    let mut stack = vec![root.to_ref()];
    while let Some(node) = stack.pop() {
        match node {
            // A star is a column reference too, and any column makes the
            // output depend on the row.
            NodeRef::ColumnRef(_) => return false,
            // An operator may be a volatile user-defined function: give up.
            NodeRef::AExpr(_) => return false,
            NodeRef::FuncCall(call) => {
                let names: Vec<&str> = call
                    .funcname
                    .iter()
                    .filter_map(|node| match &node.node {
                        Some(Node::String(string)) => Some(string.sval.as_str()),
                        _ => None,
                    })
                    .collect();
                let (schema, name) = match names.as_slice() {
                    [name] => (None, *name),
                    [schema, name] => (Some(*schema), *name),
                    _ => return false,
                };
                let procs = ctx.catalog.find_functions(schema, name);
                // Every overload counts, not just the one PG picks: a
                // volatile or set-returning candidate may make each row
                // different even when no column is read.
                if procs.is_empty()
                    || procs
                        .iter()
                        .any(|proc| proc.proretset || proc.provolatile == ProVolatile::Volatile)
                {
                    return false;
                }
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    true
}

/// Whether every output row carries the same values because each target
/// entry is a constant / parameter, or a bare column a top-level `WHERE`
/// conjunct fixes to a parameter or constant through its type's default
/// btree equality under a deterministic collation. Whichever rows a LIMIT
/// or OFFSET picks then project the same values.
fn invariant_target(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    if select.op() != typedpg_pg_query::protobuf::SetOperation::SetopNone
        || select.target_list.is_empty()
        || !select.locking_clause.is_empty()
    {
        return false;
    }
    // A single plain relation keeps the column resolution below obvious;
    // joins, subqueries and functions are refused.
    let [from] = select.from_clause.as_slice() else {
        return false;
    };
    let Some(Node::RangeVar(relation)) = from.node.as_ref() else {
        return false;
    };
    // An alias column list renames the columns; the attname mapping below
    // would then be wrong.
    if relation
        .alias
        .as_ref()
        .is_some_and(|alias| !alias.colnames.is_empty())
    {
        return false;
    }
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    let Some(class) = ctx.catalog.resolve_table(schema, &relation.relname) else {
        return false;
    };
    if !no_volatile_functions(ctx, NodeRef::SelectStmt(select)) {
        return false;
    }
    let mut equated = HashSet::new();
    let mut conjuncts = Vec::new();
    if let Some(where_clause) = select.where_clause.as_deref() {
        and_conjuncts(where_clause, &mut conjuncts);
    }
    for conjunct in conjuncts {
        if let Some(column) = equated_column(ctx, relation, class, conjunct) {
            equated.insert(column);
        }
    }
    select.target_list.iter().all(|node| {
        let Some(Node::ResTarget(target)) = node.node.as_ref() else {
            return false;
        };
        let Some(value) = target.val.as_deref() else {
            return false;
        };
        if constant_expression(ctx, value) {
            return true;
        }
        relation_column(ctx, value, relation, class).is_some_and(|column| equated.contains(&column))
    })
}

/// The bare column `node` names in `relation`, unqualified or qualified by
/// the relation's name or alias.
fn relation_column(
    ctx: &Ctx<'_>,
    node: &protobuf::Node,
    relation: &typedpg_pg_query::protobuf::RangeVar,
    class: &typedpg_analyzer::PgClass,
) -> Option<String> {
    let Node::ColumnRef(reference) = node.node.as_ref()? else {
        return None;
    };
    let (last, qualifiers) = reference.fields.split_last()?;
    let Some(Node::String(column)) = last.node.as_ref() else {
        return None;
    };
    if !ctx
        .catalog
        .attributes_of(class.oid)
        .iter()
        .any(|attribute| attribute.attname == column.sval)
    {
        return None;
    }
    let qualifier = relation
        .alias
        .as_ref()
        .map_or(relation.relname.as_str(), |alias| alias.aliasname.as_str());
    match qualifiers {
        [] => Some(column.sval.clone()),
        [node] if matches!(node.node.as_ref(), Some(Node::String(name)) if name.sval == qualifier) => {
            Some(column.sval.clone())
        }
        _ => None,
    }
}

/// The column a top-level `column = <parameter|constant>` conjunct fixes,
/// when the equality is the column type's default btree one.
fn equated_column(
    ctx: &Ctx<'_>,
    relation: &typedpg_pg_query::protobuf::RangeVar,
    class: &typedpg_analyzer::PgClass,
    node: &protobuf::Node,
) -> Option<String> {
    let Node::AExpr(expr) = node.node.as_ref()? else {
        return None;
    };
    if AExprKind::try_from(expr.kind) != Ok(AExprKind::AexprOp)
        || super::operator_name(expr) != Some("=")
        || !matches!(
            expr.name
                .iter()
                .filter_map(|node| match &node.node {
                    Some(Node::String(name)) => Some(name.sval.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .as_slice(),
            ["="] | ["pg_catalog", "="]
        )
    {
        return None;
    }
    let left = expr.lexpr.as_deref()?;
    let right = expr.rexpr.as_deref()?;
    let (name, other) = match (
        relation_column(ctx, left, relation, class),
        relation_column(ctx, right, relation, class),
    ) {
        (Some(name), _) => (name, right),
        (None, Some(name)) => (name, left),
        (None, None) => return None,
    };
    let attribute = ctx
        .catalog
        .attributes_of(class.oid)
        .iter()
        .find(|attribute| attribute.attname == name)?;
    if !attribute
        .attcollation
        .is_none_or(|collation| matches!(collation.get(), 100 | 950 | 951))
    {
        return None;
    }
    // Default btree equality is identity only for the types whose output
    // function is injective on equal values. Numeric (`1.0 = 1.00`),
    // float (`-0 = 0`), interval (`'1 day' = '24 hours'`), bpchar
    // (`'a' = 'a  '`), jsonb, arrays/ranges/composites of those, and
    // domains over any of them all compare values that print differently.
    if !equality_implies_identical_output(ctx, attribute.atttypid) {
        return None;
    }
    let other_type = match other.node.as_ref()? {
        Node::ParamRef(param) => param_type(ctx, param)?,
        // A literal is coerced to the column's type and fixes the column to
        // one value.
        Node::AConst(_) => attribute.atttypid,
        _ => return None,
    };
    if other_type != attribute.atttypid {
        return None;
    }
    let operator = ctx
        .catalog
        .find_operator("=", Some(attribute.atttypid), other_type)?;
    let default_equality = ctx
        .catalog
        .lint_is_default_btree_equality(attribute.atttypid, other_type, operator.oid)
        // `enum_ops` is a polymorphic btree opfamily: its equality member
        // is declared on `anyenum`, so the exact-type check above misses
        // it. The built-in `enum_eq` still is the type's default equality.
        || operator.code.is_some_and(|code| {
            ctx.catalog.pg_proc().get(&code).is_some_and(|proc| {
                proc.proname == "enum_eq"
                    && ctx.catalog.namespace_name(proc.pronamespace) == Some("pg_catalog")
            })
        });
    default_equality.then_some(name)
}

/// Whether two equal values of type `oid` always print the same: the type's
/// default btree equality must be identity on its output.
///
/// Only the types whose text output is a function of the value alone
/// qualify: integers, `oid`, `bool`, `text` / `varchar` (under the
/// deterministic collations checked by the caller), `uuid`, `date`,
/// `timestamp`, `timestamptz`, `bytea`, enum labels, the internal `"char"`
/// and `name`. Domains are refused: their base type may be one of the
/// non-injective ones, and this check does not look through them.
fn equality_implies_identical_output(ctx: &Ctx<'_>, oid: typedpg_analyzer::PgTypeOid) -> bool {
    let Some(ty) = ctx.catalog.type_row(oid) else {
        return false;
    };
    if ty.typbasetype.is_some() {
        return false;
    }
    if ty.typtype == typedpg_analyzer::TypType::Enum {
        return true;
    }
    ctx.catalog.namespace_name(ty.typnamespace) == Some("pg_catalog")
        && matches!(
            ty.typname.as_str(),
            "int2"
                | "int4"
                | "int8"
                | "oid"
                | "bool"
                | "text"
                | "varchar"
                | "uuid"
                | "date"
                | "timestamp"
                | "timestamptz"
                | "bytea"
                | "char"
                | "name"
        )
}

/// The parameter's resolved type OID.
fn param_type(
    ctx: &Ctx<'_>,
    param: &typedpg_pg_query::protobuf::ParamRef,
) -> Option<typedpg_analyzer::PgTypeOid> {
    let index = usize::try_from(param.number)
        .ok()
        .and_then(|number| number.checked_sub(1))?;
    let analyzed = ctx.view.analyzed.params.get(index)?;
    match &analyzed.pg_type {
        Type::Basic { schema, name, .. } | Type::Domain { schema, name, .. } => ctx
            .catalog
            .resolve_type_by_name(Some(schema), name)
            .map(|ty| ty.oid),
        _ => None,
    }
}

/// The top-level AND conjuncts of `node`.
fn and_conjuncts<'a>(node: &'a protobuf::Node, out: &mut Vec<&'a protobuf::Node>) {
    match node.node.as_ref() {
        Some(Node::BoolExpr(expr))
            if BoolExprType::try_from(expr.boolop) == Ok(BoolExprType::AndExpr) =>
        {
            for arg in &expr.args {
                and_conjuncts(arg, out);
            }
        }
        _ => out.push(node),
    }
}

/// Whether `select` calls a function that may be volatile or return a set:
/// such a call can make every row's target values differ.
fn no_volatile_functions(ctx: &Ctx<'_>, select: NodeRef<'_>) -> bool {
    let mut stack = vec![select];
    while let Some(node) = stack.pop() {
        if let NodeRef::FuncCall(call) = node {
            let names: Vec<&str> = call
                .funcname
                .iter()
                .filter_map(|node| match &node.node {
                    Some(Node::String(string)) => Some(string.sval.as_str()),
                    _ => None,
                })
                .collect();
            let (schema, name) = match names.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => return false,
            };
            let procs = ctx.catalog.find_functions(schema, name);
            if procs.is_empty()
                || procs
                    .iter()
                    .any(|proc| proc.proretset || proc.provolatile == ProVolatile::Volatile)
            {
                return false;
            }
        }
        stack.extend(node.children());
    }
    true
}

/// The `DISTINCT ON (…)` part when it has expressions.
fn distinct_on_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    // Plain `DISTINCT` is a one-NIL list; `DISTINCT ON (…)` parses as the
    // expressions themselves.
    if select.distinct_clause.is_empty() || select.distinct_clause[0].node.is_none() {
        return None;
    }
    let first = select
        .distinct_clause
        .iter()
        .filter_map(|item| {
            item.node
                .as_ref()
                .map(|inner| inner.to_ref())
                .and_then(first_location)
        })
        .min()?;
    let distinct = ctx
        .tokens
        .iter()
        .rposition(|token| token.end <= first && is_keyword(ctx, token, "DISTINCT"))?;
    let on = ctx.tokens.get(distinct + 1)?;
    if !is_keyword(ctx, on, "ON") {
        return None;
    }
    let open = ctx.tokens.get(distinct + 2)?;
    if &ctx.sql[open.start..open.end] != "(" {
        return None;
    }
    let mut depth = 0usize;
    for token in &ctx.tokens[distinct + 2..] {
        match &ctx.sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(ctx.tokens[distinct].start..token.end);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `LIMIT …` / `FETCH …` clause, when there is one (`LIMIT ALL` and
/// `LIMIT NULL` don't pick rows).
fn limit_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let count = select.limit_count.as_deref()?;
    if is_null_literal(count) {
        return None;
    }
    let at = node_start(count)?;
    let index = ctx.tokens.iter().position(|token| token.start == at)?;
    let previous = ctx.tokens.get(index.checked_sub(1)?)?;
    if is_keyword(ctx, previous, "LIMIT") {
        return Some(previous.start..clause_value_end(ctx, at));
    }
    if is_keyword(ctx, previous, "FIRST") || is_keyword(ctx, previous, "NEXT") {
        let fetch = ctx.tokens.get(index.checked_sub(2)?)?;
        if !is_keyword(ctx, fetch, "FETCH") {
            return None;
        }
        return Some(fetch.start..fetch_end(ctx, index, at));
    }
    if is_keyword(ctx, previous, "FETCH") {
        return Some(previous.start..fetch_end(ctx, index, at));
    }
    None
}

/// The end of a `FETCH` clause: its `ONLY`, or the count when it has none.
fn fetch_end(ctx: &Ctx<'_>, index: usize, count_start: usize) -> usize {
    ctx.tokens[index + 1..]
        .iter()
        .find(|token| is_keyword(ctx, token, "ONLY"))
        .map_or_else(|| clause_value_end(ctx, count_start), |token| token.end)
}

/// The `OFFSET …` clause, when there is one that skips rows.
fn offset_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let offset = select.limit_offset.as_deref()?;
    // `OFFSET NULL` is no offset at all, and `OFFSET 0` is the
    // optimization-fence idiom: neither skips rows.
    if is_null_literal(offset)
        || matches!(
            offset.node.as_ref(),
            Some(Node::AConst(value))
                if !value.isnull
                    && matches!(value.val, Some(Val::Ival(ref count)) if count.ival == 0)
        )
    {
        return None;
    }
    let at = node_start(offset)?;
    let index = ctx.tokens.iter().position(|token| token.start == at)?;
    let previous = ctx.tokens.get(index.checked_sub(1)?)?;
    if !is_keyword(ctx, previous, "OFFSET") {
        return None;
    }
    Some(previous.start..clause_value_end(ctx, at))
}

/// Whether the token reads `keyword`, case-insensitively.
fn is_keyword(ctx: &Ctx<'_>, token: &crate::Tok, keyword: &str) -> bool {
    ctx.sql[token.start..token.end].eq_ignore_ascii_case(keyword)
}

/// The first byte a protobuf node covers.
fn node_start(node: &protobuf::Node) -> Option<usize> {
    first_location(node.node.as_ref()?.to_ref())
}

/// The first byte a node covers: the minimum of its location and its
/// descendants' (a cast's location is its `::`, an expression's its
/// operator).
fn first_location(node: NodeRef<'_>) -> Option<usize> {
    let mut first = node
        .location()
        .and_then(|location| usize::try_from(location).ok());
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let Some(location) = node
            .location()
            .and_then(|location| usize::try_from(location).ok())
        {
            first = Some(first.map_or(location, |first| first.min(location)));
        }
        stack.extend(node.children());
    }
    first
}

fn is_null_literal(node: &protobuf::Node) -> bool {
    matches!(&node.node, Some(Node::AConst(value)) if value.isnull)
}

/// The end of the expression starting at `start`: the last token before the
/// next clause keyword at this parenthesis level.
fn clause_value_end(ctx: &Ctx<'_>, start: usize) -> usize {
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
        } else if text == "(" {
            depth += 1;
        } else if seen && depth == 0 && is_clause_boundary(text) {
            break;
        }
        seen = true;
        end = token.end;
    }
    end
}

fn is_clause_boundary(text: &str) -> bool {
    matches!(text, ";" | ",")
        || [
            "LIMIT",
            "OFFSET",
            "FETCH",
            "FOR",
            "UNION",
            "INTERSECT",
            "EXCEPT",
        ]
        .iter()
        .any(|keyword| text.eq_ignore_ascii_case(keyword))
}

#[cfg(test)]
mod tests {
    use crate::{Finding, LintOptions, Rule, Severity, lint_query};
    use typedpg_analyzer::PgCatalog;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id bigint PRIMARY KEY,
                    name text,
                    email text NOT NULL UNIQUE
                 );
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id bigint NOT NULL REFERENCES users
                 );",
            )
            .unwrap();
        catalog
    }

    fn lint(catalog: &PgCatalog, sql: &str) -> Vec<Finding> {
        lint_query(catalog, sql, &LintOptions::default()).unwrap()
    }

    #[test]
    fn a_limit_without_order_is_reported() {
        let db = catalog();
        let sql = "SELECT id FROM users LIMIT 10";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::LimitWithoutOrder);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert_eq!(
            findings[0].message,
            "LIMIT without ORDER BY returns arbitrary rows"
        );
        assert_eq!(
            findings[0].help.as_deref(),
            Some(
                "add an ORDER BY that makes the choice deterministic, or allow it with \
                 -- typedpg-lint: allow(limit_without_order)"
            )
        );
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 10");
        assert!(findings[0].fix.is_empty());
    }

    #[test]
    fn offset_and_distinct_on_without_order_are_reported() {
        let db = catalog();
        let sql = "SELECT id FROM users OFFSET 20";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "OFFSET without ORDER BY skips arbitrary rows"
        );
        assert_eq!(&sql[findings[0].span.clone()], "OFFSET 20");

        let sql = "SELECT DISTINCT ON (name) id FROM users";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].message,
            "DISTINCT ON without ORDER BY keeps an arbitrary row of each group"
        );
        assert_eq!(&sql[findings[0].span.clone()], "DISTINCT ON (name)");
    }

    #[test]
    fn a_set_operations_own_limit_is_reported() {
        let db = catalog();
        let sql = "SELECT id FROM users UNION SELECT user_id FROM posts LIMIT 5";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::LimitWithoutOrder);
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 5");
    }

    #[test]
    fn a_nested_select_is_reported() {
        let db = catalog();
        let sql = "SELECT (SELECT id FROM users LIMIT 1) AS id";
        let findings = lint(&db, sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, Rule::LimitWithoutOrder);
    }

    #[test]
    fn an_order_by_settles_the_choice() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users ORDER BY id LIMIT 10",
            "SELECT id FROM users ORDER BY id OFFSET 10",
            "SELECT DISTINCT ON (name) id FROM users ORDER BY name, id",
        ] {
            assert!(lint(&db, sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn a_query_with_one_row_needs_no_order() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE id = $id LIMIT 10",
            "SELECT count(*) FROM users LIMIT 10",
            "SELECT 1 LIMIT 10",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn exists_and_dml_batching_subqueries_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE EXISTS (SELECT 1 FROM posts LIMIT 5)",
            "UPDATE users SET name = 'x' WHERE id IN (SELECT user_id FROM posts LIMIT 1000)",
            "DELETE FROM users WHERE id IN (SELECT user_id FROM posts LIMIT 1000)",
            "DELETE FROM users WHERE id = ANY (SELECT user_id FROM posts LIMIT 1000)",
            "DELETE FROM users WHERE id > ALL (SELECT user_id FROM posts LIMIT 1000)",
            "SELECT id FROM users LIMIT 5 FOR UPDATE SKIP LOCKED",
            "SELECT id FROM users LIMIT 0",
            "SELECT id FROM users LIMIT ALL",
            "SELECT id FROM users LIMIT NULL",
            "SELECT id FROM users OFFSET NULL",
            "SELECT id FROM users LIMIT NULL OFFSET NULL",
            "SELECT id FROM users WHERE EXISTS \
             (SELECT 1 FROM (SELECT id FROM users LIMIT 1) AS any_row)",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn the_exists_exemption_only_propagates_through_pure_passthrough_selects() {
        let db = catalog();
        // The outer select filters or aggregates the subquery's rows, so
        // which row its LIMIT picks matters again.
        for sql in [
            "SELECT id FROM users WHERE EXISTS \
             (SELECT 1 FROM (SELECT id FROM users LIMIT 1) AS any_row WHERE any_row.id = 5)",
            "SELECT id FROM users WHERE EXISTS \
             (SELECT count(*) FROM (SELECT id FROM users LIMIT 1) AS any_row)",
            "SELECT id FROM users WHERE EXISTS \
             (SELECT 1 FROM (SELECT id FROM users LIMIT 1) AS any_row, posts)",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn an_in_sublink_outside_a_dml_where_is_reported() {
        let db = catalog();
        for sql in [
            "SELECT 1 WHERE 1 IN (SELECT id FROM users LIMIT 1)",
            "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts LIMIT 1000)",
            "SELECT id FROM users WHERE id = ANY (SELECT user_id FROM posts LIMIT 1000)",
            "SELECT id FROM users WHERE id > ALL (SELECT user_id FROM posts LIMIT 1000)",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_constant_target_makes_the_choice_irrelevant() {
        let db = catalog();
        for sql in [
            "SELECT 1 AS one FROM users LIMIT 1",
            "SELECT true FROM users LIMIT 1",
            "SELECT 1 FROM users OFFSET 10",
            // A call that is stable within the statement keeps the output
            // the same too.
            "SELECT now() FROM users LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for sql in [
            "SELECT id FROM users LIMIT 1",
            "SELECT name FROM users OFFSET 10",
            // A set-returning call returns a different value per row.
            "SELECT generate_series(1, 3) FROM users LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}");
        }
    }

    #[test]
    fn the_invariant_projection_only_trusts_injective_equality() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TYPE mood AS ENUM ('happy', 'sad');
             ALTER TABLE users
                 ADD COLUMN amount numeric,
                 ADD COLUMN ratio float8,
                 ADD COLUMN duration interval,
                 ADD COLUMN padded char(3),
                 ADD COLUMN uid uuid,
                 ADD COLUMN born date,
                 ADD COLUMN at timestamp,
                 ADD COLUMN at_tz timestamptz,
                 ADD COLUMN blob bytea,
                 ADD COLUMN feeling mood;",
        )
        .unwrap();
        // Equality is not identity for these: `1.0 = 1.00`, `-0 = 0`,
        // `'1 day' = '24 hours'`, `'a' = 'a  '`, and so on. The projected
        // value can print differently, so the LIMIT still matters.
        for (column, value) in [
            ("amount", "1"),
            ("ratio", "1"),
            ("duration", "'1 day'"),
            ("padded", "'a'"),
        ] {
            let sql = format!("SELECT {column} FROM users WHERE {column} = {value} LIMIT 1");
            let findings: Vec<Finding> = lint(&db, &sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}: {findings:?}");
        }
        // Types whose equality is identity stay exempt.
        for (column, value) in [
            ("uid", "'00000000-0000-0000-0000-000000000000'"),
            ("born", "'2026-11-10'"),
            ("at", "'2026-11-10 12:00:00'"),
            ("at_tz", "'2026-11-10 12:00:00+00'"),
            ("blob", "'\\x00'"),
            ("feeling", "'happy'"),
            ("id", "1"),
            ("name", "'x'"),
        ] {
            let sql = format!("SELECT {column} FROM users WHERE {column} = {value} LIMIT 1");
            let findings: Vec<Finding> = lint(&db, &sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn a_set_returning_function_in_the_intervening_select_keeps_the_report() {
        let mut db = catalog();
        db.apply_sql("CREATE TABLE choices (n integer NOT NULL);")
            .unwrap();
        // Choosing `n = 0` makes generate_series return no row, so which row
        // the inner LIMIT picks decides the EXISTS result.
        let sql = "SELECT EXISTS (SELECT generate_series(1, s.n) \
                   FROM (SELECT n FROM choices LIMIT 1) s)";
        assert!(
            !lint(&db, sql)
                .iter()
                .all(|finding| finding.rule != Rule::LimitWithoutOrder),
            "{sql}"
        );
        // A pure passthrough still propagates the exemption.
        let sql = "SELECT EXISTS (SELECT 1 FROM (SELECT n FROM choices LIMIT 1) s)";
        assert!(
            lint(&db, sql)
                .iter()
                .all(|finding| finding.rule != Rule::LimitWithoutOrder),
            "{sql}"
        );
    }

    #[test]
    fn a_column_fixed_by_an_equality_makes_the_choice_irrelevant() {
        let db = catalog();
        for sql in [
            "SELECT name FROM users WHERE name = $n LIMIT 1",
            "SELECT name FROM users WHERE $n = name LIMIT 1",
            "SELECT name, 1 AS one FROM users WHERE name = $n OFFSET 10",
            "SELECT name FROM users WHERE name = 'x' LIMIT 1",
            "SELECT u.name FROM users u WHERE u.name = $n LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for sql in [
            // A different column still varies with the row.
            "SELECT id FROM users WHERE name = $n LIMIT 1",
            // Not a top-level AND conjunct.
            "SELECT name FROM users WHERE name = $n OR id = 1 LIMIT 1",
            // The equality is with another column, not a constant.
            "SELECT name FROM users WHERE name = email LIMIT 1",
            // A volatile call anywhere in the select keeps the rule.
            "SELECT name FROM users WHERE name = $n AND random() < 1 LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn an_offset_zero_is_not_reported() {
        let db = catalog();
        assert!(
            lint(&db, "SELECT id FROM users OFFSET 0")
                .into_iter()
                .all(|finding| finding.rule != Rule::LimitWithoutOrder)
        );
        // Next to a LIMIT, the LIMIT is still reported.
        let sql = "SELECT id FROM users LIMIT 1 OFFSET 0";
        let findings: Vec<Finding> = lint(&db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
            .collect();
        assert_eq!(findings.len(), 1);
        assert_eq!(&sql[findings[0].span.clone()], "LIMIT 1");
    }

    #[test]
    fn an_allow_comment_suppresses_the_rule() {
        let db = catalog();
        let sql = "-- typedpg-lint: allow(limit_without_order)\nSELECT id FROM users LIMIT 10";
        assert!(lint(&db, sql).is_empty());
    }
}
