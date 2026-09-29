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
/// Returns [`AnalyzeError::Internal`] if any spread reaches this point without
/// the field list the lexer is supposed to attach — that would indicate a
/// lexer/macro contract bug rather than user input.
pub(crate) fn build_spread_sample_sql(lex_output: &LexOutput) -> Result<String, AnalyzeError> {
    let base_sql = &lex_output.sql;
    let num_regular_params = lex_output.params.len();
    let mut result = String::with_capacity(base_sql.len() + 64);
    let mut last_offset = 0;
    let mut param_counter = num_regular_params;

    for spread in &lex_output.spreads {
        result.push_str(&base_sql[last_offset..spread.offset]);
        let fields = spread.fields.as_ref().ok_or_else(|| {
            AnalyzeError::Internal(format!(
                "spread '${}' reached the analyzer without a field list",
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
/// Extract the PG-verbatim message from a `typedpg_pg_query` parse failure.
///
/// `typedpg_pg_query::Error::Parse`'s `Display` prepends `"Invalid statement: "` to the
/// server-side wording (`syntax error at or near "x"`). The error-message
/// contract requires our message to *start with* PG's verbatim text, so for the
/// `Parse` variant we return the inner string unwrapped; other variants keep
/// their full `Display`.
fn parse_error_message(e: &typedpg_pg_query::Error) -> String {
    match e {
        typedpg_pg_query::Error::Parse(msg) => msg.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn analyze_static(
    snapshot: &PgCatalog,
    sql: &str,
    param_nullability: &[Option<bool>],
) -> Result<(Vec<AnalyzedColumn>, Vec<ParamInfo>, bool), AnalyzeError> {
    let parsed = typedpg_pg_query::parse(sql)
        .map_err(|e| crate::pgmsg::grammar_error(parse_error_message(&e)))?;

    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|s| s.stmt.as_ref())
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Parse("empty statement".into()))?;

    let can_run_as_subquery = can_run_as_subquery(stmt);
    let (raw_columns, raw_params) = analyze_raw_node(snapshot, stmt, param_nullability)?;

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

    let (raw_columns, raw_params) = match stmt {
        // `SELECT … INTO t` is CREATE TABLE AS (transformSelectStmt turns it
        // into a CreateTableAsStmt): it returns no rows.
        node::Node::SelectStmt(sel) if sel.into_clause.is_some() => {
            let (_, p) = analyze_select(sel, snapshot, &mut params)?;
            (Vec::new(), p)
        }
        node::Node::SelectStmt(sel) => analyze_select(sel, snapshot, &mut params)?,
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
                    return Err(AnalyzeError::Unsupported(format!(
                        "EXPLAIN with statement type: {:?}",
                        std::mem::discriminant(inner)
                    )));
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
            return Err(AnalyzeError::Unsupported(format!(
                "statement type: {:?}",
                std::mem::discriminant(stmt)
            )));
        }
    };

    let raw_params = match raw_params {
        Some(p) => p,
        None => params.into_sorted()?,
    };

    Ok((raw_columns, raw_params))
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

/// Validate an `ON CONFLICT` clause's arbiter specification like PG:
/// `transformOnConflictArbiter` requires a target for DO UPDATE (42601), a
/// named constraint must exist on the table, and a column / expression
/// list must be inferable to a unique index (`infer_arbiter_indexes`,
/// plancat.c — 42P10 otherwise): the index's key columns and key
/// expressions equal the inference elements, and a partial index only
/// qualifies when the ON CONFLICT WHERE implies its predicate.
///
/// Predicate implication is approximated structurally: every conjunct of
/// the index predicate must appear among the WHERE clause's conjuncts
/// (PG's `predicate_implied_by` also proves weaker implications).
fn validate_on_conflict_target(
    on_conflict: &protobuf::OnConflictClause,
    snapshot: &PgCatalog,
    table_oid: crate::oid::PgClassOid,
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    let Some(infer) = on_conflict.infer.as_deref() else {
        if on_conflict.action() == protobuf::OnConflictAction::OnconflictUpdate {
            return Err(crate::error::RawError::new(
                AnalyzeError::SyntaxError(
                    "ON CONFLICT DO UPDATE requires inference specification or constraint name"
                        .into(),
                ),
                None,
                Some("For example, ON CONFLICT (column_name).".into()),
            )
            .finalize_implicit());
        }
        // `ON CONFLICT DO NOTHING` without a target matches any conflict.
        return Ok(());
    };

    // `ON CONFLICT ON CONSTRAINT <name>` — look up by name on the table.
    if !infer.conname.is_empty() {
        let found = snapshot
            .pg_constraint_values()
            .find(|c| c.conrelid == table_oid && c.conname == infer.conname);
        let Some(found) = found else {
            return Err(AnalyzeError::Invalid(format!(
                "constraint \"{}\" for table \"{}\" does not exist",
                infer.conname, table_relname,
            )));
        };
        // transformOnConflictArbiter: the constraint must be backed by an
        // index (PRIMARY KEY / UNIQUE / EXCLUDE), not a CHECK, FOREIGN KEY
        // or NOT NULL one.
        if !matches!(
            found.contype,
            ConType::PrimaryKey | ConType::Unique | ConType::Exclusion
        ) {
            return Err(AnalyzeError::WrongObjectType(
                "constraint in ON CONFLICT clause has no associated index".into(),
            ));
        }
        return Ok(());
    }

    // The inference elements: plain columns (by attnum) and expressions
    // (by location-free fingerprint).
    let mut target_cols: std::collections::BTreeSet<i16> = Default::default();
    let mut target_exprs: Vec<String> = Vec::new();
    for elem in &infer.index_elems {
        let Some(node::Node::IndexElem(ie)) = elem.node.as_ref() else {
            continue;
        };
        if !ie.name.is_empty() {
            let attnum = snapshot
                .attributes_of(table_oid)
                .iter()
                .find(|a| a.attname == ie.name)
                .map(|a| a.attnum);
            match attnum {
                Some(an) => {
                    target_cols.insert(an);
                }
                None => {
                    // PG runtime wording: `column "ghost" does not exist`.
                    // Append the ON CONFLICT context as a suffix so the
                    // execute-fallback prefix check passes while the macro
                    // caller still sees the clause that produced it.
                    return Err(AnalyzeError::UndefinedColumn(format!(
                        "column \"{}\" does not exist (referenced in ON CONFLICT)",
                        ie.name
                    )));
                }
            }
        } else if let Some(e) = ie.expr.as_deref() {
            target_exprs.push(node_fingerprint(e));
        }
    }
    target_exprs.sort();

    let decode = |ast: &crate::pg_catalog::SerializedAst| -> Option<protobuf::Node> {
        use prost::Message;
        protobuf::Node::decode(ast.ast.as_slice()).ok()
    };
    let where_conjuncts: Vec<String> = on_conflict
        .infer
        .as_deref()
        .and_then(|i| i.where_clause.as_deref())
        .map(conjunct_fingerprints)
        .unwrap_or_default();

    let index_matches = snapshot.pg_index.values().any(|idx| {
        if idx.indrelid != table_oid || !idx.indisunique {
            return false;
        }
        let key = &idx.indkey[..usize::try_from(idx.indnkeyatts)
            .unwrap_or(0)
            .min(idx.indkey.len())];
        let cols: std::collections::BTreeSet<i16> =
            key.iter().copied().filter(|&a| a != 0).collect();
        let mut exprs: Vec<String> = idx
            .indexprs
            .iter()
            .filter_map(|a| decode(a).map(|n| node_fingerprint(&n)))
            .collect();
        exprs.sort();
        if cols != target_cols || exprs != target_exprs {
            return false;
        }
        match idx.indpred.as_ref() {
            None => true,
            Some(pred) => decode(pred).is_some_and(|p| {
                conjunct_fingerprints(&p)
                    .iter()
                    .all(|c| where_conjuncts.contains(c))
            }),
        }
    });
    // Constraint-backed keys, for catalogs whose constraints carry no
    // pg_index row.
    let constraint_matches = target_exprs.is_empty()
        && snapshot.pg_constraint_values().any(|c| {
            c.conrelid == table_oid
                && matches!(c.contype, ConType::PrimaryKey | ConType::Unique)
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

/// The location-free fingerprints of `node`'s top-level AND conjuncts.
fn conjunct_fingerprints(node: &protobuf::Node) -> Vec<String> {
    match node.node.as_ref() {
        Some(node::Node::BoolExpr(b)) if b.boolop() == protobuf::BoolExprType::AndExpr => {
            b.args.iter().flat_map(conjunct_fingerprints).collect()
        }
        _ => vec![node_fingerprint(node)],
    }
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
    if tc.attnotnull {
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
    pub record_fields: Option<Vec<crate::expr::RecordField>>,
}

/// Return type for analyze_* functions: columns + optional pre-sorted params.
type AnalyzeResult = Result<(Vec<RawColumn>, Option<Vec<(i32, PgTypeOid, bool)>>), AnalyzeError>;

mod assign;
mod call;
mod cte;
mod dml;
mod from;
mod merge;
mod returning;
mod select;
mod set_ops;
mod target_list;
mod type_resolution;
mod walk;

// Re-export submodule items at the `resolve` path so intra-crate callers
// (e.g. `crate::resolve::analyze_correlated_select`) and the dispatcher in
// this module resolve them transparently. Function names are unique across
// the former monolith, so these globs never collide.
pub(crate) use assign::*;
pub(crate) use call::*;
pub(crate) use cte::*;
pub(crate) use dml::*;
pub(crate) use from::*;
pub(crate) use merge::*;
pub(crate) use returning::*;
pub(crate) use select::*;
pub(crate) use set_ops::*;
pub(crate) use target_list::*;
pub(crate) use type_resolution::*;
pub(crate) use walk::*;
