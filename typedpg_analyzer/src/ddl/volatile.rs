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

use pg_query::protobuf::{self, node};

use super::DdlError;
use crate::pg_catalog::{PgCatalog, ProVolatile};

/// Returns the qualified name (`[schema, name]` or `[name]`) of a
/// `FuncCall.funcname`. Identifiers are positional `String` nodes.
fn funcname_parts(funcname: &[protobuf::Node]) -> Option<(Option<&str>, &str)> {
    let parts: Vec<&str> = funcname
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
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
    fc: &protobuf::FuncCall,
    snapshot: &'a PgCatalog,
) -> Option<(ProVolatile, Option<&'a protobuf::Node>)> {
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
        [only] => snapshot.inline_sql_bodies.get(&only.oid),
        _ => None,
    };
    Some((least, inline))
}

/// Inlining depth cap: PG refuses to inline a function into itself.
const MAX_INLINE_DEPTH: usize = 16;

/// Walk `node` and return `Err` if any `FuncCall` resolves to a function
/// marked `VOLATILE`. `location` selects PG's wording for that context.
pub(super) fn check_no_volatile(
    node: &protobuf::Node,
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
    expr: &protobuf::Node,
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
    Some(infer_expr(expr, ctx, &mut params, TypeGoal::NONE))
}

/// `CheckMutability` over the *typed* expression: analyze `expr` over the
/// row of `relid`, recording every function it runs — called directly,
/// through an operator or through an explicit cast — and fail on the first
/// one that isn't IMMUTABLE. A simple SQL function counts by its inlined
/// body (checked with the name-based walk). Analysis errors are left to the
/// expression's own validation.
pub(super) fn check_mutability(
    interp: &PgCatalog,
    relid: crate::oid::PgClassOid,
    expr: &protobuf::Node,
    loc: ExprLocation,
) -> Result<(), DdlError> {
    let used = std::cell::RefCell::new(Vec::new());
    if !matches!(
        infer_over_relation(interp, relid, expr, Some(&used)),
        Some(Ok(_))
    ) {
        return Ok(());
    }
    for oid in used.into_inner() {
        let Some(proc) = interp.pg_proc.get(&oid) else {
            continue;
        };
        if proc.provolatile == ProVolatile::Immutable {
            continue;
        }
        match interp.inline_sql_bodies.get(&oid) {
            Some(body) => walk(body, loc, interp, 1)?,
            None => return Err(loc.error(&proc.proname)),
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum ExprLocation {
    Generated,
    Index,
    /// CheckPredicate.
    IndexPredicate,
}

impl ExprLocation {
    fn error(self, fname: &str) -> DdlError {
        match self {
            ExprLocation::Generated => DdlError::UnsupportedDdl(format!(
                "generation expression is not immutable: \
                 function \"{fname}\" must be marked IMMUTABLE"
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
    node: &protobuf::Node,
    loc: ExprLocation,
    snapshot: &PgCatalog,
    depth: usize,
) -> Result<(), DdlError> {
    let walk =
        |n: &protobuf::Node, loc: ExprLocation, snapshot: &PgCatalog| walk(n, loc, snapshot, depth);
    let Some(inner) = node.node.as_ref() else {
        return Ok(());
    };
    match inner {
        node::Node::FuncCall(fc) => {
            if let Some((ProVolatile::Volatile | ProVolatile::Stable, inline)) =
                funcall_volatility(fc, snapshot)
            {
                // expression_planner inlines a simple SQL function before
                // CheckMutability looks at it (inline_function): what
                // counts is the body's volatility, not the declaration's.
                match inline {
                    Some(body) if depth < MAX_INLINE_DEPTH => {
                        self::walk(body, loc, snapshot, depth + 1)?;
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
                walk(arg, loc, snapshot)?;
            }
            if let Some(filter) = fc.agg_filter.as_deref() {
                walk(filter, loc, snapshot)?;
            }
        }
        node::Node::AExpr(e) => {
            if let Some(l) = e.lexpr.as_deref() {
                walk(l, loc, snapshot)?;
            }
            if let Some(r) = e.rexpr.as_deref() {
                walk(r, loc, snapshot)?;
            }
        }
        node::Node::BoolExpr(b) => {
            for arg in &b.args {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::TypeCast(tc) => {
            if let Some(arg) = tc.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::NamedArgExpr(na) => {
            if let Some(arg) = na.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::CollateClause(cc) => {
            if let Some(arg) = cc.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
        }
        node::Node::CoalesceExpr(c) => {
            for a in &c.args {
                walk(a, loc, snapshot)?;
            }
        }
        node::Node::MinMaxExpr(m) => {
            for a in &m.args {
                walk(a, loc, snapshot)?;
            }
        }
        node::Node::NullIfExpr(n) => {
            for a in &n.args {
                walk(a, loc, snapshot)?;
            }
        }
        node::Node::CaseExpr(c) => {
            if let Some(arg) = c.arg.as_deref() {
                walk(arg, loc, snapshot)?;
            }
            for w in &c.args {
                walk(w, loc, snapshot)?;
            }
            if let Some(d) = c.defresult.as_deref() {
                walk(d, loc, snapshot)?;
            }
        }
        node::Node::CaseWhen(cw) => {
            if let Some(e) = cw.expr.as_deref() {
                walk(e, loc, snapshot)?;
            }
            if let Some(r) = cw.result.as_deref() {
                walk(r, loc, snapshot)?;
            }
        }
        node::Node::List(l) => {
            for item in &l.items {
                walk(item, loc, snapshot)?;
            }
        }
        node::Node::SubLink(sl) => {
            if let Some(testexpr) = sl.testexpr.as_deref() {
                walk(testexpr, loc, snapshot)?;
            }
            // We don't descend into subselects — PG's IMMUTABLE check
            // disallows them entirely in CHECK / GENERATED / index, but
            // we leave that as a separate gap.
        }
        node::Node::AArrayExpr(arr) => {
            for e in &arr.elements {
                walk(e, loc, snapshot)?;
            }
        }
        node::Node::RowExpr(r) => {
            for a in &r.args {
                walk(a, loc, snapshot)?;
            }
        }
        // Leaf-like nodes carry no sub-expression — nothing to walk.
        _ => {}
    }
    Ok(())
}
