use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Bool expressions (AND, OR, NOT) — PG uses COERCION_ASSIGNMENT for args
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_bool_expr(
    expr: &protobuf::BoolExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    // PG names the failing argument after the operator (`argument of NOT
    // must be type boolean, not type X`, likewise AND / OR) — the shared
    // clause walker owns the wording.
    let kind = match protobuf::BoolExprType::try_from(expr.boolop) {
        Ok(protobuf::BoolExprType::NotExpr) => crate::clause::ClauseKind::Not,
        Ok(protobuf::BoolExprType::OrExpr) => crate::clause::ClauseKind::Or,
        _ => crate::clause::ClauseKind::And,
    };
    let mut any_nullable = false;
    for arg in &expr.args {
        let t = crate::clause::coerce_clause_expr(arg, ctx, params, kind)?;
        any_nullable = any_nullable || t.nullable;
    }
    Ok(ExprType::scalar(oid::BOOL, any_nullable))
}

// ──────────────────────────────────────────────────────────────────────────────
// Common-type helpers shared by CASE / COALESCE / GREATEST / ARRAY
// ──────────────────────────────────────────────────────────────────────────────

/// PG's `select_common_type` for construct `label` (`CASE`, `COALESCE`, …)
/// followed by the `coerce_to_common_type` failure it implies, with PG's
/// wording for both: `CASE types A and B cannot be matched` (the running
/// candidate and the first input of another category, base types) and
/// `CASE/WHEN could not convert type A to B` (`CASE/ELSE` for the ELSE
/// result). All-unknown inputs resolve to text.
///
/// `nodes` are the inputs' expressions, parallel to `types`: the error
/// points at the one whose type failed, as PG does.
pub(crate) fn select_common_type(
    label: &str,
    types: &[PgTypeOid],
    nodes: &[&protobuf::Node],
    snapshot: &PgCatalog,
) -> Result<PgTypeOid, AnalyzeError> {
    let name = |t: PgTypeOid| crate::ddl::util::format_type_for_message(snapshot, t);
    coerce::select_common_type(types, snapshot).map_err(|e| match e {
        coerce::CommonTypeError::Mismatch(a, b) => {
            let span = failing_input_span(types, nodes, b, snapshot);
            let (a, b) = (name(a), name(b));
            let what = if matches!(label, "GREATEST" | "LEAST") {
                "arguments"
            } else {
                "branches"
            };
            crate::pgmsg::types_cannot_be_matched(
                label,
                &a,
                &b,
                "",
                Some(format!(
                    "add an explicit cast so the {what} share a type, e.g. `expr::{b}`"
                )),
                span,
            )
            .finalize_implicit()
        }
        coerce::CommonTypeError::CannotConvert { from, to } => {
            let span = failing_input_span(types, nodes, from, snapshot);
            // transformCaseExpr coerces the ELSE result (input 0) as
            // `CASE/ELSE` and each THEN result as `CASE/WHEN`.
            let context = match label {
                "CASE" if failing_input(types, from, snapshot) == Some(0) => "CASE/ELSE",
                "CASE" => "CASE/WHEN",
                other => other,
            };
            crate::pgmsg::could_not_convert_type(context, &name(from), &name(to), span)
                .finalize_implicit()
        }
    })
}

/// Where a common-type failure on (base) type `failing` is: the first
/// input of that type — `nodes` parallel to `types`.
pub(crate) fn failing_input_span(
    types: &[PgTypeOid],
    nodes: &[&protobuf::Node],
    failing: PgTypeOid,
    snapshot: &PgCatalog,
) -> Option<crate::error::SourceSpan> {
    let i = failing_input(types, failing, snapshot)?;
    nodes.get(i).and_then(|n| crate::error::expr_span(n))
}

/// The position of the first input of (base) type `failing`.
fn failing_input(types: &[PgTypeOid], failing: PgTypeOid, snapshot: &PgCatalog) -> Option<usize> {
    types
        .iter()
        .position(|&t| t != oid::UNKNOWN && snapshot.unwrap_domain(t) == failing)
}

/// PG's `exprTypmod` for CASE / COALESCE / GREATEST / ARRAY[]: the typmod
/// every input agrees on once coerced to `common`. Coercing an input to a
/// different type (an untyped NULL included) leaves it without a typmod,
/// so only inputs already of the common type can agree.
pub(crate) fn agreed_typmod(inputs: &[ExprType], common: PgTypeOid) -> Option<i32> {
    let first = inputs.first()?.typmod?;
    inputs
        .iter()
        .all(|t| t.type_oid == common && t.typmod == Some(first))
        .then_some(first)
}

// ──────────────────────────────────────────────────────────────────────────────
// COALESCE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

/// Whether at least one of the plain columns among `args` is known
/// non-NULL where they are read (a disjunction: `a IS NOT NULL OR b IS NOT
/// NULL`, `CHECK (num_nonnulls(a, b) = 1)`).
pub(crate) fn some_column_non_null(args: &[protobuf::Node], ctx: Ctx<'_>) -> bool {
    let cols: Vec<crate::nonnull::Col> = args
        .iter()
        .filter_map(|a| crate::nonnull::plain_column(a, ctx.scope).map(|(c, _)| c))
        .collect();
    cols.len() > 1 && ctx.null_ctx.some_non_null(&cols)
}

pub(crate) fn infer_coalesce(
    expr: &protobuf::CoalesceExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    // Pass 1: infer all args bottom-up. Bare string literals stay UNKNOWN —
    // exactly like PG's `select_common_type` — and get coerced (and their
    // content validated) under the resolved common type in pass 2, so
    // `COALESCE(int_col, '42')` is integer and `COALESCE(int_col, 'x')`
    // fails with PG's `invalid input syntax for type integer: "x"`.
    let mut args = Vec::with_capacity(expr.args.len());
    for arg in &expr.args {
        args.push(infer_expr(arg, ctx, params, TypeGoal::NONE)?);
    }
    // NULL only when every argument is — not when the quals or CHECK
    // constraints say one of its columns isn't.
    let all_nullable = args.iter().all(|t| t.nullable) && !some_column_non_null(&expr.args, ctx);

    // Resolve over the *full* arg list (unknowns included): the
    // all-identical fast path that preserves domains must see a NULL branch
    // — `COALESCE(d, NULL)` is the base type, `COALESCE(d, d)` stays `d`.
    let types: Vec<PgTypeOid> = args.iter().map(|t| t.type_oid).collect();
    let nodes: Vec<&protobuf::Node> = expr.args.iter().collect();
    let type_oid = select_common_type("COALESCE", &types, &nodes, snapshot)?;

    // Pass 2: back-fill UNKNOWN args with the resolved common type. Literal
    // content rejections propagate (PG raises them from this coercion).
    for (arg, t) in expr.args.iter().zip(&args) {
        if t.type_oid == oid::UNKNOWN {
            coerce_unknown_to(arg, ctx, params, type_oid)?;
        }
    }

    // A `$param` directly inside COALESCE is, by construction, expected to be
    // nullable — otherwise the COALESCE would be pointless. Override with
    // `$param!` to force non-null.
    for arg in &expr.args {
        if let Some(node::Node::ParamRef(p)) = arg.node.as_ref() {
            params.infer_nullable(p.number, true);
        }
    }

    let branches: Vec<(&protobuf::Node, &ExprType)> = expr.args.iter().zip(&args).collect();
    Ok(
        ExprType::scalar_with_typmod(type_oid, all_nullable, agreed_typmod(&args, type_oid))
            .with_collation(derive_collation(&args, type_oid, snapshot)?)
            .with_elem_nullable(branches_elem_nullable(&branches, type_oid, snapshot)),
    )
}

/// The columns a condition proves part of the current grouping set:
/// `GROUPING(a, b) = 0` (a conjunct of it) holds exactly on the rows of
/// the grouping sets holding every argument — PG's `GroupingFunc` sets a
/// column's bit when the set omits it — where those columns carry the
/// group's own value, not a NULL standing for "all".
fn grouping_present_columns(
    cond: &protobuf::Node,
    scope: &crate::scope::Scope,
) -> Vec<crate::nonnull::Col> {
    let mut out = Vec::new();
    match cond.node.as_ref() {
        Some(node::Node::BoolExpr(b))
            if protobuf::BoolExprType::try_from(b.boolop)
                == Ok(protobuf::BoolExprType::AndExpr) =>
        {
            for a in &b.args {
                out.extend(grouping_present_columns(a, scope));
            }
        }
        Some(node::Node::AExpr(e))
            if protobuf::AExprKind::try_from(e.kind) == Ok(protobuf::AExprKind::AexprOp)
                && extract_string_fields(&e.name).as_slice() == ["="] =>
        {
            let zero = |n: &protobuf::Node| {
                matches!(
                    n.node.as_ref(),
                    Some(node::Node::AConst(protobuf::AConst {
                        val: Some(a_const::Val::Ival(i)),
                        ..
                    })) if i.ival == 0
                )
            };
            fn grouping(n: &protobuf::Node) -> Option<&protobuf::GroupingFunc> {
                match n.node.as_ref() {
                    Some(node::Node::GroupingFunc(g)) => Some(g),
                    _ => None,
                }
            }
            let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
                return out;
            };
            let g = match (grouping(l), grouping(r)) {
                (Some(g), _) if zero(r) => g,
                (_, Some(g)) if zero(l) => g,
                _ => return out,
            };
            for a in &g.args {
                if let Some(c) = crate::having::column_of(a, scope) {
                    out.push(c);
                }
            }
        }
        _ => {}
    }
    out
}

/// The element nullability of a COALESCE / CASE over `branches` (each
/// one's node and inferred type), whose result has type `common`: what
/// every branch that can be the (non-NULL) result says of its elements.
/// A NULL constant is never a non-NULL array, an untyped array literal
/// (`'{}'`) says by its content, a branch of the result's type by its own
/// knowledge; a converted one says nothing.
pub(crate) fn branches_elem_nullable(
    branches: &[(&protobuf::Node, &ExprType)],
    common: PgTypeOid,
    snapshot: &PgCatalog,
) -> Option<bool> {
    coerce::element_type(snapshot.unwrap_domain(common), snapshot)?;
    let mut parts = Vec::with_capacity(branches.len());
    for (node, t) in branches {
        match node.node.as_ref() {
            Some(node::Node::AConst(c)) if c.isnull => continue,
            Some(node::Node::AConst(c)) if t.type_oid == oid::UNKNOWN => match &c.val {
                Some(a_const::Val::Sval(sv)) => parts.push(Some(
                    crate::literal_input::array_literal_may_contain_null(&sv.sval),
                )),
                _ => parts.push(None),
            },
            _ if snapshot.unwrap_domain(t.type_oid) == snapshot.unwrap_domain(common) => {
                parts.push(t.elem_nullable)
            }
            _ => parts.push(None),
        }
    }
    if parts.is_empty() {
        return None;
    }
    merge_elem_nullable(parts)
}

// ──────────────────────────────────────────────────────────────────────────────
// CASE — two-pass (PG chapter 10.5)
// ──────────────────────────────────────────────────────────────────────────────

/// PG's `transformCaseExpr`.
///
/// - Simple CASE (`CASE arg WHEN val …`): an untyped test expression is
///   forced to text first ("force it to text … good enough to handle the
///   sort of silly coding commonly seen"), then each WHEN becomes
///   `CaseTestExpr = val`, resolved as an ordinary `=` operator.
/// - Searched CASE: each WHEN condition is coerced to boolean.
/// - The result type is `select_common_type` over the ELSE result *first*
///   (`lcons(defresult, resultexprs)`; an omitted ELSE is a NULL), then the
///   THEN results in order — so `CASE WHEN b THEN varchar_col ELSE
///   char_col END` is bpchar.
pub(crate) fn infer_case(
    expr: &protobuf::CaseExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let test_oid = match expr.arg.as_deref() {
        Some(arg) => {
            let t = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
            if t.type_oid == oid::UNKNOWN {
                coerce_unknown_to(arg, ctx, params, oid::TEXT)?;
                Some(oid::TEXT)
            } else {
                Some(t.type_oid)
            }
        }
        None => None,
    };

    // Pass 1: WHEN conditions, then each THEN result with no goal — read
    // knowing what its WHEN being TRUE, and every earlier WHEN not being
    // TRUE, proves non-NULL. Facts are kept to this level's FROM entries.
    let own = crate::nonnull::own_aliases(ctx.scope);
    let mut not_taken = crate::nonnull::Facts::default();
    let mut results: Vec<(&protobuf::Node, ExprType)> = Vec::new();
    for arg in &expr.args {
        let Some(node::Node::CaseWhen(when)) = arg.node.as_ref() else {
            continue;
        };
        let log = crate::nonnull::StrictLog::default();
        let mut taken = crate::nonnull::Facts::default();
        if let Some(cond) = &when.expr {
            let cond_ctx = ctx.logging_strictness(&log);
            match (test_oid, expr.arg.as_deref()) {
                (Some(test_oid), Some(test)) => {
                    infer_synthetic_op(
                        "=",
                        &typed_null(test_oid),
                        cond,
                        when.location,
                        cond_ctx,
                        params,
                    )?;
                    // `test = value` TRUE: both non-NULL, if `=` is strict.
                    if log.is_strict(when.location, crate::nonnull::StrictNode::Op) {
                        taken = crate::nonnull::nonnullable(test, false, ctx.scope, &log, snapshot)
                            .union(crate::nonnull::nonnullable(
                                cond, false, ctx.scope, &log, snapshot,
                            ));
                    }
                }
                // `argument of CASE/WHEN must be type boolean, not type X` —
                // wording and ordering live in the shared clause walker.
                _ => {
                    crate::clause::coerce_clause_expr(
                        cond,
                        cond_ctx,
                        params,
                        crate::clause::ClauseKind::CaseWhen,
                    )?;
                    taken = crate::nonnull::nonnullable(cond, true, ctx.scope, &log, snapshot);
                }
            }
        }
        // Untyped string literals stay UNKNOWN for branch reconciliation and
        // are validated under the resolved common type in pass 2.
        if let Some(result) = &when.result {
            let facts = not_taken.clone().union(taken).restricted_to(&own);
            let mut narrowed = ctx.null_ctx.with_local_facts(facts);
            // `WHEN grouping(g) = 0`: the row's grouping set holds `g`.
            let present = match (&expr.arg, &when.expr) {
                (None, Some(cond)) => grouping_present_columns(cond, ctx.scope),
                _ => Vec::new(),
            };
            if !present.is_empty() {
                let mut n = narrowed.unwrap_or_else(|| ctx.null_ctx.clone());
                for c in &present {
                    n.grouping_omitted.remove(c);
                }
                narrowed = Some(n);
            }
            let result_ctx = match &narrowed {
                Some(n) => ctx.with_null_ctx(n),
                None => ctx,
            };
            results.push((
                result,
                infer_expr(result, result_ctx, params, TypeGoal::NONE)?,
            ));
        }
        if let (None, Some(cond)) = (&expr.arg, &when.expr) {
            not_taken = not_taken.union(crate::nonnull::nonnullable_unless_true(
                cond, ctx.scope, &log, snapshot,
            ));
        }
    }
    // The ELSE result (or PG's implicit `ELSE NULL`) leads the list.
    let default = match &expr.defresult {
        Some(d) => {
            let narrowed = ctx.null_ctx.with_local_facts(not_taken.restricted_to(&own));
            let default_ctx = match &narrowed {
                Some(n) => ctx.with_null_ctx(n),
                None => ctx,
            };
            Some((
                d.as_ref(),
                infer_expr(d, default_ctx, params, TypeGoal::NONE)?,
            ))
        }
        None => None,
    };
    let mut inputs: Vec<ExprType> = Vec::with_capacity(results.len() + 1);
    inputs.push(
        default
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| ExprType::scalar(oid::UNKNOWN, true)),
    );
    inputs.extend(results.iter().map(|(_, t)| t.clone()));

    let types: Vec<PgTypeOid> = inputs.iter().map(|t| t.type_oid).collect();
    // Parallel to `types` (the implicit ELSE NULL has no node: an untyped
    // NULL never fails the match).
    let null_node = protobuf::Node { node: None };
    let nodes: Vec<&protobuf::Node> =
        std::iter::once(default.as_ref().map_or(&null_node, |(n, _)| *n))
            .chain(results.iter().map(|(n, _)| *n))
            .collect();
    let type_oid = select_common_type("CASE", &types, &nodes, snapshot)?;

    // Pass 2: back-fill UNKNOWN results with the common type. Literal
    // content rejections propagate (PG raises them from this coercion).
    for (node, t) in default.iter().chain(results.iter()) {
        if t.type_oid == oid::UNKNOWN {
            coerce_unknown_to(node, ctx, params, type_oid)?;
        }
    }

    let nullable = inputs.iter().any(|t| t.nullable);
    // Collations merge in tree order: THEN results, then ELSE.
    let collation = derive_collation(inputs[1..].iter().chain(&inputs[..1]), type_oid, snapshot)?;
    // (The implicit ELSE NULL is never a non-NULL array.)
    let branches: Vec<(&protobuf::Node, &ExprType)> = default
        .iter()
        .chain(results.iter())
        .map(|(n, t)| (*n, t))
        .collect();
    Ok(
        ExprType::scalar_with_typmod(type_oid, nullable, agreed_typmod(&inputs, type_oid))
            .with_collation(collation)
            .with_elem_nullable(branches_elem_nullable(&branches, type_oid, snapshot)),
    )
}
