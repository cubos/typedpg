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
    self, CommonTableExpr, DeleteStmt, GroupingSetKind, InsertStmt, JoinType, LimitOption,
    MergeStmt, ReturningClause, SelectStmt, SetOperation, UpdateStmt, WithClause, a_const::Val,
    node::Node,
};

use super::Ctx;
use super::dedup::{self, CteColumn, CteEnv, CteInfo, Fields, PredicateSources, Scope};
use super::proof::{self, FromScope};
use crate::pg_catalog::{PgCatalog, ProKind, QueryView, RelKind, oid};

impl QueryView {
    /// Whether evaluating `select` — a node of this view at any nesting
    /// depth — can never raise an error: every expression of its target
    /// list, WHERE, joins, GROUP BY, HAVING, DISTINCT and ORDER BY is one
    /// [`proof::cannot_error`] accepts, and its FROM reads only stored
    /// tables. A `false` answer only withholds a rewrite, so anything the
    /// proof can't see through is `false`.
    pub fn select_is_error_free(&self, catalog: &PgCatalog, select: &SelectStmt) -> bool {
        if !self.analyzed.spreads.is_empty() {
            return false;
        }
        let ctx = Ctx {
            sql: &self.sql,
            tree: &self.tree,
            catalog,
            analyzed: &self.analyzed,
        };
        select_is_error_free_in(&ctx, select)
    }

    /// The spans of the plain `DISTINCT` keywords the `distinct` proof
    /// finds redundant, each with whether removing it can't change the
    /// errors the select raises (the lint fix's condition; a finding is
    /// still reported when it is `false`, the simplification then keeping
    /// the query as written).
    pub fn redundant_distinct_spans_with_fix(
        &self,
        catalog: &PgCatalog,
    ) -> Vec<(std::ops::Range<usize>, bool)> {
        if !self.analyzed.spreads.is_empty() {
            return Vec::new();
        }
        let ctx = Ctx {
            sql: &self.sql,
            tree: &self.tree,
            catalog,
            analyzed: &self.analyzed,
        };
        dedup::distinct_candidates_with_select(&ctx)
            .into_iter()
            .flat_map(|(select, candidate)| {
                let error_free = select_is_error_free_in(&ctx, select);
                candidate
                    .edits
                    .into_iter()
                    .map(move |edit| (edit.range, error_free))
            })
            .collect()
    }
}

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
/// - it has an aggregate or `HAVING` without `GROUP BY` (one group) — at a
///   level no outer query encloses, whatever the aggregate's arguments read,
///   a `FROM` subquery included — or only `GROUP BY ()` (one empty grouping
///   set);
/// - every `FROM` relation is a base table whose unique key is equated,
///   column by column, to constants, parameters, outer-query columns,
///   scalar subqueries uncorrelated with this level or columns of
///   already-determined relations — the `distinct` rule's "equalities to a
///   key" proof (see [`super::dedup`]), propagated until every relation is
///   determined — a single-row CTE, a `FROM` function that can't return
///   a set, a subquery the proof bounds to one row (`LATERAL` or not), or a
///   `pg_catalog` system view with a key PostgreSQL itself guarantees (see
///   below);
/// - it is a plain `DISTINCT` whose targets are all fixed per evaluation:
///   literals, parameters or non-volatile expressions of them, with no
///   column reference or window function.
///
/// A CTE the statement's own `WITH` clause proves yields at most one row is
/// a relation with no key needed: it is determined from the start, may be
/// referenced more than once, and its columns are values a key equality may
/// read. CTEs are judged in declaration order, so a body may use the ones
/// before it; a recursive `WITH` or a reference to an unproven one makes the
/// proof give up. A CTE's output columns are listed even when its body isn't
/// proven one row, and a `FROM` subquery's names likewise, so a nested
/// level's aggregate argument resolves against them and counts as that
/// level's aggregate ([`aggregate_is_this_level`]). A `FROM` function call
/// every overload of which is non-set-returning also yields exactly one row
/// (`ROWS FROM` of several such calls and `WITH ORDINALITY` included).
///
/// A volatile call only matters as an equality's value operand, which
/// decides which row a key matches: [`super::dedup`] refuses a volatile one.
/// Volatility anywhere else — the target list, `ORDER BY`, another `WHERE`
/// conjunct, a DML's `SET` or `RETURNING` — leaves the row count alone. A
/// locking clause is handled on its own (it needs a single relation).
///
/// Anything else is `false`: `GROUP BY` other than `()` always is (a group
/// per key), and so is every relation the proof can't see through (a view
/// other than the trusted `pg_catalog` system views, a subquery it can't
/// prove one row, a function that returns a set, an inheritance parent, a
/// partitioned table or one under row security), a set-returning call, a
/// `FULL JOIN` (two one-row sides still produce an unmatched row from each
/// when their keys differ), or an outer join whose nullable side isn't
/// determined.
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
            find_select(
                &ctx,
                root.to_ref(),
                None,
                &env,
                select,
                SelectCheck::OneRow,
                &mut found,
            );
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

/// Whether `select` — a node of `ctx`'s tree at any nesting depth — is
/// error-free by [`QueryView::select_is_error_free`]'s proof.
///
/// The lint rules use it to withhold a fix, and the `distinct` and
/// `union_all` simplifications to withhold the rewrite itself: dropping a
/// `DISTINCT` or a `UNION`'s deduplication changes which rows PG evaluates,
/// so an expression that can raise there must keep the original query.
pub(super) fn select_is_error_free_in(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let mut found = None;
    let env = CteEnv::default();
    for raw in &ctx.tree.stmts {
        if let Some(root) = raw.stmt.as_deref().and_then(|node| node.node.as_ref()) {
            find_select(
                ctx,
                root.to_ref(),
                None,
                &env,
                select,
                SelectCheck::ErrorFree,
                &mut found,
            );
            if found.is_some() {
                break;
            }
        }
    }
    found.unwrap_or_else(|| {
        proof::error_free_select_allow_count(ctx, select, &FromScope::opaque(None))
    })
}

/// Whether the top-level statement of `view` provably yields at most one
/// row, counting its own `LIMIT` / `FETCH FIRST`.
///
/// A `SELECT` is at most one row when [`select_at_most_one_row`] proves it
/// or its `LIMIT` / `FETCH FIRST` is the constant `0` or `1` (without
/// `WITH TIES`). An `INSERT ... VALUES` with exactly one row is, whatever
/// its `ON CONFLICT`; `INSERT ... DEFAULT VALUES` is. An `INSERT ... SELECT`
/// is when its select is. An `UPDATE` / `DELETE` is when its target relation
/// is determined by its `WHERE` equalities (a target row is written once
/// however many `FROM` / `USING` rows match it). Every
/// data-modifying statement also needs a target relation no rewrite rule can
/// replace (see [`dml_target_without_rules`]). Everything else — `MERGE`, a
/// statement the proof can't see through — is `false`.
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
    statement_at_most_one_row_in(&ctx, statement.to_ref(), &CteEnv::default(), None)
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
///
/// `parent` is the scope of the query level around the one the clause
/// belongs to: a `WITH` written inside a sublink or subquery makes CTE
/// bodies that can read that level's columns, so their own aggregate with
/// no `GROUP BY` need not be theirs (see [`has_own_aggregate`]).
fn visible_ctes(
    ctx: &Ctx<'_>,
    env: &CteEnv,
    with: Option<&WithClause>,
    parent: Option<&FromScope<'_>>,
) -> VisibleCtes {
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
        let info = single_row_cte(ctx, cte, &after, parent);
        after.insert(cte.ctename.clone(), info);
    }
    VisibleCtes { after, bodies }
}

/// The CTE environment a FROM subquery's own `WITH` clause makes visible:
/// its CTEs shadow the enclosing level's, and its bodies are judged around
/// `parent`, the scope of the subquery's own level (nested in the levels
/// around it). A CTE whose body isn't one row is still listed for the
/// columns a nested aggregate argument can resolve against (see
/// [`single_row_cte`]).
pub(super) fn subquery_cte_env(
    ctx: &Ctx<'_>,
    env: &CteEnv,
    select: &SelectStmt,
    parent: &FromScope<'_>,
) -> CteEnv {
    visible_ctes(ctx, env, select.with_clause.as_ref(), Some(parent)).after
}

/// Whether `cte`'s output columns are known, and whether its body — with
/// its `RETURNING` clause for a data-modifying statement — yields at most
/// one row by [`statement_at_most_one_row`]'s rules. `parent` is the scope
/// around the query level the `WITH` belongs to.
///
/// A CTE whose body isn't one row still lists its columns when they are
/// known: a reference to it is a relation the proof never determines, but a
/// nested level's aggregate argument can be placed on its columns.
fn single_row_cte(
    ctx: &Ctx<'_>,
    cte: &CommonTableExpr,
    env: &CteEnv,
    parent: Option<&FromScope<'_>>,
) -> Option<CteInfo> {
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
    if !returns_rows {
        return None;
    }
    let mut columns = cte_output_columns(
        ctx,
        body,
        &visible_ctes(ctx, env, with_clause(body), parent).after,
    );
    for (column, alias) in columns.iter_mut().zip(&cte.aliascolnames) {
        let Some(Node::String(name)) = &alias.node else {
            return None;
        };
        column.name = Some(name.sval.clone());
    }
    if cte.aliascolnames.len() > columns.len() {
        return None;
    }
    // A body whose output columns can't all be listed is a reference the
    // proof can't follow: a name it doesn't list may be the one a query uses.
    if columns.is_empty() {
        return None;
    }
    let single_row = statement_at_most_one_row_in(ctx, body, env, parent);
    Some(CteInfo {
        columns,
        single_row,
    })
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
            &[],
            insert.returning_clause.as_ref(),
            env,
        ),
        NodeRef::UpdateStmt(update) => returning_columns(
            ctx,
            update.relation.as_ref(),
            &update.from_clause,
            update.returning_clause.as_ref(),
            env,
        ),
        NodeRef::DeleteStmt(delete) => returning_columns(
            ctx,
            delete.relation.as_ref(),
            &delete.using_clause,
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
                    name: Some(format!("column{}", i + 1)),
                    typ,
                    deterministic,
                }
            })
            .collect();
    }
    // Columns are matched to the CTE's alias list by position, so a target
    // whose columns can't all be listed makes the whole list unknown (an
    // empty list: no column of the CTE takes part in a key equality).
    let root = FromScope::opaque(None);
    let scope = Scope::build_env(ctx, env, &select.from_clause, &root);
    let mut out = Vec::new();
    for target in &select.target_list {
        let Some(Node::ResTarget(target)) = &target.node else {
            return Vec::new();
        };
        let Some(value) = target.val.as_deref() else {
            return Vec::new();
        };
        if let Some(Node::ColumnRef(column)) = &value.node
            && let Fields::Star(qualifier) = dedup::column_ref_fields(column)
        {
            let Some(scope) = scope.as_ref() else {
                return Vec::new();
            };
            let rels: Vec<usize> = match qualifier {
                // A bare `*` merges USING columns: its columns aren't listed.
                None if scope.joins.iter().any(|join| !join.using.is_empty()) => {
                    return Vec::new();
                }
                None => (0..scope.rels.len()).collect(),
                Some(qualifier) => match scope.rel_named(&qualifier) {
                    Some(r) => vec![r],
                    None => return Vec::new(),
                },
            };
            for r in rels {
                // A relation whose output names aren't all certain can't be
                // listed column by column.
                if !scope.rels[r].columns_known {
                    return Vec::new();
                }
                for (name, attnum) in &scope.rels[r].columns {
                    let (typ, deterministic) = scope.rels[r]
                        .column_info(ctx, *attnum)
                        .map_or((None, false), |(typ, deterministic)| {
                            (Some(typ), deterministic)
                        });
                    out.push(CteColumn {
                        name: Some(name.clone()),
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
                Some(target.name.clone())
            },
            typ,
            deterministic,
        });
    }
    out
}

/// The columns a `RETURNING` clause yields. A star expands the target
/// table's columns with their types only when it is unqualified and there
/// is no `FROM` / `USING`, or when it is qualified by the target's name or
/// alias. Any other star — `s.*` of a `FROM` relation, or a bare `*` that
/// also merges the `FROM` relations' columns — is still part of a one-row
/// CTE, but its columns' types are unknown: a key equality can't read them.
fn returning_columns(
    ctx: &Ctx<'_>,
    relation: Option<&protobuf::RangeVar>,
    from: &[protobuf::Node],
    returning: Option<&ReturningClause>,
    env: &CteEnv,
) -> Vec<CteColumn> {
    let (Some(relation), Some(returning)) = (relation, returning) else {
        return Vec::new();
    };
    // As in `select_output_columns`: a target whose columns can't all be
    // listed, by position, makes the whole list unknown.
    if !returning.options.is_empty() {
        // `RETURNING WITH (OLD AS o, NEW AS n)` renames the row references.
        return Vec::new();
    }
    let scope = Scope::dml(ctx, env, relation, from);
    let mut out = Vec::new();
    for target in &returning.exprs {
        let Some(Node::ResTarget(target)) = &target.node else {
            return Vec::new();
        };
        let Some(value) = target.val.as_deref() else {
            return Vec::new();
        };
        if let Some(Node::ColumnRef(column)) = &value.node
            && let Some(qualifier) = star_ref(column)
        {
            let target_star = match qualifier {
                Some(qualifier) => scope
                    .as_ref()
                    .is_some_and(|scope| target_named(&scope.rels[0], qualifier)),
                None => from.is_empty(),
            };
            let Some(scope) = &scope else {
                return Vec::new();
            };
            if target_star {
                for (name, attnum) in &scope.rels[0].columns {
                    let (typ, deterministic) = scope.rels[0]
                        .column_info(ctx, *attnum)
                        .map_or((None, false), |(typ, deterministic)| {
                            (Some(typ), deterministic)
                        });
                    out.push(CteColumn {
                        name: Some(name.clone()),
                        typ,
                        deterministic,
                    });
                }
            } else {
                // Whose columns the star expands, with every type unknown.
                let rels: Vec<usize> = match qualifier {
                    Some(qualifier) => match scope.rel_named(qualifier) {
                        Some(r) => vec![r],
                        None => return Vec::new(),
                    },
                    None => (0..scope.rels.len()).collect(),
                };
                for r in rels {
                    // A relation whose output names aren't all certain can't
                    // be listed column by column.
                    if !scope.rels[r].columns_known {
                        return Vec::new();
                    }
                    for (name, _) in &scope.rels[r].columns {
                        out.push(CteColumn {
                            name: Some(name.clone()),
                            typ: None,
                            deterministic: false,
                        });
                    }
                }
            }
            continue;
        }
        let (typ, deterministic) = value_type(ctx, scope.as_ref(), value);
        out.push(CteColumn {
            name: if target.name.is_empty() {
                output_name(value)
            } else {
                Some(target.name.clone())
            },
            typ,
            deterministic,
        });
    }
    out
}

/// The qualifier of a star reference: `Some(None)` for `*`, `Some(Some(q))`
/// for `q.*` (or the table part of `schema.q.*`), `None` for anything else.
fn star_ref(column: &protobuf::ColumnRef) -> Option<Option<&str>> {
    let (last, names) = column.fields.split_last()?;
    if !matches!(last.node, Some(Node::AStar(_))) {
        return None;
    }
    match names {
        [] => Some(None),
        [qualifier] => match &qualifier.node {
            Some(Node::String(string)) => Some(Some(string.sval.as_str())),
            _ => Some(Some("")),
        },
        // A schema-qualified star: not the plain target name/alias, so let
        // it expand no known relation (its columns stay unknown).
        _ => Some(Some("")),
    }
}

/// Whether the target relation is named by `qualifier`: its alias, or its
/// table name when it has none.
fn target_named(rel: &dedup::Rel, qualifier: &str) -> bool {
    qualifier == rel.refname || (!rel.aliased && qualifier == rel.relname)
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

/// The column name PG gives a target that has none written, when it is
/// certain: a bare column reference's last field. `FigureColname` derives
/// other names from the expression (a cast from its argument, a function call
/// from the function, `CASE` as `case`), which this doesn't reproduce, so
/// anything else is unknown.
fn output_name(value: &protobuf::Node) -> Option<String> {
    let Node::ColumnRef(column) = value.node.as_ref()? else {
        return None;
    };
    let Fields::Names(names) = dedup::column_ref_fields(column) else {
        return None;
    };
    names.last().map(|name| (*name).to_owned())
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

/// What [`find_select`] runs on the target once it finds it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectCheck {
    /// [`select_at_most_one_row_in`].
    OneRow,
    /// [`proof::error_free_select_allow_count`] (the lint fix's proof).
    ErrorFree,
}

/// Search `node`'s subtree for `target`, giving each nested query level the
/// [`FromScope`]s and CTEs it is evaluated in. Once found, run `check`'s
/// proof with that scope and store the answer in `out`.
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
    check: SelectCheck,
    out: &mut Option<bool>,
) {
    if out.is_some() {
        return;
    }
    match node {
        NodeRef::SelectStmt(select) => {
            let visible = visible_ctes(ctx, env, select.with_clause.as_ref(), parent);
            if std::ptr::eq(select, target) {
                let scope = FromScope::of_select(ctx, select, parent);
                *out = Some(match check {
                    SelectCheck::OneRow => {
                        select_at_most_one_row_in(ctx, select, &scope, &visible.after)
                    }
                    SelectCheck::ErrorFree => {
                        proof::error_free_select_allow_count(ctx, select, &scope)
                    }
                });
                return;
            }
            find_cte_bodies(
                ctx,
                select.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                check,
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
                        check,
                        out,
                    );
                }
                return;
            }
            let scope = FromScope::of_select(ctx, select, parent);
            for child in select_children(select) {
                find_select(ctx, child, Some(&scope), &visible.after, target, check, out);
            }
        }
        NodeRef::InsertStmt(insert) => {
            let visible = visible_ctes(ctx, env, insert.with_clause.as_ref(), parent);
            find_cte_bodies(
                ctx,
                insert.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                check,
                out,
            );
            let scope = FromScope::of_dml(ctx, insert.relation.as_ref(), &[], parent);
            for child in insert_children(insert) {
                find_select(ctx, child, Some(&scope), &visible.after, target, check, out);
            }
        }
        NodeRef::UpdateStmt(update) => {
            let visible = visible_ctes(ctx, env, update.with_clause.as_ref(), parent);
            find_cte_bodies(
                ctx,
                update.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                check,
                out,
            );
            let scope =
                FromScope::of_dml(ctx, update.relation.as_ref(), &update.from_clause, parent);
            for child in update_children(update) {
                find_select(ctx, child, Some(&scope), &visible.after, target, check, out);
            }
        }
        NodeRef::DeleteStmt(delete) => {
            let visible = visible_ctes(ctx, env, delete.with_clause.as_ref(), parent);
            find_cte_bodies(
                ctx,
                delete.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                check,
                out,
            );
            let scope =
                FromScope::of_dml(ctx, delete.relation.as_ref(), &delete.using_clause, parent);
            for child in delete_children(delete) {
                find_select(ctx, child, Some(&scope), &visible.after, target, check, out);
            }
        }
        NodeRef::MergeStmt(merge) => {
            let visible = visible_ctes(ctx, env, merge.with_clause.as_ref(), parent);
            find_cte_bodies(
                ctx,
                merge.with_clause.as_ref(),
                &visible.bodies,
                parent,
                target,
                check,
                out,
            );
            let source: &[protobuf::Node] = merge
                .source_relation
                .as_deref()
                .map(std::slice::from_ref)
                .unwrap_or_default();
            let scope = FromScope::of_dml(ctx, merge.relation.as_ref(), source, parent);
            for child in merge_children(merge) {
                find_select(ctx, child, Some(&scope), &visible.after, target, check, out);
            }
        }
        _ => {
            for child in node.children() {
                find_select(ctx, child, parent, env, target, check, out);
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
    check: SelectCheck,
    out: &mut Option<bool>,
) {
    let Some(with) = with else {
        return;
    };
    for (node, env) in with.ctes.iter().zip(bodies) {
        if let Some(Node::CommonTableExpr(cte)) = &node.node
            && let Some(body) = cte.ctequery.as_deref().and_then(|n| n.node.as_ref())
        {
            find_select(ctx, body.to_ref(), parent, env, target, check, out);
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

/// Whether `statement` yields at most one row by [`select_at_most_one_row`]'s
/// rules, judged at the level `parent` encloses (`None` for the top level):
/// a CTE body written inside a nested `WITH` can read the columns around it,
/// so an aggregate there may belong to that outer level.
fn statement_at_most_one_row_in(
    ctx: &Ctx<'_>,
    statement: NodeRef<'_>,
    env: &CteEnv,
    parent: Option<&FromScope<'_>>,
) -> bool {
    let env = visible_ctes(ctx, env, with_clause(statement), parent).after;
    match statement {
        NodeRef::SelectStmt(select) => {
            let scope = FromScope::of_select(ctx, select, parent);
            select_at_most_one_row_in(ctx, select, &scope, &env) || limit_bounds_one(select)
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

pub(super) fn select_at_most_one_row_in(
    ctx: &Ctx<'_>,
    select: &SelectStmt,
    outer: &FromScope<'_>,
    env: &CteEnv,
) -> bool {
    if select.op() != SetOperation::SetopNone || select.into_clause.is_some() {
        return false;
    }
    // `GROUP BY ()` is a single empty grouping set: one group, like an
    // aggregate without `GROUP BY`. Any other grouping (including
    // `GROUPING SETS ((), ())`, which is two groups) makes a group per key.
    let empty_grouping_set = single_empty_grouping_set(select);
    if select.group_distinct || (!select.group_clause.is_empty() && !empty_grouping_set) {
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
    // A plain `DISTINCT` whose targets are all one value per evaluation —
    // literals, parameters, non-volatile expressions of them, with no
    // column reference — deduplicates every row to at most one.
    if dedup::is_plain_distinct(select) && fixed_distinct_targets(ctx, select) {
        return true;
    }
    // A FULL JOIN emits the unmatched rows of both sides: two relations that
    // are each one row can still produce two (one from each side when their
    // join keys don't match), so no key equality bounds it.
    if has_full_join(&select.from_clause) {
        return false;
    }
    if !select.values_lists.is_empty() {
        return select.from_clause.is_empty() && select.values_lists.len() == 1;
    }
    if select.from_clause.is_empty() {
        return true;
    }
    if empty_grouping_set
        || select.having_clause.is_some()
        || has_own_aggregate(ctx, select, env, outer)
    {
        // One group, and nothing groups it further.
        return true;
    }
    let Some(scope) = Scope::build_env(ctx, env, &select.from_clause, outer) else {
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

/// Whether `select` has exactly one `GROUP BY` item and it is the empty
/// grouping set (`GROUP BY ()`): one group over every input row.
fn single_empty_grouping_set(select: &SelectStmt) -> bool {
    let [group] = select.group_clause.as_slice() else {
        return false;
    };
    matches!(
        &group.node,
        Some(Node::GroupingSet(set))
            if set.kind() == GroupingSetKind::GroupingSetEmpty && set.content.is_empty()
    )
}

/// Whether every plain-`DISTINCT` target is one value per evaluation of this
/// level: a literal, a parameter, a clock reading or a non-volatile
/// expression of those, with no column reference, subquery or window
/// function. Rows that all evaluate to the same target values deduplicate to
/// at most one.
fn fixed_distinct_targets(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    select.target_list.iter().all(|target| {
        let Some(Node::ResTarget(target)) = &target.node else {
            return false;
        };
        target
            .val
            .as_deref()
            .is_some_and(|value| fixed_per_evaluation(ctx, value))
    })
}

/// Whether `value` evaluates to the same result on every row of a query
/// level: it reads no column and no nested query, and every call it makes is
/// non-volatile, non-aggregate and non-window.
fn fixed_per_evaluation(ctx: &Ctx<'_>, value: &protobuf::Node) -> bool {
    if proof::has_volatile(ctx, value) {
        return false;
    }
    let Some(root) = value.node.as_ref() else {
        return false;
    };
    let mut stack = vec![root.to_ref()];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::ColumnRef(_)
            | NodeRef::SubLink(_)
            | NodeRef::RangeSubselect(_)
            | NodeRef::CommonTableExpr(_)
            | NodeRef::SelectStmt(_)
            | NodeRef::RangeVar(_)
            | NodeRef::RangeFunction(_)
            | NodeRef::RangeTableSample(_) => return false,
            // A window function numbers the rows it sees, so it differs
            // from one row to the next even over constants.
            NodeRef::FuncCall(call) if call.over.is_some() => return false,
            _ => {}
        }
        stack.extend(node.children());
    }
    true
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

/// Whether `select`'s own expressions call an aggregate that belongs to
/// this level (a window function over one doesn't count: `over` is set).
///
/// PG assigns an aggregate to the innermost query level its aggregated
/// arguments — arguments, ORDER BY and FILTER — read; with no column
/// reference at all (`count(*)`) that is the level it is written in. So
/// `count(outer.x)` inside a subquery is the outer level's aggregate and
/// does not collapse the subquery's rows, while `count(role)` over
/// `FROM (SELECT role …) s` or a CTE reference resolves in [`Scope`] and
/// collapses this level. Nested queries are separate levels; when one is
/// found inside the arguments this gives up (a reference there might still
/// bind to this level).
///
/// At a level no outer query encloses — the statement's own `SELECT`, a
/// top-level CTE body or set-operation branch (`outer.has_parent()` is
/// false) — there is no other level an aggregate written here could belong
/// to, whatever its arguments reference (a `FROM` subquery included): an
/// aggregate with no `GROUP BY` still collapses the level to one group.
fn has_own_aggregate(
    ctx: &Ctx<'_>,
    select: &SelectStmt,
    env: &CteEnv,
    outer: &FromScope<'_>,
) -> bool {
    let top_level = !outer.has_parent();
    let scope = (!top_level)
        .then(|| Scope::build_env(ctx, env, &select.from_clause, outer))
        .flatten();
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
                if !procs.is_empty()
                    && procs.iter().all(|proc| proc.prokind == ProKind::Aggregate)
                    && (top_level || aggregate_is_this_level(scope.as_ref(), call))
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

/// Whether every column reference in aggregate call `call`'s aggregated
/// arguments (arguments, ORDER BY and FILTER) resolves in `scope`, this
/// select's own FROM — or there is none. Any reference this can't place
/// (an outer column, a name an opaque item may bind) makes it another
/// level's aggregate.
fn aggregate_is_this_level(scope: Option<&Scope<'_>>, call: &protobuf::FuncCall) -> bool {
    let sort_exprs = call
        .agg_order
        .iter()
        .filter_map(|node| match node.node.as_ref() {
            Some(Node::SortBy(by)) => by.node.as_deref(),
            _ => Some(node),
        });
    let mut stack: Vec<NodeRef<'_>> = call
        .args
        .iter()
        .filter_map(|n| n.node.as_ref().map(|inner| inner.to_ref()))
        .collect();
    stack.extend(sort_exprs.filter_map(|n| n.node.as_ref().map(|inner| inner.to_ref())));
    stack.extend(
        call.agg_filter
            .as_deref()
            .and_then(|n| n.node.as_ref())
            .map(|inner| inner.to_ref()),
    );
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::SubLink(_) => {
                // Only a subquery that reads no column can't hide a
                // reference to this level.
                if contains_column_ref(node) {
                    return false;
                }
                continue;
            }
            NodeRef::RangeSubselect(_) | NodeRef::CommonTableExpr(_) => return false,
            NodeRef::ColumnRef(column) => {
                let Fields::Names(names) = dedup::column_ref_fields(column) else {
                    return false;
                };
                if !scope.is_some_and(|scope| matches!(scope.resolve(&names), dedup::Col::Rel(..)))
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

/// Whether any column reference appears anywhere below `node`.
fn contains_column_ref(node: NodeRef<'_>) -> bool {
    match node {
        NodeRef::ColumnRef(_) => true,
        _ => node.children().into_iter().any(contains_column_ref),
    }
}

/// Whether the FROM clause contains a FULL JOIN at any depth.
fn has_full_join(from: &[protobuf::Node]) -> bool {
    from.iter().any(node_has_full_join)
}

fn node_has_full_join(item: &protobuf::Node) -> bool {
    match item.node.as_ref() {
        Some(Node::JoinExpr(join)) => {
            join.jointype() == JoinType::JoinFull
                || [&join.larg, &join.rarg]
                    .into_iter()
                    .flatten()
                    .any(|side| node_has_full_join(side))
        }
        _ => false,
    }
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
/// output row, and so do the `ON` conjuncts of inner joins that no outer
/// join null-extends: an inner join only emits rows its condition matched.
/// A join's `ON` facts hold only on the rows it matches, so they may
/// determine an outer join's nullable side — never its preserved one; an
/// inner join inside an outer join's nullable side keeps that scoped
/// treatment too. A key may be equated partly by the `WHERE`, partly by
/// one join's `ON` and partly by another's: all of them hold on every row
/// the joins contribute.
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
    // The level's facts: WHERE's, plus the ON conjuncts of every inner join
    // that isn't inside an outer join's nullable side (a null-extended row
    // never evaluates those, so there they only hold on matched rows and
    // stay with their join below).
    let mut level_conjuncts = dedup::conjuncts(select.where_clause.as_deref());
    for join in &scope.joins {
        if join.kind == JoinType::JoinInner && !join.nullable {
            level_conjuncts.extend(dedup::conjuncts(join.quals));
        }
    }
    let level_facts: Vec<dedup::EqFact> = level_conjuncts
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
                conjuncts: &level_conjuncts,
                facts: &level_facts,
            };
            if scope
                .key_equated_with(ctx, b, &level_facts, known, Some(&sources))
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
            merged.extend(level_facts.iter().cloned());
            let mut sources = conjuncts.clone();
            sources.extend(level_conjuncts.iter().copied());
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

/// Whether a statement may write `relation` without a rewrite rule changing
/// what it returns: an ordinary (or partitioned) table with no rewrite rules.
///
/// A view is refused even when PostgreSQL would make it auto-updatable: DML
/// on it runs its `_RETURN` rule's query and the base table's own rules, and
/// an `INSTEAD OF` trigger decides how many rows the statement's `RETURNING`
/// clause yields. A table's `INSTEAD` rule likewise replaces the statement
/// (and its `RETURNING` list) with whatever its actions return, so no rule
/// at all is allowed. `DO ALSO` rules and triggers can add effects, but a
/// plain table without rewrite rules writes one row per source row.
fn dml_target_without_rules(ctx: &Ctx<'_>, relation: &protobuf::RangeVar) -> bool {
    if !relation.catalogname.is_empty() {
        return false;
    }
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    let Some(class) = ctx.catalog.resolve_table(schema, &relation.relname) else {
        return false;
    };
    matches!(class.relkind, RelKind::Table | RelKind::Partitioned)
        && !ctx.catalog.lint_relation_has_rules(class.oid)
}

/// `INSERT ... VALUES` with one row, `INSERT ... SELECT` whose select is at
/// most one row, or `INSERT ... DEFAULT VALUES` (exactly one row). Every
/// case also needs a target relation no rewrite rule can replace (see
/// [`dml_target_without_rules`]).
fn insert_at_most_one_row(ctx: &Ctx<'_>, insert: &InsertStmt, env: &CteEnv) -> bool {
    let Some(relation) = insert.relation.as_ref() else {
        return false;
    };
    if !dml_target_without_rules(ctx, relation) {
        return false;
    }
    let Some(node) = insert
        .select_stmt
        .as_deref()
        .and_then(|node| node.node.as_ref())
    else {
        // `INSERT ... DEFAULT VALUES` writes exactly one row.
        return true;
    };
    let NodeRef::SelectStmt(select) = node.to_ref() else {
        return false;
    };
    // The source query's own `WITH` shadows the statement's CTE names, just
    // as it does in a FROM subquery.
    let env = visible_ctes(ctx, env, select.with_clause.as_ref(), None).after;
    let scope = FromScope::of_select(ctx, select, None);
    select_at_most_one_row_in(ctx, select, &scope, &env) || limit_bounds_one(select)
}

/// An `UPDATE ... FROM` / `DELETE ... USING` whose target relation is
/// determined by the `WHERE` fixed point, the target counting as one more
/// relation. A target row is updated or deleted once however many `FROM` /
/// `USING` rows match it, so those other relations need not be determined:
/// mapping their columns into the target's key doesn't multiply the
/// statement's output rows.
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
    if !dml_target_without_rules(ctx, relation) {
        return false;
    }
    let Some(scope) = Scope::dml(ctx, env, relation, from) else {
        return false;
    };
    let n = scope.rels.len();
    if n == 0 || n > 63 {
        return false;
    }
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
    // Relation 0 is the target (`Scope::dml` adds it first).
    known & 1 == 1
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
             CREATE TYPE paint_color AS ENUM ('red', 'green');
             CREATE TABLE paints (
                id bigint PRIMARY KEY,
                shop text NOT NULL,
                color paint_color NOT NULL,
                UNIQUE (shop, color)
             );
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
             CREATE UNIQUE INDEX ON flags (code) WHERE NOT hidden;
             CREATE TABLE dst (id bigint PRIMARY KEY, z int);
             CREATE TABLE src (id bigint PRIMARY KEY, dst_id bigint);
             CREATE TABLE src_empty ();
             CREATE TABLE alias_partial (k int, active boolean, inactive boolean);
             CREATE UNIQUE INDEX ON alias_partial (k) WHERE active;",
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

    /// The SELECT inside the first target's `ARRAY(...)` sublink.
    fn array_select(select: &SelectStmt) -> &SelectStmt {
        let Some(Node::ResTarget(target)) = select.target_list[0].node.as_ref() else {
            panic!("not a target");
        };
        let Some(Node::SubLink(sub)) = target.val.as_deref().and_then(|n| n.node.as_ref()) else {
            panic!("not an ARRAY sublink");
        };
        match sub.subselect.as_deref().and_then(|n| n.node.as_ref()) {
            Some(Node::SelectStmt(inner)) => inner,
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
    fn an_enum_column_in_a_unique_key_is_one_row() {
        let db = catalog();
        // `color = …` resolves to the polymorphic `enum_eq(anyenum, anyenum)`,
        // the `enum_ops` family's btree equality: read as the column's type.
        assert!(select_one(
            &db,
            "SELECT id FROM paints WHERE shop = $1 AND color = 'red'"
        ));
        assert!(select_one(
            &db,
            "SELECT id FROM paints WHERE shop = $1 AND color = $2"
        ));
        // Only part of the key: not one row.
        assert!(!select_one(&db, "SELECT id FROM paints WHERE shop = $1"));
        // Comparing the enum as text is not the key's own operator family:
        // the key equality isn't there.
        assert!(!select_one(
            &db,
            "SELECT id FROM paints WHERE shop = $1 AND color::text = 'red'"
        ));
    }

    #[test]
    fn allowlisted_system_views_prove_key_lookups() {
        let db = catalog();
        assert!(select_one(
            &db,
            "SELECT setting FROM pg_settings WHERE name = 'deadlock_timeout'"
        ));
        assert!(select_one(
            &db,
            "SELECT setting FROM pg_settings WHERE name = $1"
        ));
        assert!(select_one(
            &db,
            "SELECT rolname FROM pg_roles WHERE rolname = 'postgres'"
        ));
        assert!(select_one(
            &db,
            "SELECT rolname FROM pg_roles WHERE oid = 10"
        ));
        assert!(select_one(
            &db,
            "SELECT tablename FROM pg_tables \
             WHERE schemaname = 'public' AND tablename = 't'"
        ));
        assert!(select_one(
            &db,
            "SELECT viewname FROM pg_views \
             WHERE schemaname = 'public' AND viewname = 'v'"
        ));
        assert!(select_one(
            &db,
            "SELECT indexname FROM pg_indexes \
             WHERE schemaname = 'public' AND indexname = 'i'"
        ));
        assert!(select_one(
            &db,
            "SELECT usename FROM pg_user WHERE usename = 'postgres'"
        ));
        // A non-key column, an operator that isn't equality, and half a
        // composite key prove nothing.
        assert!(!select_one(
            &db,
            "SELECT setting FROM pg_settings WHERE category = 'x'"
        ));
        assert!(!select_one(
            &db,
            "SELECT setting FROM pg_settings WHERE name LIKE 'dead%'"
        ));
        assert!(!select_one(
            &db,
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public'"
        ));
        // A statistics view has no key PostgreSQL promises.
        assert!(!select_one(
            &db,
            "SELECT pid FROM pg_stat_activity WHERE pid = 1"
        ));
    }

    #[test]
    fn a_replaced_system_view_is_not_trusted() {
        let mut db = catalog();
        let sql = "SELECT viewname FROM pg_views \
                   WHERE schemaname = 'public' AND viewname = 'v'";
        assert!(select_one(&db, sql));
        // `CREATE OR REPLACE VIEW` keeps the OID, so the allowlist still
        // finds the seeded object; its rows are whatever the replacement
        // selects now.
        db.apply_sql(
            "CREATE OR REPLACE VIEW pg_catalog.pg_views AS
             SELECT n.nspname AS schemaname, c.relname AS viewname,
                    pg_get_userbyid(c.relowner) AS viewowner,
                    pg_get_viewdef(c.oid) AS definition
             FROM pg_class c, pg_namespace n",
        )
        .unwrap();
        assert!(!select_one(&db, sql));
        // A system view the migrations didn't touch stays trusted.
        assert!(select_one(
            &db,
            "SELECT name FROM pg_settings WHERE name = 'deadlock_timeout'"
        ));
    }

    #[test]
    fn a_user_view_named_like_a_system_view_is_not_trusted() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE settings (name text NOT NULL, setting text NOT NULL);
             CREATE VIEW public.pg_settings AS SELECT name, setting FROM settings;",
        )
        .unwrap();
        // Same name, another schema: not the system view.
        assert!(!select_one(
            &db,
            "SELECT setting FROM public.pg_settings WHERE name = 'deadlock_timeout'"
        ));
        // Any other view stays a relation the proof can't see through.
        assert!(!select_one(&db, "SELECT name FROM v WHERE id = $1"));
    }

    #[test]
    fn a_stable_call_of_determined_columns_is_a_value() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE owners (id bigint PRIMARY KEY, team bigint NOT NULL, tool_id bigint);
             CREATE TABLE tools (id bigint PRIMARY KEY, name text NOT NULL);
             CREATE TABLE keyed (id bigint PRIMARY KEY, code text NOT NULL UNIQUE);
             CREATE TABLE a_keys (id bigint PRIMARY KEY);
             CREATE TABLE b_keys (id bigint PRIMARY KEY);
             CREATE SEQUENCE pick_seq;
             CREATE FUNCTION pick_tool(o bigint) RETURNS bigint STABLE LANGUAGE sql
                AS $$ SELECT tool_id FROM owners WHERE id = o $$;
             CREATE FUNCTION pick_tool_volatile(o bigint) RETURNS bigint VOLATILE LANGUAGE sql
                AS $$ SELECT tool_id FROM owners WHERE id = o $$;
             CREATE FUNCTION pick_tools(o bigint) RETURNS SETOF bigint STABLE LANGUAGE sql
                AS $$ SELECT tool_id FROM owners WHERE id = o $$;
             CREATE FUNCTION pick_default(o bigint DEFAULT nextval('pick_seq'))
                RETURNS bigint STABLE LANGUAGE sql AS $$ SELECT o $$;
             CREATE FUNCTION pick_named(a bigint, b bigint) RETURNS bigint STABLE LANGUAGE sql
                AS $$ SELECT a + b $$;
             CREATE FUNCTION pick_variadic(VARIADIC xs bigint[]) RETURNS bigint STABLE LANGUAGE sql
                AS $$ SELECT xs[1] $$;",
        )
        .unwrap();
        // The issue's repro: `t.id` is equated to a STABLE call of the
        // determined `o` row.
        assert!(select_one(
            &db,
            "SELECT o.id, t.name FROM owners o \
             LEFT JOIN tools t ON t.id = pick_tool(o.id) WHERE o.id = $1"
        ));
        // The same facts in a comma join.
        assert!(select_one(
            &db,
            "SELECT o.id, t.name FROM owners o, tools t \
             WHERE o.id = $1 AND t.id = pick_tool(o.id)"
        ));
        // A cast of a parameter, and of a literal.
        assert!(select_one(
            &db,
            "SELECT id FROM tools WHERE id = $1::bigint"
        ));
        assert!(select_one(
            &db,
            "SELECT id FROM keyed WHERE code = 'a'::text"
        ));
        // An operator over accepted operands.
        assert!(select_one(
            &db,
            "SELECT id FROM tools WHERE id = ($1::bigint + 0)"
        ));
        // A parameter left to its default is replaced by the default
        // expression at the call site: `nextval` runs per evaluation, so the
        // value isn't fixed for the statement.
        assert!(!select_one(
            &db,
            "SELECT id FROM tools WHERE id = pick_default()"
        ));
        // Named notation may leave an earlier parameter to its default even
        // when the call spells `pronargs` arguments, so it isn't read.
        assert!(!select_one(
            &db,
            "SELECT id FROM tools WHERE id = pick_named(b => 1, a => 2)"
        ));
        // Every parameter spelled positionally, none defaulted: a value.
        assert!(select_one(
            &db,
            "SELECT id FROM tools WHERE id = pick_named(1, 2)"
        ));
        // A VARIADIC routine packs its arguments into an array through a
        // function this doesn't resolve, with or without the keyword.
        assert!(!select_one(
            &db,
            "SELECT id FROM tools WHERE id = pick_variadic(1)"
        ));
        assert!(!select_one(
            &db,
            "SELECT id FROM tools WHERE id = pick_variadic(VARIADIC ARRAY[1])"
        ));
        // A cast to a type with a typmod runs the type's length-coercion
        // function after the value is made; the bare type runs none.
        assert!(!select_one(
            &db,
            "SELECT id FROM keyed WHERE code = 'a'::varchar(10)"
        ));
        assert!(select_one(
            &db,
            "SELECT id FROM keyed WHERE code = 'a'::varchar"
        ));
        // The issue's LATERAL form: the subquery is proven one row per
        // `o` row, so the LEFT JOIN yields at most one.
        let lateral = "SELECT o.id, x.name FROM owners o \
             LEFT JOIN LATERAL \
                (SELECT t.name FROM tools t WHERE t.id = pick_tool(o.id)) x ON true \
             WHERE o.id = $1";
        assert!(select_one(&db, lateral));
        assert!(statement_one(&db, lateral));
        // A LATERAL subquery the proof can't bound stays a give-up.
        assert!(!select_one(
            &db,
            "SELECT o.id, x.name FROM owners o \
             LEFT JOIN LATERAL (SELECT t.name FROM tools t) x ON true \
             WHERE o.id = $1"
        ));
        // A LATERAL subquery's unqualified `id` is its own `USING`-merged
        // column, not the same-named outer `o.id`: `a.id = id` holds for
        // every pair the join matches, so the subquery returns a row per
        // pair and isn't one row.
        assert!(!select_one(
            &db,
            "SELECT o.id, x.id FROM owners o \
             JOIN LATERAL (SELECT a.id FROM a_keys a JOIN b_keys b USING (id) \
                           WHERE a.id = id AND b.id = a.id) x ON true \
             WHERE o.id = $1"
        ));
        // A LATERAL subquery with an aggregate of its own is one row.
        assert!(select_one(
            &db,
            "SELECT o.id, x.n FROM owners o \
             LEFT JOIN LATERAL (SELECT count(*) AS n FROM tools t) x ON true \
             WHERE o.id = $1"
        ));
        // A volatile call is re-evaluated per row pair: refused.
        assert!(!select_one(
            &db,
            "SELECT o.id, t.name FROM owners o \
             LEFT JOIN tools t ON t.id = pick_tool_volatile(o.id) WHERE o.id = $1"
        ));
        assert!(!select_one(
            &db,
            "SELECT o.id, t.name FROM owners o \
             LEFT JOIN tools t ON t.id = (random() * 10)::bigint WHERE o.id = $1"
        ));
        // A set-returning call multiplies rows wherever it runs.
        assert!(!select_one(
            &db,
            "SELECT pick_tools(o.id) FROM owners o WHERE o.id = $1"
        ));
    }

    #[test]
    fn an_inner_joins_on_facts_hold_at_the_level() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE owners (id bigint PRIMARY KEY, team bigint NOT NULL);
             CREATE TABLE kids (
                id bigint PRIMARY KEY,
                team bigint NOT NULL,
                slug text NOT NULL,
                UNIQUE (team, slug)
             );
             CREATE TABLE links (
                parent bigint NOT NULL,
                kid bigint NOT NULL,
                PRIMARY KEY (parent, kid)
             );
             CREATE TABLE a (id bigint PRIMARY KEY, k bigint);
             CREATE TABLE b (id bigint PRIMARY KEY, cid bigint, aid bigint, k bigint);
             CREATE TABLE c (id bigint PRIMARY KEY, k bigint);
             CREATE TABLE calls (
                conversation_id bigint NOT NULL,
                call_id bigint NOT NULL,
                type text NOT NULL
             );
             CREATE UNIQUE INDEX ON calls (conversation_id, call_id) WHERE type = 'result';
             CREATE TABLE sinks (call_id bigint PRIMARY KEY);",
        )
        .unwrap();
        // The issue's query: `l.kid = k.id` sits in `k`'s ON and `l.parent
        // = o.id` in `l`'s; both hold on every output row, so the key of
        // `links` can be completed from them.
        assert!(select_one(
            &db,
            "SELECT k.id FROM owners o \
             JOIN links l ON l.parent = o.id \
             JOIN kids k ON k.id = l.kid \
             WHERE o.id = $1 AND k.team = o.team AND k.slug = $2"
        ));
        // The same conditions with the joins in another order, and all in
        // WHERE.
        assert!(select_one(
            &db,
            "SELECT k.id FROM owners o \
             JOIN kids k ON k.team = o.team \
             JOIN links l ON l.parent = o.id AND l.kid = k.id \
             WHERE o.id = $1 AND k.slug = $2"
        ));
        assert!(select_one(
            &db,
            "SELECT k.id FROM owners o, links l, kids k \
             WHERE o.id = $1 AND l.parent = o.id AND k.id = l.kid \
               AND k.team = o.team AND k.slug = $2"
        ));
        // A parenthesized inner join's ON holds at this level too.
        assert!(select_one(
            &db,
            "SELECT k.id FROM owners o \
             JOIN (links l JOIN kids k ON k.id = l.kid) ON l.parent = o.id \
             WHERE o.id = $1 AND k.team = o.team AND k.slug = $2"
        ));
        // An inner join inside a LEFT join's nullable side keeps its scoped
        // treatment: its ON holds only on the rows the outer join matches —
        // and PG doesn't even let it reference the preserved side.
        assert!(!select_one(
            &db,
            "SELECT a.id FROM a \
             LEFT JOIN (b JOIN c ON c.id = b.cid AND c.k = b.k) ON b.aid = a.id \
             WHERE a.id = $1"
        ));
        // The same beneath a RIGHT join's nullable side, which is its left:
        // a null-extended row never evaluates the inner ON either.
        assert!(!select_one(
            &db,
            "SELECT a.id FROM (b JOIN c ON c.id = b.cid AND c.k = b.k) \
             RIGHT JOIN a ON b.aid = a.id \
             WHERE a.id = $1"
        ));
        // A partial index's predicate written in an inner join's ON holds
        // on every output row like one written in WHERE.
        assert!(select_one(
            &db,
            "SELECT c.call_id FROM calls c \
             JOIN sinks s ON s.call_id = c.call_id AND c.type = 'result' \
             WHERE c.conversation_id = $1 AND c.call_id = $2"
        ));
        assert!(!select_one(
            &db,
            "SELECT c.call_id FROM calls c \
             JOIN sinks s ON s.call_id = c.call_id AND c.type = 'other' \
             WHERE c.conversation_id = $1 AND c.call_id = $2"
        ));
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
    fn an_aggregate_over_a_from_subquery_is_one_row() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE a (id bigint PRIMARY KEY, due timestamptz);
             CREATE TABLE b (id bigint PRIMARY KEY, due timestamptz);",
        )
        .unwrap();
        // A top-level level has no other level an aggregate written in it
        // could belong to, whatever its arguments read.
        assert!(statement_one(
            &db,
            "SELECT min(due) AS soonest FROM (SELECT due FROM a) q"
        ));
        assert!(statement_one(
            &db,
            "SELECT min(due) AS soonest FROM \
             (SELECT due FROM a UNION ALL SELECT due FROM b) q"
        ));
        assert!(statement_one(&db, "SELECT min(due) AS soonest FROM a"));
        // A top-level CTE body is judged the same way.
        assert!(statement_one(
            &db,
            "WITH c AS (SELECT min(due) AS soonest FROM (SELECT due FROM a) q) \
             SELECT soonest FROM c"
        ));
        // A top-level set-operation branch too.
        let union = view(
            &db,
            "SELECT min(due) AS x FROM (SELECT due FROM a) q \
             UNION ALL SELECT now()",
        );
        let arm = top(&union).larg.as_deref().expect("left arm");
        assert!(select_at_most_one_row(&db, &union, arm));
        // A nested level's aggregate over an outer column stays the outer
        // level's: the inner select returns a row per `b` row, not one.
        let outer = view(
            &db,
            "SELECT (SELECT min(o.id) FROM (SELECT due FROM b) s) FROM a o",
        );
        let aggregate = outer
            .tree
            .nodes()
            .into_iter()
            .find_map(|(node, _)| match node {
                NodeRef::SelectStmt(select)
                    if select.target_list.iter().any(|target| {
                        matches!(&target.node, Some(Node::ResTarget(target))
                            if matches!(target.val.as_deref().and_then(|v| v.node.as_ref()),
                                Some(Node::FuncCall(call))
                                    if string_list(&call.funcname) == ["min"]))
                    }) =>
                {
                    Some(select)
                }
                _ => None,
            })
            .expect("aggregate select");
        assert!(!select_at_most_one_row(&db, &outer, aggregate));
        // The same at a nested level with a FROM subquery: the argument
        // resolves against the subquery's output, so the aggregate is this
        // level's and the inner select is one row.
        let nested = view(
            &db,
            "SELECT (SELECT min(s.due) FROM (SELECT due FROM a) s) FROM b o",
        );
        let aggregate = nested
            .tree
            .nodes()
            .into_iter()
            .find_map(|(node, _)| match node {
                NodeRef::SelectStmt(select)
                    if select.target_list.iter().any(|target| {
                        matches!(&target.node, Some(Node::ResTarget(target))
                            if matches!(target.val.as_deref().and_then(|v| v.node.as_ref()),
                                Some(Node::FuncCall(call))
                                    if string_list(&call.funcname) == ["min"]))
                    }) =>
                {
                    Some(select)
                }
                _ => None,
            })
            .expect("aggregate select");
        assert!(select_at_most_one_row(&db, &nested, aggregate));
    }

    #[test]
    fn a_nested_ctes_aggregate_may_belong_to_the_level_around_it() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE u (id integer);
             CREATE TABLE v (id integer);
             CREATE TABLE keyed (id integer PRIMARY KEY);",
        )
        .unwrap();
        // The issue's repro: `count(u.id)` is the *outer* level's
        // aggregate, so `c` has one row per `v` row and the ARRAY subquery
        // has more than one.
        let repro = view(
            &db,
            "SELECT ARRAY(WITH c AS (SELECT count(u.id) AS n FROM v) \
             SELECT n FROM c) FROM u",
        );
        assert!(!select_at_most_one_row(
            &db,
            &repro,
            array_select(top(&repro))
        ));
        // The same aggregate written only in the CTE body's ORDER BY.
        let ordered = view(
            &db,
            "SELECT ARRAY(WITH c AS (SELECT v.id FROM v ORDER BY count(u.id)) \
             SELECT 1 FROM c) FROM u",
        );
        assert!(!select_at_most_one_row(
            &db,
            &ordered,
            array_select(top(&ordered))
        ));
        // And one written only in a window function's FILTER.
        let filtered = view(
            &db,
            "SELECT ARRAY(WITH c AS (SELECT v.id, \
                sum(v.id) FILTER (WHERE count(u.id) > 0) OVER () AS s FROM v) \
             SELECT 1 FROM c) FROM u",
        );
        assert!(!select_at_most_one_row(
            &db,
            &filtered,
            array_select(top(&filtered))
        ));
        // An aggregate over the nested body's own FROM is still its own.
        let own = view(
            &db,
            "SELECT ARRAY(WITH c AS (SELECT count(v.id) AS n FROM v) \
             SELECT n FROM c) FROM u",
        );
        assert!(select_at_most_one_row(&db, &own, array_select(top(&own))));
        // A top-level CTE's aggregate still collapses its body to one row.
        assert!(statement_one(
            &db,
            "WITH c AS (SELECT count(v.id) AS n FROM v) SELECT n FROM c"
        ));
        assert!(statement_one(
            &db,
            "WITH c AS (SELECT min(id) AS n FROM keyed) SELECT n FROM c"
        ));
    }

    #[test]
    fn a_from_subquery_proven_one_row_is_one_relation() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql("CREATE TABLE a (id bigint PRIMARY KEY, x int);")
            .unwrap();
        // A single-row VALUES list, named by its alias column list.
        assert!(select_one(&db, "SELECT v FROM (VALUES (1)) AS one(v)"));
        // An aggregate over a table, named by the target alias.
        assert!(select_one(
            &db,
            "SELECT s.m FROM (SELECT min(x) AS m FROM a) s"
        ));
        // A key lookup the subquery bounds, with no LATERAL.
        assert!(select_one(
            &db,
            "SELECT s.x FROM (SELECT x FROM a WHERE id = $1) s"
        ));
        // A single-row CTE-less derived table in a join.
        assert!(select_one(
            &db,
            "SELECT a.id FROM a JOIN (SELECT 1 AS x) s ON s.x = a.x WHERE a.id = $1"
        ));
        // More than one VALUES row, a keyless subquery, and a subquery whose
        // target names aren't certain (so a reference can't bind to it) stay
        // unproven.
        assert!(!select_one(
            &db,
            "SELECT v FROM (VALUES (1), (2)) AS two(v)"
        ));
        assert!(!select_one(&db, "SELECT x FROM (SELECT x FROM a) s"));
        assert!(!select_one(
            &db,
            "SELECT s.id FROM (SELECT * FROM a) s WHERE s.id = $1"
        ));
    }

    #[test]
    fn a_from_subquery_with_its_own_with_uses_it() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql("CREATE TABLE a (id bigint PRIMARY KEY, x int);")
            .unwrap();
        // The review's repro: the subquery's own `c` (two rows) shadows the
        // outer one-row `c`. Proving the body against the outer environment
        // would read the outer CTE and call this one row.
        assert!(!select_one(
            &db,
            "WITH c(x) AS (VALUES (0)) \
             SELECT x FROM (WITH c(x) AS (VALUES (1), (2)) SELECT x FROM c) AS s"
        ));
        // Without the inner WITH the outer one-row CTE proves it, so the
        // shadowing case above is not a proof that always fails.
        assert!(select_one(
            &db,
            "WITH c(x) AS (VALUES (0)) SELECT x FROM (SELECT x FROM c) AS s"
        ));
        // An inner one-row `c` shadows the outer one just the same.
        assert!(select_one(
            &db,
            "WITH c(x) AS (VALUES (0)) \
             SELECT x FROM (WITH c(x) AS (VALUES (1)) SELECT x FROM c) AS s"
        ));
        // The SELECT an INSERT reads may carry its own WITH too, with the
        // same shadowing: the statement inserts two rows, not one.
        assert!(!statement_one(
            &db,
            "WITH c(x) AS (VALUES (0)) \
             INSERT INTO a (id) WITH c(x) AS (VALUES (1), (2)) SELECT x FROM c"
        ));
    }

    #[test]
    fn a_distinct_constant_projection_is_one_row() {
        let db = catalog();
        assert!(select_one(&db, "SELECT DISTINCT 1 AS present FROM users"));
        assert!(select_one(
            &db,
            "SELECT DISTINCT $1::text AS present FROM users"
        ));
        assert!(select_one(&db, "SELECT DISTINCT 1, 'x' FROM users"));
        assert!(select_one(&db, "SELECT DISTINCT 1 + 2 FROM users"));
        assert!(select_one(&db, "SELECT DISTINCT now() FROM users"));
        assert!(select_one(&db, "SELECT DISTINCT count(*) FROM users"));
        // A column can differ from row to row.
        assert!(!select_one(&db, "SELECT DISTINCT id FROM users"));
        assert!(!select_one(&db, "SELECT DISTINCT id + 1 FROM users"));
        // Volatile calls, window functions and set-returning calls.
        assert!(!select_one(&db, "SELECT DISTINCT random() FROM users"));
        assert!(!select_one(
            &db,
            "SELECT DISTINCT row_number() OVER () FROM users"
        ));
        assert!(!select_one(
            &db,
            "SELECT DISTINCT generate_series(1, 3) FROM users"
        ));
        // `DISTINCT ON` is not a plain `DISTINCT`.
        assert!(!select_one(&db, "SELECT DISTINCT ON (id) 1 FROM users"));
    }

    #[test]
    fn an_empty_grouping_set_is_one_row() {
        let db = catalog();
        assert!(select_one(&db, "SELECT count(*) FROM users GROUP BY ()"));
        assert!(select_one(&db, "SELECT 1 FROM users GROUP BY ()"));
        assert!(statement_one(&db, "SELECT count(*) FROM users GROUP BY ()"));
        // A repeated empty set is two groups.
        assert!(!select_one(
            &db,
            "SELECT count(*) FROM users GROUP BY GROUPING SETS ((), ())"
        ));
        // Any real grouping is a group per key.
        assert!(!select_one(&db, "SELECT count(*) FROM users GROUP BY id"));
        assert!(!select_one(&db, "SELECT 1 FROM users GROUP BY (), id"));
    }

    #[test]
    fn an_aggregate_argument_resolves_against_derived_tables() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE m (
                tenant_id bigint NOT NULL,
                user_id bigint NOT NULL,
                role text NOT NULL,
                PRIMARY KEY (tenant_id, user_id)
             );
             CREATE TABLE u (id bigint PRIMARY KEY);",
        )
        .unwrap();
        let aggregate_is_one_row = |sql: &str| {
            let view = view(&db, sql);
            let select = view
                .tree
                .nodes()
                .into_iter()
                .find_map(|(node, _)| match node {
                    NodeRef::SelectStmt(select)
                        if select.target_list.iter().any(|target| {
                            matches!(&target.node, Some(Node::ResTarget(target))
                                if matches!(target.val.as_deref().and_then(|v| v.node.as_ref()),
                                    Some(Node::FuncCall(call))
                                        if string_list(&call.funcname) == ["count"]))
                        }) =>
                    {
                        Some(select)
                    }
                    _ => None,
                })
                .expect("count select");
            select_at_most_one_row(&db, &view, select)
        };
        // The issue's repro: `role` resolves to the FROM subquery's output,
        // so the aggregate belongs to this level and collapses it to one row.
        assert!(aggregate_is_one_row(
            "SELECT (SELECT count(DISTINCT role) FROM (SELECT role FROM m) s) FROM u"
        ));
        // A CTE reference's columns resolve the same way, through an alias
        // column list too.
        assert!(aggregate_is_one_row(
            "WITH c AS (SELECT role FROM m) \
             SELECT (SELECT count(c.role) FROM c) FROM u"
        ));
        assert!(aggregate_is_one_row(
            "WITH c(role_name) AS (SELECT role FROM m) \
             SELECT (SELECT count(c.role_name) FROM c) FROM u"
        ));
        // A reference to the outer level stays that level's aggregate: the
        // inner select then returns a row per `u` row.
        assert!(!aggregate_is_one_row(
            "SELECT (SELECT count(u.id) FROM (SELECT role FROM m) s) FROM u"
        ));
        // A subquery output name that isn't certain refuses placement.
        assert!(!aggregate_is_one_row(
            "SELECT (SELECT count(\"?column?\") FROM (SELECT role || '' FROM m) s) FROM u"
        ));
    }

    #[test]
    fn a_cast_or_call_output_name_is_not_trusted() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE src (v int);
             CREATE TABLE outer_rows (\"?column?\" int);",
        )
        .unwrap();
        let aggregate_is_one_row = |sql: &str| {
            let view = view(&db, sql);
            let select = view
                .tree
                .nodes()
                .into_iter()
                .find_map(|(node, _)| match node {
                    NodeRef::SelectStmt(select)
                        if select.target_list.iter().any(|target| {
                            matches!(&target.node, Some(Node::ResTarget(target))
                                if matches!(target.val.as_deref().and_then(|v| v.node.as_ref()),
                                    Some(Node::FuncCall(call))
                                        if string_list(&call.funcname) == ["count"]))
                        }) =>
                    {
                        Some(select)
                    }
                    _ => None,
                })
                .expect("count select");
            select_at_most_one_row(&db, &view, select)
        };
        // The review's repro: PG names `v::int` `v`, so `"?column?"` is the
        // outer relation's column. The count is then the outer level's
        // aggregate and the inner select returns one row per `src` row.
        assert!(!aggregate_is_one_row(
            "WITH c AS (SELECT v::int FROM src) \
             SELECT ARRAY(SELECT count(\"?column?\") FROM c) FROM outer_rows"
        ));
        // A function call's name is not trusted either, even when PG names
        // the column after the function: the relation's columns stay
        // uncertain, so the count can't be placed on this level.
        assert!(!aggregate_is_one_row(
            "WITH c AS (SELECT lower(v::text) FROM src) \
             SELECT ARRAY(SELECT count(\"lower\") FROM c) FROM outer_rows"
        ));
        // An explicit alias makes the name certain, so the same aggregate is
        // placed on this level and collapses it.
        assert!(aggregate_is_one_row(
            "WITH c(x) AS (SELECT v::int FROM src) \
             SELECT ARRAY(SELECT count(x) FROM c) FROM outer_rows"
        ));
        // An alias column list on the derived table does the same.
        assert!(aggregate_is_one_row(
            "SELECT ARRAY(SELECT count(x) \
              FROM (SELECT v::int FROM src) AS c(x)) FROM outer_rows"
        ));
    }

    #[test]
    fn a_set_operation_or_repeated_grouping_set_is_not_one_row() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql("CREATE TABLE keyed (id integer PRIMARY KEY);")
            .unwrap();
        // Each arm matches at most one row; the UNION ALL emits both.
        assert!(!statement_one(
            &db,
            "SELECT id FROM keyed WHERE id = $1 \
             UNION ALL SELECT id FROM keyed WHERE id = $1"
        ));
        // The empty grouping set written twice produces two groups.
        assert!(!statement_one(
            &db,
            "SELECT count(*) FROM keyed GROUP BY GROUPING SETS ((), ())"
        ));
        assert!(!select_one(
            &db,
            "SELECT count(*) FROM keyed GROUP BY GROUPING SETS ((), ())"
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
    fn a_cte_whose_columns_cant_all_be_listed_types_none_of_them() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE fdst (v float8);
             CREATE TABLE bids (id bigint PRIMARY KEY);",
        )
        .unwrap();
        // `n.*` names NEW under another alias: `k` must not take the type
        // of the column after it (two bigints can equal one float8).
        assert!(!statement_one(
            &db,
            "WITH r(k) AS (INSERT INTO fdst VALUES (9007199254740992) \
             RETURNING WITH (NEW AS n) n.*, 1 AS tail) \
             SELECT bids.id FROM r JOIN bids ON bids.id = r.k"
        ));
        assert!(!statement_one(
            &db,
            "WITH r(k) AS (INSERT INTO fdst VALUES (9007199254740992) RETURNING old.*, 1 AS tail) \
             SELECT bids.id FROM r JOIN bids ON bids.id = r.k"
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
        // `DEFAULT VALUES` writes exactly one row.
        assert!(statement_one(&db, "INSERT INTO defaults DEFAULT VALUES"));
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
    fn update_from_and_delete_using_need_only_a_determined_target() {
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
        // The target is determined even though the FROM / USING relation
        // matches many rows: the target row is written once.
        assert!(statement_one(
            &db,
            "UPDATE users SET name = 'x' FROM orgs o WHERE users.id = $1",
        ));
        assert!(statement_one(
            &db,
            "DELETE FROM users USING orgs o WHERE users.id = $1",
        ));
        // The target is not determined by an undetermined FROM relation.
        assert!(!statement_one(
            &db,
            "UPDATE users SET name = o.name FROM orgs o WHERE users.id = o.id",
        ));
        assert!(!statement_one(
            &db,
            "DELETE FROM users USING orgs o WHERE users.id = o.id",
        ));
    }

    #[test]
    fn a_full_join_is_never_one_row_even_between_single_row_sides() {
        let db = catalog();
        assert!(!select_one(
            &db,
            "WITH a AS MATERIALIZED (SELECT 1 AS x), b AS MATERIALIZED (SELECT 2 AS y) \
             SELECT a.x, b.y FROM a FULL JOIN b ON a.x = b.y",
        ));
        // LEFT / RIGHT with the nullable side determined stay fine.
        assert!(select_one(
            &db,
            "WITH a AS MATERIALIZED (SELECT 1 AS x) \
             SELECT u.id FROM users u LEFT JOIN a ON a.x = u.id WHERE u.id = $1",
        ));
    }

    #[test]
    fn an_aggregate_over_outer_columns_is_the_outer_levels() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE u (id bigint PRIMARY KEY);
             CREATE TABLE v (id bigint PRIMARY KEY);",
        )
        .unwrap();
        // `count(u.id)` reads only the outer `u`, so the inner select is not
        // an aggregate query: it returns one row per `v` row.
        let check_inner = |sql: &str| {
            let view = view(&db, sql);
            let selects: Vec<&SelectStmt> = view
                .tree
                .nodes()
                .into_iter()
                .filter_map(|(node, _)| match node {
                    NodeRef::SelectStmt(select) => Some(select),
                    _ => None,
                })
                .collect();
            assert_eq!(selects.len(), 2, "{sql}");
            select_at_most_one_row(&db, &view, selects[1])
        };
        assert!(!check_inner(
            "SELECT (SELECT count(u.id) FROM v LIMIT 1) FROM u",
        ));
        // `count(*)` and an aggregate over the inner FROM are its own.
        assert!(check_inner(
            "SELECT (SELECT count(*) FROM v LIMIT 1) FROM u",
        ));
        assert!(check_inner("SELECT (SELECT count(v.id) FROM v) FROM u"));
    }

    #[test]
    fn a_partial_index_predicate_resolves_against_the_tables_real_names() {
        let db = catalog();
        // The alias renames attnum 2 (`active`) to `inactive` and attnum 3
        // (`inactive`) to `active`: the stored predicate's `active` is the
        // boolean column, not the query's `x.active` (an int).
        assert!(!select_one(
            &db,
            "SELECT x.k FROM alias_partial AS x(k, inactive, active) \
             WHERE x.k = 1 AND x.active",
        ));
        // `x.inactive` is the table's `active` boolean, so the predicate
        // holds.
        assert!(select_one(
            &db,
            "SELECT x.k FROM alias_partial AS x(k, inactive, active) \
             WHERE x.k = 1 AND x.inactive",
        ));
        assert!(select_one(
            &db,
            "SELECT k FROM alias_partial WHERE k = 1 AND active",
        ));
    }

    #[test]
    fn a_sublink_with_its_own_query_levels_is_not_a_fixed_value() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE TABLE u (id bigint PRIMARY KEY);
             CREATE TABLE v (id bigint PRIMARY KEY);",
        )
        .unwrap();
        // The CTE body's `id` binds to `u.id`, outside the sublink, not to
        // `v`; the old single-scope read took it as local.
        assert!(!select_one(
            &db,
            "SELECT u.id FROM u WHERE u.id = ( \
                 WITH c AS (SELECT id) \
                 SELECT v.id FROM v WHERE v.id = (SELECT id FROM c) LIMIT 1 \
             )",
        ));
        // A nested sublink can read the level being proven too.
        assert!(!select_one(
            &db,
            "SELECT u.id FROM u \
             WHERE u.id = (SELECT v.id FROM v WHERE v.id = (SELECT u.id))",
        ));
        // A plain uncorrelated scalar subquery still counts.
        assert!(select_one(
            &db,
            "SELECT u.id FROM u WHERE u.id = (SELECT v.id FROM v WHERE v.id = $1)",
        ));
    }

    #[test]
    fn a_returning_star_from_another_relation_has_unknown_columns() {
        let db = catalog();
        // `s.*` expands the FROM relation, not the target; its columns keep
        // the CTE single-row but carry no type, so `b.id` can't equate the
        // joined key.
        assert!(!statement_one(
            &db,
            "WITH r AS ( \
                 UPDATE dst d SET z = 1 FROM src s WHERE d.id = $1 RETURNING s.* \
             ) \
             SELECT b.id FROM r b JOIN other o ON o.id = b.id",
        ));
        // A bare `RETURNING *` with a FROM merges the FROM columns too, so
        // even a from relation with no columns leaves the star unknown.
        assert!(!statement_one(
            &db,
            "WITH r AS ( \
                 UPDATE dst SET z = 1 FROM src_empty WHERE dst.id = $1 RETURNING * \
             ) \
             SELECT b.id FROM r b JOIN other o ON o.id = b.id",
        ));
        // A target-qualified star is still the target's typed columns.
        assert!(statement_one(
            &db,
            "WITH r AS ( \
                 UPDATE dst d SET z = 1 FROM src s WHERE d.id = $1 RETURNING d.* \
             ) \
             SELECT b.id FROM r b JOIN other o ON o.id = b.id",
        ));
    }

    #[test]
    fn an_update_from_target_is_one_row_even_with_an_undetermined_source() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "UPDATE dst SET z = 1 FROM src WHERE dst.id = $1 RETURNING dst.id",
        ));
        assert!(statement_one(
            &db,
            "DELETE FROM dst USING src WHERE dst.id = $1 RETURNING dst.id",
        ));
        assert!(!statement_one(
            &db,
            "UPDATE dst SET z = 1 FROM src WHERE dst.id = src.dst_id RETURNING dst.id",
        ));
        assert!(!statement_one(
            &db,
            "DELETE FROM dst USING src WHERE dst.id = src.dst_id RETURNING dst.id",
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

    #[test]
    fn a_data_modifying_statement_on_a_rewritten_relation_is_not_one_row() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE destination (id integer);
             CREATE VIEW incoming AS SELECT id FROM destination;
             CREATE RULE incoming_insert AS ON INSERT TO incoming
                DO INSTEAD INSERT INTO destination
                    SELECT n FROM generate_series(1, 2) AS g(n) RETURNING id;
             CREATE TABLE ruled (id integer PRIMARY KEY);
             CREATE RULE ruled_noop AS ON UPDATE TO ruled DO INSTEAD NOTHING;
             CREATE TABLE ruled_parent (id integer PRIMARY KEY);
             CREATE RULE ruled_parent_noop AS ON DELETE TO ruled_parent DO INSTEAD NOTHING;
             CREATE VIEW auto_updatable AS SELECT id FROM ruled_parent;",
        )
        .unwrap();
        // The issue's repro: the view's INSTEAD rule inserts two rows.
        assert!(!statement_one(
            &db,
            "INSERT INTO incoming VALUES (99) RETURNING id"
        ));
        // An auto-updatable view over a table with a rule is refused too.
        assert!(!statement_one(
            &db,
            "DELETE FROM auto_updatable WHERE id = 1"
        ));
        // A table with a rule of its own: its `RETURNING` is the rule's.
        assert!(!statement_one(&db, "UPDATE ruled SET id = 2 WHERE id = 1"));
        assert!(!statement_one(&db, "DELETE FROM ruled_parent WHERE id = 1"));
        // Without rules the same statements stay proven.
        assert!(statement_one(
            &db,
            "INSERT INTO destination VALUES (99) RETURNING id"
        ));
    }

    #[test]
    fn insert_default_values_is_one_row() {
        let db = catalog();
        assert!(statement_one(
            &db,
            "INSERT INTO defaults DEFAULT VALUES RETURNING id"
        ));
        assert!(statement_one(
            &db,
            "INSERT INTO defaults DEFAULT VALUES ON CONFLICT DO NOTHING RETURNING id"
        ));
    }

    #[test]
    fn do_nothing_never_proves_a_multirow_values_insert() {
        let mut db = catalog();
        db.apply_sql(
            "CREATE TABLE dup_keys (id integer PRIMARY KEY, v text);
             CREATE TABLE dup_nulls (k integer UNIQUE, v text);
             CREATE TABLE dup_deferred (k integer UNIQUE DEFERRABLE, v text);
             CREATE TABLE dup_partial (k integer, v text);
             CREATE UNIQUE INDEX ON dup_partial (k) WHERE v IS NOT NULL;
             CREATE TABLE dup_trigger (k integer PRIMARY KEY);
             CREATE TABLE dup_infer (id integer PRIMARY KEY, v text UNIQUE);
             CREATE TABLE dup_identity (
                id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v text
             );
             CREATE TABLE dup_cast (k text PRIMARY KEY, v text);
             CREATE TABLE dup_volatile (id integer PRIMARY KEY, v text);
             CREATE FUNCTION dup_bump() RETURNS trigger LANGUAGE plpgsql
                AS $$ BEGIN NEW.k := NEW.k + 1; RETURN NEW; END $$;
             CREATE TRIGGER dup_before BEFORE INSERT ON dup_trigger
                FOR EACH ROW EXECUTE FUNCTION dup_bump();
             CREATE FUNCTION dup_cast_fn(integer) RETURNS text LANGUAGE sql VOLATILE
                AS $$ SELECT $1::text || random()::text $$;
             CREATE CAST (integer AS text)
                WITH FUNCTION dup_cast_fn(integer) AS ASSIGNMENT;
             CREATE FUNCTION dup_wipe() RETURNS text LANGUAGE plpgsql VOLATILE
                AS $$ BEGIN DELETE FROM dup_volatile; RETURN 'x'; END $$;",
        )
        .unwrap();
        // A duplicate key may make `DO NOTHING` skip the later row, but the
        // statement still writes one row per `VALUES` row: an identity the
        // `OVERRIDING USER VALUE` clause replaces, an assignment cast's
        // function may map equal literals to different keys, a volatile
        // non-key expression may delete the earlier row before the later
        // row's uniqueness check (its `RETURNING` row is already counted),
        // and any other key shape may simply not conflict. None of these is
        // proven one row.
        for sql in [
            // Every row gives the primary key the same literal.
            "INSERT INTO dup_keys (id, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT DO NOTHING RETURNING id",
            "INSERT INTO dup_keys (id, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT (id) DO NOTHING RETURNING id",
            // Different keys: both rows insert.
            "INSERT INTO dup_keys (id, v) VALUES (1, 'a'), (2, 'b') \
             ON CONFLICT DO NOTHING RETURNING id",
            // Only the non-key column is equal.
            "INSERT INTO dup_keys (id, v) VALUES (1, 'a'), (2, 'a') \
             ON CONFLICT DO NOTHING RETURNING id",
            // `DO UPDATE` returns the rows that conflicted too.
            "INSERT INTO dup_keys (id, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT (id) DO UPDATE SET v = excluded.v RETURNING id",
            // NULLs never conflict under a unique index.
            "INSERT INTO dup_nulls (k, v) VALUES (NULL, 'a'), (NULL, 'b') \
             ON CONFLICT DO NOTHING RETURNING k",
            // A deferrable key is checked at commit, not by `DO NOTHING`.
            "INSERT INTO dup_deferred (k, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT DO NOTHING RETURNING k",
            // A partial index does not cover both rows.
            "INSERT INTO dup_partial (k, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT DO NOTHING RETURNING k",
            // A BEFORE INSERT trigger may rewrite the key.
            "INSERT INTO dup_trigger (k) VALUES (1), (1) \
             ON CONFLICT DO NOTHING RETURNING k",
            // A parameter is not a literal the parser fixes once.
            "INSERT INTO dup_keys (id, v) VALUES ($1, 'a'), ($1, 'b') \
             ON CONFLICT DO NOTHING RETURNING id",
            // The infer list must name the key's columns.
            "INSERT INTO dup_infer (id, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT (v) DO NOTHING RETURNING id",
            // The identity generates a different key for the second row.
            "INSERT INTO dup_identity (id, v) OVERRIDING USER VALUE \
             VALUES (1, 'a'), (1, 'b') ON CONFLICT DO NOTHING RETURNING id",
            // The assignment cast can turn both literals into different keys.
            "INSERT INTO dup_cast (k, v) VALUES (1, 'a'), (1, 'b') \
             ON CONFLICT DO NOTHING RETURNING k",
            // A volatile non-key call removes the earlier row first.
            "INSERT INTO dup_volatile (id, v) VALUES (1, 'a'), (1, dup_wipe()) \
             ON CONFLICT DO NOTHING RETURNING id",
        ] {
            assert!(!statement_one(&db, sql), "{sql}");
        }
    }
}
