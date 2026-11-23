//! Mutability check for DDL expressions.
//!
//! PostgreSQL requires index expressions and `GENERATED ... STORED`
//! expressions to be IMMUTABLE (`CheckMutability`, after
//! `expression_planner` has inlined simple SQL functions): a VOLATILE *or
//! STABLE* callee is rejected. CHECK constraints are not checked at DDL
//! time.
//!
//! This module walks the expression AST and resolves each `FuncCall`
//! against `pg_proc`. Without argument types at hand it takes the
//! overloads that can take the call's argument count and rejects the call
//! when none of them is IMMUTABLE.

use typedpg_pg_query::ast;

use super::DdlError;
use crate::pg_catalog::{PgCatalog, ProVolatile};

/// Returns the qualified name (`[schema, name]` or `[name]`) of a
/// `FuncCall.funcname`. Identifiers are positional `String` nodes.
fn funcname_parts(funcname: &[Option<ast::Node>]) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = funcname
        .iter()
        .filter_map(|n| match n.as_ref()? {
            ast::Node::String(s) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [name] => Some((None, *name)),
        [schema, name] => Some((Some(*schema), *name)),
        _ => None,
    }
}

/// The mutability of a `FuncCall`: whether every overload that can take
/// the call's arguments is non-IMMUTABLE, and the body PG would inline in
/// the call's place when exactly one overload fits and is inlinable.
/// `None` if no function of that name fits.
fn funcall_volatility<'a>(
    fc: &ast::FuncCall,
    snapshot: &'a PgCatalog,
) -> Option<(ProVolatile, Option<&'a ast::Node>)> {
    let (schema, name) = funcname_parts(&fc.funcname)?;
    let nargs = fc.args.len();
    let fits: Vec<&crate::pg_catalog::PgProc> = snapshot
        .find_functions(schema, name)
        .into_iter()
        .filter(|p| {
            let pronargs = p.proargtypes.len();
            let defaults = p.pronargdefaults.max(0) as usize;
            pronargs == nargs
                || (pronargs > nargs && pronargs - defaults <= nargs)
                || (p.provariadic.is_some() && nargs + 1 >= pronargs)
        })
        .collect();
    if fits.is_empty() {
        return None;
    }
    let least = if fits.iter().any(|p| p.provolatile == ProVolatile::Immutable) {
        ProVolatile::Immutable
    } else if fits.iter().any(|p| p.provolatile == ProVolatile::Stable) {
        ProVolatile::Stable
    } else {
        ProVolatile::Volatile
    };
    let inline = match fits.as_slice() {
        [only] => snapshot
            .inline_sql_bodies
            .get(&only.oid)
            .and_then(Option::as_ref),
        _ => None,
    };
    Some((least, inline))
}

/// Every overload of the function `fc` calls that can take its arguments
/// is VOLATILE.
pub(crate) fn surely_volatile(fc: &ast::FuncCall, snapshot: &PgCatalog) -> bool {
    funcall_volatility(fc, snapshot).is_some_and(|(v, _)| v == ProVolatile::Volatile)
}

/// Whether `node` calls a surely VOLATILE function, a simple SQL function
/// counting for the body PG inlines in its place.
#[cfg(any(test, feature = "internal"))]
pub(crate) fn volatile_after_inlining(
    node: &ast::Node,
    snapshot: &PgCatalog,
    depth: usize,
) -> bool {
    node.nodes().any(|(n, _)| {
        let typedpg_pg_query::NodeRef::FuncCall(fc) = n else {
            return false;
        };
        match funcall_volatility(fc, snapshot) {
            Some((ProVolatile::Volatile, Some(body))) if depth < MAX_INLINE_DEPTH => {
                volatile_after_inlining(body, snapshot, depth + 1)
            }
            Some((ProVolatile::Volatile, _)) => true,
            _ => false,
        }
    })
}

/// Inlining depth cap: PG refuses to inline a function into itself.
const MAX_INLINE_DEPTH: usize = 16;

/// Walk `node` and return `Err` if any `FuncCall` resolves to a function
/// marked `VOLATILE`. `location` selects PG's wording for that context.
pub(crate) fn check_no_volatile(
    node: Option<&ast::Node>,
    location: ExprLocation,
    snapshot: &PgCatalog,
) -> Result<(), DdlError> {
    walk(node, location, snapshot, 0)
}

/// Analyze `expr` with the row of `relid` in scope — the way PG
/// transforms CHECK, index, generation and policy expressions — optionally
/// recording the functions it runs. `None` if the relation is unknown.
pub(crate) fn infer_over_relation(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
    expr: Option<&ast::Node>,
    used: Option<&std::cell::RefCell<Vec<crate::oid::PgProcOid>>>,
) -> Option<Result<crate::expr::ExprType, crate::error::AnalyzeError>> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::Scope;

    let class = interp.pg_class.get(&relid)?;
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
        &attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let mut ctx = crate::expr::Ctx::new(&scope, &null_ctx, interp);
    if let Some(used) = used {
        ctx = ctx.recording(used);
    }
    let expr = crate::ddl::stored_exprs::over_own_row(interp, relid, expr);
    Some(infer_expr(expr.as_ref(), ctx, &mut params, TypeGoal::NONE))
}

/// The columns of `relid` an expression over its row is strict in: when
/// the expression is non-NULL, so are they (what a partition key
/// expression's `IS NOT NULL` proves). Empty when it doesn't analyze.
pub(crate) fn strict_columns_over_relation(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
    expr: Option<&ast::Node>,
) -> Vec<String> {
    use crate::expr::{TypeGoal, infer_expr};
    let Some(class) = interp.pg_class.get(&relid) else {
        return Vec::new();
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = crate::scope::Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
        &attrs,
    );
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let log = crate::nonnull::StrictLog::default();
    let ctx = crate::expr::Ctx::new(&scope, &null_ctx, interp).logging_strictness(&log);
    let expr = &crate::ddl::stored_exprs::over_own_row(interp, relid, expr);
    if infer_expr(expr.as_ref(), ctx, &mut params, TypeGoal::NONE).is_err() {
        return Vec::new();
    }
    let mut cols: Vec<String> =
        crate::nonnull::nonnullable(expr.as_ref(), false, &scope, &log, interp)
            .columns
            .into_iter()
            .filter(|(a, _)| *a == class.relname)
            .map(|(_, c)| c)
            .collect();
    cols.sort();
    cols
}

/// What an expression over the row of `relid` resolves to, as a CHECK
/// constraint's is when it is created: its built-in comparisons and
/// `num_nulls` calls, the strictness of its nodes and the types of its
/// literals (see [`crate::nonnull::TrustedNodes`]). `None` when it doesn't
/// analyze.
pub(crate) fn trusted_over_relation(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
    expr: Option<&ast::Node>,
) -> Option<crate::nonnull::TrustedNodes> {
    use crate::expr::{TypeGoal, infer_expr};
    let class = interp.pg_class.get(&relid)?;
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = crate::scope::Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
        &attrs,
    );
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let log = crate::nonnull::StrictLog::default();
    let ctx = crate::expr::Ctx::new(&scope, &null_ctx, interp).logging_strictness(&log);
    let expr = crate::ddl::stored_exprs::over_own_row(interp, relid, expr);
    let (inferred, _) = crate::ddl::depend::collect(|| {
        let _level = crate::resolve::QueryLevel::enter();
        infer_expr(expr.as_ref(), ctx, &mut params, TypeGoal::NONE)
    });
    inferred.ok().map(|_| log.trusted())
}

/// `CheckMutability` over the *typed* expression: analyze `expr` over the
/// row of `relid`, recording every function it runs — called directly,
/// through an operator or through an explicit cast — and fail on the first
/// one that isn't IMMUTABLE. A simple SQL function counts by its inlined
/// body (checked with the name-based walk). Analysis errors are left to the
/// expression's own validation, except the planner's.
pub(crate) fn check_mutability(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
    expr: Option<&ast::Node>,
    loc: ExprLocation,
) -> Result<(), DdlError> {
    let used = std::cell::RefCell::new(Vec::new());
    // CheckMutability plans the expression first (`expression_planner`),
    // folding its constants: an error that adds (`40000::int2`) is the
    // statement's.
    match crate::expr::with_plan_time_checks(|| {
        infer_over_relation(interp, relid, expr, Some(&used))
    }) {
        Some(Ok(_)) => {}
        Some(Err(e)) if matches!(infer_over_relation(interp, relid, expr, None), Some(Ok(_))) => {
            return Err(DdlError::UnsupportedDdl(e.to_string()));
        }
        _ => return Ok(()),
    }
    for oid in used.into_inner() {
        let Some(proc) = interp.pg_proc.get(&oid) else {
            continue;
        };
        if proc.provolatile == ProVolatile::Immutable {
            continue;
        }
        match interp.inline_sql_bodies.get(&oid) {
            Some(body) => walk(body.as_ref(), loc, interp, 1)?,
            None => return Err(loc.error(&proc.proname)),
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum ExprLocation {
    Generated,
    Index,
    /// CheckPredicate.
    IndexPredicate,
    /// ComputePartitionAttrs.
    PartitionKey,
}

impl ExprLocation {
    fn error(self, fname: &str) -> DdlError {
        match self {
            ExprLocation::Generated => DdlError::UnsupportedDdl(format!(
                "generation expression is not immutable: \
                 function \"{fname}\" must be marked IMMUTABLE"
            )),
            ExprLocation::PartitionKey => DdlError::UnsupportedDdl(format!(
                "functions in partition key expression must be marked IMMUTABLE \
                 (function \"{fname}\" is not)"
            )),
            ExprLocation::IndexPredicate => DdlError::UnsupportedDdl(format!(
                "functions in index predicate must be marked IMMUTABLE \
                 (function \"{fname}\" is not)"
            )),
            ExprLocation::Index => DdlError::UnsupportedDdl(format!(
                "functions in index expression must be marked IMMUTABLE \
                 (function \"{fname}\" is not)"
            )),
        }
    }
}

fn walk(
    node: Option<&ast::Node>,
    loc: ExprLocation,
    snapshot: &PgCatalog,
    depth: usize,
) -> Result<(), DdlError> {
    let walk = |n: Option<&ast::Node>, loc: ExprLocation, snapshot: &PgCatalog| {
        walk(n, loc, snapshot, depth)
    };
    let Some(inner) = node else {
        return Ok(());
    };
    match inner {
        ast::Node::FuncCall(fc) => {
            if let Some((ProVolatile::Volatile | ProVolatile::Stable, inline)) =
                funcall_volatility(fc, snapshot)
            {
                // expression_planner inlines a simple SQL function before
                // CheckMutability looks at it (inline_function): what
                // counts is the body's volatility, not the declaration's.
                match inline {
                    Some(body) if depth < MAX_INLINE_DEPTH => {
                        self::walk(Some(body), loc, snapshot, depth + 1)?;
                    }
                    _ => {
                        let name = funcname_parts(&fc.funcname)
                            .map(|(_, n)| n.to_owned())
                            .unwrap_or_else(|| "<unknown>".into());
                        return Err(loc.error(&name));
                    }
                }
            }
            for arg in &fc.args {
                walk(arg.as_ref(), loc, snapshot)?;
            }
            if let Some(filter) = fc.agg_filter.as_ref() {
                walk(Some(filter), loc, snapshot)?;
            }
        }
        ast::Node::AExpr(e) => {
            if let Some(l) = e.lexpr.as_ref() {
                walk(Some(l), loc, snapshot)?;
            }
            if let Some(r) = e.rexpr.as_ref() {
                walk(Some(r), loc, snapshot)?;
            }
        }
        ast::Node::BoolExpr(b) => {
            for arg in &b.args {
                walk(arg.as_ref(), loc, snapshot)?;
            }
        }
        ast::Node::TypeCast(tc) => {
            if let Some(arg) = tc.arg.as_ref() {
                walk(Some(arg), loc, snapshot)?;
            }
        }
        ast::Node::NamedArgExpr(na) => {
            if let Some(arg) = na.arg.as_ref() {
                walk(Some(arg), loc, snapshot)?;
            }
        }
        ast::Node::CollateClause(cc) => {
            if let Some(arg) = cc.arg.as_ref() {
                walk(Some(arg), loc, snapshot)?;
            }
        }
        ast::Node::CoalesceExpr(c) => {
            for a in &c.args {
                walk(a.as_ref(), loc, snapshot)?;
            }
        }
        ast::Node::MinMaxExpr(m) => {
            for a in &m.args {
                walk(a.as_ref(), loc, snapshot)?;
            }
        }
        ast::Node::NullIfExpr(n) => {
            for a in &n.args {
                walk(a.as_ref(), loc, snapshot)?;
            }
        }
        ast::Node::CaseExpr(c) => {
            if let Some(arg) = c.arg.as_ref() {
                walk(Some(arg), loc, snapshot)?;
            }
            for w in &c.args {
                walk(w.as_ref(), loc, snapshot)?;
            }
            if let Some(d) = c.defresult.as_ref() {
                walk(Some(d), loc, snapshot)?;
            }
        }
        ast::Node::CaseWhen(cw) => {
            if let Some(e) = cw.expr.as_ref() {
                walk(Some(e), loc, snapshot)?;
            }
            if let Some(r) = cw.result.as_ref() {
                walk(Some(r), loc, snapshot)?;
            }
        }
        ast::Node::List(l) => {
            for item in &l.items {
                walk(item.as_ref(), loc, snapshot)?;
            }
        }
        ast::Node::SubLink(sl) => {
            if let Some(testexpr) = sl.testexpr.as_ref() {
                walk(Some(testexpr), loc, snapshot)?;
            }
            // We don't descend into subselects — PG's IMMUTABLE check
            // disallows them entirely in CHECK / GENERATED / index, but
            // we leave that as a separate gap.
        }
        ast::Node::AArrayExpr(arr) => {
            for e in &arr.elements {
                walk(e.as_ref(), loc, snapshot)?;
            }
        }
        ast::Node::RowExpr(r) => {
            for a in &r.args {
                walk(a.as_ref(), loc, snapshot)?;
            }
        }
        // Leaf-like nodes carry no sub-expression — nothing to walk.
        _ => {}
    }
    Ok(())
}
