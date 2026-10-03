//! Top-level query analysis: lex the SQL template, parse it, walk the AST,
//! and produce an [`AnalyzedQuery`] combining lexer positions with inferred
//! types.

use std::collections::HashMap;

use typedpg_pg_query::protobuf::{self, CmdType, JoinType, SetOperation, node};

use crate::error::AnalyzeError;
use crate::expr::{self, Ctx, TypeGoal};
use crate::functions;
use crate::grouping;
use crate::nullability::{self, NullabilityContext};
use crate::oid::PgTypeOid;
use crate::param::LexOutput;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{AttIdentity, ConType, PgCatalog, TypCategory, TypType, oid};
use crate::qualified_name::QualifiedName;
use crate::scope::{Scope, ScopeColumn};
use crate::types::Type;

/// Internal parameter representation produced by [`analyze_static`] before
/// being fused with lexer-side info (name, sql offsets) into [`AnalyzedParam`].
pub(crate) struct ParamInfo {
    pub pg_type: Type,
    pub nullable: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────────────────────

/// A single output column of an analyzed query.
#[derive(Debug, Clone)]
pub struct AnalyzedColumn {
    pub name: String,
    pub pg_type: Type,
    pub nullable: bool,
}

/// A named query parameter (`$name`) with lexer position plus inferred type.
#[derive(Debug, Clone)]
pub struct AnalyzedParam {
    /// Parameter name without the `$` prefix and `?`/`!` suffix.
    pub name: String,
    /// Byte offsets in [`AnalyzedQuery::sql`] immediately after each `$N`
    /// placeholder for this parameter. Used by code generators to insert
    /// type casts (e.g. `::jsonb`). A param referenced multiple times has
    /// multiple offsets.
    pub sql_offsets: Vec<usize>,
    pub pg_type: Type,
    pub nullable: bool,
}

/// A field inside a spread parameter (`$..name { field1, field2 }`), with
/// inferred type.
#[derive(Debug, Clone)]
pub struct AnalyzedSpreadField {
    pub name: String,
    pub pg_type: Type,
    pub nullable: bool,
}

/// A spread parameter (`$..name { ... }`) with its offset in the rewritten SQL
/// and the typed field list.
#[derive(Debug, Clone)]
pub struct AnalyzedSpread {
    pub name: String,
    /// Byte offset in [`AnalyzedQuery::sql`] where the expanded
    /// `($N, $M, ...), ...` placeholders should be inserted.
    pub offset: usize,
    pub fields: Vec<AnalyzedSpreadField>,
}

/// The full result of analyzing a SQL query template.
#[derive(Debug, Clone)]
pub struct AnalyzedQuery {
    /// SQL rewritten with positional placeholders (`$1`, `$2`, …). Spread
    /// tokens are removed; the caller must expand them at each spread's
    /// [`AnalyzedSpread::offset`].
    pub sql: String,
    pub params: Vec<AnalyzedParam>,
    pub spreads: Vec<AnalyzedSpread>,
    pub columns: Vec<AnalyzedColumn>,
    /// True when the query is safe to embed as the body of a subquery
    /// (`SELECT * FROM (<query>) …`). False for top-level
    /// `INSERT`/`UPDATE`/`DELETE`/`MERGE`, for utility statements like
    /// `EXPLAIN`/`NOTIFY`/`LISTEN`/`UNLISTEN`, and for `WITH …
    /// (INSERT/UPDATE/DELETE/MERGE …) SELECT …` — PG only accepts a
    /// data-modifying CTE at the top level, not nested in a subquery.
    pub can_run_as_subquery: bool,
}

/// Build a "sample" SQL for analysis when the query contains spreads.
///
/// Replaces each spread insertion point with a single row of positional
/// placeholders numbered after the last regular parameter. Field mapping is
/// mandatory for spreads, so `fields.len()` gives the column count.
///
/// Returns [`AnalyzeError::Invalid`] for a spread written without a field
/// list (`$..items` rather than `$..items { a, b }`): the lexer accepts the
/// bare form, but nothing then says which item fields fill which columns.
///
/// Returned as a [`LexOutput`] over the sample SQL whose rewrites map its
/// offsets back to the original SQL — each spread's placeholder row maps
/// onto the `$..name` token — so diagnostics render against what the user
/// wrote.
pub(crate) fn build_spread_sample_sql(lex_output: &LexOutput) -> Result<LexOutput, AnalyzeError> {
    let sql = spread_sample_sql(lex_output)?;
    // The lexer records a zero-length rewrite for each spread, in the
    // spreads' order; the sample replaces it with the placeholder row.
    let mut rows = lex_output
        .spreads
        .iter()
        .map(|s| s.fields.as_ref().map_or(0, Vec::len));
    let mut shift = 0usize;
    let mut counter = lex_output.params.len();
    let mut rewrites = Vec::with_capacity(lex_output.rewrites.len());
    for rw in &lex_output.rewrites {
        let mut out = rw.clone();
        out.post_lex_at += shift;
        if rw.post_lex_len == 0
            && let Some(n) = rows.next()
        {
            // `(` + n placeholders `$k` joined by `, ` + `)`, as
            // `spread_sample_sql` writes them.
            let row_len = 2
                + (0..n)
                    .map(|i| {
                        counter += 1;
                        1 + counter.to_string().len() + if i > 0 { 2 } else { 0 }
                    })
                    .sum::<usize>();
            out.post_lex_len = row_len;
            shift += row_len;
        }
        rewrites.push(out);
    }
    debug_assert_eq!(lex_output.sql.len() + shift, sql.len());
    Ok(LexOutput {
        sql,
        params: lex_output.params.clone(),
        spreads: lex_output.spreads.clone(),
        rewrites,
    })
}

/// The text of [`build_spread_sample_sql`].
fn spread_sample_sql(lex_output: &LexOutput) -> Result<String, AnalyzeError> {
    let base_sql = &lex_output.sql;
    let num_regular_params = lex_output.params.len();
    let mut result = String::with_capacity(base_sql.len() + 64);
    let mut last_offset = 0;
    let mut param_counter = num_regular_params;

    for spread in &lex_output.spreads {
        result.push_str(&base_sql[last_offset..spread.offset]);
        let fields = spread.fields.as_ref().ok_or_else(|| {
            AnalyzeError::Invalid(format!(
                "spread `$..{0}` needs a field list naming the item fields to bind, \
                 e.g. `$..{0} {{ field1, field2 }}`",
                spread.name
            ))
        })?;
        result.push('(');
        for (i, _) in fields.iter().enumerate() {
            if i > 0 {
                result.push_str(", ");
            }
            param_counter += 1;
            result.push('$');
            result.push_str(&param_counter.to_string());
        }
        result.push(')');
        last_offset = spread.offset;
    }

    result.push_str(&base_sql[last_offset..]);
    Ok(result)
}

pub(crate) fn fuse(
    lex_output: LexOutput,
    columns: Vec<AnalyzedColumn>,
    info_params: Vec<ParamInfo>,
    can_run_as_subquery: bool,
) -> Result<AnalyzedQuery, AnalyzeError> {
    let LexOutput {
        sql,
        params: lex_params,
        spreads: lex_spreads,
        rewrites: _,
    } = lex_output;

    let num_regular = lex_params.len();

    // Regular params: zip lex params with the first N inferred params.
    let mut params = Vec::with_capacity(num_regular);
    for (p, pi) in lex_params
        .into_iter()
        .zip(info_params.iter().take(num_regular))
    {
        params.push(AnalyzedParam {
            name: p.name,
            sql_offsets: p.sql_offsets,
            pg_type: pi.pg_type.clone(),
            nullable: pi.nullable,
        });
    }

    // Spread fields: consume the remaining inferred params in order.
    let mut spread_param_cursor = num_regular;
    let mut spreads = Vec::with_capacity(lex_spreads.len());
    for spread in lex_spreads {
        let lex_fields = spread.fields.ok_or_else(|| {
            AnalyzeError::Internal(format!(
                "spread '${}' reached fuse() without a field list",
                spread.name
            ))
        })?;
        let mut fields = Vec::with_capacity(lex_fields.len());
        for lf in lex_fields {
            let pi = &info_params[spread_param_cursor];
            fields.push(AnalyzedSpreadField {
                name: lf.name,
                pg_type: pi.pg_type.clone(),
                nullable: pi.nullable,
            });
            spread_param_cursor += 1;
        }
        spreads.push(AnalyzedSpread {
            name: spread.name,
            offset: spread.offset,
            fields,
        });
    }

    Ok(AnalyzedQuery {
        sql,
        params,
        spreads,
        columns,
        can_run_as_subquery,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal static analyzer
// ──────────────────────────────────────────────────────────────────────────────

/// Parse `sql` with `typedpg_pg_query`, walk the AST, and produce the resolved output
/// columns, parameter type information, and the subquery-wrap eligibility
/// flag.
///
/// `param_nullability` seeds explicit `$foo?`/`$foo!` annotations indexed by
/// 1-based positional parameter index minus one.
/// Turn a `typedpg_pg_query` parse failure into the analyzer's error.
///
/// `typedpg_pg_query::Error::Parse`'s `Display` prepends `"Invalid statement: "` to the
/// server-side wording (`syntax error at or near "x"`). The error-message
/// contract requires our message to *start with* PG's verbatim text, so for the
/// `Parse` variant we take the inner message unwrapped — with its position,
/// for the caret — while other variants keep their full `Display`.
pub(crate) fn parse_failure(e: typedpg_pg_query::Error, sql: &str) -> crate::error::RawError {
    match e {
        typedpg_pg_query::Error::Parse { message, position } => {
            let span =
                position.map(|p| crate::error::SourceSpan::syntax_error_at(sql, p, &message));
            crate::pgmsg::grammar_error(message, span)
        }
        other => crate::pgmsg::grammar_error(other.to_string(), None),
    }
}

pub(crate) fn analyze_static(
    snapshot: &PgCatalog,
    sql: &str,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<AnalyzedColumn>, Vec<ParamInfo>, bool), AnalyzeError> {
    let parsed =
        typedpg_pg_query::parse(sql).map_err(|e| parse_failure(e, sql).finalize_implicit())?;

    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Parse("empty statement".into()))?;

    let can_run_as_subquery = can_run_as_subquery(stmt);
    let (raw_columns, raw_params) =
        expr::with_plan_time_checks(|| analyze_raw_node(snapshot, stmt, param_nullability))?;
    if let Some(e) = values_sort_srf_error(stmt, snapshot) {
        return Err(e);
    }

    let columns = raw_columns
        .into_iter()
        .map(|mut rc| {
            // PG resolves any `unknown`-typed top-level output column (bare
            // string literal, NULL, untyped param that stayed unresolved) to
            // `text` before sending it to the client. `analyze_raw_node` is
            // also used for view-column analysis, which needs the raw OID,
            // so apply the coercion only here at the statement boundary.
            if rc.type_oid == oid::UNKNOWN {
                rc.type_oid = oid::TEXT;
            }
            build_column(rc, snapshot)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let params_info = raw_params
        .into_iter()
        .map(|(_, type_oid, nullable)| build_param_info(type_oid, nullable, snapshot))
        .collect::<Result<Vec<_>, _>>()?;

    Ok((columns, params_info, can_run_as_subquery))
}

/// The error every execution of `stmt` raises when the rows it reads
/// come from a VALUES list sorted by a set-returning call (`VALUES (1)
/// ORDER BY generate_series(1, 2)`): parse analysis accepts the call as a
/// resjunk sort key, but a VALUES query gets no ProjectSet, so the
/// executor initializing the expression fails (`ExecInitFunc`: 0A000
/// `set-valued function called in context that cannot accept a set`),
/// whatever the data. Only a VALUES certain to be planned counts: the
/// statement's own (an arm of its set operation, an INSERT's source, what
/// EXPLAIN plans) or a scalar / ARRAY subquery making up a whole entry of
/// its select list — which the planner turns into a subplan, initialized
/// with the plan even when no row ever reaches it. Elsewhere it is dropped
/// when the planner proves it unneeded (`WHERE false`, an unreferenced
/// CTE, `EXISTS`, `CASE WHEN false`), and the statement then runs fine.
fn values_sort_srf_error(stmt: &node::Node, snapshot: &PgCatalog) -> Option<AnalyzeError> {
    fn select(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> Option<i32> {
        if sel.op != SetOperation::SetopNone as i32 {
            return [&sel.larg, &sel.rarg]
                .into_iter()
                .find_map(|arm| select(arm.as_deref()?, snapshot));
        }
        if sel.values_lists.is_empty() {
            return sel.target_list.iter().find_map(|t| {
                let node::Node::ResTarget(rt) = t.node.as_ref()? else {
                    return None;
                };
                let node::Node::SubLink(sl) = rt.val.as_deref()?.node.as_ref()? else {
                    return None;
                };
                if !matches!(
                    protobuf::SubLinkType::try_from(sl.sub_link_type),
                    Ok(protobuf::SubLinkType::ExprSublink | protobuf::SubLinkType::ArraySublink)
                ) {
                    return None;
                }
                match sl.subselect.as_deref()?.node.as_ref()? {
                    node::Node::SelectStmt(sub) => select(sub, snapshot),
                    _ => None,
                }
            });
        }
        let mut location = None;
        for n in &sel.sort_clause {
            visit_same_level(n, &mut |e| {
                if let Some(node::Node::FuncCall(fc)) = e.node.as_ref()
                    && location.is_none()
                    && is_srf_call(fc, snapshot)
                {
                    location = Some(fc.location);
                }
            });
        }
        location
    }
    let location = match stmt {
        node::Node::SelectStmt(sel) => select(sel, snapshot),
        node::Node::InsertStmt(ins) => match ins.select_stmt.as_deref()?.node.as_ref()? {
            node::Node::SelectStmt(sel) => select(sel, snapshot),
            _ => None,
        },
        node::Node::ExplainStmt(es) => {
            return values_sort_srf_error(es.query.as_deref()?.node.as_ref()?, snapshot);
        }
        _ => None,
    }?;
    Some(
        crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(
                "set-valued function called in context that cannot accept a set".into(),
            ),
            crate::error::SourceSpan::from_node_qname(location),
            None,
        )
        .finalize_implicit(),
    )
}

/// True when `stmt` can appear as the body of a `SELECT * FROM (<stmt>) …`
/// subquery. False for top-level DML (`INSERT`/`UPDATE`/`DELETE`/`MERGE`),
/// utility statements (`EXPLAIN`/`NOTIFY`/`LISTEN`/`UNLISTEN`), and
/// `WITH … (DML …) SELECT …` — PG only allows a data-modifying CTE at the
/// top level, not nested inside a subquery (`E0A000`).
///
/// CTEs nested deeper than the top-level `WITH` don't need to be inspected:
/// PG already rejects `WITH (DML)` outside the top level, so any query that
/// reaches the analyzer has its DML-CTEs (if any) attached to the root node.
fn can_run_as_subquery(stmt: &node::Node) -> bool {
    let node::Node::SelectStmt(sel) = stmt else {
        return false;
    };
    if sel.into_clause.is_some() {
        return false;
    }
    let Some(with) = &sel.with_clause else {
        return true;
    };
    !with.ctes.iter().any(|cte_node| {
        let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() else {
            return false;
        };
        matches!(
            cte.ctequery.as_deref().and_then(|q| q.node.as_ref()),
            Some(
                node::Node::InsertStmt(_)
                    | node::Node::UpdateStmt(_)
                    | node::Node::DeleteStmt(_)
                    | node::Node::MergeStmt(_)
            )
        )
    })
}

/// A positional parameter slot: `(position, type_oid, nullable)`. Shared by
/// the analyzer internals that thread params through overload resolution
/// before they are merged with lexer-side info.
pub(crate) type RawParam = (i32, PgTypeOid, bool);

/// The expressions of a `RETURNING` clause (none when there is no clause).
pub(crate) fn returning_exprs(clause: &Option<protobuf::ReturningClause>) -> &[protobuf::Node] {
    clause.as_ref().map_or(&[], |c| c.exprs.as_slice())
}

/// Lower-level analyzer entry point: walks a pre-parsed AST node and returns
/// the raw columns (keyed by OID) and sorted param list without converting to
/// [`Type`]. Used by [`analyze_static`] (after parsing) and by the DDL view
/// handling, which only needs OIDs to rebuild catalog entries and reuses a
/// stored AST to skip the deparse → reparse round-trip.
pub(crate) fn analyze_raw_node(
    snapshot: &PgCatalog,
    stmt: &node::Node,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<RawColumn>, Vec<RawParam>), AnalyzeError> {
    let mut params = ParamCollector::default();

    // Seed explicit nullable annotations from lexer ($foo? / $foo! syntax).
    for (i, &nullable) in param_nullability.iter().enumerate() {
        if let Some(explicit) = nullable {
            params.set_nullable((i + 1) as i32, explicit);
        }
    }
    analyze_raw_node_with(snapshot, stmt, params)
}

/// [`analyze_raw_node`] for a statement whose `$1…$n` already have types —
/// a SQL function body, whose parameters are the function's arguments.
pub(crate) fn analyze_raw_node_with_param_types(
    snapshot: &PgCatalog,
    stmt: &node::Node,
    param_types: &[crate::oid::PgTypeOid],
) -> Result<(Vec<RawColumn>, Vec<RawParam>), AnalyzeError> {
    let mut params = ParamCollector::default();
    for (i, &t) in param_types.iter().enumerate() {
        params.record((i + 1) as i32, t);
    }
    analyze_raw_node_with(snapshot, stmt, params)
}

/// Analyze `stmt` until its params' nullability is settled. A walk can
/// read a `$N` as non-NULL (`COALESCE(col, $1)` taking its nullability
/// from it) before a later site infers it nullable (the same `COALESCE`,
/// or `$1` assigned to a nullable column): the statement is then analyzed
/// again with those params nullable from the start, so no inference rests
/// on a value the caller may pass as NULL. Nullability only ever turns
/// on, so this ends after at most one pass per param.
fn analyze_raw_node_with(
    snapshot: &PgCatalog,
    stmt: &node::Node,
    params: ParamCollector,
) -> Result<(Vec<RawColumn>, Vec<RawParam>), AnalyzeError> {
    // A statement analyzed within one that locks rows (a view it reads)
    // is re-checked along with it.
    let locks = crate::nonnull::row_locking() || statement_locks_rows(snapshot, stmt);
    let mut seeded = params;
    loop {
        let (analysis, stale) = crate::nonnull::with_row_locking(locks, || {
            analyze_raw_node_once(snapshot, stmt, seeded.clone())
        })?;
        if stale.is_empty() {
            return Ok(analysis);
        }
        for n in stale {
            seeded.infer_nullable(n, true);
        }
    }
}

/// Whether `stmt` has a `FOR UPDATE` / `FOR SHARE` clause anywhere.
pub(crate) fn locks_rows(stmt: &node::Node) -> bool {
    let locks = |n: typedpg_pg_query::NodeRef<'_>| matches!(n, typedpg_pg_query::NodeRef::SelectStmt(s) if !s.locking_clause.is_empty());
    matches!(stmt, node::Node::SelectStmt(s) if !s.locking_clause.is_empty())
        || stmt.nodes().into_iter().any(|(n, ..)| locks(n))
}

/// Whether `stmt` locks rows: it has a locking clause, or reads a view
/// whose query does ([`crate::ddl::views::view_locks_rows`]). A name a
/// CTE takes counts as the view it would name otherwise (only ever too
/// cautious).
fn statement_locks_rows(snapshot: &PgCatalog, stmt: &node::Node) -> bool {
    locks_rows(stmt)
        || stmt.nodes().into_iter().any(|(n, ..)| match n {
            typedpg_pg_query::NodeRef::RangeVar(rv) => {
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                snapshot
                    .resolve_table(schema, &rv.relname)
                    .is_some_and(|c| {
                        c.relkind == crate::pg_catalog::RelKind::View
                            && crate::ddl::views::view_locks_rows(snapshot, c.oid)
                    })
            }
            _ => false,
        })
}

/// A statement's output columns and parameters.
type RawAnalysis = (Vec<RawColumn>, Vec<RawParam>);

/// One pass of [`analyze_raw_node_with`], also returning the params read
/// as non-NULL that ended up nullable.
fn analyze_raw_node_once(
    snapshot: &PgCatalog,
    stmt: &node::Node,
    mut params: ParamCollector,
) -> Result<(RawAnalysis, Vec<i32>), AnalyzeError> {
    let (raw_columns, raw_params) = match stmt {
        // `SELECT … INTO t` is CREATE TABLE AS (transformSelectStmt turns it
        // into a CreateTableAsStmt): it returns no rows.
        node::Node::SelectStmt(sel) if sel.into_clause.is_some() => {
            let (_, p) = analyze_select(sel, snapshot, &mut params)?;
            expr::resolve_untyped_output_params(sel, snapshot, &mut params)?;
            (Vec::new(), p)
        }
        node::Node::SelectStmt(sel) => {
            let r = analyze_select(sel, snapshot, &mut params)?;
            // The statement-level `resolveTargetListUnknowns`.
            expr::resolve_untyped_output_params(sel, snapshot, &mut params)?;
            r
        }
        node::Node::InsertStmt(ins) => analyze_insert(ins, snapshot, &mut params)?,
        node::Node::UpdateStmt(upd) => analyze_update(upd, snapshot, &mut params)?,
        node::Node::DeleteStmt(del) => analyze_delete(del, snapshot, &mut params)?,
        node::Node::MergeStmt(merge) => analyze_merge(merge, snapshot, &mut params)?,
        node::Node::CallStmt(call) => analyze_call(call, snapshot, &mut params)?,
        // `EXPLAIN <query>` — recurse into the wrapped statement so its
        // parameters are harvested into the outer `ParamCollector`, then
        // replace the column list with PG's fixed `QUERY PLAN` row
        // description (single text column, never NULL — even an empty
        // plan emits at least one row).
        node::Node::ExplainStmt(es) => {
            let inner = es
                .query
                .as_deref()
                .and_then(|q| q.node.as_ref())
                .ok_or_else(|| AnalyzeError::Unsupported("EXPLAIN with no inner query".into()))?;
            // Dispatch into the same per-stmt analyzers we use at the top
            // level so the params collector stays shared (calling
            // `analyze_raw_node` recursively would allocate a fresh
            // collector and the outer call would see zero params).
            let _ = match inner {
                node::Node::SelectStmt(sel) => analyze_select(sel, snapshot, &mut params)?,
                node::Node::InsertStmt(ins) => analyze_insert(ins, snapshot, &mut params)?,
                node::Node::UpdateStmt(upd) => analyze_update(upd, snapshot, &mut params)?,
                node::Node::DeleteStmt(del) => analyze_delete(del, snapshot, &mut params)?,
                node::Node::MergeStmt(merge) => analyze_merge(merge, snapshot, &mut params)?,
                _ => {
                    return Err(crate::error::RawError::unsupported(
                        format!(
                            "typedpg does not support EXPLAIN of {} yet",
                            crate::error::statement_name(inner)
                        ),
                        crate::error::statement_span(),
                        None,
                    )
                    .finalize_implicit());
                }
            };
            (
                vec![RawColumn {
                    name: "QUERY PLAN".to_owned(),
                    type_oid: oid::TEXT,
                    nullable: false,
                    typmod: None,
                    collation: None,
                    record_fields: None,
                    elem_nullable: None,
                    origin: None,
                }],
                None,
            )
        }
        // `NOTIFY channel [, 'payload']` and `LISTEN/UNLISTEN channel`
        // produce no result rows. PG's payload is a string literal in the
        // standard form (no expressions / parameters); for parameterized
        // notifications callers use `SELECT pg_notify($1, $2)` which goes
        // through the regular function-call path.
        node::Node::NotifyStmt(_) | node::Node::ListenStmt(_) | node::Node::UnlistenStmt(_) => {
            (Vec::new(), None)
        }
        _ => {
            return Err(crate::error::RawError::unsupported(
                format!(
                    "typedpg does not support {} statements in queries yet",
                    crate::error::statement_name(stmt)
                ),
                crate::error::statement_span(),
                Some(
                    "a query can be SELECT, VALUES, INSERT, UPDATE, DELETE, MERGE, CALL, \
                     EXPLAIN, NOTIFY, LISTEN or UNLISTEN; schema changes belong in migrations"
                        .into(),
                ),
            )
            .finalize_implicit());
        }
    };

    let stale = params.stale_non_null_reads();
    let raw_params = match raw_params {
        Some(p) => p,
        None => params.into_sorted()?,
    };

    Ok(((raw_columns, raw_params), stale))
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Infer an expression type, propagating only `TypeMismatch` errors.
///
/// Other errors (e.g. `UndefinedColumn` from correlated subqueries referencing
/// outer scope) are swallowed — they represent pre-existing analyzer
/// limitations, not user errors.
/// Returns true when `node` is a bare SQL `NULL` (`AConst` with `isnull` set
/// and no concrete `Val`). Typed NULLs like `NULL::int` become
/// `TypeCast { arg: AConst NULL, typename: int }` — we don't treat those as
/// unconditionally NULL because PG allows `SET col = NULL::t` to perform an
/// assignment the caller has explicitly typed.
fn is_sql_null_literal(node: &protobuf::Node) -> bool {
    matches!(
        node.node.as_ref(),
        Some(node::Node::AConst(c)) if c.isnull
    )
}

/// `true` for the literal `DEFAULT` keyword used in INSERT VALUES /
/// UPDATE SET. Mirrors PG's `SetToDefault` AST node.
fn is_set_to_default(node: &protobuf::Node) -> bool {
    matches!(node.node.as_ref(), Some(node::Node::SetToDefault(_)))
}

/// The ON CONFLICT arbiter specification after parse analysis
/// (`transformOnConflictArbiter`), ready for the planner's inference.
pub(crate) struct Arbiter {
    /// Attnums of the plain-column inference elements (system columns
    /// included, as PG's Vars carry them).
    pub cols: std::collections::BTreeSet<i16>,
    /// The expression inference elements.
    pub exprs: Vec<protobuf::Node>,
    /// An element named the target row itself (`ON CONFLICT (t)`).
    pub whole_row: bool,
    /// The inference WHERE clause.
    pub where_clause: Option<protobuf::Node>,
    /// The elements that name a collation or operator class
    /// (`InferenceElem.infercollid` / `inferopclass`).
    pub qualified: Vec<ArbiterElem>,
}

/// An inference element with an explicit COLLATE or operator class.
#[derive(Clone, Debug)]
pub(crate) struct ArbiterElem {
    /// The column it names, or `None` for an expression.
    pub attnum: Option<i16>,
    /// The expression it names.
    pub expr: Option<protobuf::Node>,
    pub collation: Option<crate::oid::PgCollationOid>,
    pub opclass: Option<crate::oid::PgOpclassOid>,
}

/// infer_collation_opclass_match (plancat.c): some key column of `idx`
/// that is `elem`'s column or expression has its collation and an
/// operator class of the same family and input type as its operator
/// class. A key column whose class the analyzer couldn't resolve counts as
/// a match.
fn infer_collation_opclass_match(
    snapshot: &PgCatalog,
    elem: &ArbiterElem,
    idx: &crate::pg_catalog::PgIndex,
    idx_exprs: &[protobuf::Node],
) -> bool {
    let class_of = |c: crate::oid::PgOpclassOid| crate::ddl::opclass::opclass_by_oid(snapshot, c);
    let infer = elem.opclass.and_then(class_of);
    let nkey = usize::try_from(idx.indnkeyatts)
        .unwrap_or(0)
        .min(idx.indkey.len());
    let mut nplain = 0;
    for natt in 0..nkey {
        let attno = idx.indkey[natt];
        if attno != 0 {
            nplain += 1;
        }
        if let Some(infer) = infer
            && let Some(class) = idx.indclass.get(natt).copied().flatten().and_then(class_of)
            && (infer.opcfamily != class.opcfamily
                || infer.opcfamilynamespace != class.opcfamilynamespace
                || infer.opcmethod != class.opcmethod
                || infer.opcintype != class.opcintype)
        {
            continue;
        }
        if elem.collation.is_some()
            && elem.collation != idx.indcollation.get(natt).copied().flatten()
        {
            continue;
        }
        match (elem.attnum, &elem.expr) {
            (Some(a), _) if a == attno => return true,
            (None, Some(e))
                if attno == 0
                    && idx_exprs
                        .get(natt - nplain)
                        .is_some_and(|x| node_fingerprint(x) == node_fingerprint(e)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// The planner's side of an `ON CONFLICT` target (`infer_arbiter_indexes`,
/// plancat.c — PG raises these when the statement is planned, so every
/// execution fails): a named constraint must be backed by an index that
/// can arbitrate the action, and a column / expression list must be
/// inferable to a valid unique index whose key columns and expressions
/// are the inference elements, and — when partial — whose predicate the
/// ON CONFLICT WHERE implies (`predicate_implied_by`, see [`predtest`]).
/// 42P10 otherwise.
fn validate_on_conflict_target(
    on_conflict: &protobuf::OnConflictClause,
    arbiter: &Arbiter,
    snapshot: &PgCatalog,
    table_oid: crate::oid::PgClassOid,
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    let Some(infer) = on_conflict.infer.as_deref() else {
        // `ON CONFLICT DO NOTHING` without a target matches any conflict.
        return Ok(());
    };

    if arbiter.whole_row {
        return Err(AnalyzeError::FeatureNotSupported(
            "whole row unique index inference specifications are not supported".into(),
        ));
    }

    // `ON CONFLICT ON CONSTRAINT <name>` (its existence was checked during
    // parse analysis).
    if !infer.conname.is_empty() {
        let Some(found) = snapshot
            .pg_constraint_values()
            .find(|c| c.conrelid == table_oid && c.conname == infer.conname)
        else {
            return Ok(());
        };
        // The constraint must be backed by an index (PRIMARY KEY / UNIQUE /
        // EXCLUDE), not a CHECK, FOREIGN KEY or NOT NULL one.
        if !matches!(
            found.contype,
            ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
        ) {
            return Err(AnalyzeError::WrongObjectType(
                "constraint in ON CONFLICT clause has no associated index".into(),
            ));
        }
        // An exclusion constraint's index — an EXCLUDE one, or PG 18's
        // WITHOUT OVERLAPS key — can't arbitrate an update.
        if on_conflict.action() == protobuf::OnConflictAction::OnconflictUpdate
            && (found.contype == ConType::Exclusion || found.conperiod)
        {
            return Err(AnalyzeError::WrongObjectType(
                "ON CONFLICT DO UPDATE not supported with exclusion constraints".into(),
            ));
        }
        return Ok(());
    }

    let mut target_cols = arbiter.cols.clone();
    let mut target_expr_nodes = arbiter.exprs.clone();
    let mut where_clause = arbiter.where_clause.clone();
    let mut qualified = arbiter.qualified.clone();

    // Through an automatically updatable view, the planner infers the
    // arbiter on the base relation (rewriteTargetView rewrites the
    // inference elements onto its columns). A view that isn't updatable
    // fails in the rewriter first.
    let mut table_oid = table_oid;
    while let Some(upd) = snapshot.view_updatability.get(&table_oid) {
        let Some(base) = upd.base else {
            return Ok(());
        };
        let view_attrs = snapshot.attributes_of(table_oid);
        let base_attrs = snapshot.attributes_of(base);
        let base_attnum = |attnum: i16| -> Option<i16> {
            let pos = view_attrs.iter().position(|a| a.attnum == attnum)?;
            upd.columns.get(pos)?.as_ref().ok().copied()
        };
        let base_name = |name: &str| -> Option<String> {
            let attnum = view_attrs.iter().find(|a| a.attname == name)?.attnum;
            let b = base_attnum(attnum)?;
            base_attrs
                .iter()
                .find(|a| a.attnum == b)
                .map(|a| a.attname.clone())
        };
        // A view column that isn't a base column can't match any index key.
        target_cols = target_cols
            .iter()
            .map(|&a| base_attnum(a).unwrap_or(i16::MIN))
            .collect();
        for e in &mut target_expr_nodes {
            *e = rename_column_refs(e, &base_name);
        }
        if let Some(w) = &mut where_clause {
            *w = rename_column_refs(w, &base_name);
        }
        for q in &mut qualified {
            q.attnum = q.attnum.map(|a| base_attnum(a).unwrap_or(i16::MIN));
            if let Some(e) = &mut q.expr {
                *e = rename_column_refs(e, &base_name);
            }
        }
        table_oid = base;
    }
    let mut target_exprs: Vec<String> = target_expr_nodes.iter().map(node_fingerprint).collect();
    target_exprs.sort();
    target_exprs.dedup();

    let decode = |ast: &crate::pg_catalog::SerializedAst| -> Option<protobuf::Node> {
        use prost::Message;
        protobuf::Node::decode(ast.ast.as_slice()).ok()
    };

    let index_matches = snapshot.pg_index.values().any(|idx| {
        // A WITHOUT OVERLAPS key is unique but really an exclusion
        // constraint: inference skips it, and invalid indexes too.
        if idx.indrelid != table_oid
            || !idx.indisunique
            || idx.indisexclusion
            || snapshot.invalid_indexes.contains(&idx.indexrelid)
        {
            return false;
        }
        let key = &idx.indkey[..usize::try_from(idx.indnkeyatts)
            .unwrap_or(0)
            .min(idx.indkey.len())];
        let cols: std::collections::BTreeSet<i16> =
            key.iter().copied().filter(|&a| a != 0).collect();
        let idx_exprs: Vec<protobuf::Node> = idx.indexprs.iter().filter_map(decode).collect();
        let mut exprs: Vec<String> = idx_exprs.iter().map(node_fingerprint).collect();
        exprs.sort();
        exprs.dedup();
        if cols != target_cols || exprs != target_exprs {
            return false;
        }
        if !qualified
            .iter()
            .all(|q| infer_collation_opclass_match(snapshot, q, idx, &idx_exprs))
        {
            return false;
        }
        let pred = idx.indpred.as_ref().and_then(decode);
        predtest::predicate_implied_by(pred.as_ref(), where_clause.as_ref())
    });
    // Constraint-backed keys, for catalogs whose constraints carry no
    // pg_index row (and so no collations or operator classes to match).
    let constraint_matches = target_exprs.is_empty()
        && qualified.is_empty()
        && snapshot.pg_constraint_values().any(|c| {
            c.conrelid == table_oid
                && matches!(c.contype, ConType::PrimaryKey | ConType::Unique)
                && !c.conperiod
                && c.conkey
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>()
                    == target_cols
        });
    if !index_matches && !constraint_matches {
        return Err(crate::pgmsg::no_on_conflict_arbiter(table_relname).finalize_implicit());
    }
    Ok(())
}

/// `node` with every column reference's column name mapped through
/// `rename` (names it doesn't map keep theirs) — rewriteTargetView's
/// re-pointing of view columns at the base relation's.
fn rename_column_refs(
    node: &protobuf::Node,
    rename: &dyn Fn(&str) -> Option<String>,
) -> protobuf::Node {
    let mut tree = protobuf::ParseResult {
        version: 0,
        stmts: vec![protobuf::RawStmt {
            stmt: Some(Box::new(node.clone())),
            stmt_location: 0,
            stmt_len: 0,
        }],
    };
    // SAFETY: the tree is neither moved nor dropped while the pointers are
    // used, and only the `String` leaves of column references are written —
    // no subtree any other pointer refers into is replaced.
    unsafe {
        for (n, _) in tree.nodes_mut() {
            if let typedpg_pg_query::NodeMut::ColumnRef(cr) = n
                && let Some(last) = (*cr).fields.last_mut()
                && let Some(node::Node::String(s)) = last.node.as_mut()
                && let Some(new) = rename(&s.sval)
            {
                s.sval = new;
            }
        }
    }
    tree.stmts
        .pop()
        .and_then(|s| s.stmt)
        .map(|b| *b)
        .unwrap_or_default()
}

/// If assigning a literal `NULL` to `tc` would violate a NOT-NULL guarantee
/// (either column-level `attnotnull`, or a domain in the type chain whose
/// `typnotnull` is set), return the matching `AnalyzeError`. `op` selects
/// the wording — `"insert"` mirrors PG's INSERT-time message, `"assign"`
/// covers UPDATE / MERGE UPDATE.
///
/// Both branches start with PG's exact runtime wording so the `pg_sanity`
/// execute-fallback prefix check passes; the analyzer's stricter form
/// (table+column qualified) follows in parentheses for the macro caller.
///
/// A NOT NULL domain fails as the row is built; the column's own NOT NULL
/// only in ExecConstraints, after the BEFORE ROW triggers — one of which
/// may replace the NULL, so the column's is not reported then.
fn null_assignment_error(
    tc: &crate::pg_catalog::PgAttribute,
    snapshot: &PgCatalog,
    table_relname: &str,
    op: &'static str,
) -> Option<AnalyzeError> {
    if let Some(domain) = snapshot.domain_not_null_name(tc.atttypid) {
        return Some(AnalyzeError::Invalid(format!(
            "domain {domain} does not allow null values"
        )));
    }
    // A view's columns carry no NOT NULL of their own (their inferred
    // non-nullability is not a constraint); the base relation's is checked
    // when the rewriter reaches it.
    let on_view = snapshot
        .pg_class
        .get(&tc.attrelid)
        .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::View);
    let event = if op == "insert" {
        DmlEvent::Insert
    } else {
        DmlEvent::Update
    };
    if tc.attnotnull && !on_view && !before_row_trigger_rewrites(snapshot, tc.attrelid, event) {
        let verb = match op {
            "insert" => "insert NULL into",
            _ => "assign NULL to",
        };
        let qualified = QualifiedName::new(table_relname, &tc.attname);
        return Some(AnalyzeError::Invalid(format!(
            "null value in column \"{}\" of relation \"{table_relname}\" \
             violates not-null constraint \
             (cannot {verb} NOT NULL column `{qualified}`)",
            tc.attname,
        )));
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────────
// Raw output types (before Rust type mapping)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct RawColumn {
    pub name: String,
    pub type_oid: PgTypeOid,
    pub nullable: bool,
    /// Optional `pg_attribute.atttypmod`-shaped modifier (`varchar(n)` length,
    /// `numeric(p,s)`, pgvector dimension, …). `None` matches PG's `-1`.
    pub typmod: Option<i32>,
    /// Effective `pg_collation.oid` derived for the column / expression.
    /// Threaded straight from the inner [`ExprType::collation`] so the
    /// final `Type` can render the non-default name.
    pub collation: Option<crate::oid::PgCollationOid>,
    /// Named-field structure when this column holds a record. Sourced from
    /// SRF out_args, ROW constructors, or propagated through subqueries.
    /// Used both to surface `Type::AnonymousRecord` in the final output and
    /// to feed downstream `(x).field` resolution via the scope.
    pub record_fields: Option<crate::expr::RecordShape>,
    /// For an array column, whether its elements can be NULL, where known
    /// (see [`crate::types::Type::Array`]).
    pub elem_nullable: Option<bool>,
    /// The base-table column the value is passed through from, if any.
    pub origin: Option<crate::scope::Origin>,
}

/// Return type for analyze_* functions: columns + optional pre-sorted params.
type AnalyzeResult = Result<(Vec<RawColumn>, Option<Vec<(i32, PgTypeOid, bool)>>), AnalyzeError>;

mod assign;
mod call;
mod copy_in;
mod cte;
mod dml;
mod from;
mod merge;
mod predtest;
mod returning;
pub(crate) mod rewrite;
mod row_guarantees;
mod select;
mod set_ops;
mod target_list;
mod type_resolution;
mod walk;
mod written_row;

// Re-export submodule items at the `resolve` path so intra-crate callers
// (e.g. `crate::resolve::analyze_correlated_select`) and the dispatcher in
// this module resolve them transparently. Function names are unique across
// the former monolith, so these globs never collide.
pub(crate) use assign::*;
pub(crate) use call::*;
pub use copy_in::AnalyzedCopyIn;
pub(crate) use cte::*;
pub(crate) use dml::*;
pub(crate) use from::*;
pub(crate) use merge::*;
pub(crate) use predtest::unqualify as predtest_unqualify;
pub(crate) use returning::*;
pub(crate) use rewrite::*;
pub(crate) use row_guarantees::*;
pub(crate) use select::*;
pub(crate) use set_ops::*;
pub(crate) use target_list::*;
pub(crate) use type_resolution::*;
pub(crate) use walk::*;
pub(crate) use written_row::*;
