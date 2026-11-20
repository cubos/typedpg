//! What the planner's constant folding (`eval_const_expressions`) may
//! reduce an expression to.
//!
//! CASE drops a WHEN folded to FALSE or NULL without folding its result,
//! and everything after one folded to TRUE; COALESCE drops its NULL
//! arguments and stops at the first other constant; AND / OR stop at the
//! first FALSE / TRUE. What comes after a condition or argument that may
//! fold is then never certainly folded itself (see
//! [`super::skipped_if`]). Folding reaches much further than constants: a
//! strict operator with a NULL argument is NULL (`NULL + x`), `x > 0 AND
//! false` is FALSE, `ROW(x, 1) IS NULL` is FALSE, a column of a subquery
//! the planner pulls up is the subquery's expression (maybe a constant), a
//! SQL function is inlined. So the question asked is the other way round:
//! what certainly *never* folds — a column read from a table, and
//! expressions over such columns no constant can short-circuit. Anything
//! else may fold.

use typedpg_pg_query::ast;

use super::Ctx;
use crate::pg_catalog::{AttGenerated, PgCatalog, SQL_LANGUAGE};

/// The constants an expression may fold to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Foldable {
    /// It may fold to NULL.
    pub null: bool,
    /// It may fold to a value other than NULL.
    pub value: bool,
}

impl Foldable {
    /// Never folded: it reads a table's row.
    const NEVER: Foldable = Foldable {
        null: false,
        value: false,
    };
    /// May fold to any constant.
    const ANY: Foldable = Foldable {
        null: true,
        value: true,
    };
    /// May fold to a non-NULL value (a test: `IS NULL`, `IS TRUE`).
    const VALUE: Foldable = Foldable {
        null: false,
        value: true,
    };

    /// It may fold to some constant.
    pub(crate) fn may_fold(self) -> bool {
        self.null || self.value
    }
}

/// What `node` may fold to, in the scope of `ctx`.
pub(crate) fn foldable(node: Option<&ast::Node>, ctx: Ctx<'_>) -> Foldable {
    foldable_node(node, ctx)
}

fn foldable_node(node: Option<&ast::Node>, ctx: Ctx<'_>) -> Foldable {
    if casts_may_inline(ctx.snapshot) {
        return Foldable::ANY;
    }
    shape(node, ctx)
}

/// Whether the test of a CASE's WHEN may fold: the condition, or for
/// `CASE arg WHEN v` the comparison `arg = v`.
pub(crate) fn case_test_may_fold(
    arg: Option<&ast::Node>,
    cond: Option<&ast::Node>,
    ctx: Ctx<'_>,
) -> bool {
    let Some(cond) = cond else {
        return true;
    };
    match arg {
        None => foldable_node(Some(cond), ctx).may_fold(),
        Some(arg) => strict([
            foldable_node(Some(arg), ctx),
            foldable_node(Some(cond), ctx),
        ])
        .may_fold(),
    }
}

fn shape(node: Option<&ast::Node>, ctx: Ctx<'_>) -> Foldable {
    crate::stack::step(|| shape_here(node, ctx))
}

fn shape_here(node: Option<&ast::Node>, ctx: Ctx<'_>) -> Foldable {
    let Some(inner) = node else {
        return Foldable::ANY;
    };
    let all = |nodes: &mut dyn Iterator<Item = Option<&ast::Node>>| {
        nodes.map(|n| shape(n, ctx)).collect::<Vec<_>>()
    };
    match inner {
        ast::Node::AConst(c) => {
            if c.isnull {
                Foldable {
                    null: true,
                    value: false,
                }
            } else {
                Foldable::VALUE
            }
        }
        // A custom plan substitutes the parameter's value.
        ast::Node::ParamRef(_) => Foldable::ANY,
        ast::Node::ColumnRef(cr) => column(cr, ctx),
        ast::Node::TypeCast(tc) => match tc.arg.as_ref() {
            Some(arg) => strict([shape(Some(arg), ctx)]),
            None => Foldable::ANY,
        },
        ast::Node::CollateClause(c) => match c.arg.as_ref() {
            Some(arg) => shape(Some(arg), ctx),
            None => Foldable::ANY,
        },
        ast::Node::AExpr(e) => {
            let names = super::extract_string_fields(&e.name);
            if matches!(
                e.kind,
                ast::AExprKind::AexprOp | ast::AExprKind::AexprOpAny | ast::AExprKind::AexprOpAll
            ) && names
                .last()
                .is_some_and(|op| operator_may_inline(op, ctx.snapshot))
            {
                return Foldable::ANY;
            }
            // The operands, a list's items each (`IN (…)`, `BETWEEN`); a
            // row comparison compares field by field, an AND / OR of
            // strict comparisons — a row with a field that may fold may
            // too (see `row`).
            let operands: Vec<Option<&ast::Node>> = [e.lexpr.as_ref(), e.rexpr.as_ref()]
                .into_iter()
                .flatten()
                .flat_map(|n| match n {
                    ast::Node::List(l) => l.items.iter().map(Option::as_ref).collect(),
                    _ => vec![Some(n)],
                })
                .collect();
            strict(operands.into_iter().map(|n| shape(n, ctx)))
        }
        ast::Node::BoolExpr(b) => {
            let args = all(&mut b.args.iter().map(Option::as_ref));
            match b.boolop {
                ast::BoolExprType::NotExpr => args.first().copied().unwrap_or(Foldable::ANY),
                // `simplify_and_arguments` / `simplify_or_arguments`: a FALSE
                // (TRUE) argument is the result; constants only otherwise.
                _ => {
                    let every = args.iter().all(|a| a.may_fold());
                    Foldable {
                        null: every,
                        value: every || args.iter().any(|a| a.value),
                    }
                }
            }
        }
        ast::Node::NullTest(t) => match t.arg.as_ref() {
            Some(arg) if shape(Some(arg), ctx).may_fold() => Foldable::VALUE,
            Some(_) => Foldable::NEVER,
            None => Foldable::VALUE,
        },
        ast::Node::BooleanTest(t) => match t.arg.as_ref() {
            Some(arg) if shape(Some(arg), ctx).may_fold() => Foldable::VALUE,
            Some(_) => Foldable::NEVER,
            None => Foldable::VALUE,
        },
        // A CASE folds only through a test that does.
        ast::Node::CaseExpr(c) => {
            let arg = c.arg.as_ref();
            let any_test = c.args.iter().any(|w| match w.as_ref() {
                Some(ast::Node::CaseWhen(w)) => case_test_may_fold(arg, w.expr.as_ref(), ctx),
                _ => true,
            });
            if any_test {
                Foldable::ANY
            } else {
                Foldable::NEVER
            }
        }
        // NULL arguments are dropped, and the arguments after the first
        // other constant.
        ast::Node::CoalesceExpr(c) => {
            let mut out = Foldable {
                null: true,
                value: false,
            };
            for a in &c.args {
                let a = shape(a.as_ref(), ctx);
                out.value |= a.value;
                if !a.null {
                    out.null = false;
                    break;
                }
            }
            out
        }
        // A sublink becomes a SubPlan: it is never folded.
        ast::Node::SubLink(_) => Foldable::NEVER,
        ast::Node::RowExpr(r) => row(&r.args, ctx),
        ast::Node::FuncCall(f) => {
            // A window function, or an aggregate over `*`: never folded.
            if f.over.is_some() || f.agg_star {
                return Foldable::NEVER;
            }
            let names = super::extract_string_fields(&f.funcname);
            if names
                .last()
                .is_none_or(|name| function_may_inline(name, ctx.snapshot))
            {
                return Foldable::ANY;
            }
            // A stable or volatile function is never evaluated while
            // folding (evaluate_function).
            if super::plan_fold::never_folded(f, ctx.snapshot) {
                return Foldable::NEVER;
            }
            strict(f.args.iter().map(|a| shape(a.as_ref(), ctx)))
        }
        ast::Node::NamedArgExpr(a) => match a.arg.as_ref() {
            Some(arg) => shape(Some(arg), ctx),
            None => Foldable::ANY,
        },
        ast::Node::AIndirection(i) => strict(
            i.arg
                .as_ref()
                .into_iter()
                .map(Some)
                .chain(i.indirection.iter().map(Option::as_ref))
                .map(|n| shape(n, ctx)),
        ),
        ast::Node::AIndices(i) => strict(
            [i.lidx.as_ref(), i.uidx.as_ref()]
                .into_iter()
                .flatten()
                .map(|n| shape(Some(n), ctx)),
        ),
        // A field name of an indirection.
        ast::Node::String(_) | ast::Node::AStar(_) => Foldable::NEVER,
        ast::Node::AArrayExpr(a) => strict(a.elements.iter().map(|n| shape(n.as_ref(), ctx))),
        ast::Node::MinMaxExpr(m) => strict(m.args.iter().map(|n| shape(n.as_ref(), ctx))),
        // Anything else (a SQL/JSON or XML construct, …) may.
        _ => Foldable::ANY,
    }
}

/// A strict function of `args` (built-in operators, casts and functions
/// mostly are; a non-strict one folds less): NULL when an argument is,
/// anything when they all are constants — a value only when none is NULL.
fn strict(args: impl IntoIterator<Item = Foldable>) -> Foldable {
    let (mut any_null, mut every, mut values) = (false, true, true);
    for a in args {
        any_null |= a.null;
        every &= a.may_fold();
        values &= a.value;
    }
    Foldable {
        null: any_null || every,
        value: values,
    }
}

/// A row (`ROW(…)`, `(a, b)`): compared or tested field by field, so a
/// field that may fold makes the comparison or test one that may.
fn row(fields: &[Option<ast::Node>], ctx: Ctx<'_>) -> Foldable {
    if fields.iter().any(|f| shape(f.as_ref(), ctx).may_fold()) {
        Foldable::ANY
    } else {
        Foldable::NEVER
    }
}

/// A column reference: a table's column is never folded. A subquery's or
/// a VALUES list's output, once the planner pulls it up, is the expression
/// it holds; a rule's NEW / OLD the statement's value; a SQL function's
/// parameter the call's argument; a virtual generated column its
/// expression — all may be constants.
fn column(cr: &ast::ColumnRef, ctx: Ctx<'_>) -> Foldable {
    let parts: Vec<&str> = cr
        .fields
        .iter()
        .map(|f| match f.as_ref() {
            Some(ast::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    let (table, name) = match parts.as_slice() {
        [name] => (None, *name),
        [table, name] | [_, table, name] => (Some(*table), *name),
        _ => return Foldable::ANY,
    };
    let scope = ctx.scope;
    if let Some(t) = table {
        let nearest = std::iter::once(&scope.sources)
            .chain(scope.outer_tiers.iter())
            .chain(std::iter::once(&scope.outer_sources))
            .find_map(|tier| tier.iter().find(|s| s.alias == t));
        if nearest.is_none_or(|s| s.table_only) {
            return Foldable::ANY;
        }
    }
    let Ok(col) = scope.resolve_column(table, name, None) else {
        return Foldable::ANY;
    };
    let Some(origin) = &col.origin else {
        return Foldable::ANY;
    };
    let virtual_generated = ctx
        .snapshot
        .attributes_of(origin.relid)
        .iter()
        .any(|a| a.attname == origin.column && a.attgenerated == Some(AttGenerated::Virtual));
    if virtual_generated {
        Foldable::ANY
    } else {
        Foldable::NEVER
    }
}

/// Whether a function named `name` may be a SQL function the planner
/// inlines whose body need not use its arguments: one outside
/// `pg_catalog` (the built-in SQL functions — `log(numeric)`,
/// `integer_pl_date`, … — are their arguments run through more built-in
/// functions, which [`strict`] covers).
fn function_may_inline(name: &str, snapshot: &PgCatalog) -> bool {
    snapshot.namespace_by_name.iter().any(|(nspname, ns)| {
        nspname != "pg_catalog"
            && snapshot
                .proc_by_qname
                .get(&(*ns, name.to_owned()))
                .is_some_and(|procs| procs.iter().any(|p| is_sql(*p, snapshot)))
    })
}

/// Whether an operator named `op` may run such a function (see
/// [`function_may_inline`]).
fn operator_may_inline(op: &str, snapshot: &PgCatalog) -> bool {
    snapshot.pg_operator.values().any(|o| {
        o.oprname == op
            && o.oprcode
                .is_some_and(|p| is_sql(p, snapshot) && !in_pg_catalog(p, snapshot))
    })
}

/// Whether some cast runs such a function: one the parser inserts
/// implicitly may then be inlined anywhere.
fn casts_may_inline(snapshot: &PgCatalog) -> bool {
    snapshot.pg_cast.values().any(|c| {
        c.castfunc
            .is_some_and(|p| is_sql(p, snapshot) && !in_pg_catalog(p, snapshot))
    })
}

fn is_sql(p: crate::oid::PgProcOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .pg_proc
        .get(&p)
        .is_some_and(|p| p.prolang == SQL_LANGUAGE)
}

fn in_pg_catalog(p: crate::oid::PgProcOid, snapshot: &PgCatalog) -> bool {
    snapshot
        .pg_proc
        .get(&p)
        .is_some_and(|p| snapshot.namespace_name(p.pronamespace) == Some("pg_catalog"))
}
