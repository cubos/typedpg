//! The `limit_without_order` rule: reports a `LIMIT`, an `OFFSET` or a
//! `DISTINCT ON` without an `ORDER BY`, which picks arbitrary rows (and, for
//! `DISTINCT ON`, an arbitrary row per group).
//!
//! Not reported when the choice doesn't matter: a query proven to return at
//! most one row ([`typedpg_analyzer::select_at_most_one_row`]; for `DISTINCT
//! ON`, to have at most one row before it), a select list that is the same
//! for every row (constants, or columns `WHERE` fixes), a subquery
//! of `EXISTS` (existence doesn't care which rows), an `IN` / `= ANY` /
//! `ALL` in the `WHERE` of an `UPDATE` or `DELETE` (the batching idiom), a
//! `LIMIT 0`, or a locking clause with `SKIP LOCKED` (the queue pattern
//! wants any unlocked row).

use std::collections::HashSet;

use typedpg_analyzer::{ProKind, ProVolatile, Type, select_at_most_one_row};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{
    self, AConstValue, AExprKind, BoolExprType, LockWaitPolicy, Node, SelectStmt, SubLinkType,
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
        if picks_from_at_most_one_row(ctx, select) {
            continue;
        }
        // DISTINCT ON may still pick from several rows when it returns one;
        // LIMIT and OFFSET apply to what it returns.
        let one_row = select_at_most_one_row(ctx.catalog, ctx.view, select);
        if let Some(span) = distinct_on_span(ctx, select) {
            out.push(RawFinding {
                span,
                message: "DISTINCT ON without ORDER BY keeps an arbitrary row of each group"
                    .to_owned(),
                help: Some(HELP.to_owned()),
                fix: Vec::new(),
            });
        }
        if one_row {
            continue;
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

/// Whether `select` has at most one row to pick from
/// ([`select_at_most_one_row`]). With `DISTINCT ON` that is the rows
/// before it: a `DISTINCT ON` whose keys `WHERE` fixes returns one row, but
/// still an arbitrary one of the group's rows.
pub(super) fn picks_from_at_most_one_row(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    if !matches!(select.distinct_clause.first(), Some(Some(_))) {
        return select_at_most_one_row(ctx.catalog, ctx.view, select);
    }
    // A copy is judged without the levels around it, which only withholds
    // proofs that need an outer column.
    let mut rows = select.clone();
    rows.distinct_clause.clear();
    select_at_most_one_row(ctx.catalog, ctx.view, &rows)
}

/// The selects whose arbitrary row choice doesn't matter: the subquery of
/// an `EXISTS` (existence doesn't care which rows), or an `IN` / `= ANY` /
/// `ALL` in the `WHERE` of an `UPDATE` or `DELETE` — the batching idiom
/// (`DELETE … WHERE id IN (SELECT … LIMIT 1000)`) deliberately takes any
/// batch. An `IN` elsewhere (a `SELECT`'s WHERE, a projection) does care:
/// the limit changes which values the outer query sees.
pub(super) fn batched_subqueries(ctx: &Ctx<'_>) -> HashSet<*const SelectStmt> {
    let mut batched = HashSet::new();
    // The EXISTS exemption propagates through FROM subqueries an
    // existence-only select reads; the DML batching exemption does not (the
    // IN list's values matter there).
    let mut existence = HashSet::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::SubLink(sublink) = node else {
            continue;
        };
        match sublink.sub_link_type {
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
            let Some(Node::RangeSubselect(range)) =
                select.from_clause.first().and_then(|node| node.as_ref())
            else {
                continue;
            };
            let Some(Node::SelectStmt(subquery)) = range.subquery.as_ref() else {
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

fn insert_sublink_select(batched: &mut HashSet<*const SelectStmt>, sublink: &ast::SubLink) {
    if let Some(Node::SelectStmt(select)) = sublink.subselect.as_ref() {
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
    select.op == typedpg_pg_query::ast::SetOperation::SetopNone
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
    let mut stack: Vec<NodeRef<'_>> = NodeRef::SelectStmt(select).children().collect();
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
                .filter_map(|node| match node {
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
        .filter_map(|node| node.as_ref().map(|inner| inner.to_ref()))
        .collect();
    while let Some(node) = stack.pop() {
        if let NodeRef::FuncCall(call) = node {
            let names: Vec<&str> = call
                .funcname
                .iter()
                .filter_map(|node| match node {
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
fn in_dml_where(ctx: &Ctx<'_>, target: &ast::SubLink) -> bool {
    let target: *const ast::SubLink = target;
    for (node, _) in ctx.tree.nodes() {
        let where_clause = match node {
            NodeRef::UpdateStmt(update) => update.where_clause.as_ref(),
            NodeRef::DeleteStmt(delete) => delete.where_clause.as_ref(),
            _ => continue,
        };
        let Some(where_clause) = where_clause else {
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
fn where_contains(node: NodeRef<'_>, target: *const ast::SubLink) -> bool {
    match node {
        NodeRef::SelectStmt(_) => false,
        NodeRef::SubLink(sublink) => std::ptr::eq(sublink, target),
        _ => node
            .children()
            .into_iter()
            .any(|child| where_contains(child, target)),
    }
}

pub(super) fn has_skip_locked(select: &SelectStmt) -> bool {
    select.locking_clause.iter().any(|node| {
        matches!(node, Some(Node::LockingClause(locking))
            if locking.wait_policy == LockWaitPolicy::LockWaitSkip)
    })
}

/// Whether the clause is a literal `LIMIT 0` / `FETCH FIRST 0 ROWS`.
pub(super) fn limit_is_zero(select: &SelectStmt) -> bool {
    matches!(
        select.limit_count.as_ref(),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(AConstValue::Integer(ref count)) if count.ival == 0)
    )
}

/// Whether every output row carries the same values: the target list reads
/// no column (anywhere, sublinks included) and calls nothing that may vary
/// per row. Whichever rows the plan picks, the output is then the same.
pub(super) fn constant_target(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    // A set operation's rows come from its arms; there is no target list.
    if select.op != typedpg_pg_query::ast::SetOperation::SetopNone || select.target_list.is_empty()
    {
        return false;
    }
    select.target_list.iter().all(|node| {
        node.as_ref()
            .is_some_and(|_| constant_expression(ctx, node.as_ref()))
    })
}

/// Whether every output row carries the same values: the expression reads
/// no column (anywhere, sublinks included) and calls nothing that may vary
/// per row — no function any of whose overloads is VOLATILE or returns a
/// set, no operator that resolved to one (or whose operands' implicit
/// coercions run one), no explicit cast through a VOLATILE cast function.
/// `1 + 1`, `now()` and `'a' || 'b'` are the same on every row.
fn constant_expression(ctx: &Ctx<'_>, expression: Option<&ast::Node>) -> bool {
    let Some(root) = expression else {
        return false;
    };
    let mut stack = vec![root.to_ref()];
    while let Some(node) = stack.pop() {
        match node {
            // A star is a column reference too, and any column makes the
            // output depend on the row.
            NodeRef::ColumnRef(_) => return false,
            // The operator the analyzer resolved there: a user-defined one
            // may run a volatile function. Not resolved (a kind the
            // analyzer doesn't record): give up.
            NodeRef::AExpr(expr) => {
                if ctx.view.operator_is_nonvolatile(expr.location) != Some(true) {
                    return false;
                }
            }
            NodeRef::TypeCast(cast) => {
                if let Some(arg) = cast.arg.as_ref()
                    && let Some(conversion) = ctx.view.value_conversion(arg.to_ref())
                    && cast_may_vary(ctx, conversion.from, conversion.to)
                {
                    return false;
                }
            }
            NodeRef::FuncCall(call) => {
                let names: Vec<&str> = call
                    .funcname
                    .iter()
                    .filter_map(|node| match node {
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

/// Whether the cast from `from` to `to` runs a cast function that is
/// VOLATILE or returns a set.
fn cast_may_vary(
    ctx: &Ctx<'_>,
    from: typedpg_analyzer::PgTypeOid,
    to: typedpg_analyzer::PgTypeOid,
) -> bool {
    ctx.catalog
        .iter_casts()
        .find(|cast| cast.castsource == from && cast.casttarget == to)
        .and_then(|cast| cast.castfunc)
        .is_some_and(|function| {
            ctx.catalog
                .pg_proc()
                .get(&function)
                .is_none_or(|proc| proc.proretset || proc.provolatile == ProVolatile::Volatile)
        })
}

/// Whether every output row carries the same values because each target
/// entry is a constant / parameter, or a bare column of a table of the
/// `FROM` clause that a top-level `WHERE` conjunct fixes to a parameter or
/// constant through its type's default btree equality under a
/// deterministic collation. Whichever rows a LIMIT or OFFSET picks then
/// project the same values. Joins don't change that: they only multiply
/// or drop rows, and every row they leave has the fixed values.
pub(super) fn invariant_target(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    if select.op != typedpg_pg_query::ast::SetOperation::SetopNone
        || select.target_list.is_empty()
        || !select.locking_clause.is_empty()
    {
        return false;
    }
    let mut relations = Vec::new();
    for item in &select.from_clause {
        from_relations(ctx, item.as_ref(), &mut relations);
    }
    if relations.is_empty() {
        return false;
    }
    if !no_volatile_functions(ctx, NodeRef::SelectStmt(select)) {
        return false;
    }
    let mut equated = HashSet::new();
    let mut conjuncts = Vec::new();
    if select.where_clause.is_some() {
        and_conjuncts(select.where_clause.as_ref(), &mut conjuncts);
    }
    for conjunct in conjuncts {
        if let Some(column) = equated_column(ctx, &relations, conjunct) {
            equated.insert(column);
        }
    }
    select.target_list.iter().all(|node| {
        let Some(Node::ResTarget(target)) = node.as_ref() else {
            return false;
        };
        let value = &target.val;
        if value.is_none() {
            return false;
        }
        if constant_expression(ctx, value.as_ref()) {
            return true;
        }
        relation_column(ctx, value.as_ref(), &relations)
            .is_some_and(|column| equated.contains(&column))
    })
}

/// A plain table of a `FROM` clause, as the analyzer resolved it.
struct FromRelation<'a> {
    range: &'a typedpg_pg_query::ast::RangeVar,
    class: &'a typedpg_analyzer::PgClass,
}

impl FromRelation<'_> {
    /// The name a column reference qualifies it with.
    fn refname(&self) -> &str {
        self.range
            .alias
            .as_ref()
            .map_or(self.range.relname.as_str(), |alias| {
                alias.aliasname.as_str()
            })
    }
}

/// The plain tables of the `FROM` item `node`, through its joins. Any other
/// item (a subquery, a function, an aliased join) contributes none: a
/// reference to its columns then resolves to no table.
fn from_relations<'a>(ctx: &Ctx<'a>, node: Option<&'a ast::Node>, out: &mut Vec<FromRelation<'a>>) {
    match node {
        Some(Node::RangeVar(range)) => {
            // An alias column list renames the columns; the attname mapping
            // below would then be wrong.
            if range
                .alias
                .as_ref()
                .is_some_and(|alias| !alias.colnames.is_empty())
            {
                return;
            }
            // The relation as the analyzer resolved it (a `WITH` query of
            // the same name is none).
            let Some(typedpg_analyzer::RelationRef::Relation(oid)) = ctx.view.relation_of(range)
            else {
                return;
            };
            if let Some(class) = ctx.catalog.class_row(oid) {
                out.push(FromRelation { range, class });
            }
        }
        Some(Node::JoinExpr(join)) if join.alias.is_none() => {
            from_relations(ctx, join.larg.as_ref(), out);
            from_relations(ctx, join.rarg.as_ref(), out);
        }
        _ => {}
    }
}

/// The bare column `node` names — by its position in `relations` and its
/// name — in one of `relations`: the table the analyzer bound it to at this
/// query level, or, when only one table is in `relations` and no binding
/// was recorded, that table's column unqualified or qualified by its name
/// or alias.
fn relation_column(
    ctx: &Ctx<'_>,
    node: Option<&ast::Node>,
    relations: &[FromRelation<'_>],
) -> Option<(usize, String)> {
    let Node::ColumnRef(reference) = node? else {
        return None;
    };
    let (last, qualifiers) = reference.fields.split_last()?;
    let Some(Node::String(column)) = last.as_ref() else {
        return None;
    };
    let index = match ctx.view.column_ref_binding(reference.location) {
        Some(binding) => {
            if binding.outer {
                return None;
            }
            let source = binding.source.as_deref()?;
            let mut found = relations
                .iter()
                .enumerate()
                .filter(|(_, relation)| relation.refname() == source);
            match (found.next(), found.next()) {
                (Some((index, _)), None) => index,
                _ => return None,
            }
        }
        None => {
            let [relation] = relations else {
                return None;
            };
            match qualifiers {
                [] => {}
                [node] if matches!(node.as_ref(), Some(Node::String(name)) if name.sval == relation.refname()) =>
                    {}
                _ => return None,
            }
            0
        }
    };
    ctx.catalog
        .attributes_of(relations[index].class.oid)
        .iter()
        .any(|attribute| attribute.attname == column.sval)
        .then(|| (index, column.sval.clone()))
}

/// The column a top-level `column = <parameter|constant>` conjunct fixes,
/// when the equality is the column type's default btree one.
fn equated_column(
    ctx: &Ctx<'_>,
    relations: &[FromRelation<'_>],
    node: Option<&ast::Node>,
) -> Option<(usize, String)> {
    let Node::AExpr(expr) = node? else {
        return None;
    };
    if expr.kind != AExprKind::AexprOp
        || super::operator_name(expr) != Some("=")
        || !matches!(
            expr.name
                .iter()
                .filter_map(|node| match node {
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
    let (left, right) = (&expr.lexpr, &expr.rexpr);
    if left.is_none() || right.is_none() {
        return None;
    }
    let (column, other) = match (
        relation_column(ctx, left.as_ref(), relations),
        relation_column(ctx, right.as_ref(), relations),
    ) {
        (Some(column), _) => (column, right),
        (None, Some(column)) => (column, left),
        (None, None) => return None,
    };
    let class = relations[column.0].class;
    let name = &column.1;
    let attribute = ctx
        .catalog
        .attributes_of(class.oid)
        .iter()
        .find(|attribute| attribute.attname == *name)?;
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
    let other_type = match other.as_ref()? {
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
    default_equality.then_some(column)
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
    param: &typedpg_pg_query::ast::ParamRef,
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
fn and_conjuncts<'a>(node: Option<&'a ast::Node>, out: &mut Vec<Option<&'a ast::Node>>) {
    match node {
        Some(Node::BoolExpr(expr)) if expr.boolop == BoolExprType::AndExpr => {
            for arg in &expr.args {
                and_conjuncts(arg.as_ref(), out);
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
                .filter_map(|node| match node {
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
pub(super) fn distinct_on_span(
    ctx: &Ctx<'_>,
    select: &SelectStmt,
) -> Option<std::ops::Range<usize>> {
    // Plain `DISTINCT` is a one-NIL list; `DISTINCT ON (…)` parses as the
    // expressions themselves.
    if select.distinct_clause.is_empty() || select.distinct_clause[0].is_none() {
        return None;
    }
    let first = select
        .distinct_clause
        .iter()
        .filter_map(|item| {
            item.as_ref()
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
pub(super) fn limit_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let count = select.limit_count.as_ref()?;
    if is_null_literal(count) {
        return None;
    }
    let Some(at) = node_start(count) else {
        return implicit_fetch(ctx, select).map(|fetch| fetch.start..fetch.end);
    };
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

/// A `FETCH FIRST ROW ONLY` clause written without a row count.
pub(super) struct ImplicitFetch {
    /// The `FETCH` keyword's first byte.
    pub start: usize,
    /// The clause's last byte (its `ONLY`, or `TIES` of `WITH TIES`).
    pub end: usize,
}

/// The `FETCH FIRST | NEXT ROW | ROWS ONLY` (or `WITH TIES`) clause of
/// `select` when it has no row count. PostgreSQL stores the implicit 1
/// without a location (`makeIntConst(1, -1)` in gram.y), so the clause is
/// read from the tokens: the first `FETCH` after everything else `select`
/// covers, outside any parenthesis opened after that point (a subquery of
/// an `OFFSET`). It may lie after the parenthesis closing `select`: the
/// limit of `(SELECT …) FETCH FIRST ROW ONLY` is the inner select's.
pub(super) fn implicit_fetch(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<ImplicitFetch> {
    if !matches!(select.limit_count.as_ref(), Some(count) if node_start(count).is_none()) {
        return None;
    }
    let last = covered_end(ctx, select)?;
    let mut depth = 0isize;
    let mut fetch = None;
    for (index, token) in ctx.tokens.iter().enumerate() {
        if token.start <= last {
            continue;
        }
        match &ctx.sql[token.start..token.end] {
            "(" => depth += 1,
            ")" => depth -= 1,
            _ if depth <= 0 && is_keyword(ctx, token, "FETCH") => {
                fetch = Some(index);
                break;
            }
            _ => {}
        }
    }
    let fetch = fetch?;
    let words = ctx.tokens.get(fetch + 1..fetch + 4)?;
    let reads = |i: usize, options: &[&str]| options.iter().any(|w| is_keyword(ctx, &words[i], w));
    if !reads(0, &["FIRST", "NEXT"]) || !reads(1, &["ROW", "ROWS"]) {
        return None;
    }
    let end = if reads(2, &["ONLY"]) {
        words[2].end
    } else if reads(2, &["WITH"]) {
        let ties = ctx.tokens.get(fetch + 4)?;
        if !is_keyword(ctx, ties, "TIES") {
            return None;
        }
        ties.end
    } else {
        return None;
    };
    Some(ImplicitFetch {
        start: ctx.tokens[fetch].start,
        end,
    })
}

/// The last byte position `select` covers apart from its limit and offset:
/// the greatest location of its other nodes, or the end of a nested
/// select's own count-less `FETCH` (which has no location either).
fn covered_end(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<usize> {
    let mut body = select.clone();
    body.limit_count = None;
    body.limit_offset = None;
    let mut last: Option<usize> = None;
    let mut stack = vec![NodeRef::SelectStmt(&body)];
    let mut root = true;
    while let Some(node) = stack.pop() {
        let mut here = node
            .location()
            .and_then(|location| usize::try_from(location).ok());
        if let NodeRef::SelectStmt(nested) = node
            && !std::mem::take(&mut root)
            && let Some(fetch) = implicit_fetch(ctx, nested)
        {
            here = Some(here.map_or(fetch.end, |here| here.max(fetch.end)));
        }
        if let Some(here) = here {
            last = Some(last.map_or(here, |last| last.max(here)));
        }
        stack.extend(node.children());
    }
    last
}

/// The end of a `FETCH` clause: its `ONLY`, or the count when it has none.
fn fetch_end(ctx: &Ctx<'_>, index: usize, count_start: usize) -> usize {
    ctx.tokens[index + 1..]
        .iter()
        .find(|token| is_keyword(ctx, token, "ONLY"))
        .map_or_else(|| clause_value_end(ctx, count_start), |token| token.end)
}

/// The `OFFSET …` clause, when there is one that skips rows.
pub(super) fn offset_span(ctx: &Ctx<'_>, select: &SelectStmt) -> Option<std::ops::Range<usize>> {
    let offset = select.limit_offset.as_ref()?;
    // `OFFSET NULL` is no offset at all, and `OFFSET 0` is the
    // optimization-fence idiom: neither skips rows.
    if is_null_literal(offset)
        || matches!(
            offset,
            Node::AConst(value)
                if !value.isnull
                    && matches!(value.val, Some(AConstValue::Integer(ref count)) if count.ival == 0)
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

/// The first byte an AST node covers.
fn node_start(node: &ast::Node) -> Option<usize> {
    first_location(node.to_ref())
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

fn is_null_literal(node: &ast::Node) -> bool {
    matches!(node, Node::AConst(value) if value.isnull)
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
        assert_eq!(findings[0].severity, Severity::Error);
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
    fn a_fetch_without_a_count_is_a_limit_of_one() {
        let db = catalog();
        // PG stores the implicit 1 with no location: the clause is read
        // from the tokens, wherever it is written.
        for (sql, clause) in [
            (
                "SELECT id FROM users FETCH FIRST ROW ONLY",
                "FETCH FIRST ROW ONLY",
            ),
            (
                "SELECT id FROM users fetch next row only",
                "fetch next row only",
            ),
            (
                "SELECT id FROM users FETCH FIRST ROWS ONLY",
                "FETCH FIRST ROWS ONLY",
            ),
            (
                "SELECT id FROM users FETCH NEXT ROWS ONLY OFFSET 2",
                "FETCH NEXT ROWS ONLY",
            ),
            (
                "SELECT id FROM users OFFSET (SELECT 1 FROM posts FETCH FIRST 1 ROW ONLY) \
                 ROWS FETCH FIRST ROW ONLY",
                "FETCH FIRST ROW ONLY",
            ),
            (
                "(SELECT id FROM users) FETCH FIRST ROW ONLY",
                "FETCH FIRST ROW ONLY",
            ),
            (
                "SELECT id FROM users UNION SELECT user_id FROM posts FETCH FIRST ROW ONLY",
                "FETCH FIRST ROW ONLY",
            ),
            (
                "SELECT (SELECT id FROM users FETCH FIRST ROW ONLY) AS id",
                "FETCH FIRST ROW ONLY",
            ),
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|f| f.rule == Rule::LimitWithoutOrder)
                .filter(|f| f.message.starts_with("LIMIT"))
                .collect();
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], clause, "{sql}");
        }
        // Each select's own clause: the nested one's inside its parentheses.
        let sql = "SELECT * FROM (SELECT id FROM users FETCH FIRST ROW ONLY) AS u, \
                   (SELECT id FROM posts) AS p FETCH NEXT ROW ONLY";
        let spans: Vec<&str> = lint(&db, sql)
            .into_iter()
            .filter(|f| f.rule == Rule::LimitWithoutOrder)
            .map(|f| &sql[f.span])
            .collect();
        assert_eq!(spans, ["FETCH FIRST ROW ONLY", "FETCH NEXT ROW ONLY"]);
        let sql = "SELECT id FROM users UNION (SELECT user_id FROM posts FETCH FIRST ROW ONLY) \
                   FETCH NEXT ROW ONLY";
        let mut spans: Vec<&str> = lint(&db, sql)
            .into_iter()
            .filter(|f| f.rule == Rule::LimitWithoutOrder)
            .map(|f| &sql[f.span])
            .collect();
        spans.sort_unstable();
        assert_eq!(spans, ["FETCH FIRST ROW ONLY", "FETCH NEXT ROW ONLY"]);
        // One row: nothing to pick from, as with LIMIT 1.
        assert!(
            lint(
                &db,
                "SELECT id FROM users WHERE id = $1 FETCH FIRST ROW ONLY"
            )
            .iter()
            .all(|f| f.rule != Rule::LimitWithoutOrder)
        );
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
    fn distinct_on_picking_from_one_group_is_reported() {
        let db = catalog();
        // WHERE fixes the DISTINCT ON key: one row comes back, but which of
        // the group's rows is arbitrary. The LIMIT, past DISTINCT ON, then
        // picks from one row.
        for sql in [
            "SELECT DISTINCT ON (name) id FROM users WHERE name = $1",
            "SELECT DISTINCT ON (name) id FROM users WHERE name = $1 LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], "DISTINCT ON (name)");
        }
        // One row before DISTINCT ON.
        let sql = "SELECT DISTINCT ON (name) id FROM users WHERE id = $1";
        assert!(lint(&db, sql).is_empty(), "{sql}");
    }

    #[test]
    fn output_fixed_by_where_needs_no_order_through_joins() {
        let db = catalog();
        for sql in [
            "SELECT g.name FROM users g JOIN users m ON m.id = g.id WHERE g.name = 'x' LIMIT 1",
            "SELECT g.name, m.email FROM users g, users m \
             WHERE g.name = $1 AND m.email = $2 LIMIT 1",
            "SELECT name FROM users g JOIN posts p ON p.user_id = g.id WHERE name = $1 LIMIT 1",
            "SELECT g.name FROM users g LEFT JOIN posts p ON p.user_id = g.id \
             WHERE g.name = $1 LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for sql in [
            // The other table's column isn't fixed.
            "SELECT m.name FROM users g JOIN users m ON m.id <> g.id WHERE g.name = 'x' LIMIT 1",
            // Fixed only in a LEFT JOIN's ON.
            "SELECT p.id FROM users g LEFT JOIN posts p ON p.user_id = g.id AND p.id = $1 \
             LIMIT 1",
            // A join's merged USING column.
            "SELECT id FROM users JOIN posts USING (id) WHERE id = $1 OR id = $2 LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
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
            // So do operators over constants.
            "SELECT 1 + 1 FROM users LIMIT 1",
            "SELECT 'a' || 'b', 2 * 3 = 6 FROM users LIMIT 1",
            "SELECT now() - interval '1 day' FROM users LIMIT 1",
            "SELECT 1 = ANY (ARRAY[1, 2]) FROM users OFFSET 1",
            "SELECT 1 IN (1, 2), 2 BETWEEN 1 AND 3 FROM users LIMIT 1",
            "SELECT $1::int + 1 FROM users LIMIT 1",
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
            // A column under an operator.
            "SELECT id + 1 FROM users LIMIT 1",
            "SELECT 1 + random() FROM users LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}");
        }
    }

    #[test]
    fn a_volatile_operator_or_cast_keeps_the_report() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE FUNCTION jitter(int, int) RETURNS int VOLATILE LANGUAGE sql
                 AS 'SELECT $1 + $2 + (random() * 10)::int';
             CREATE OPERATOR <+> (LEFTARG = int, RIGHTARG = int, FUNCTION = jitter);
             CREATE FUNCTION steady(int, int) RETURNS int IMMUTABLE LANGUAGE sql
                 AS 'SELECT $1 + $2';
             CREATE OPERATOR ### (LEFTARG = int, RIGHTARG = int, FUNCTION = steady);
             CREATE TYPE token AS (v int);
             CREATE FUNCTION token_of(int) RETURNS token VOLATILE LANGUAGE sql
                 AS 'SELECT ROW((random() * $1)::int)::token';
             CREATE CAST (int AS token) WITH FUNCTION token_of(int);",
        )
        .unwrap();
        for sql in [
            "SELECT 1 <+> 2 FROM users LIMIT 1",
            "SELECT 1::token FROM users LIMIT 1",
        ] {
            let findings: Vec<Finding> = lint(&db, sql)
                .into_iter()
                .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
                .collect();
            assert!(!findings.is_empty(), "{sql}");
        }
        let sql = "SELECT 1 ### 2 FROM users LIMIT 1";
        let findings: Vec<Finding> = lint(&db, sql)
            .into_iter()
            .filter(|finding| finding.rule == Rule::LimitWithoutOrder)
            .collect();
        assert!(findings.is_empty(), "{sql}: {findings:?}");
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
