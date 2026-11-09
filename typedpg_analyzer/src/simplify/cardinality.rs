//! Query cardinality proofs for the lint rules that need them: whether a
//! `SELECT` (or a whole statement) provably yields at most one row, and
//! which plain `DISTINCT` keywords are redundant.
//!
//! Unlike the simplification rules, these prove nothing is *rewritten*:
//! [`redundant_distinct_spans`] only reports the spans the `distinct`
//! simplification would delete, and [`select_at_most_one_row`] backs the
//! `redundant_limit` rule (and, later, `fetch_one` checks). A `false`
//! answer is always sound, so every case this can't prove gives up.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::protobuf::{
    self, CommonTableExpr, DeleteStmt, InsertStmt, LimitOption, MergeStmt, ReturningClause,
    SelectStmt, SetOperation, UpdateStmt, WithClause, a_const::Val, node::Node,
};

use super::Ctx;
use super::dedup::{self, CteColumn, CteEnv, CteInfo, Fields, PredicateSources, Scope};
use super::proof::FromScope;
use crate::pg_catalog::{PgCatalog, ProKind, QueryView, oid};

/// The ranges in `view.sql` of the plain `DISTINCT` keywords the `distinct`
/// simplification proves redundant (exactly the spans it would delete).
///
/// Empty for a query with spreads: their sample rows are not the query.
pub fn redundant_distinct_spans(
    catalog: &PgCatalog,
    view: &QueryView,
) -> Vec<std::ops::Range<usize>> {
    if !view.analyzed.spreads.is_empty() {
        return Vec::new();
    }
    let ctx = Ctx {
        sql: &view.sql,
        tree: &view.tree,
        catalog,
        analyzed: &view.analyzed,
    };
    dedup::distinct_candidates(&ctx)
        .into_iter()
        .flat_map(|candidate| candidate.edits)
        .map(|edit| edit.range)
        .collect()
}

/// Whether `select`, a node borrowed from `view.tree` at any nesting depth,
/// provably yields at most one row per evaluation.
///
/// Its own `LIMIT` / `OFFSET` / `FETCH` are ignored (they only remove rows).
/// At most one row when:
///
/// - it is not a set operation, and no set-returning function can multiply
///   its target list, `VALUES` rows or `FROM` functions;
/// - it has no `FROM` (and a bare `VALUES` has exactly one row);
/// - it has an aggregate or `HAVING` without `GROUP BY` (one group);
/// - every `FROM` relation is a base table whose unique key is equated,
///   column by column, to constants, parameters, outer-query columns,
///   scalar subqueries uncorrelated with this level or columns of
///   already-determined relations — the `distinct` rule's "equalities to a
///   key" proof (see [`super::dedup`]), propagated until every relation is
///   determined — a single-row CTE, or a `FROM` function that can't return
///   a set (see below).
///
/// A CTE the statement's own `WITH` clause proves yields at most one row is
/// a relation with no key needed: it is determined from the start, may be
/// referenced more than once, and its columns are values a key equality may
/// read. CTEs are judged in declaration order, so a body may use the ones
/// before it; a recursive `WITH`, a body that isn't proven one-row or a
/// reference to such a CTE makes the proof give up. A `FROM` function call
/// every overload of which is non-set-returning also yields exactly one row
/// (`ROWS FROM` of several such calls and `WITH ORDINALITY` included).
///
/// A volatile call only matters as an equality's value operand, which
/// decides which row a key matches: [`super::dedup`] refuses a volatile one.
/// Volatility anywhere else — the target list, `ORDER BY`, another `WHERE`
/// conjunct, a DML's `SET` or `RETURNING` — leaves the row count alone. A
/// locking clause is handled on its own (it needs a single relation).
///
/// Anything else is `false`: `GROUP BY` always is (a group per key), and so
/// is every relation the proof can't see through (a view, a subquery, a
/// function that returns a set, an unproven CTE, an inheritance parent, a
/// partitioned table or one under row security), a set-returning call, or an
/// outer join whose nullable side isn't determined.
pub fn select_at_most_one_row(catalog: &PgCatalog, view: &QueryView, select: &SelectStmt) -> bool {
    if !view.analyzed.spreads.is_empty() {
        return false;
    }
    let ctx = Ctx {
        sql: &view.sql,
        tree: &view.tree,
        catalog,
        analyzed: &view.analyzed,
    };
    let mut found = None;
    let env = CteEnv::default();
    for raw in &view.tree.stmts {
        if let Some(root) = raw.stmt.as_deref().and_then(|node| node.node.as_ref()) {
            find_select(&ctx, root.to_ref(), None, &env, select, &mut found);
            if found.is_some() {
                break;
            }
        }
    }
    // A node from another tree: judge it without any outer level, with every
    // CTE name in this tree a reference the proof gives up on (the node may
    // be inside one, shadowing a table of the same name).
    found.unwrap_or_else(|| {
        let env = CteEnv::blocked(&dedup::cte_names(&view.tree));
        select_at_most_one_row_in(&ctx, select, &FromScope::opaque(None), &env)
    })
}

/// Whether the top-level statement of `view` provably yields at most one
/// row, counting its own `LIMIT` / `FETCH FIRST`.
///
/// A `SELECT` is at most one row when [`select_at_most_one_row`] proves it
/// or its `LIMIT` / `FETCH FIRST` is the constant `0` or `1` (without
/// `WITH TIES`). An `INSERT ... VALUES` with exactly one row is, whatever
/// its `ON CONFLICT`; an `INSERT ... SELECT` is when its select is. An
/// `UPDATE` / `DELETE` is when its target and every `FROM` / `USING`
/// relation are all determined by its `WHERE` equalities. Everything else —
/// `MERGE`, `INSERT ... DEFAULT VALUES`, a statement the proof can't see
/// through — is `false`.
///
/// The statement's own `WITH` clause provides its single-row CTEs, judged in
/// declaration order (see [`select_at_most_one_row`]).
pub fn statement_at_most_one_row(catalog: &PgCatalog, view: &QueryView) -> bool {
    if !view.analyzed.spreads.is_empty() {
        return false;
    }
    let ctx = Ctx {
        sql: &view.sql,
        tree: &view.tree,
        catalog,
        analyzed: &view.analyzed,
    };
    let Some(statement) = view
        .tree
        .stmts
        .first()
        .and_then(|raw| raw.stmt.as_deref())
        .and_then(|node| node.node.as_ref())
    else {
        return false;
    };
    statement_at_most_one_row_in(&ctx, statement.to_ref(), &CteEnv::default())
}

// ── CTEs ────────────────────────────────────────────────────────────────────

/// The CTEs a statement's `WITH` clause makes visible: the environment
/// after it, and the one each body is judged in (the ones declared before
/// it).
struct VisibleCtes {
    after: CteEnv,
    bodies: Vec<CteEnv>,
}

/// Extend `env` with `with`'s CTEs, judging them in declaration order. A
/// recursive `WITH` (or a CTE that references itself) contributes no
/// single-row CTE.
fn visible_ctes(ctx: &Ctx<'_>, env: &CteEnv, with: Option<&WithClause>) -> VisibleCtes {
    let mut after = env.clone();
    let mut bodies = Vec::new();
    let Some(with) = with else {
        return VisibleCtes { after, bodies };
    };
    for node in &with.ctes {
        let Some(Node::CommonTableExpr(cte)) = &node.node else {
            continue;
        };
        if with.recursive {
            // A recursive CTE may reference itself (and every CTE before
            // it), so the whole level is a reference the proof gives up on.
            after.insert(cte.ctename.clone(), None);
            bodies.push(after.clone());
            continue;
        }
        bodies.push(after.clone());
        let info = single_row_cte(ctx, cte, &after);
        after.insert(cte.ctename.clone(), info);
    }
    VisibleCtes { after, bodies }
}

/// Whether `cte` is a single-row CTE: a non-recursive body that, with its
/// `RETURNING` clause for a data-modifying statement, yields at most one
/// row by [`statement_at_most_one_row`]'s rules.
fn single_row_cte(ctx: &Ctx<'_>, cte: &CommonTableExpr, env: &CteEnv) -> Option<CteInfo> {
    if cte.cterecursive || cte.search_clause.is_some() || cte.cycle_clause.is_some() {
        return None;
    }
    let body = cte
        .ctequery
        .as_deref()
        .and_then(|node| node.node.as_ref())?
        .to_ref();
    let returns_rows = match body {
        NodeRef::SelectStmt(_) => true,
        NodeRef::InsertStmt(insert) => insert.returning_clause.is_some(),
        NodeRef::UpdateStmt(update) => update.returning_clause.is_some(),
        NodeRef::DeleteStmt(delete) => delete.returning_clause.is_some(),
        _ => false,
    };
    if !returns_rows || !statement_at_most_one_row_in(ctx, body, env) {
        return None;
    }
    let mut columns =
        cte_output_columns(ctx, body, &visible_ctes(ctx, env, with_clause(body)).after);
    for (column, alias) in columns.iter_mut().zip(&cte.aliascolnames) {
        let Some(Node::String(name)) = &alias.node else {
            return None;
        };
        column.name = name.sval.clone();
    }
    if cte.aliascolnames.len() > columns.len() {
        return None;
    }
    Some(CteInfo { columns })
}

/// The WITH clause a statement node carries.
fn with_clause(node: NodeRef<'_>) -> Option<&WithClause> {
    match node {
        NodeRef::SelectStmt(statement) => statement.with_clause.as_ref(),
        NodeRef::InsertStmt(statement) => statement.with_clause.as_ref(),
        NodeRef::UpdateStmt(statement) => statement.with_clause.as_ref(),
        NodeRef::DeleteStmt(statement) => statement.with_clause.as_ref(),
        NodeRef::MergeStmt(statement) => statement.with_clause.as_ref(),
        _ => None,
    }
}

/// The columns a CTE body's `RETURNING` clause or target list yields.
fn cte_output_columns(ctx: &Ctx<'_>, body: NodeRef<'_>, env: &CteEnv) -> Vec<CteColumn> {
    match body {
        NodeRef::SelectStmt(select) => select_output_columns(ctx, select, env),
        NodeRef::InsertStmt(insert) => returning_columns(
            ctx,
            insert.relation.as_ref(),
            insert.returning_clause.as_ref(),
            env,
        ),
        NodeRef::UpdateStmt(update) => returning_columns(
            ctx,
            update.relation.as_ref(),
            update.returning_clause.as_ref(),
            env,
        ),
        NodeRef::DeleteStmt(delete) => returning_columns(
            ctx,
            delete.relation.as_ref(),
            delete.returning_clause.as_ref(),
            env,
        ),
        _ => Vec::new(),
    }
}

fn select_output_columns(ctx: &Ctx<'_>, select: &SelectStmt, env: &CteEnv) -> Vec<CteColumn> {
    if select.op() != SetOperation::SetopNone {
        let (Some(left), Some(right)) = (select.larg.as_deref(), select.rarg.as_deref()) else {
            return Vec::new();
        };
        let mut columns = select_output_columns(ctx, left, env);
        let other = select_output_columns(ctx, right, env);
        if columns.len() != other.len() {
            return Vec::new();
        }
        for (column, other) in columns.iter_mut().zip(&other) {
            if column.typ != other.typ || !column.deterministic || !other.deterministic {
                column.typ = None;
                column.deterministic = false;
            }
        }
        return columns;
    }
    if let Some(Node::List(first)) = select
        .values_lists
        .first()
        .and_then(|node| node.node.as_ref())
    {
        return first
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let (typ, deterministic) = value_type(ctx, None, item);
                CteColumn {
                    name: format!("column{}", i + 1),
                    typ,
                    deterministic,
                }
            })
            .collect();
    }
    let scope = Scope::build_env(ctx, env, &select.from_clause);
    let mut out = Vec::new();
    for target in &select.target_list {
        let Some(Node::ResTarget(target)) = &target.node else {
            continue;
        };
        let Some(value) = target.val.as_deref() else {
            continue;
        };
        if let Some(Node::ColumnRef(column)) = &value.node
            && let Fields::Star(qualifier) = dedup::column_ref_fields(column)
        {
            let Some(scope) = scope.as_ref() else {
                continue;
            };
            let rels: Vec<usize> = match qualifier {
                // A bare `*` merges USING columns: leave it out then.
                None if scope.joins.iter().any(|join| !join.using.is_empty()) => Vec::new(),
                None => (0..scope.rels.len()).collect(),
                Some(qualifier) => scope.rel_named(&qualifier).into_iter().collect(),
            };
            for r in rels {
                for (name, attnum) in &scope.rels[r].columns {
                    let (typ, deterministic) = scope.rels[r]
                        .column_info(ctx, *attnum)
                        .map_or((None, false), |(typ, deterministic)| {
                            (Some(typ), deterministic)
                        });
                    out.push(CteColumn {
                        name: name.clone(),
                        typ,
                        deterministic,
                    });
                }
            }
            continue;
        }
        let (typ, deterministic) = value_type(ctx, scope.as_ref(), value);
        out.push(CteColumn {
            name: if target.name.is_empty() {
                output_name(value)
            } else {
                target.name.clone()
            },
            typ,
            deterministic,
        });
    }
    out
}

fn returning_columns(
    ctx: &Ctx<'_>,
    relation: Option<&protobuf::RangeVar>,
    returning: Option<&ReturningClause>,
    env: &CteEnv,
) -> Vec<CteColumn> {
    let (Some(relation), Some(returning)) = (relation, returning) else {
        return Vec::new();
    };
    let Some(scope) = Scope::dml(ctx, env, relation, &[]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for target in &returning.exprs {
        let Some(Node::ResTarget(target)) = &target.node else {
            continue;
        };
        let Some(value) = target.val.as_deref() else {
            continue;
        };
        if let Some(Node::ColumnRef(column)) = &value.node
            && let Fields::Star(_) = dedup::column_ref_fields(column)
        {
            for (name, attnum) in &scope.rels[0].columns {
                let (typ, deterministic) = scope.rels[0]
                    .column_info(ctx, *attnum)
                    .map_or((None, false), |(typ, deterministic)| {
                        (Some(typ), deterministic)
                    });
                out.push(CteColumn {
                    name: name.clone(),
                    typ,
                    deterministic,
                });
            }
            continue;
        }
        let (typ, deterministic) = value_type(ctx, Some(&scope), value);
        out.push(CteColumn {
            name: if target.name.is_empty() {
                output_name(value)
            } else {
                target.name.clone()
            },
            typ,
            deterministic,
        });
    }
    out
}

/// The type of a value a CTE's output exposes, when it is a column, a
/// parameter, a constant or a cast of one.
fn value_type(
    ctx: &Ctx<'_>,
    scope: Option<&Scope<'_>>,
    value: &protobuf::Node,
) -> (Option<crate::oid::PgTypeOid>, bool) {
    match value.node.as_ref() {
        Some(Node::ColumnRef(column)) => {
            let (Some(scope), Fields::Names(names)) = (scope, dedup::column_ref_fields(column))
            else {
                return (None, false);
            };
            match scope.resolve(&names) {
                dedup::Col::Rel(r, attnum) => scope.rels[r]
                    .column_info(ctx, attnum)
                    .map_or((None, false), |(typ, deterministic)| {
                        (Some(typ), deterministic)
                    }),
                dedup::Col::Other => (None, false),
            }
        }
        Some(Node::ParamRef(param)) => usize::try_from(param.number)
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| ctx.analyzed.params.get(index))
            .map_or((None, false), |param| {
                (
                    dedup::type_oid(ctx, &param.pg_type),
                    dedup::deterministic_type_collation(&param.pg_type),
                )
            }),
        Some(Node::AConst(constant)) if !constant.isnull => match constant.val.as_ref() {
            Some(Val::Ival(_)) => (Some(oid::INT4), true),
            Some(Val::Sval(_)) => (Some(oid::TEXT), true),
            Some(Val::Boolval(_)) => (Some(oid::BOOL), true),
            _ => (None, false),
        },
        Some(Node::TypeCast(cast)) => {
            let names = cast
                .type_name
                .as_ref()
                .map(|name| string_list(&name.names))
                .unwrap_or_default();
            let typ = match names.as_slice() {
                [name] => ctx.catalog.resolve_type_by_name(None, name),
                [schema, name] => ctx.catalog.resolve_type_by_name(Some(schema), name),
                _ => None,
            };
            (typ.map(|typ| typ.oid), false)
        }
        _ => (None, false),
    }
}

/// The column name PG gives a target that has none written.
fn output_name(value: &protobuf::Node) -> String {
    match value.node.as_ref() {
        Some(Node::ColumnRef(column)) => match dedup::column_ref_fields(column) {
            Fields::Names(names) => names
                .last()
                .map_or_else(|| "?column?".into(), |n| (*n).into()),
            _ => "?column?".into(),
        },
        Some(Node::FuncCall(call)) => string_list(&call.funcname)
            .last()
            .map_or_else(|| "?column?".into(), |n| (*n).into()),
        _ => "?column?".into(),
    }
}

fn string_list(nodes: &[protobuf::Node]) -> Vec<&str> {
    nodes
        .iter()
        .filter_map(|node| match &node.node {
            Some(Node::String(string)) => Some(string.sval.as_str()),
            _ => None,
        })
        .collect()
}

// ── Finding the node's outer scopes ─────────────────────────────────────────

/// Search `node`'s subtree for `target`, giving each nested query level the
/// [`FromScope`]s and CTEs it is evaluated in. Once found, run the proof
/// with that scope and store the answer in `out`.
///
/// The scopes may include items a subquery can't actually see without
/// `LATERAL`; that only matters for queries PG rejects, which never reach
/// here (the view analyzed).
fn find_select(
    ctx: &Ctx<'_>,
    node: NodeRef<'_>,
    parent: Option<&FromScope<'_>>,
    env: &CteEnv,
    target: &SelectStmt,
    out: &mut Option<bool>,
) {
    if out.is_some() {
        return;
    }
    match node {
        NodeRef::SelectStmt(select) => {
            let visible = visible_ctes(ctx, env, select.with_clause.as_ref());
            if std::ptr::eq(select, target) {
                let scope = FromScope::of_select(ctx, select, parent);
                *out = Some(select_at_most_one_row_in(
                    ctx,
                    select,
                    &scope,
                    &visible.after,
                ));
                return;
            }
            find_cte_bodies(
                ctx,
                select.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                out,
            );
            if select.op() != SetOperation::SetopNone {
                for arm in [select.larg.as_deref(), select.rarg.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    find_select(
                        ctx,
                        NodeRef::SelectStmt(arm),
                        parent,
                        &visible.after,
                        target,
                        out,
                    );
                }
                return;
            }
            let scope = FromScope::of_select(ctx, select, parent);
            for child in select_children(select) {
                find_select(ctx, child, Some(&scope), &visible.after, target, out);
            }
        }
        NodeRef::InsertStmt(insert) => {
            let visible = visible_ctes(ctx, env, insert.with_clause.as_ref());
            find_cte_bodies(
                ctx,
                insert.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                out,
            );
            let scope = FromScope::of_dml(ctx, insert.relation.as_ref(), &[], parent);
            for child in insert_children(insert) {
                find_select(ctx, child, Some(&scope), &visible.after, target, out);
            }
        }
        NodeRef::UpdateStmt(update) => {
            let visible = visible_ctes(ctx, env, update.with_clause.as_ref());
            find_cte_bodies(
                ctx,
                update.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                out,
            );
            let scope =
                FromScope::of_dml(ctx, update.relation.as_ref(), &update.from_clause, parent);
            for child in update_children(update) {
                find_select(ctx, child, Some(&scope), &visible.after, target, out);
            }
        }
        NodeRef::DeleteStmt(delete) => {
            let visible = visible_ctes(ctx, env, delete.with_clause.as_ref());
            find_cte_bodies(
                ctx,
                delete.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                out,
            );
            let scope =
                FromScope::of_dml(ctx, delete.relation.as_ref(), &delete.using_clause, parent);
            for child in delete_children(delete) {
                find_select(ctx, child, Some(&scope), &visible.after, target, out);
            }
        }
        NodeRef::MergeStmt(merge) => {
            let visible = visible_ctes(ctx, env, merge.with_clause.as_ref());
            find_cte_bodies(
                ctx,
                merge.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                out,
            );
            let source: &[protobuf::Node] = merge
                .source_relation
                .as_deref()
                .map(std::slice::from_ref)
                .unwrap_or_default();
            let scope = FromScope::of_dml(ctx, merge.relation.as_ref(), source, parent);
            for child in merge_children(merge) {
                find_select(ctx, child, Some(&scope), &visible.after, target, out);
            }
        }
        _ => {
            for child in node.children() {
                find_select(ctx, child, parent, env, target, out);
            }
        }
    }
}

/// Search each CTE body in the environment of the CTEs before it.
fn find_cte_bodies(
    ctx: &Ctx<'_>,
    with: Option<&WithClause>,
    bodies: &[CteEnv],
    parent: Option<&FromScope<'_>>,
    target: &SelectStmt,
    out: &mut Option<bool>,
) {
    let Some(with) = with else {
        return;
    };
    for (node, env) in with.ctes.iter().zip(bodies) {
        if let Some(Node::CommonTableExpr(cte)) = &node.node
            && let Some(body) = cte.ctequery.as_deref().and_then(|n| n.node.as_ref())
        {
            find_select(ctx, body.to_ref(), parent, env, target, out);
        }
    }
}

/// A SELECT's direct children, excluding its own WITH clause.
fn select_children(select: &SelectStmt) -> Vec<NodeRef<'_>> {
    let mut out = Vec::new();
    for list in [
        &select.target_list,
        &select.from_clause,
        &select.group_clause,
        &select.window_clause,
        &select.values_lists,
        &select.sort_clause,
        &select.distinct_clause,
        &select.locking_clause,
    ] {
        out.extend(refs(list));
    }
    for node in [
        &select.where_clause,
        &select.having_clause,
        &select.limit_count,
        &select.limit_offset,
    ] {
        if let Some(node) = node.as_deref().and_then(|node| node.node.as_ref()) {
            out.push(node.to_ref());
        }
    }
    out
}

fn insert_children(insert: &InsertStmt) -> Vec<NodeRef<'_>> {
    let mut out: Vec<NodeRef<'_>> = refs(&insert.cols).collect();
    if let Some(node) = insert
        .select_stmt
        .as_deref()
        .and_then(|node| node.node.as_ref())
    {
        out.push(node.to_ref());
    }
    if let Some(conflict) = &insert.on_conflict_clause {
        out.push(NodeRef::OnConflictClause(conflict));
    }
    if let Some(returning) = &insert.returning_clause {
        out.push(NodeRef::ReturningClause(returning));
    }
    out
}

fn update_children(update: &UpdateStmt) -> Vec<NodeRef<'_>> {
    let mut out: Vec<NodeRef<'_>> = refs(&update.target_list).collect();
    out.extend(refs(&update.from_clause));
    if let Some(node) = update
        .where_clause
        .as_deref()
        .and_then(|node| node.node.as_ref())
    {
        out.push(node.to_ref());
    }
    if let Some(returning) = &update.returning_clause {
        out.push(NodeRef::ReturningClause(returning));
    }
    out
}

fn delete_children(delete: &DeleteStmt) -> Vec<NodeRef<'_>> {
    let mut out: Vec<NodeRef<'_>> = refs(&delete.using_clause).collect();
    if let Some(node) = delete
        .where_clause
        .as_deref()
        .and_then(|node| node.node.as_ref())
    {
        out.push(node.to_ref());
    }
    if let Some(returning) = &delete.returning_clause {
        out.push(NodeRef::ReturningClause(returning));
    }
    out
}

fn merge_children(merge: &MergeStmt) -> Vec<NodeRef<'_>> {
    let mut out: Vec<NodeRef<'_>> = refs(&merge.merge_when_clauses).collect();
    for node in [&merge.source_relation, &merge.join_condition] {
        if let Some(node) = node.as_deref().and_then(|node| node.node.as_ref()) {
            out.push(node.to_ref());
        }
    }
    if let Some(returning) = &merge.returning_clause {
        out.push(NodeRef::ReturningClause(returning));
    }
    out
}

// ── The SELECT proof ────────────────────────────────────────────────────────

fn statement_at_most_one_row_in(ctx: &Ctx<'_>, statement: NodeRef<'_>, env: &CteEnv) -> bool {
    let env = visible_ctes(ctx, env, with_clause(statement)).after;
    match statement {
        NodeRef::SelectStmt(select) => {
            select_at_most_one_row_in(ctx, select, &FromScope::opaque(None), &env)
                || limit_bounds_one(select)
        }
        NodeRef::InsertStmt(insert) => insert_at_most_one_row(ctx, insert, &env),
        NodeRef::UpdateStmt(update) => dml_at_most_one_row(
            ctx,
            &env,
            update.relation.as_ref(),
            &update.from_clause,
            update.where_clause.as_deref(),
        ),
        NodeRef::DeleteStmt(delete) => dml_at_most_one_row(
            ctx,
            &env,
            delete.relation.as_ref(),
            &delete.using_clause,
            delete.where_clause.as_deref(),
        ),
        _ => false,
    }
}

fn select_at_most_one_row_in(
    ctx: &Ctx<'_>,
    select: &SelectStmt,
    outer: &FromScope<'_>,
    env: &CteEnv,
) -> bool {
    if select.op() != SetOperation::SetopNone || select.into_clause.is_some() {
        return false;
    }
    if select.group_distinct || !select.group_clause.is_empty() {
        return false;
    }
    // A set-returning call multiplies this level's rows wherever it runs:
    // the target list, a `FROM` function, an operator. A volatile call
    // elsewhere (a target, ORDER BY, another WHERE conjunct, a DML's SET or
    // RETURNING) doesn't: only an equality's value operand decides which row
    // a key matches, and [`dedup::Scope::cardinality_facts`] refuses a
    // volatile one.
    if has_set_returning(ctx, select) {
        return false;
    }
    if !select.values_lists.is_empty() {
        return select.from_clause.is_empty() && select.values_lists.len() == 1;
    }
    if select.from_clause.is_empty() {
        return true;
    }
    if select.having_clause.is_some() || has_own_aggregate(ctx, select) {
        // One group, and nothing groups it further.
        return true;
    }
    let Some(scope) = Scope::build_env(ctx, env, &select.from_clause) else {
        return false;
    };
    // A locking clause makes EvalPlanQual re-fetch a row's newest version
    // and recheck the WHERE on it. Over one relation the scan still returns
    // at most the one row whose key equals the value — and `SELECT … WHERE
    // id = $1 FOR UPDATE` is what `fetch_one` locks rows with. With joins,
    // the re-fetched row is re-joined, and may match several rows on the
    // other side.
    if !select.locking_clause.is_empty() && scope.rels.len() != 1 {
        return false;
    }
    all_relations_determined(ctx, select, &scope, outer)
}

/// The [`NodeRef`]s of a list of protobuf nodes, skipping empty slots.
fn refs(nodes: &[protobuf::Node]) -> impl Iterator<Item = NodeRef<'_>> {
    nodes
        .iter()
        .filter_map(|node| node.node.as_ref().map(|inner| inner.to_ref()))
}

/// Whether a call in `select`'s own level can multiply rows: a
/// set-returning function or operator in the target list, the `VALUES`
/// rows, a `FROM` function or any other expression. Nested queries have
/// their own rows and are skipped.
fn has_set_returning(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = Vec::new();
    stack.extend(refs(&select.target_list));
    stack.extend(refs(&select.values_lists));
    stack.extend(refs(&select.from_clause));
    stack.extend(refs(&select.group_clause));
    stack.extend(refs(&select.window_clause));
    stack.extend(refs(&select.sort_clause));
    stack.extend(refs(&select.distinct_clause));
    for node in [
        &select.where_clause,
        &select.having_clause,
        &select.limit_count,
        &select.limit_offset,
    ] {
        if let Some(node) = node.as_deref().and_then(|node| node.node.as_ref()) {
            stack.push(node.to_ref());
        }
    }
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::SubLink(_) | NodeRef::RangeSubselect(_) | NodeRef::CommonTableExpr(_) => {
                continue;
            }
            NodeRef::FuncCall(call) => {
                let Some((schema, name)) = function_name(&call.funcname) else {
                    return true;
                };
                let procs = ctx.catalog.find_functions(schema, name);
                if procs.is_empty() || procs.iter().any(|proc| proc.proretset) {
                    return true;
                }
            }
            NodeRef::AExpr(expr) => {
                if let Some(name) = expr.name.iter().rev().find_map(|node| match &node.node {
                    Some(Node::String(string)) => Some(string.sval.as_str()),
                    _ => None,
                }) && ctx.catalog.pg_operator.values().any(|operator| {
                    operator.oprname == name
                        && operator
                            .oprcode
                            .and_then(|code| ctx.catalog.pg_proc.get(&code))
                            .is_none_or(|proc| proc.proretset)
                }) {
                    return true;
                }
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    false
}

/// Whether `select`'s own expressions call an aggregate (a window function
/// over one doesn't count: `over` is set).
fn has_own_aggregate(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut stack: Vec<NodeRef<'_>> = Vec::new();
    for list in [
        &select.target_list,
        &select.group_clause,
        &select.sort_clause,
        &select.distinct_clause,
        &select.window_clause,
        &select.values_lists,
    ] {
        stack.extend(refs(list));
    }
    for node in [
        &select.having_clause,
        &select.limit_count,
        &select.limit_offset,
    ] {
        if let Some(node) = node.as_deref().and_then(|node| node.node.as_ref()) {
            stack.push(node.to_ref());
        }
    }
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::SubLink(_) | NodeRef::RangeSubselect(_) | NodeRef::CommonTableExpr(_) => {
                continue;
            }
            NodeRef::FuncCall(call) if call.over.is_none() => {
                let Some((schema, name)) = function_name(&call.funcname) else {
                    continue;
                };
                let procs = ctx.catalog.find_functions(schema, name);
                // Only when every overload is an aggregate: a same-named
                // scalar function would not collapse the rows.
                if !procs.is_empty() && procs.iter().all(|proc| proc.prokind == ProKind::Aggregate)
                {
                    return true;
                }
            }
            _ => {}
        }
        stack.extend(node.children());
    }
    false
}

/// The relations a relation-independent fixed point starts with: every
/// single-row CTE reference is already known to contribute at most one row.
fn starting_relations(scope: &Scope<'_>) -> u64 {
    let mut known = 0u64;
    for (r, rel) in scope.rels.iter().enumerate() {
        if rel.single_row {
            known |= 1 << r;
        }
    }
    known
}

/// The relations of `scope` all determined: a relation is determined when
/// one of its unique keys is equated to constants, parameters, outer-query
/// columns or already-determined relations. `WHERE` facts hold on every
/// output row; a join's `ON` facts hold only on the rows it matches, so
/// they may determine an inner join's sides and an outer join's nullable
/// side, never its preserved one. A key may be equated partly by the
/// `WHERE` and partly by a join's `ON`: both hold on every row the join
/// contributes.
fn all_relations_determined(
    ctx: &Ctx<'_>,
    select: &SelectStmt,
    scope: &Scope<'_>,
    outer: &FromScope<'_>,
) -> bool {
    let n = scope.rels.len();
    if n == 0 || n > 63 {
        return false;
    }
    let all: u64 = (1u64 << n) - 1;
    let where_conjuncts = dedup::conjuncts(select.where_clause.as_deref());
    let where_facts: Vec<dedup::EqFact> = where_conjuncts
        .iter()
        .flat_map(|conjunct| scope.cardinality_facts(ctx, conjunct, Some(outer)))
        .collect();
    let join_facts: Vec<(&dedup::Join<'_>, Vec<dedup::EqFact>, Vec<&protobuf::Node>)> = scope
        .joins
        .iter()
        .map(|join| {
            let conjuncts = dedup::conjuncts(join.quals);
            let mut facts = Vec::new();
            for conjunct in &conjuncts {
                facts.extend(scope.cardinality_facts(ctx, conjunct, Some(outer)));
            }
            for name in &join.using {
                facts.extend(scope.cardinality_using_facts(ctx, name, join.left, join.right));
            }
            (join, facts, conjuncts)
        })
        .collect();
    let mut known = starting_relations(scope);
    loop {
        let before = known;
        for b in 0..n {
            if known & (1 << b) != 0 {
                continue;
            }
            let sources = PredicateSources {
                conjuncts: &where_conjuncts,
                facts: &where_facts,
            };
            if scope
                .key_equated_with(ctx, b, &where_facts, known, Some(&sources))
                .is_some()
            {
                known |= 1 << b;
            }
        }
        for (join, facts, conjuncts) in &join_facts {
            let (targets, allowed): (u64, u64) = match join.kind {
                typedpg_pg_query::protobuf::JoinType::JoinInner => {
                    (join.rels() & !known, known & join.rels())
                }
                typedpg_pg_query::protobuf::JoinType::JoinLeft => {
                    (join.right_rel.map_or(0, |b| 1 << b), known & join.left)
                }
                typedpg_pg_query::protobuf::JoinType::JoinRight => {
                    (join.left_rel.map_or(0, |b| 1 << b), known & join.right)
                }
                // Nothing an ON says bounds a FULL join's unmatched rows.
                _ => (0, 0),
            };
            if targets == 0 {
                continue;
            }
            let mut merged = facts.clone();
            merged.extend(where_facts.iter().cloned());
            let mut sources = conjuncts.clone();
            sources.extend(where_conjuncts.iter().copied());
            let sources = PredicateSources {
                conjuncts: &sources,
                facts: &merged,
            };
            for b in 0..n {
                if targets & (1 << b) == 0 || known & (1 << b) != 0 {
                    continue;
                }
                let allowed = allowed & !(1 << b);
                if scope
                    .key_equated_with(ctx, b, &merged, allowed, Some(&sources))
                    .is_some()
                {
                    known |= 1 << b;
                }
            }
        }
        if known == before {
            break;
        }
    }
    known & all == all
}

// ── Statements ──────────────────────────────────────────────────────────────

/// `INSERT ... VALUES` with one row, or `INSERT ... SELECT` whose select is
/// at most one row.
fn insert_at_most_one_row(ctx: &Ctx<'_>, insert: &InsertStmt, env: &CteEnv) -> bool {
    let Some(node) = insert
        .select_stmt
        .as_deref()
        .and_then(|node| node.node.as_ref())
    else {
        return false;
    };
    let NodeRef::SelectStmt(select) = node.to_ref() else {
        return false;
    };
    select_at_most_one_row_in(ctx, select, &FromScope::opaque(None), env)
        || limit_bounds_one(select)
}

/// An `UPDATE ... FROM` / `DELETE ... USING` whose target and every
/// `FROM` / `USING` relation are determined by the same `WHERE` fixed
/// point, the target counting as one more relation.
fn dml_at_most_one_row(
    ctx: &Ctx<'_>,
    env: &CteEnv,
    relation: Option<&protobuf::RangeVar>,
    from: &[protobuf::Node],
    where_clause: Option<&protobuf::Node>,
) -> bool {
    let Some(relation) = relation else {
        return false;
    };
    let Some(scope) = Scope::dml(ctx, env, relation, from) else {
        return false;
    };
    let n = scope.rels.len();
    if n == 0 || n > 63 {
        return false;
    }
    let all: u64 = (1u64 << n) - 1;
    let conjuncts = dedup::conjuncts(where_clause);
    let facts: Vec<dedup::EqFact> = conjuncts
        .iter()
        .flat_map(|conjunct| scope.cardinality_facts(ctx, conjunct, None))
        .collect();
    let sources = PredicateSources {
        conjuncts: &conjuncts,
        facts: &facts,
    };
    let mut known = starting_relations(&scope);
    loop {
        let before = known;
        for b in 0..n {
            if known & (1 << b) == 0
                && scope
                    .key_equated_with(ctx, b, &facts, known, Some(&sources))
                    .is_some()
            {
                known |= 1 << b;
            }
        }
        if known == before {
            break;
        }
    }
    known & all == all
}

/// Whether `select`'s `LIMIT` / `FETCH FIRST` is the constant `0` or `1`
/// (without `WITH TIES`). Its `OFFSET` doesn't affect the bound.
fn limit_bounds_one(select: &SelectStmt) -> bool {
    if select.limit_option() == LimitOption::WithTies {
        return false;
    }
    matches!(
        select.limit_count.as_deref().and_then(|node| node.node.as_ref()),
        Some(Node::AConst(value))
            if !value.isnull
                && matches!(value.val, Some(Val::Ival(count)) if count.ival == 0 || count.ival == 1)
    )
}

/// The last name of a function call, with its schema when written.
fn function_name(nodes: &[protobuf::Node]) -> Option<(Option<&str>, &str)> {
    let names: Vec<&str> = nodes
        .iter()
        .filter_map(|node| match &node.node {
            Some(Node::String(string)) => Some(string.sval.as_str()),
            _ => None,
        })
        .collect();
    match names.as_slice() {
        [name] => Some((None, *name)),
        [schema, name] => Some((Some(*schema), *name)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> PgCatalog {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE orgs (id bigint PRIMARY KEY, name text NOT NULL);
             CREATE TABLE users (
                id bigint PRIMARY KEY,
                org_id bigint REFERENCES orgs,
                name text,
                email text NOT NULL UNIQUE
             );
             CREATE TABLE memberships (
                user_id bigint NOT NULL,
                team_id bigint NOT NULL,
                PRIMARY KEY (user_id, team_id)
             );
             CREATE TABLE lines (a int NOT NULL, b int NOT NULL, UNIQUE (a, b));
             CREATE TABLE customers (id bigint PRIMARY KEY);
             CREATE TABLE orders (
                id bigint PRIMARY KEY,
                customer_id bigint NOT NULL REFERENCES customers
             );
             CREATE TABLE defaults (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                v int DEFAULT 1 NOT NULL
             );
             CREATE TABLE partials (id int PRIMARY KEY, code text NOT NULL);
             CREATE UNIQUE INDEX ON partials (code) WHERE id > 0;
             CREATE TABLE deferred (
                code text NOT NULL UNIQUE DEFERRABLE INITIALLY IMMEDIATE
             );
             CREATE COLLATION ci (
                provider = icu, locale = 'und-u-ks-level2', deterministic = false
             );
             CREATE TABLE nondeterministic (
                id int PRIMARY KEY, code text COLLATE ci NOT NULL UNIQUE
             );
             CREATE TABLE secured (id int PRIMARY KEY);
             ALTER TABLE secured ENABLE ROW LEVEL SECURITY;
             CREATE TABLE parent (id int PRIMARY KEY);
             CREATE TABLE child () INHERITS (parent);
             CREATE TABLE partitioned (id int PRIMARY KEY) PARTITION BY RANGE (id);
             CREATE VIEW v AS SELECT id, name FROM users;
             CREATE TABLE conversations (
                id bigint PRIMARY KEY,
                next_seq bigint NOT NULL DEFAULT 1,
                tenant_id bigint NOT NULL,
                user_id bigint,
                updated_at timestamptz
             );
             CREATE TYPE conversation_event_type AS ENUM ('user_message', 'agent_message');
             CREATE TABLE events (
                tenant_id bigint NOT NULL,
                conversation_id bigint NOT NULL,
                seq bigint NOT NULL,
                type conversation_event_type,
                content text,
                created_at timestamptz,
                PRIMARY KEY (tenant_id, conversation_id, seq)
             );
             CREATE TABLE memory (
                id bigint,
                tenant_id bigint NOT NULL,
                user_id bigint NOT NULL,
                PRIMARY KEY (tenant_id, user_id)
             );
             CREATE TABLE things (
                id bigint PRIMARY KEY,
                other_id bigint,
                name text
             );
             CREATE TABLE other (id bigint PRIMARY KEY, note text);
             CREATE TABLE jobs (
                tenant_id bigint NOT NULL,
                model_id bigint NOT NULL,
                state text NOT NULL,
                PRIMARY KEY (tenant_id, model_id)
             );
             CREATE TABLE tool_calls (
                conversation_id bigint NOT NULL,
                tool_call_id bigint NOT NULL,
                type text NOT NULL
             );
             CREATE UNIQUE INDEX ON tool_calls (conversation_id, tool_call_id)
                WHERE type = 'tool_result';
             CREATE TABLE lanes (
                channel_id bigint NOT NULL,
                identity_id bigint NOT NULL,
                key text NOT NULL,
                archived_at timestamptz
             );
             CREATE UNIQUE INDEX ON lanes (channel_id, identity_id, key)
                WHERE archived_at IS NULL AND channel_id IS NOT NULL AND key IS NOT NULL;
             CREATE TABLE any_codes (code text NOT NULL, kind text NOT NULL);
             CREATE UNIQUE INDEX ON any_codes (code) WHERE kind IN ('a', 'b');
             CREATE UNIQUE INDEX ON any_codes (code) WHERE kind = ANY (ARRAY['c', 'd']);
             CREATE TABLE not_in_codes (conversation_id bigint NOT NULL, state text NOT NULL);
             CREATE UNIQUE INDEX ON not_in_codes (conversation_id)
                WHERE state NOT IN ('finished', 'abandoned');
             CREATE TABLE not_in_all_codes (conversation_id bigint NOT NULL, state text NOT NULL);
             CREATE UNIQUE INDEX ON not_in_all_codes (conversation_id)
                WHERE state <> ALL (ARRAY['finished', 'abandoned']);
             CREATE TABLE flags (code text NOT NULL, hidden boolean NOT NULL);
             CREATE UNIQUE INDEX ON flags (code) WHERE hidden;
             CREATE UNIQUE INDEX ON flags (code) WHERE NOT hidden;",
        )
        .unwrap();
        db
    }

    fn view(db: &PgCatalog, sql: &str) -> QueryView {
        db.query_view(sql).unwrap()
    }

    fn top(view: &QueryView) -> &SelectStmt {
        match view
            .tree
            .stmts
            .first()
            .and_then(|raw| raw.stmt.as_deref())
            .and_then(|node| node.node.as_ref())
        {
            Some(Node::SelectStmt(select)) => select,
            other => panic!("not a SELECT: {other:?}"),
        }
    }

    #[track_caller]
    fn select_one(db: &PgCatalog, sql: &str) -> bool {
        let view = view(db, sql);
        select_at_most_one_row(db, &view, top(&view))
    }

    #[track_caller]
    fn statement_one(db: &PgCatalog, sql: &str) -> bool {
        let view = view(db, sql);
        statement_at_most_one_row(db, &view)
    }

    #[test]
    fn a_primary_key_equality_is_one_row() {
        let db = catalog();
        assert!(select_one(&db, "SELECT id, name FROM users WHERE id = $1"));
        assert!(select_one(&db, "SELECT id FROM users WHERE id = 1"));
        assert!(statement_one(
            &db,
            "SELECT id, name FROM users WHERE id = $1"
        ));
        // Still at most one row under a LIMIT the proof ignores.
        assert!(select_one(
            &db,
            "SELECT id FROM users WHERE id = $1 OFFSET 5"
        ));
    }

    #[test]
    fn a_fully_equated_composite_key_is_one_row() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT user_id FROM memberships WHERE user_id = $1 AND team_id = $2"
        ));
        assert!(select_one(
            &db,
            "SELECT a FROM lines WHERE a = $1 AND b = $2"
        ));
        assert!(!select_one(&db, "SELECT a FROM lines WHERE a = $1"));
    }

    #[test]
    fn a_unique_column_equality_is_one_row() {
        let db = catalog();
        assert!(select_one(&db, "SELECT id FROM users WHERE email = $1"));
    }

    #[test]
    fn a_join_determined_through_a_foreign_key_is_one_row() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT u.id, o.name FROM users u JOIN orgs o ON o.id = u.org_id WHERE u.id = $1"
        ));
        // The comma join spells the same fact in WHERE.
        assert!(select_one(
            &db,
            "SELECT u.id, o.name FROM users u, orgs o WHERE u.id = $1 AND o.id = u.org_id"
        ));
        // A USING join.
        assert!(select_one(
            &db,
            "SELECT u.id FROM users u JOIN orgs o USING (id) WHERE u.id = $1"
        ));
    }

    #[test]
    fn a_left_join_with_a_determined_nullable_side_is_one_row() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT o.name FROM orgs o LEFT JOIN users u ON u.id = o.id WHERE o.id = $1"
        ));
        // The nullable side matches many rows: not one row.
        assert!(!select_one(
            &db,
            "SELECT o.id FROM users u LEFT JOIN orders o ON o.customer_id = u.id WHERE u.id = $1"
        ));
    }

    #[test]
    fn a_correlated_outer_column_is_fixed() {
        let db = catalog();
        let outer = view(
            &db,
            "SELECT (SELECT name FROM users u WHERE u.id = o.id) FROM orgs o WHERE o.id = $1",
        );
        let selects: Vec<&SelectStmt> = outer
            .tree
            .nodes()
            .into_iter()
            .filter_map(|(node, _)| match node {
                NodeRef::SelectStmt(select) => Some(select),
                _ => None,
            })
            .collect();
        assert_eq!(selects.len(), 2);
        assert!(select_at_most_one_row(&db, &outer, selects[1]));
        // An outer column alone, without the equality, determines nothing.
        let inner = view(
            &db,
            "SELECT (SELECT name FROM users u WHERE u.id = u.id) FROM orgs o WHERE o.id = $1",
        );
        let selects: Vec<&SelectStmt> = inner
            .tree
            .nodes()
            .into_iter()
            .filter_map(|(node, _)| match node {
                NodeRef::SelectStmt(select) => Some(select),
                _ => None,
            })
            .collect();
        assert!(!select_at_most_one_row(&db, &inner, selects[1]));
    }

    #[test]
    fn aggregates_without_group_by_are_one_row() {
        let db = catalog();
        assert!(select_one(&db, "SELECT count(*) FROM users"));
        assert!(select_one(&db, "SELECT 1 FROM users HAVING count(*) > 0"));
        assert!(select_one(&db, "SELECT 1 FROM users ORDER BY max(id)"));
        // A plain column query over an aggregate's input is not.
        assert!(!select_one(&db, "SELECT id FROM users"));
        assert!(!select_one(
            &db,
            "SELECT id, count(*) FROM users GROUP BY id"
        ));
    }

    #[test]
    fn a_locked_key_lookup_is_one_row_but_a_locked_join_is_not() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "SELECT name FROM users WHERE id = $1 FOR UPDATE"
        ));
        assert!(!statement_one(
            &db,
            "SELECT u.name FROM users u JOIN orgs o ON o.id = u.org_id WHERE u.id = $1 FOR UPDATE"
        ));
    }

    #[test]
    fn select_without_from_is_one_row() {
        let db = catalog();
        assert!(select_one(&db, "SELECT 1"));
        assert!(select_one(&db, "SELECT now()"));
        assert!(select_one(&db, "VALUES (1)"));
        assert!(!select_one(&db, "VALUES (1), (2)"));
        // A set-returning call multiplies even with no FROM.
        assert!(!select_one(&db, "SELECT generate_series(1, 3)"));
        assert!(!select_one(
            &db,
            "SELECT count(*) FROM generate_series(1, 3)"
        ));
    }

    #[test]
    fn set_operations_are_not_one_row() {
        let db = catalog();
        assert!(!select_one(
            &db,
            "SELECT id FROM users UNION SELECT id FROM orgs"
        ));
        assert!(statement_one(
            &db,
            "SELECT id FROM users UNION SELECT id FROM orgs LIMIT 1"
        ));
    }

    #[test]
    fn a_partial_or_deferrable_unique_key_proves_nothing() {
        let db = catalog();
        assert!(!select_one(&db, "SELECT id FROM partials WHERE code = $1"));
        assert!(!select_one(
            &db,
            "SELECT code FROM deferred WHERE code = $1"
        ));
    }

    #[test]
    fn a_nonconjunctive_or_nonliteral_equality_proves_nothing() {
        let db = catalog();
        assert!(!select_one(&db, "SELECT id FROM users WHERE id IN (1, 2)"));
        assert!(!select_one(&db, "SELECT id FROM users WHERE id = ANY($1)"));
        assert!(!select_one(
            &db,
            "SELECT id FROM users WHERE id = $1 OR id = $2"
        ));
        assert!(!select_one(
            &db,
            "SELECT id FROM users WHERE id = org_id AND org_id = $1"
        ));
    }

    #[test]
    fn a_one_to_many_join_is_not_one_row() {
        let db = catalog();
        // The customer is determined; its orders are not.
        assert!(!select_one(
            &db,
            "SELECT o.id FROM customers c JOIN orders o ON o.customer_id = c.id WHERE c.id = $1"
        ));
    }

    #[test]
    fn relations_the_proof_cannot_see_through_are_not_one_row() {
        let db = catalog();
        for sql in [
            "SELECT id FROM nondeterministic WHERE code = $1",
            "SELECT id FROM secured WHERE id = $1",
            "SELECT id FROM parent WHERE id = $1",
            "SELECT id FROM partitioned WHERE id = $1",
            "SELECT id FROM v WHERE id = $1",
            "SELECT s.id FROM (SELECT id FROM users) s WHERE s.id = $1",
            "WITH c AS (SELECT id FROM users) SELECT id FROM c WHERE id = $1",
        ] {
            assert!(!select_one(&db, sql), "{sql}");
        }
    }

    #[test]
    fn statements_count_their_own_limits() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users LIMIT 1",
            "SELECT id FROM users LIMIT 0",
            "SELECT id FROM users FETCH FIRST 1 ROWS ONLY",
            "SELECT id FROM users ORDER BY id LIMIT 1 OFFSET 10",
        ] {
            assert!(statement_one(&db, sql), "{sql}");
        }
        for sql in [
            "SELECT id FROM users LIMIT 2",
            "SELECT id FROM users ORDER BY id FETCH FIRST 2 ROWS ONLY",
            "SELECT id FROM users ORDER BY id FETCH FIRST 1 ROWS WITH TIES",
        ] {
            assert!(!statement_one(&db, sql), "{sql}");
        }
    }

    #[test]
    fn inserts_are_one_row_when_their_source_is() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "INSERT INTO orgs (id, name) VALUES (1, 'a')"
        ));
        assert!(statement_one(
            &db,
            "INSERT INTO orgs (id, name) VALUES (1, 'a') ON CONFLICT DO NOTHING"
        ));
        assert!(!statement_one(
            &db,
            "INSERT INTO orgs (id, name) VALUES (1, 'a'), (2, 'b')"
        ));
        assert!(statement_one(
            &db,
            "INSERT INTO orgs (id, name) SELECT id, name FROM orgs WHERE id = $1"
        ));
        assert!(!statement_one(
            &db,
            "INSERT INTO orgs (id, name) SELECT id, name FROM orgs"
        ));
        assert!(!statement_one(&db, "INSERT INTO defaults DEFAULT VALUES"));
    }

    #[test]
    fn update_and_delete_count_a_determined_target() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "UPDATE users SET name = 'x' WHERE id = $1"
        ));
        assert!(statement_one(&db, "DELETE FROM users WHERE email = $1"));
        assert!(!statement_one(&db, "UPDATE users SET name = 'x'"));
        assert!(!statement_one(
            &db,
            "UPDATE users SET name = 'x' WHERE org_id = $1"
        ));
        assert!(!statement_one(
            &db,
            "UPDATE users SET name = o.name FROM orgs o WHERE o.id = users.id"
        ));
        assert!(!statement_one(
            &db,
            "DELETE FROM users USING orgs o WHERE o.id = users.id"
        ));
    }

    #[test]
    fn a_with_clause_is_judged_by_its_main_statement() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "WITH c AS (SELECT id FROM users) SELECT 1"
        ));
        assert!(statement_one(
            &db,
            "WITH c AS (SELECT id FROM users) SELECT id FROM orgs WHERE id = $1"
        ));
        assert!(!statement_one(
            &db,
            "WITH c AS (SELECT id FROM users) SELECT id FROM c"
        ));
    }

    #[test]
    fn merge_is_not_proved() {
        let db = catalog();
        assert!(!statement_one(
            &db,
            "MERGE INTO users u USING orgs o ON u.id = o.id \
             WHEN MATCHED THEN UPDATE SET name = o.name"
        ));
    }

    #[test]
    fn a_single_row_cte_is_one_relation_with_no_key() {
        let db = catalog();
        // A data-modifying CTE: one bumped conversation row, inserted into
        // the event log.
        assert!(statement_one(
            &db,
            "WITH bump AS ( \
                 UPDATE conversations SET next_seq = next_seq + 1 WHERE id = $1 \
                 RETURNING next_seq - 1 AS seq, tenant_id \
             ) \
             INSERT INTO events (tenant_id, conversation_id, seq) \
             SELECT bump.tenant_id, $1, bump.seq FROM bump RETURNING seq",
        ));
        // A SELECT CTE with its own LIMIT counts the limit.
        assert!(statement_one(
            &db,
            "WITH candidate AS ( \
                 SELECT tenant_id, model_id FROM jobs \
                 WHERE state = 'queued' ORDER BY tenant_id FOR UPDATE SKIP LOCKED LIMIT 1 \
             ) \
             UPDATE jobs j SET state = 'running' FROM candidate c \
             WHERE j.tenant_id = c.tenant_id AND j.model_id = c.model_id \
             RETURNING j.tenant_id",
        ));
        // A CTE's columns are values a key equality may read.
        assert!(statement_one(
            &db,
            "WITH updated AS ( \
                 UPDATE things SET name = 'x' WHERE id = $1 RETURNING * \
             ) \
             SELECT updated.id, o.id FROM updated LEFT JOIN other o ON o.id = updated.other_id",
        ));
        // The same single-row CTE may be referenced twice.
        assert!(statement_one(
            &db,
            "WITH c AS (SELECT id FROM users WHERE id = $1) \
             SELECT a.id FROM c a JOIN c b ON b.id = a.id",
        ));
        // INSERT ... VALUES / INSERT ... SELECT CTEs with RETURNING.
        assert!(statement_one(
            &db,
            "WITH c AS (INSERT INTO orgs (id, name) VALUES (1, 'a') RETURNING id) \
             SELECT id FROM c WHERE id = $1",
        ));
        assert!(statement_one(
            &db,
            "WITH c AS ( \
                 INSERT INTO orgs (id, name) SELECT id, name FROM orgs WHERE id = $1 RETURNING id \
             ) \
             SELECT id FROM c",
        ));
    }

    #[test]
    fn a_cte_that_is_not_one_row_makes_the_proof_give_up() {
        let db = catalog();
        for sql in [
            // A multi-row body.
            "WITH c AS (SELECT id FROM users) SELECT id FROM c WHERE id = $1",
            // More than one VALUES row.
            "WITH c AS (INSERT INTO orgs (id, name) VALUES (1, 'a'), (2, 'b') RETURNING id) \
             SELECT id FROM c",
            // An UPDATE that isn't by key.
            "WITH c AS (UPDATE things SET name = 'x' RETURNING id) SELECT id FROM c",
            // A self-referencing WITH.
            "WITH RECURSIVE c AS ( \
                 SELECT 1 AS x UNION ALL SELECT x + 1 FROM c WHERE x < 5 \
             ) SELECT x FROM c WHERE x = 1",
        ] {
            assert!(!statement_one(&db, sql), "{sql}");
        }
    }

    #[test]
    fn a_recursive_cte_body_is_not_the_table_of_the_same_name() {
        let db = catalog();
        let view = view(
            &db,
            "WITH RECURSIVE users AS ( \
                 SELECT id FROM orgs \
                 UNION ALL \
                 SELECT id FROM users WHERE id = $1 \
             ) SELECT id FROM users WHERE id = $1",
        );
        let cte = top(&view)
            .with_clause
            .as_ref()
            .and_then(|with| with.ctes.first())
            .and_then(|node| match &node.node {
                Some(Node::CommonTableExpr(cte)) => Some(cte),
                _ => None,
            })
            .unwrap();
        let Some(Node::SelectStmt(body)) =
            cte.ctequery.as_deref().and_then(|node| node.node.as_ref())
        else {
            panic!("not a set operation");
        };
        let term = body.rarg.as_deref().expect("recursive term");
        // The recursive term's `users` is the CTE, not the base table whose
        // primary key would otherwise prove the reference one row.
        assert!(!select_at_most_one_row(&db, &view, term));
    }

    #[test]
    fn update_from_and_delete_using_need_every_relation_determined() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "UPDATE users SET name = o.name FROM orgs o \
             WHERE users.id = $1 AND o.id = users.org_id",
        ));
        assert!(statement_one(
            &db,
            "DELETE FROM users USING orgs o \
             WHERE users.id = $1 AND o.id = users.org_id",
        ));
        // The FROM / USING relation isn't determined by the WHERE.
        assert!(!statement_one(
            &db,
            "UPDATE users SET name = 'x' FROM orgs o WHERE users.id = $1",
        ));
        assert!(!statement_one(
            &db,
            "DELETE FROM users USING orgs o WHERE users.id = $1",
        ));
    }

    #[test]
    fn a_partial_unique_index_counts_when_its_predicate_holds() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT tool_call_id FROM tool_calls \
             WHERE conversation_id = $1 AND tool_call_id = $2 AND type = 'tool_result'",
        ));
        // Without the predicate's conjunct the index can't be used.
        assert!(!select_one(
            &db,
            "SELECT tool_call_id FROM tool_calls WHERE conversation_id = $1 AND tool_call_id = $2",
        ));
        assert!(!select_one(
            &db,
            "SELECT tool_call_id FROM tool_calls \
             WHERE conversation_id = $1 AND tool_call_id = $2 AND type = 'other'",
        ));
        // The predicate inside an OR isn't a fact at every row.
        assert!(!select_one(
            &db,
            "SELECT tool_call_id FROM tool_calls \
             WHERE conversation_id = $1 AND tool_call_id = $2 \
               AND (type = 'tool_result' OR type = 'other')",
        ));
    }

    #[test]
    fn an_in_or_any_partial_predicate_counts() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT code FROM any_codes WHERE code = $1 AND kind IN ('a', 'b')",
        ));
        assert!(select_one(
            &db,
            "SELECT code FROM any_codes \
             WHERE code = $1 AND kind = ANY (ARRAY['c', 'd'])",
        ));
        // A different list is not the predicate.
        assert!(!select_one(
            &db,
            "SELECT code FROM any_codes WHERE code = $1 AND kind IN ('a', 'c')",
        ));
    }

    #[test]
    fn a_bare_or_negated_boolean_partial_predicate_counts() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT code FROM flags WHERE code = $1 AND hidden",
        ));
        assert!(select_one(
            &db,
            "SELECT code FROM flags WHERE code = $1 AND NOT hidden",
        ));
    }

    #[test]
    fn a_strict_equality_implies_the_predicates_is_not_null() {
        let db = catalog();
        // `channel_id = $1` and `key = $3` are strict: a row they select has
        // neither column NULL, which is what the index's predicate asks.
        assert!(select_one(
            &db,
            "SELECT channel_id FROM lanes \
             WHERE channel_id = $1 AND identity_id = $2 AND key = $3 AND archived_at IS NULL",
        ));
        // The archived-at conjunct has to be written: nothing implies it.
        assert!(!select_one(
            &db,
            "SELECT channel_id FROM lanes \
             WHERE channel_id = $1 AND identity_id = $2 AND key = $3",
        ));
    }

    #[test]
    fn volatile_calls_outside_a_key_equality_do_not_block() {
        let db = catalog();
        // A target-list clock read and a cast to a user enum, over the
        // single-row bump CTE; its SET, RETURNING and the INSERT's RETURNING
        // are volatile too.
        assert!(statement_one(
            &db,
            "WITH bump AS ( \
                 UPDATE conversations \
                 SET next_seq = next_seq + 1, updated_at = clock_timestamp() WHERE id = $1 \
                 RETURNING next_seq - 1 AS seq, tenant_id \
             ) \
             INSERT INTO events (tenant_id, conversation_id, seq, created_at) \
             SELECT bump.tenant_id, $1, bump.seq, clock_timestamp() FROM bump \
             RETURNING clock_timestamp()",
        ));
        assert!(statement_one(
            &db,
            "WITH bump AS ( \
                 UPDATE conversations SET next_seq = next_seq + 1 WHERE id = $1 \
                 RETURNING next_seq - 1 AS seq, tenant_id \
             ) \
             INSERT INTO events (tenant_id, conversation_id, seq, type) \
             SELECT bump.tenant_id, $1, bump.seq, 'user_message'::conversation_event_type \
             FROM bump RETURNING seq",
        ));
        // Other WHERE conjuncts, the target list and ORDER BY.
        assert!(select_one(
            &db,
            "SELECT id FROM users WHERE id = $1 AND random() < 0.5"
        ));
        assert!(select_one(&db, "SELECT random() FROM users WHERE id = $1"));
        assert!(select_one(
            &db,
            "SELECT id FROM users WHERE id = $1 ORDER BY random()"
        ));
        // A volatile value operand decides which rows match: not provable.
        assert!(!select_one(
            &db,
            "SELECT id FROM users WHERE id = (random() * 10)::int"
        ));
        assert!(!select_one(
            &db,
            "SELECT id FROM users WHERE id = (random() * 10)::bigint"
        ));
    }

    #[test]
    fn a_volatile_cast_on_the_value_is_not_a_key_equality() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE keyed (id bigint PRIMARY KEY);
             CREATE TABLE spectral (id bigint PRIMARY KEY, v text NOT NULL);
             CREATE FUNCTION flaky(text) RETURNS bigint
                LANGUAGE sql VOLATILE AS 'SELECT length($1)::bigint';
             CREATE CAST (text AS bigint) WITH FUNCTION flaky(text) AS IMPLICIT;",
        )
        .unwrap();
        // The value is re-cast per candidate row, so several rows may match
        // the (unique) key even though the other side is determined.
        assert!(!select_one(
            &db,
            "SELECT k.id FROM keyed k JOIN spectral s ON k.id = s.v WHERE s.id = $1",
        ));

        // A cast that can't change its value per row keeps the proof.
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE keyed (id bigint PRIMARY KEY);
             CREATE TABLE spectral (id bigint PRIMARY KEY, v text NOT NULL);
             CREATE FUNCTION steady(text) RETURNS bigint
                LANGUAGE sql IMMUTABLE AS 'SELECT length($1)::bigint';
             CREATE CAST (text AS bigint) WITH FUNCTION steady(text) AS IMPLICIT;",
        )
        .unwrap();
        assert!(select_one(
            &db,
            "SELECT k.id FROM keyed k JOIN spectral s ON k.id = s.v WHERE s.id = $1",
        ));
    }

    #[test]
    fn an_uncorrelated_scalar_sublink_is_a_value() {
        let db = catalog();
        // A row locked by an uncorrelated scalar subquery.
        assert!(statement_one(
            &db,
            "UPDATE things SET name = 'x' \
             WHERE id = (SELECT id FROM things WHERE name = 'pending' \
                         ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1) \
             RETURNING id",
        ));
        // One half of a composite key read from another table.
        assert!(select_one(
            &db,
            "SELECT m.id FROM memory m \
             WHERE m.tenant_id = $1 \
               AND m.user_id = (SELECT user_id FROM conversations WHERE id = $2)",
        ));
        // A volatile call inside the still uncorrelated subquery is fine.
        assert!(select_one(
            &db,
            "SELECT id FROM users WHERE id = (SELECT id FROM users WHERE random() < 0.5)",
        ));
        // A subquery reading this level decides its value per row.
        assert!(!select_one(
            &db,
            "SELECT id FROM users u \
             WHERE id = (SELECT id FROM users WHERE id = u.id)",
        ));
        assert!(!select_one(
            &db,
            "SELECT id FROM users u \
             WHERE id = (SELECT o.id FROM other o WHERE o.note = u.name)",
        ));
        // A target that isn't a bare column is not typed here.
        assert!(!select_one(
            &db,
            "SELECT id FROM users WHERE id = (SELECT id + 1 FROM users LIMIT 1)",
        ));
        // A non-scalar sublink yields many values.
        assert!(!select_one(
            &db,
            "SELECT id FROM users WHERE id IN (SELECT id FROM users)",
        ));
    }

    #[test]
    fn a_non_set_returning_function_in_from_is_one_row() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT true AS taken FROM pg_advisory_xact_lock($1::bigint)",
        ));
        assert!(select_one(&db, "SELECT 1 FROM abs(1)"));
        assert!(select_one(
            &db,
            "SELECT 1 FROM abs(-1) WITH ORDINALITY AS t(n, ord)",
        ));
        assert!(select_one(
            &db,
            "SELECT 1 FROM ROWS FROM (abs(1), length('a'))"
        ));
        // A function returning a set multiplies rows.
        assert!(!select_one(&db, "SELECT 1 FROM generate_series(1, 3)"));
        assert!(!select_one(&db, "SELECT 1 FROM unnest(ARRAY[1, 2])"));
        assert!(!select_one(
            &db,
            "SELECT 1 FROM ROWS FROM (abs(1), generate_series(1, 2))",
        ));
        // A determined table joined to the one-row function stays one row.
        assert!(select_one(
            &db,
            "SELECT u.id FROM users u JOIN pg_advisory_xact_lock($1::bigint) l ON true \
             WHERE u.id = $2",
        ));
    }

    #[test]
    fn a_not_in_or_not_all_partial_predicate_counts() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT conversation_id FROM not_in_codes \
             WHERE conversation_id = $1 AND state NOT IN ('finished', 'abandoned')",
        ));
        assert!(select_one(
            &db,
            "SELECT conversation_id FROM not_in_all_codes \
             WHERE conversation_id = $1 AND state <> ALL (ARRAY['finished', 'abandoned'])",
        ));
        // A different list is not the predicate.
        assert!(!select_one(
            &db,
            "SELECT conversation_id FROM not_in_codes \
             WHERE conversation_id = $1 AND state NOT IN ('finished', 'other')",
        ));
        assert!(!select_one(
            &db,
            "SELECT conversation_id FROM not_in_all_codes \
             WHERE conversation_id = $1 AND state <> ALL (ARRAY['finished', 'other'])",
        ));
    }

    #[test]
    fn redundant_distinct_spans_are_the_keywords_to_delete() {
        let db = catalog();
        let q = view(&db, "SELECT DISTINCT id FROM users");
        let spans = redundant_distinct_spans(&db, &q);
        assert_eq!(spans.len(), 1);
        assert_eq!(&q.sql[spans[0].clone()], "DISTINCT ");
        assert!(
            redundant_distinct_spans(&db, &view(&db, "SELECT DISTINCT name FROM users")).is_empty()
        );
    }
}
