//! The migration session's prepared statements (prepare.c): `PREPARE`
//! analyzes a query and keeps it with its parameter types, `EXECUTE` —
//! alone or as `CREATE TABLE ... AS EXECUTE` — runs it with arguments
//! coerced to those types, `DEALLOCATE` (and `DISCARD ALL`) forget it.

use typedpg_pg_query::protobuf::{
    DeallocateStmt, DiscardMode, DiscardStmt, ExecuteStmt, Node, PrepareStmt, node,
};

use super::DdlError;
use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::PgCatalog;

/// A prepared statement: its query and parameter types.
#[derive(Clone, Debug)]
pub(crate) struct PreparedStatement {
    pub(crate) query: Node,
    pub(crate) param_types: Vec<PgTypeOid>,
    /// How many times it was planned for its arguments (a custom plan).
    pub(crate) custom_plans: u32,
}

impl PreparedStatement {
    /// The query returns rows the way CREATE TABLE AS EXECUTE needs: a
    /// SELECT (ExecuteQuery's "prepared statement is not a SELECT").
    pub(crate) fn is_select(&self) -> bool {
        matches!(self.query.node.as_ref(), Some(node::Node::SelectStmt(_)))
    }
}

/// An analysis error PG would report for the query too (the ones the
/// analyzer raises for its own gaps are let through, as for DML).
fn analysis_error(e: AnalyzeError) -> Option<DdlError> {
    match e {
        AnalyzeError::Unsupported(_)
        | AnalyzeError::UnsupportedJoinType(_)
        | AnalyzeError::Internal(_)
        | AnalyzeError::Lex(_)
        | AnalyzeError::Serde(_)
        | AnalyzeError::Io(_) => None,
        e => Some(DdlError::UnsupportedDdl(e.to_string())),
    }
}

/// `PREPARE name [(types)] AS query` (PrepareQuery): the name must be
/// free; the query is analyzed with the declared parameter types, and the
/// ones it uses beyond them are inferred. It isn't run: the errors only
/// its execution raises are EXECUTE's.
pub(crate) fn prepare(interp: &mut PgCatalog, stmt: &PrepareStmt) -> Result<(), DdlError> {
    let Some(query) = stmt.query.as_deref() else {
        return Ok(());
    };
    let mut declared: Vec<PgTypeOid> = Vec::new();
    for t in &stmt.argtypes {
        if let Some(node::Node::TypeName(tn)) = t.node.as_ref() {
            declared.push(super::util::lookup_type_name(tn, interp)?);
        }
    }
    if interp.prepared_statements.contains_key(&stmt.name) {
        return Err(DdlError::DuplicateObject(format!(
            "prepared statement \"{}\" already exists",
            stmt.name
        )));
    }
    let mut param_types = declared.clone();
    if let Some(inner) = query.node.as_ref() {
        // PrepareQuery analyzes and rewrites the query (and caches the
        // plan); it runs at EXECUTE.
        match crate::resolve::without_execution(|| {
            crate::expr::with_rewriting(true, || {
                crate::resolve::analyze_raw_node_with_param_types(interp, inner, &declared)
            })
        }) {
            Ok((_, params)) => {
                for (number, typ, _) in params {
                    let i = usize::try_from(number - 1).unwrap_or(0);
                    if i >= param_types.len() {
                        param_types.resize(i + 1, crate::pg_catalog::oid::UNKNOWN);
                    }
                    if i >= declared.len() {
                        param_types[i] = typ;
                    }
                }
            }
            Err(e) => {
                if let Some(e) = analysis_error(e) {
                    return Err(e);
                }
            }
        }
    }
    interp.prepared_statements.insert(
        stmt.name.clone(),
        PreparedStatement {
            query: query.clone(),
            param_types,
            custom_plans: 0,
        },
    );
    Ok(())
}

/// FetchPreparedStatement + EvaluateParams: the statement must exist, and
/// its arguments must be as many as its parameters, each assignable to
/// the parameter's type.
pub(crate) fn lookup_execute(
    interp: &PgCatalog,
    stmt: &ExecuteStmt,
) -> Result<PreparedStatement, DdlError> {
    let Some(prepared) = interp.prepared_statements.get(&stmt.name).cloned() else {
        return Err(DdlError::TableNotFound(format!(
            "prepared statement \"{}\" does not exist",
            stmt.name
        )));
    };
    let expected = prepared.param_types.len();
    if stmt.params.len() != expected {
        return Err(DdlError::Parse(format!(
            "wrong number of parameters for prepared statement \"{}\" (Expected {expected} \
             parameters but got {}.)",
            stmt.name,
            stmt.params.len()
        )));
    }
    let scope = crate::scope::Scope::default();
    let null_ctx = crate::nullability::NullabilityContext::default();
    let ctx = || crate::expr::Ctx::new(&scope, &null_ctx, interp);
    for (i, (arg, &target)) in stmt.params.iter().zip(&prepared.param_types).enumerate() {
        let mut params = crate::param_collector::ParamCollector::default();
        let given = crate::expr::infer_expr(arg, ctx(), &mut params, crate::expr::TypeGoal::NONE)
            .map_err(|e| DdlError::Parse(e.to_string()))?;
        if given.type_oid == crate::pg_catalog::oid::UNKNOWN {
            // An untyped literal goes through the type's input function.
            crate::expr::infer_expr(
                arg,
                ctx(),
                &mut params,
                crate::expr::TypeGoal::assignment(target),
            )
            .map_err(|e| DdlError::Parse(e.to_string()))?;
            continue;
        }
        if target != crate::pg_catalog::oid::UNKNOWN
            && !crate::coerce::can_coerce(
                given.type_oid,
                target,
                crate::coerce::CoercionContext::Assignment,
                interp,
            )
        {
            return Err(DdlError::Parse(format!(
                "parameter ${} of type {} cannot be coerced to the expected type {} (You will \
                 need to rewrite or cast the expression.)",
                i + 1,
                super::util::format_type_for_message(interp, given.type_oid),
                super::util::format_type_for_message(interp, target)
            )));
        }
    }
    Ok(prepared)
}

/// Run `plan`, the planning of an execution `stmt` of `prepared`, as
/// GetCachedPlan plans it: with arguments, the first five executions get
/// a custom plan (`choose_custom_plan`), whose constant folding and
/// selectivity estimates see the arguments as constants — `'a' ~ $1`
/// compiles the pattern `EXECUTE p('(')` gives. Later ones may get the
/// generic plan, which doesn't; so does every one under a
/// `plan_cache_mode` the migration set.
pub(crate) fn planned_for_arguments<R>(
    interp: &mut PgCatalog,
    stmt: &ExecuteStmt,
    prepared: &PreparedStatement,
    plan: impl FnOnce(&PgCatalog) -> R,
) -> R {
    if stmt.params.is_empty() {
        return plan(interp);
    }
    let Some(entry) = interp.prepared_statements.get_mut(&stmt.name) else {
        return plan(interp);
    };
    entry.custom_plans += 1;
    if prepared.custom_plans >= 5 || interp.planner_settings_changed {
        return plan(interp);
    }
    let interp = &*interp;
    // An argument the parameter's type keeps as written: a string constant
    // for a `text` / `varchar` parameter.
    let values = stmt
        .params
        .iter()
        .zip(&prepared.param_types)
        .enumerate()
        .filter(|(_, (_, t))| {
            [
                crate::pg_catalog::oid::TEXT,
                crate::pg_catalog::oid::VARCHAR,
            ]
            .contains(t)
        })
        .filter_map(|(i, (arg, _))| {
            let number = i32::try_from(i + 1).ok()?;
            Some((number, crate::expr::operators::const_string(arg, interp)?))
        })
        .collect();
    crate::expr::plan_fold::with_custom_plan(values, || plan(interp))
}

/// `EXECUTE name [(args)]`: the statement runs, its query re-analyzed
/// against the current catalog (a changed schema replans it) — and
/// planned, with the checks of a query that runs
/// ([`crate::resolve::analyze_executed`]) and its arguments
/// ([`planned_for_arguments`]).
pub(crate) fn execute(interp: &mut PgCatalog, stmt: &ExecuteStmt) -> Result<(), DdlError> {
    let prepared = lookup_execute(interp, stmt)?;
    let Some(inner) = prepared.query.node.as_ref() else {
        return Ok(());
    };
    let analyzed = planned_for_arguments(interp, stmt, &prepared, |interp| {
        crate::resolve::analyze_executed(interp, inner, || {
            crate::resolve::analyze_raw_node_with_param_types(interp, inner, &prepared.param_types)
        })
    });
    if let Err(e) = analyzed
        && let Some(e) = analysis_error(e)
    {
        return Err(e);
    }
    Ok(())
}

/// `DEALLOCATE [PREPARE] {name | ALL}` (DeallocateQuery / DropAllPreparedStatements).
pub(crate) fn deallocate(interp: &mut PgCatalog, stmt: &DeallocateStmt) -> Result<(), DdlError> {
    if stmt.isall || stmt.name.is_empty() {
        interp.prepared_statements.clear();
        return Ok(());
    }
    if interp.prepared_statements.remove(&stmt.name).is_none() {
        return Err(DdlError::TableNotFound(format!(
            "prepared statement \"{}\" does not exist",
            stmt.name
        )));
    }
    Ok(())
}

/// `DISCARD ALL` deallocates every prepared statement and closes every
/// cursor (among the rest of the session state the analyzer doesn't keep).
pub(crate) fn discard(interp: &mut PgCatalog, stmt: &DiscardStmt) -> Result<(), DdlError> {
    if DiscardMode::try_from(stmt.target) == Ok(DiscardMode::DiscardAll) {
        interp.prepared_statements.clear();
        interp.cursors.close_all();
    }
    Ok(())
}
