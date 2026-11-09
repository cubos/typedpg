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
    self, InsertStmt, LimitOption, SelectStmt, SetOperation, a_const::Val, node::Node,
};

use super::Ctx;
use super::dedup::{self, Scope};
use super::proof::FromScope;
use crate::pg_catalog::{PgCatalog, ProKind, QueryView};

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
/// - or every `FROM` relation is a base table whose unique key is equated,
///   column by column, to constants, parameters, outer-query columns or
///   columns of already-determined relations — the `distinct` rule's
///   "equalities to a key" proof (see [`super::dedup`]), propagated until
///   every relation is determined.
///
/// Anything else is `false`: `GROUP BY` always is (a group per key), and so
/// is every relation the proof can't see through (a view, a subquery, a
/// function, a CTE, an inheritance parent, a partitioned table or one under
/// row security), a volatile or set-returning call, or an outer join whose
/// nullable side isn't determined.
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
    for raw in &view.tree.stmts {
        if let Some(root) = raw.stmt.as_deref().and_then(|node| node.node.as_ref()) {
            find_select(&ctx, root.to_ref(), None, select, &mut found);
            if found.is_some() {
                break;
            }
        }
    }
    // A node from another tree: judge it without any outer level.
    found.unwrap_or_else(|| select_at_most_one_row_in(&ctx, select, &FromScope::opaque(None)))
}

/// Whether the top-level statement of `view` provably yields at most one
/// row, counting its own `LIMIT` / `FETCH FIRST`.
///
/// A `SELECT` is at most one row when [`select_at_most_one_row`] proves it
/// or its `LIMIT` / `FETCH FIRST` is the constant `0` or `1` (without
/// `WITH TIES`). An `INSERT ... VALUES` with exactly one row is, whatever
/// its `ON CONFLICT`; an `INSERT ... SELECT` is when its select is. An
/// `UPDATE` / `DELETE` is when it has no `FROM` / `USING` and its target
/// relation's key is equated by its `WHERE`. Everything else — `MERGE`,
/// `INSERT ... DEFAULT VALUES`, a statement the proof can't see through —
/// is `false`.
///
/// A statement with a `WITH` clause is judged by its main statement alone;
/// a CTE reference in its `FROM` makes the proof give up.
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
    match statement {
        Node::SelectStmt(select) => {
            select_at_most_one_row_in(&ctx, select, &FromScope::opaque(None))
                || limit_bounds_one(select)
        }
        Node::InsertStmt(insert) => insert_at_most_one_row(&ctx, insert),
        Node::UpdateStmt(update) => {
            update.from_clause.is_empty()
                && target_determined(
                    &ctx,
                    update.relation.as_ref(),
                    update.where_clause.as_deref(),
                )
        }
        Node::DeleteStmt(delete) => {
            delete.using_clause.is_empty()
                && target_determined(
                    &ctx,
                    delete.relation.as_ref(),
                    delete.where_clause.as_deref(),
                )
        }
        _ => false,
    }
}

// ── Finding the node's outer scopes ─────────────────────────────────────────

/// Search `node`'s subtree for `target`, giving each nested query level the
/// [`FromScope`]s it is evaluated in. Once found, run the proof with that
/// scope and store the answer in `out`.
///
/// The scopes may include items a subquery can't actually see without
/// `LATERAL`; that only matters for queries PG rejects, which never reach
/// here (the view analyzed).
fn find_select(
    ctx: &Ctx<'_>,
    node: NodeRef<'_>,
    parent: Option<&FromScope<'_>>,
    target: &SelectStmt,
    out: &mut Option<bool>,
) {
    if out.is_some() {
        return;
    }
    match node {
        NodeRef::SelectStmt(select) => {
            if std::ptr::eq(select, target) {
                let scope = FromScope::of_select(ctx, select, parent);
                *out = Some(select_at_most_one_row_in(ctx, select, &scope));
                return;
            }
            // A set operation has no FROM of its own and its arms see the
            // levels around it, not a scope of its own.
            if select.op() != SetOperation::SetopNone {
                for child in node.children() {
                    find_select(ctx, child, parent, target, out);
                }
                return;
            }
            let scope = FromScope::of_select(ctx, select, parent);
            for child in node.children() {
                find_select(ctx, child, Some(&scope), target, out);
            }
        }
        NodeRef::InsertStmt(insert) => {
            let scope = FromScope::of_dml(ctx, insert.relation.as_ref(), &[], parent);
            for child in node.children() {
                find_select(ctx, child, Some(&scope), target, out);
            }
        }
        NodeRef::UpdateStmt(update) => {
            let scope =
                FromScope::of_dml(ctx, update.relation.as_ref(), &update.from_clause, parent);
            for child in node.children() {
                find_select(ctx, child, Some(&scope), target, out);
            }
        }
        NodeRef::DeleteStmt(delete) => {
            let scope =
                FromScope::of_dml(ctx, delete.relation.as_ref(), &delete.using_clause, parent);
            for child in node.children() {
                find_select(ctx, child, Some(&scope), target, out);
            }
        }
        _ => {
            for child in node.children() {
                find_select(ctx, child, parent, target, out);
            }
        }
    }
}

// ── The SELECT proof ────────────────────────────────────────────────────────

fn select_at_most_one_row_in(ctx: &Ctx<'_>, select: &SelectStmt, outer: &FromScope<'_>) -> bool {
    if select.op() != SetOperation::SetopNone || select.into_clause.is_some() {
        return false;
    }
    if select.group_distinct || !select.group_clause.is_empty() {
        return false;
    }
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
    let ctes = dedup::cte_names(ctx.tree);
    let Some(scope) = Scope::build(ctx, &ctes, &select.from_clause) else {
        return false;
    };
    // Volatile calls (and anything else the distinctness proof cannot see
    // through) make the proof give up; aggregates are already handled.
    //
    // A locking clause counts as volatile there. Over one relation it
    // doesn't bound anything less: EvalPlanQual re-fetches a locked row's
    // newest version and rechecks the WHERE on it, so the scan still
    // returns at most the one row whose key equals the value — and
    // `SELECT … WHERE id = $1 FOR UPDATE` is what `fetch_one` locks rows
    // with. With joins, a re-fetched row is re-joined, so they keep the
    // conservative answer.
    let unsafe_call = if select.locking_clause.is_empty() || scope.rels.len() != 1 {
        dedup::has_unsafe_call(ctx, select)
    } else {
        let mut unlocked = select.clone();
        unlocked.locking_clause.clear();
        dedup::has_unsafe_call(ctx, &unlocked)
    };
    if unsafe_call {
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

/// The relations of `scope` all determined, starting from nothing: a
/// relation is determined when one of its unique keys is equated to
/// constants, parameters, outer-query columns or already-determined
/// relations. `WHERE` facts hold on every output row; a join's `ON` facts
/// hold only on the rows it matches, so they may determine an inner join's
/// sides and an outer join's nullable side, never its preserved one.
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
    let where_facts: Vec<dedup::EqFact> = dedup::conjuncts(select.where_clause.as_deref())
        .into_iter()
        .flat_map(|conjunct| scope.equality_facts(ctx, conjunct, Some(outer)))
        .collect();
    let join_facts: Vec<(&dedup::Join<'_>, Vec<dedup::EqFact>)> = scope
        .joins
        .iter()
        .map(|join| {
            let mut facts = Vec::new();
            for conjunct in dedup::conjuncts(join.quals) {
                facts.extend(scope.equality_facts(ctx, conjunct, Some(outer)));
            }
            for name in &join.using {
                facts.extend(scope.using_facts(ctx, name, join.left, join.right));
            }
            (join, facts)
        })
        .collect();
    let mut known: u64 = 0;
    loop {
        let before = known;
        for b in 0..n {
            if known & (1 << b) == 0 && scope.key_equated(ctx, b, &where_facts, known).is_some() {
                known |= 1 << b;
            }
        }
        for (join, facts) in &join_facts {
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
            for b in 0..n {
                if targets & (1 << b) == 0 || known & (1 << b) != 0 {
                    continue;
                }
                let allowed = allowed & !(1 << b);
                if scope.key_equated(ctx, b, facts, allowed).is_some() {
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
fn insert_at_most_one_row(ctx: &Ctx<'_>, insert: &InsertStmt) -> bool {
    let Some(Node::SelectStmt(select)) = insert
        .select_stmt
        .as_deref()
        .and_then(|node| node.node.as_ref())
    else {
        return false;
    };
    select_at_most_one_row_in(ctx, select, &FromScope::opaque(None)) || limit_bounds_one(select)
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

/// Whether an `UPDATE` / `DELETE` target relation is determined by its
/// `WHERE`: a unique key of it equated, column by column, to constants,
/// parameters or outer columns. `Scope::single` gives up on every relation
/// whose rows the proof can't trust.
fn target_determined(
    ctx: &Ctx<'_>,
    relation: Option<&protobuf::RangeVar>,
    where_clause: Option<&protobuf::Node>,
) -> bool {
    let Some(relation) = relation else {
        return false;
    };
    let ctes = dedup::cte_names(ctx.tree);
    let Some(scope) = Scope::single(ctx, &ctes, relation) else {
        return false;
    };
    let facts: Vec<dedup::EqFact> = dedup::conjuncts(where_clause)
        .into_iter()
        .flat_map(|conjunct| scope.equality_facts(ctx, conjunct, None))
        .collect();
    scope.key_equated(ctx, 0, &facts, 0).is_some()
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
             CREATE VIEW v AS SELECT id, name FROM users;",
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
            "SELECT id FROM users WHERE id = $1 AND random() < 0.5",
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
