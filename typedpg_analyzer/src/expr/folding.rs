//! What the planner's constant folding (`eval_const_expressions`,
//! clauses.c) makes of an expression — as far as the CASE and COALESCE
//! simplifications that can change a node's collation need it (see
//! [`super::Planned`]).

use typedpg_pg_query::protobuf::{self, a_const, node};

use super::{Ctx, ExprType, Planned};
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, ProKind, SQL_LANGUAGE};

/// An expression, once `eval_const_expressions` has simplified it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Folding {
    /// A NULL constant.
    Null,
    /// A boolean constant.
    Bool(bool),
    /// Another non-NULL constant.
    Value,
    /// Never a constant: it reads a table column, and nothing it goes
    /// through can drop that (a strict function of it and constants, a
    /// boolean operator over such expressions, ...).
    Never,
    /// Anything else: it may fold into a constant the analyzer can't
    /// evaluate (`1 = 1`, `$1` bound by EXECUTE, a column of a subquery the
    /// planner pulls up, an inlined SQL function).
    Maybe,
}

/// What `node` folds into.
pub(crate) fn folding(node: &protobuf::Node, ctx: Ctx<'_>) -> Folding {
    fold(node, ctx, false)
}

/// What `node` folds into; `generation` when it is a virtual generated
/// column's expression, whose column references are the table's stored
/// columns (a generation expression can't read another generated column).
fn fold(node: &protobuf::Node, ctx: Ctx<'_>, generation: bool) -> Folding {
    let Some(inner) = node.node.as_ref() else {
        return Folding::Maybe;
    };
    let snapshot = ctx.snapshot;
    match inner {
        node::Node::AConst(c) => match &c.val {
            _ if c.isnull => Folding::Null,
            Some(a_const::Val::Boolval(b)) => Folding::Bool(b.boolval),
            _ => Folding::Value,
        },
        // A Var of a table (read directly or passed through as is) stays
        // a Var.
        node::Node::ColumnRef(_) if generation => Folding::Never,
        node::Node::ColumnRef(cr) => {
            let parts = super::extract_string_fields(&cr.fields);
            if parts.len() != cr.fields.len() {
                return Folding::Maybe;
            }
            let (table, column) = match parts.as_slice() {
                [c] => (None, c.as_str()),
                [t, c] => (Some(t.as_str()), c.as_str()),
                [_, t, c] => (Some(t.as_str()), c.as_str()),
                _ => return Folding::Maybe,
            };
            let Ok(col) = ctx.scope.resolve_column(table, column, None) else {
                return Folding::Maybe;
            };
            let Some(origin) = col.origin.as_ref() else {
                return Folding::Maybe;
            };
            let Some(attr) = snapshot
                .attributes_of(origin.relid)
                .iter()
                .find(|a| a.attname == origin.column && a.attnum > 0)
            else {
                return Folding::Never;
            };
            if attr.attgenerated != Some(crate::pg_catalog::AttGenerated::Virtual) {
                return Folding::Never;
            }
            // expand_virtual_generated_columns: the planner replaces a
            // virtual generated column by its expression (under a
            // CollateExpr giving it the column's collation, which a
            // constant absorbs), then folds that.
            match snapshot
                .attr_default_exprs
                .get(&(origin.relid, attr.attnum))
            {
                Some(crate::ddl::tables::check_inherit::StoredExpr::Written(expr)) => {
                    fold(expr, ctx, true)
                }
                _ => Folding::Maybe,
            }
        }
        // A cast of a non-constant is no constant, unless its function is
        // inlined.
        node::Node::TypeCast(c) => {
            let Some(arg) = c.arg.as_deref() else {
                return Folding::Maybe;
            };
            // coerce_type makes a NULL literal a NULL constant of the type.
            if let Some(node::Node::AConst(a)) = arg.node.as_ref()
                && a.isnull
            {
                return Folding::Null;
            }
            let Some(target) = c
                .type_name
                .as_ref()
                .and_then(|tn| crate::ddl::util::lookup_type_name(tn, snapshot).ok())
            else {
                return Folding::Maybe;
            };
            let sql_cast = snapshot.pg_cast.values().any(|cast| {
                (cast.casttarget == target || cast.casttarget == snapshot.unwrap_domain(target))
                    && cast.castfunc.is_some_and(|f| is_sql(snapshot, f))
            });
            match fold(arg, ctx, generation) {
                Folding::Never if !sql_cast => Folding::Never,
                _ => Folding::Maybe,
            }
        }
        node::Node::AExpr(e) => {
            use protobuf::AExprKind as K;
            if !matches!(
                K::try_from(e.kind),
                Ok(K::AexprOp | K::AexprLike | K::AexprIlike)
            ) {
                return Folding::Maybe;
            }
            let names = super::extract_string_fields(&e.name);
            let Some(name) = names.last() else {
                return Folding::Maybe;
            };
            if operator_may_inline(snapshot, name) {
                return Folding::Maybe;
            }
            strict_call(
                e.lexpr
                    .iter()
                    .chain(e.rexpr.iter())
                    .map(|a| fold(a, ctx, generation)),
            )
        }
        node::Node::BoolExpr(b) => {
            let args = b.args.iter().map(|a| fold(a, ctx, generation));
            match protobuf::BoolExprType::try_from(b.boolop) {
                Ok(protobuf::BoolExprType::AndExpr) => bool_args(args, false),
                Ok(protobuf::BoolExprType::OrExpr) => bool_args(args, true),
                Ok(protobuf::BoolExprType::NotExpr) => {
                    match b.args.first().map(|a| fold(a, ctx, generation)) {
                        Some(Folding::Bool(v)) => Folding::Bool(!v),
                        Some(f @ (Folding::Null | Folding::Never)) => f,
                        _ => Folding::Maybe,
                    }
                }
                _ => Folding::Maybe,
            }
        }
        node::Node::NullTest(t) => match t.arg.as_deref().map(|a| fold(a, ctx, generation)) {
            Some(Folding::Never) => Folding::Never,
            _ => Folding::Maybe,
        },
        node::Node::BooleanTest(t) => match t.arg.as_deref().map(|a| fold(a, ctx, generation)) {
            Some(Folding::Never) => Folding::Never,
            _ => Folding::Maybe,
        },
        node::Node::FuncCall(f) => {
            let names = super::extract_string_fields(&f.funcname);
            let Some(name) = names.last() else {
                return Folding::Maybe;
            };
            let candidates: Vec<_> = snapshot
                .pg_proc
                .values()
                .filter(|p| &p.proname == name)
                .collect();
            if candidates.is_empty() || candidates.iter().any(|p| p.prolang == SQL_LANGUAGE) {
                return Folding::Maybe;
            }
            // An aggregate or window call is never folded.
            if f.over.is_some()
                || f.agg_star
                || candidates
                    .iter()
                    .all(|p| matches!(p.prokind, ProKind::Aggregate | ProKind::Window))
            {
                return Folding::Never;
            }
            strict_call(f.args.iter().map(|a| {
                match a.node.as_ref() {
                    Some(node::Node::NamedArgExpr(na)) => na
                        .arg
                        .as_deref()
                        .map_or(Folding::Maybe, |a| fold(a, ctx, generation)),
                    _ => fold(a, ctx, generation),
                }
            }))
        }
        _ => Folding::Maybe,
    }
}

/// An AND (`decisive` FALSE) or OR (`decisive` TRUE) over arguments
/// folded into `args` (`simplify_and_arguments` / `simplify_or_arguments`):
/// the decisive constant if one argument is it; the other constant ones
/// dropped, then NULL if one was NULL and nothing else remains, the other
/// constant if nothing remains at all, and otherwise not a constant.
fn bool_args(args: impl Iterator<Item = Folding>, decisive: bool) -> Folding {
    let (mut null, mut never, mut maybe) = (false, false, false);
    for a in args {
        match a {
            Folding::Bool(v) if v == decisive => return Folding::Bool(decisive),
            Folding::Bool(_) => {}
            Folding::Null => null = true,
            Folding::Never => never = true,
            Folding::Value | Folding::Maybe => maybe = true,
        }
    }
    if maybe {
        Folding::Maybe
    } else if never {
        Folding::Never
    } else if null {
        Folding::Null
    } else {
        Folding::Bool(!decisive)
    }
}

/// A call of a function no support function simplifies, over arguments
/// folded into `args`: a constant when they all are (evaluated), NULL when
/// one is NULL and it is strict, and otherwise left alone.
fn strict_call(args: impl Iterator<Item = Folding>) -> Folding {
    let mut never = false;
    for a in args {
        match a {
            Folding::Never => never = true,
            Folding::Bool(_) | Folding::Value => {}
            Folding::Null | Folding::Maybe => return Folding::Maybe,
        }
    }
    if never {
        Folding::Never
    } else {
        Folding::Maybe
    }
}

fn is_sql(snapshot: &PgCatalog, proc: crate::oid::PgProcOid) -> bool {
    snapshot
        .pg_proc
        .get(&proc)
        .is_some_and(|p| p.prolang == SQL_LANGUAGE)
}

/// Some operator named `name` is a `LANGUAGE sql` function the planner may
/// inline.
pub(crate) fn operator_may_inline(snapshot: &PgCatalog, name: &str) -> bool {
    snapshot
        .pg_operator
        .values()
        .any(|o| o.oprname == name && o.oprcode.is_some_and(|f| is_sql(snapshot, f)))
}

/// The collation a CASE or COALESCE ends up with when planning replaces it
/// by `branch`, of type `common` before its coercion to the expression's
/// type.
fn replaced_by(branch: &ExprType, common: PgTypeOid, snapshot: &PgCatalog) -> Planned {
    // An untyped literal becomes a constant of the common type, with that
    // type's collation.
    if branch.type_oid == common || branch.type_oid == crate::pg_catalog::oid::UNKNOWN {
        return branch.planned_replacement();
    }
    // Another type is coerced by a node of its own, collated from the
    // branch at parse time and kept so (unless its function is inlined).
    let cast_func = snapshot
        .cast_by_pair
        .get(&(
            snapshot.unwrap_domain(branch.type_oid),
            snapshot.unwrap_domain(common),
        ))
        .and_then(|oid| snapshot.pg_cast.get(oid))
        .and_then(|c| c.castfunc);
    if cast_func.is_some_and(|f| is_sql(snapshot, f)) {
        return Planned::Unknown;
    }
    match super::derive_collation([branch], common, snapshot) {
        Ok(d) => Planned::Folded {
            collation: d.collation,
            missing: d.missing.is_some(),
        },
        Err(_) => Planned::Unknown,
    }
}

/// What `eval_const_expressions` makes of a CASE (whose WHEN results are
/// `results`, ELSE `default`, of type `common`): it drops a WHEN whose
/// condition is constant FALSE or NULL, and the CASE is the result of the
/// first remaining one if its condition is constant TRUE, or the ELSE if
/// none remains. Otherwise the CASE stays, with its parsed collation.
pub(crate) fn case_planned(
    expr: &protobuf::CaseExpr,
    results: &[ExprType],
    default: Option<&ExprType>,
    common: PgTypeOid,
    ctx: Ctx<'_>,
) -> Planned {
    let mut whens = expr.args.iter().filter_map(|a| match a.node.as_ref() {
        Some(node::Node::CaseWhen(w)) => Some(w),
        _ => None,
    });
    // Once a WHEN with a condition that isn't constant is kept, the CASE
    // stays.
    if let Some(test) = expr.arg.as_deref() {
        // A simple CASE's conditions are `test = value`: with a test that
        // never folds, one with a value that isn't NULL is left alone.
        if folding(test, ctx) != Folding::Never || operator_may_inline(ctx.snapshot, "=") {
            return Planned::Unknown;
        }
        return match whens
            .next()
            .and_then(|w| w.expr.as_deref())
            .map(|v| folding(v, ctx))
        {
            Some(Folding::Never | Folding::Value | Folding::Bool(_)) => Planned::AsParsed,
            _ => Planned::Unknown,
        };
    }
    for (when, result) in whens.zip(results) {
        match when.expr.as_deref().map(|c| folding(c, ctx)) {
            Some(Folding::Bool(false) | Folding::Null) => {}
            Some(Folding::Never) => return Planned::AsParsed,
            Some(Folding::Bool(true)) => return replaced_by(result, common, ctx.snapshot),
            _ => return Planned::Unknown,
        }
    }
    match default {
        Some(d) => replaced_by(d, common, ctx.snapshot),
        // The implicit ELSE NULL: a constant of the CASE's type.
        None => Planned::Folded {
            collation: None,
            missing: false,
        },
    }
}

/// What `eval_const_expressions` makes of a COALESCE (of arguments `args`,
/// of type `common`): it drops NULL constants, and is its first remaining
/// argument if that is a constant. Otherwise it stays, with its parsed
/// collation (a NULL constant of it, when every argument is NULL).
pub(crate) fn coalesce_planned(
    expr: &protobuf::CoalesceExpr,
    args: &[ExprType],
    common: PgTypeOid,
    ctx: Ctx<'_>,
) -> Planned {
    for (arg, t) in expr.args.iter().zip(args) {
        match folding(arg, ctx) {
            Folding::Null => {}
            Folding::Bool(_) | Folding::Value => return replaced_by(t, common, ctx.snapshot),
            // Kept, the COALESCE stays.
            Folding::Never => return Planned::AsParsed,
            Folding::Maybe => return Planned::Unknown,
        }
    }
    Planned::AsParsed
}
