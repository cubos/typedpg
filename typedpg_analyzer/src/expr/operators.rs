use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Operators (A_Expr) — two-pass (PG chapter 10.2)
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve an `A_Expr` node to its result type.
///
/// PG's parser special-cases a handful of `A_Expr` shapes before reaching the
/// generic operator-resolution path (NULLIF, IS DISTINCT FROM, BETWEEN, IN,
/// ANY/ALL, and the two ROW-comparison forms). Each is handled by a dedicated
/// `handle_*` helper that returns `Some(type)` when it claims the node and
/// `None` to fall through. The generic binary-operator resolution (two-pass
/// type inference + `find_operator`) lives at the bottom of this function.
pub(crate) fn infer_a_expr(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let op_name = extract_string_fields(&expr.name).join(".");
    let op_name = op_name.as_str();

    // Kind-tagged special forms, in PG's recognition order.
    if let Some(t) = handle_nullif(expr, ctx, params)? {
        return Ok(t);
    }
    if let Some(t) = handle_distinct_from(expr, ctx, params)? {
        return Ok(t);
    }
    if let Some(t) = handle_between(expr, ctx, params)? {
        return Ok(t);
    }
    if let Some(t) = handle_in_list(expr, ctx, params)? {
        return Ok(t);
    }
    if let Some(t) = handle_any_all(expr, ctx, params)? {
        return Ok(t);
    }
    // ROW-shaped comparisons keyed off the operator symbol, not the kind.
    if let Some(t) = handle_row_row(expr, op_name, ctx, params)? {
        return Ok(t);
    }
    if let Some(t) = handle_row_subselect(expr, op_name, ctx, params)? {
        return Ok(t);
    }

    infer_generic_binary_op(expr, op_name, ctx, params)
}

/// `NULLIF(v1, v2)` — an AExpr of kind NULLIF with op `=`. PG's
/// `transformAExprNullIf` resolves `v1 = v2` like any operator (`make_op`),
/// requires it to yield boolean, and types the result as the operator's
/// *coerced left input* — so `NULLIF(varchar_col, 'x')` is text (varchar
/// has no `=` of its own; `text = text` is chosen) and keeps the left
/// input's typmod only when it needed no coercion. Always nullable.
fn handle_nullif(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    if !matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprNullif)
    ) {
        return Ok(None);
    }
    let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Err(AnalyzeError::Internal("NULLIF without operands".into()));
    };
    let left = infer_expr(lexpr, ctx, params, TypeGoal::NONE)?;
    let right = infer_expr(rexpr, ctx, params, TypeGoal::NONE)?;
    let name = |t: PgTypeOid| crate::ddl::util::format_type_for_message(snapshot, t);
    let op = match snapshot.find_operator_detailed("=", Some(left.type_oid), right.type_oid) {
        crate::lookup::OperatorMatch::Found(op) => op,
        crate::lookup::OperatorMatch::Ambiguous => {
            return Err(crate::pgmsg::operator_is_not_unique(
                &name(left.type_oid),
                "=",
                &name(right.type_oid),
                None,
            )
            .finalize_implicit());
        }
        crate::lookup::OperatorMatch::NotFound => {
            return Err(crate::pgmsg::nullif_types_mismatch(
                &name(left.type_oid),
                &name(right.type_oid),
            ));
        }
        crate::lookup::OperatorMatch::Error(e) => return Err(e),
    };
    // The coercion make_op performs: untyped sides take the declared types.
    let declared_left = op.left_type_oid.unwrap_or(left.type_oid);
    if left.type_oid == oid::UNKNOWN {
        coerce_unknown_to(lexpr, ctx, params, declared_left)?;
    }
    if right.type_oid == oid::UNKNOWN {
        coerce_unknown_to(rexpr, ctx, params, op.right_type_oid)?;
    }
    if op.result_type_oid != oid::BOOL {
        return Err(AnalyzeError::DatatypeMismatch(
            "NULLIF requires = operator to yield boolean".into(),
        ));
    }
    // A polymorphic / pseudo declared input (anyarray, anyenum, record, …)
    // is coerced to the actual argument type, not to the pseudo-type —
    // except that `coerce_type` relabels a domain to its base type for the
    // pseudo-types whose value must be a true array, enum, range or
    // multirange (`NULLIF(intarr_col, '{}')` is `integer[]`).
    let declared_type = snapshot.get_type(declared_left);
    let pseudo = declared_type.is_some_and(|t| t.typtype == TypType::Pseudo);
    let flattens_domain = declared_type.is_some_and(|t| {
        t.typtype == TypType::Pseudo
            && matches!(
                t.typname.as_str(),
                "anyarray"
                    | "anyenum"
                    | "anyrange"
                    | "anymultirange"
                    | "anycompatiblearray"
                    | "anycompatiblerange"
                    | "anycompatiblemultirange"
            )
    });
    let result_oid = if pseudo || declared_left == oid::UNKNOWN {
        let actual = if left.type_oid == oid::UNKNOWN {
            right.type_oid
        } else {
            left.type_oid
        };
        if flattens_domain {
            snapshot.unwrap_domain(actual)
        } else {
            actual
        }
    } else {
        declared_left
    };
    let typmod = (result_oid == left.type_oid)
        .then_some(left.typmod)
        .flatten();
    let state = derive_collation([&left, &right], result_oid, snapshot)?;
    Ok(Some(
        ExprType::scalar_with_typmod(result_oid, true, typmod).with_collation(state),
    ))
}

/// `expr IS [NOT] DISTINCT FROM other` — PG's `transformAExprDistinct`:
/// when either raw side is an undecorated `NULL` it becomes a NullTest of
/// the other side (`$1 IS DISTINCT FROM NULL` is `$1 IS NOT NULL`, which
/// leaves `$1` untypable); otherwise the `=` operator is resolved like any
/// other. The result is always bool NOT NULL.
fn handle_distinct_from(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    if !matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprDistinct) | Ok(protobuf::AExprKind::AexprNotDistinct)
    ) {
        return Ok(None);
    }
    let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Err(AnalyzeError::Internal(
            "IS DISTINCT FROM without operands".into(),
        ));
    };
    let is_null_const =
        |n: &protobuf::Node| matches!(n.node.as_ref(), Some(node::Node::AConst(c)) if c.isnull);
    let null_test_of = if is_null_const(rexpr) {
        Some(lexpr)
    } else if is_null_const(lexpr) {
        Some(rexpr)
    } else {
        None
    };
    // ExecEvalDistinct: two NULLs are not distinct, one NULL is; two
    // values compare with `=`, and a NULL from it is the result.
    let mut nullable = false;
    if let Some(arg) = null_test_of {
        let t = protobuf::NullTest {
            arg: Some(Box::new(arg.clone())),
            nulltesttype: protobuf::NullTestType::IsNotNull as i32,
            ..Default::default()
        };
        let n = protobuf::Node {
            node: Some(node::Node::NullTest(Box::new(t))),
        };
        infer_expr(&n, ctx, params, TypeGoal::NONE)?;
    } else if let (Some(node::Node::RowExpr(l)), Some(node::Node::RowExpr(r))) =
        (lexpr.node.as_ref(), rexpr.node.as_ref())
    {
        // make_row_distinct_op: a pairwise `=` per column, ORed.
        let ops = row_pairwise_op("=", &l.args, &r.args, expr.location, ctx, params, |_| {
            "IS DISTINCT FROM requires = operator to yield boolean".to_string()
        })?
        .1;
        nullable = ops.iter().flatten().any(|o| {
            functions::operator_result_nullable(ctx.snapshot, "=", o.code, &[false, false])
        });
    } else {
        infer_synthetic_op("=", lexpr, rexpr, expr.location, ctx, params)?;
        nullable = pair_operator("=", lexpr, rexpr, ctx, params).is_some_and(|o| {
            functions::operator_result_nullable(ctx.snapshot, "=", o.code, &[false, false])
        });
        // Not distinct from a non-NULL side, the other side is non-NULL
        // too (see `nonnull::distinct_facts`). A peek, on a throwaway
        // collector.
        if ctx.strict_log.is_some() {
            for (side, kind) in [
                (lexpr, crate::nonnull::StrictNode::DistinctLeftNonNull),
                (rexpr, crate::nonnull::StrictNode::DistinctRightNonNull),
            ] {
                let mut scratch = params.clone();
                let non_null =
                    infer_expr(side, ctx, &mut scratch, TypeGoal::NONE).is_ok_and(|t| !t.nullable);
                ctx.note_strict(expr.location, kind, non_null);
            }
        }
    }
    Ok(Some(ExprType::scalar(oid::BOOL, nullable)))
}

/// `expr [NOT] BETWEEN lo AND hi` (and the SYM variants) — rexpr is a
/// `Node::List` holding the two bounds. PG's `transformAExprBetween`
/// rewrites the construct into plain comparisons and transforms those:
///
/// - `a BETWEEN b AND c` → `a >= b AND a <= c`
/// - `a NOT BETWEEN b AND c` → `a < b OR a > c`
/// - `a BETWEEN SYMMETRIC b AND c` → `(a >= b AND a <= c) OR (a >= c AND a <= b)`
/// - `a NOT BETWEEN SYMMETRIC b AND c` → `(a < b OR a > c) AND (a < c OR a > b)`
///
/// Each comparison goes through ordinary operator resolution, in that
/// order, so an untyped side is typed by its peer (`$p BETWEEN 1 AND 5`
/// pins `$p` to int4 in the first comparison).
fn handle_between(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    use protobuf::AExprKind as Kind;
    let kind = protobuf::AExprKind::try_from(expr.kind);
    if !matches!(
        kind,
        Ok(Kind::AexprBetween
            | Kind::AexprNotBetween
            | Kind::AexprBetweenSym
            | Kind::AexprNotBetweenSym)
    ) {
        return Ok(None);
    }
    let (Some(a), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Err(AnalyzeError::Internal("BETWEEN without operands".into()));
    };
    let Some(node::Node::List(list)) = rexpr.node.as_ref() else {
        return Err(AnalyzeError::Internal(
            "BETWEEN bounds are not a list".into(),
        ));
    };
    let [b, c] = list.items.as_slice() else {
        return Err(AnalyzeError::Internal("BETWEEN needs two bounds".into()));
    };
    let comparisons: &[(&str, &protobuf::Node)] = match kind {
        Ok(Kind::AexprBetween) => &[(">=", b), ("<=", c)],
        Ok(Kind::AexprNotBetween) => &[("<", b), (">", c)],
        Ok(Kind::AexprBetweenSym) => &[(">=", b), ("<=", c), (">=", c), ("<=", b)],
        _ => &[("<", b), (">", c), ("<", c), (">", b)],
    };
    let mut nullable = false;
    for &(op, bound) in comparisons {
        let t = infer_synthetic_op(op, a, bound, expr.location, ctx, params)?;
        nullable |= t.nullable;
    }
    Ok(Some(ExprType::scalar(oid::BOOL, nullable)))
}

/// `a IN (x, y, …)` / `a NOT IN (…)` (typedpg_pg_query tags NOT IN with op `<>`) —
/// PG's `transformAExprIn`. When more than one list item is free of Vars of
/// the current query level, PG tries to fold those into one
/// `a op ANY(ARRAY[…])`: it selects the common type of `a` and those items
/// (`a` first, so it wins ties such as all-unknown items), coerces them to
/// it and resolves `a op common`. Every other item (all of them when that
/// fails) becomes its own `a op item` comparison, resolved like any binary
/// operator — which is what types `$p` in `$p IN (n, 2)` as int4.
fn handle_in_list(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    if !matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprIn)
    ) {
        return Ok(None);
    }
    let Ctx { snapshot, .. } = ctx;
    let (Some(a), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Err(AnalyzeError::Internal("IN without operands".into()));
    };
    let Some(node::Node::List(list)) = rexpr.node.as_ref() else {
        return Err(AnalyzeError::Internal("IN list is not a list".into()));
    };
    let op_name = extract_string_fields(&expr.name).join(".");
    let op = if op_name == "<>" { "<>" } else { "=" };

    let left = infer_expr(a, ctx, params, TypeGoal::NONE)?;
    let mut nullable = left.nullable;
    let mut items = Vec::with_capacity(list.items.len());
    for item in &list.items {
        let t = infer_expr(item, ctx, params, TypeGoal::NONE)?;
        nullable |= t.nullable;
        items.push((item, t, contains_level0_column_ref(item)));
    }

    let nonvars: Vec<usize> = (0..items.len()).filter(|&i| !items[i].2).collect();
    let mut folded = vec![false; items.len()];
    if nonvars.len() > 1 {
        let mut all = vec![left.type_oid];
        all.extend(nonvars.iter().map(|&i| items[i].1.type_oid));
        if let Some(common) = coerce::find_common_type(&all, snapshot)
            && common != oid::RECORD
            && snapshot.array_type_of(common).is_some()
        {
            for &i in &nonvars {
                if items[i].1.type_oid == oid::UNKNOWN {
                    coerce_unknown_to(items[i].0, ctx, params, common)?;
                }
                folded[i] = true;
                // Each item is coerced to the common type.
                nullable |= super::coercion_can_return_null(items[i].1.type_oid, common, snapshot);
            }
            // `a op ANY(common[])` resolves `a op common`.
            infer_synthetic_op(op, a, &typed_null(common), expr.location, ctx, params)?;
            // That comparison is itself a call, on the operands coerced to
            // its declared types.
            let left_oid = if left.type_oid == oid::UNKNOWN {
                common
            } else {
                left.type_oid
            };
            if let Some(o) = snapshot.find_operator(op, Some(left_oid), common) {
                nullable |= operator_call_nullable(snapshot, op, &o, Some(left_oid), common);
            }
        }
    }
    // transformAExprIn builds each remaining comparison over a *copy* of
    // the once-transformed left operand. A bare parameter that was still
    // untyped then (and was not coerced in place by the folded `= ANY`
    // above) stays `unknown` in every copy, so each comparison coerces it
    // afresh through `variable_coerce_param_hook` — and two comparisons
    // deducing different types is `inconsistent types deduced for
    // parameter $N` (`$1 IN (int_col, text_col)`), not an operator error.
    let untyped_param = match a.node.as_ref() {
        Some(node::Node::ParamRef(p))
            if left.type_oid == oid::UNKNOWN && !folded.iter().any(|&f| f) =>
        {
            Some(p)
        }
        _ => None,
    };
    for (i, (item, _, _)) in items.iter().enumerate() {
        if folded[i] {
            continue;
        }
        // Each remaining comparison is an operator call: NULL-able when it
        // can yield NULL (on top of its operands being so).
        let Some(p) = untyped_param else {
            nullable |= infer_synthetic_op(op, a, item, expr.location, ctx, params)?.nullable;
            continue;
        };
        let (r, deduced) = params.with_param_untyped(p.number, |scratch| {
            infer_synthetic_op(op, a, item, expr.location, ctx, scratch)
        });
        nullable |= r?.nullable;
        if deduced != oid::UNKNOWN
            && let Err(prev) = params.coerce_untyped(p.number, deduced)
        {
            return Err(inconsistent_param_error(
                p.number,
                prev,
                deduced,
                p.location,
                ctx.snapshot,
            ));
        }
    }
    Ok(Some(ExprType::scalar(oid::BOOL, nullable)))
}

/// Resolve `l op r` exactly as a written binary operator would be — the
/// rewrite target of BETWEEN / IN (PG's `makeSimpleA_Expr` + transform).
pub(crate) fn infer_synthetic_op(
    op: &str,
    l: &protobuf::Node,
    r: &protobuf::Node,
    location: i32,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let e = protobuf::AExpr {
        kind: protobuf::AExprKind::AexprOp as i32,
        name: vec![protobuf::Node {
            node: Some(node::Node::String(protobuf::String { sval: op.into() })),
        }],
        lexpr: Some(Box::new(l.clone())),
        rexpr: Some(Box::new(r.clone())),
        // No IN list: `makeSimpleA_Expr` leaves the list bounds unset.
        rexpr_list_start: -1,
        rexpr_list_end: -1,
        location,
    };
    infer_a_expr(&e, ctx, params)
}

/// Resolve `l op r` as PG's `make_op` does on already-transformed operands:
/// the plain operator lookup, with no row-constructor special case — so the
/// columns of a row comparison that are themselves `ROW(…)` compare as
/// `record = record` and their contents are not typed pairwise.
fn infer_plain_op(
    op: &str,
    l: &protobuf::Node,
    r: &protobuf::Node,
    location: i32,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let e = protobuf::AExpr {
        kind: protobuf::AExprKind::AexprOp as i32,
        name: vec![protobuf::Node {
            node: Some(node::Node::String(protobuf::String { sval: op.into() })),
        }],
        lexpr: Some(Box::new(l.clone())),
        rexpr: Some(Box::new(r.clone())),
        // No IN list: `makeSimpleA_Expr` leaves the list bounds unset.
        rexpr_list_start: -1,
        rexpr_list_end: -1,
        location,
    };
    infer_generic_binary_op(&e, op, ctx, params)
}

/// A `NULL::T` node standing for "some value of type T" (the folded array
/// element of an IN list).
pub(crate) fn typed_null(t: PgTypeOid) -> protobuf::Node {
    let null = protobuf::Node {
        node: Some(node::Node::AConst(protobuf::AConst {
            isnull: true,
            ..Default::default()
        })),
    };
    protobuf::Node {
        node: Some(node::Node::TypeCast(Box::new(protobuf::TypeCast {
            arg: Some(Box::new(null)),
            type_name: Some(protobuf::TypeName {
                type_oid: t.get(),
                location: -1,
                ..Default::default()
            }),
            location: -1,
        }))),
    }
}

/// Approximates PG's `contain_vars_of_level(expr, 0)`: a column reference
/// outside any sub-select. (A correlated reference *inside* a sub-select
/// would also count in PG; treating those items as Var-free only changes
/// which IN items get folded together.)
pub(crate) fn contains_level0_column_ref(n: &protobuf::Node) -> bool {
    let any = |ns: &[protobuf::Node]| ns.iter().any(contains_level0_column_ref);
    let opt =
        |n: &Option<Box<protobuf::Node>>| n.as_deref().is_some_and(contains_level0_column_ref);
    match n.node.as_ref() {
        Some(node::Node::ColumnRef(_)) => true,
        Some(node::Node::AExpr(e)) => opt(&e.lexpr) || opt(&e.rexpr),
        Some(node::Node::BoolExpr(b)) => any(&b.args),
        Some(node::Node::FuncCall(f)) => any(&f.args) || opt(&f.agg_filter),
        Some(node::Node::NamedArgExpr(na)) => opt(&na.arg),
        Some(node::Node::TypeCast(c)) => opt(&c.arg),
        Some(node::Node::CollateClause(c)) => opt(&c.arg),
        Some(node::Node::NullTest(t)) => opt(&t.arg),
        Some(node::Node::BooleanTest(t)) => opt(&t.arg),
        Some(node::Node::CoalesceExpr(c)) => any(&c.args),
        Some(node::Node::MinMaxExpr(m)) => any(&m.args),
        Some(node::Node::RowExpr(r)) => any(&r.args),
        Some(node::Node::AArrayExpr(a)) => any(&a.elements),
        Some(node::Node::AIndirection(i)) => opt(&i.arg),
        Some(node::Node::List(l)) => any(&l.items),
        Some(node::Node::CaseExpr(c)) => opt(&c.arg) || any(&c.args) || opt(&c.defresult),
        Some(node::Node::CaseWhen(w)) => opt(&w.expr) || opt(&w.result),
        _ => false,
    }
}

/// `col = ANY($arr)` / `col = ALL($arr)`: lexpr is scalar, rexpr is array.
/// The generic back-fill would assign the wrong type (element ↔ array
/// confusion), so we handle it first and return early.
fn handle_any_all(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    if !matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprOpAny) | Ok(protobuf::AExprKind::AexprOpAll)
    ) {
        return Ok(None);
    }
    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;

    let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    // A concrete right side must actually be an array — PG checks this
    // before any operator resolution: `1 = ANY(42)` and
    // `1 = ANY('{}'::jsonb)` both fail with this exact wording (42809).
    // UNKNOWN right sides are excluded: they get coerced to `T[]` below.
    if right_oid != oid::UNKNOWN && array_element_type(snapshot, right_oid).is_none() {
        let span = crate::error::SourceSpan::from_location(expr.location);
        return Err(crate::error::RawError::invalid(
            "op ANY/ALL (array) requires array on right side".to_string(),
            span,
            Some("wrap the values in ARRAY[…] or pass an array value".into()),
        )
        .finalize_implicit());
    }

    // left is concrete T, right is unknown → right must be T[]. Operators
    // resolve over the domain's *base* type, so a domain column pins the
    // param to the base's array (`email_col = ANY($1)` → text[], matching
    // PG's Describe), not the domain's. When T has no array type at all —
    // T is itself an array; PG has no array-of-array — PG fails the same
    // lookup with `could not find array type for data type integer[]`.
    let mut op_nullable = false;
    if left_oid != oid::UNKNOWN && right_oid == oid::UNKNOWN {
        match snapshot.array_type_of(snapshot.unwrap_domain(left_oid)) {
            Some(arr_oid) => {
                if let Some(rexpr) = &expr.rexpr {
                    coerce_unknown_to(rexpr, ctx, params, arr_oid)?;
                }
                // The per-element operator over the left type: strict or
                // not, as for a typed array (left to PG to reject, should
                // the lookup differ).
                let op_name = extract_string_fields(&expr.name).join(".");
                let elem = snapshot.unwrap_domain(left_oid);
                if !op_name.is_empty()
                    && !op_name.contains('.')
                    && let Some(op) = snapshot.find_operator(&op_name, Some(left_oid), elem)
                {
                    ctx.note_strict(
                        expr.location,
                        crate::nonnull::StrictNode::Op,
                        ctx.proc_is_strict(op.code)
                            && ctx
                                .coercion_is_strict(left_oid, op.left_type_oid.unwrap_or(left_oid))
                            && ctx.coercion_is_strict(elem, op.right_type_oid),
                    );
                    op_nullable =
                        operator_call_nullable(snapshot, &op_name, &op, Some(left_oid), elem);
                }
            }
            None => {
                let l = crate::ddl::util::format_type_for_message(snapshot, left_oid);
                return Err(crate::pgmsg::no_array_type_for(&l));
            }
        }
    }

    // right is concrete T[], left is unknown → left must be the element type T.
    if right_oid != oid::UNKNOWN
        && left_oid == oid::UNKNOWN
        && let Some(elem_oid) = array_element_type(snapshot, right_oid)
        && let Some(lexpr) = &expr.lexpr
    {
        coerce_unknown_to(lexpr, ctx, params, elem_oid)?;
    }

    // Both sides concrete: PG resolves `<left> <op> <element>` against the
    // operator catalog — `prefs = ANY(ARRAY[1,2,3])` fails at parse time
    // with `operator does not exist: jsonb = integer`. Mirror it (the
    // previous behavior accepted any concrete pair). A non-array right side
    // is a different PG error ("op ANY/ALL (array) requires array on right
    // side") with riskier corner cases (jsonb, record), so that check stays
    // out of scope.
    if left_oid != oid::UNKNOWN
        && right_oid != oid::UNKNOWN
        && let Some(elem_oid) = array_element_type(snapshot, right_oid)
    {
        let op_name = extract_string_fields(&expr.name).join(".");
        if !op_name.is_empty() && !op_name.contains('.') {
            match snapshot.find_operator(&op_name, Some(left_oid), elem_oid) {
                // The per-element operator call can itself yield NULL.
                Some(op) => {
                    ctx.note_strict(
                        expr.location,
                        crate::nonnull::StrictNode::Op,
                        ctx.proc_is_strict(op.code)
                            && ctx
                                .coercion_is_strict(left_oid, op.left_type_oid.unwrap_or(left_oid))
                            && ctx.coercion_is_strict(elem_oid, op.right_type_oid),
                    );
                    // So can the coercion of the left operand or of an
                    // element to the operator's declared types.
                    op_nullable =
                        operator_call_nullable(snapshot, &op_name, &op, Some(left_oid), elem_oid);
                }
                None => {
                    let l = crate::ddl::util::format_type_for_message(snapshot, left_oid);
                    let r = crate::ddl::util::format_type_for_message(snapshot, elem_oid);
                    // PG positions it at the operator, as for a plain one.
                    let span = (expr.location >= 0).then(|| {
                        crate::error::SourceSpan::at_length(expr.location as usize, op_name.len())
                    });
                    let err = crate::pgmsg::operator_does_not_exist(&l, &op_name, &r, span);
                    return Err(with_cast_note(err, snapshot, &op_name, left_oid, elem_oid)
                        .finalize_implicit());
                }
            }
        }
    }

    // ExecEvalScalarArrayOp: with no element deciding the result, a NULL
    // element makes the whole `op ANY/ALL` NULL (`3 = ANY('{1,NULL}')`), so
    // besides a NULL operand the array's *elements* matter — and those are
    // only provably NOT NULL for an ARRAY[...] constructor over NOT NULL
    // elements (or a literal without NULL elements).
    let any_nullable = op_nullable
        || left.as_ref().is_some_and(|l| l.nullable)
        || right.as_ref().is_some_and(|r| r.nullable)
        // Elements known non-NULL from the value's own type (`ARRAY(SELECT
        // nn_col …)`), or from its constructor / literal.
        || (right.as_ref().and_then(|r| r.elem_nullable) != Some(false)
            && expr
                .rexpr
                .as_deref()
                .is_none_or(|r| array_elements_may_be_null(r, ctx, params)));
    Ok(Some(ExprType::scalar(oid::BOOL, any_nullable)))
}

/// Whether a call of the resolved operator `op` on non-NULL operands of
/// types `left` / `right` can yield NULL: its function can
/// ([`functions::operator_result_nullable`]), or coercing an operand to the
/// operator's declared type runs a cast function that maps it to NULL.
pub(crate) fn operator_call_nullable(
    snapshot: &PgCatalog,
    op_name: &str,
    op: &crate::lookup::ResolvedOperator,
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> bool {
    let operands = if left.is_some() { 2 } else { 1 };
    functions::operator_result_nullable(snapshot, op_name, op.code, &[false, false][..operands])
        || left
            .zip(op.left_type_oid)
            .is_some_and(|(a, d)| super::coercion_can_return_null(a, d, snapshot))
        || super::coercion_can_return_null(right, op.right_type_oid, snapshot)
}

/// Whether the array value `node` evaluates to may contain NULL elements.
/// `false` only when provable: an `ARRAY[...]` constructor (possibly cast)
/// whose scalar elements are all NOT NULL, recursing into nested
/// sub-array constructors, or an array literal with no NULL element. Array
/// columns, parameters and function results can always hold NULLs — PG has
/// no NOT NULL constraint on elements. The elements are re-inferred against
/// a throwaway collector, so the peek has no side effects.
pub(crate) fn array_elements_may_be_null(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> bool {
    match node.node.as_ref() {
        Some(node::Node::AArrayExpr(arr)) => arr.elements.iter().any(|e| {
            if matches!(e.node.as_ref(), Some(node::Node::AArrayExpr(_))) {
                return array_elements_may_be_null(e, ctx, params);
            }
            let mut scratch = params.clone();
            // An array-valued element makes a multi-dimensional array whose
            // inner elements are that value's — unknown here.
            !infer_expr(e, ctx, &mut scratch, TypeGoal::NONE).is_ok_and(|t| {
                !t.nullable && array_element_type(ctx.snapshot, t.type_oid).is_none()
            })
        }),
        Some(node::Node::TypeCast(c)) => c
            .arg
            .as_deref()
            .is_none_or(|a| array_elements_may_be_null(a, ctx, params)),
        // An untyped literal (`'{1,2}'`, cast or coerced to the array type).
        Some(node::Node::AConst(ac)) => match &ac.val {
            Some(a_const::Val::Sval(sv)) if !ac.isnull => {
                crate::literal_input::array_literal_may_contain_null(&sv.sval)
            }
            _ => true,
        },
        _ => true,
    }
}

/// `ROW(a, b) op ROW(c, d)` (also the implicit `(a, b) op (c, d)`), for any
/// operator — PG's `make_row_comparison_op`, reached whenever both raw
/// operands are row constructors. Each column pair is resolved with the
/// ordinary operator machinery (`make_op`), so `(n, s) = (1, 2)` is
/// `operator does not exist: text = integer` and untyped elements are typed
/// by their peer; every pairwise operator must yield boolean. (The btree
/// opfamily check PG applies to multi-column rows is not modeled.)
fn handle_row_row(
    expr: &protobuf::AExpr,
    op_name: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Ok(None);
    };
    let (Some(node::Node::RowExpr(lrow)), Some(node::Node::RowExpr(rrow))) =
        (lexpr.node.as_ref(), rexpr.node.as_ref())
    else {
        return Ok(None);
    };
    let (t, ops) = row_pairwise_op(
        op_name,
        &lrow.args,
        &rrow.args,
        expr.location,
        ctx,
        params,
        |t| format!("row comparison operator must yield type boolean, not type {t}"),
    )?;
    check_row_comparison_interpretation(ctx.snapshot, op_name, &ops, expr.location)?;
    Ok(Some(t))
}

/// The pairwise core of `make_row_comparison_op` / `make_row_distinct_op`:
/// equal arity (`unequal number of entries in row expressions`), at least
/// one column (`cannot compare rows of zero length`), and each pair's
/// operator resolved and required to return boolean (`not_bool` renders
/// the construct's wording). The result is bool, nullable if any column is.
pub(crate) fn row_pairwise_op(
    op_name: &str,
    largs: &[protobuf::Node],
    rargs: &[protobuf::Node],
    location: i32,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    not_bool: impl Fn(&str) -> String,
) -> Result<(ExprType, Vec<Option<crate::lookup::ResolvedOperator>>), AnalyzeError> {
    let largs = &*expand_row_args(largs, ctx, params);
    let rargs = &*expand_row_args(rargs, ctx, params);
    if largs.len() != rargs.len() {
        return Err(AnalyzeError::SyntaxError(
            "unequal number of entries in row expressions".to_owned(),
        ));
    }
    if largs.is_empty() {
        return Err(AnalyzeError::Invalid(
            "cannot compare rows of zero length".to_owned(),
        ));
    }
    let mut nullable = false;
    let mut ops = Vec::with_capacity(largs.len());
    for (l, r) in largs.iter().zip(rargs) {
        let t = infer_plain_op(op_name, l, r, location, ctx, params)?;
        if t.type_oid != oid::BOOL {
            let name = crate::ddl::util::format_type_for_message(ctx.snapshot, t.type_oid);
            return Err(AnalyzeError::DatatypeMismatch(not_bool(&name)));
        }
        nullable |= t.nullable;
        ops.push(pair_operator(op_name, l, r, ctx, params));
    }
    Ok((ExprType::scalar(oid::BOOL, nullable), ops))
}

/// The operator `l op r` resolved to, once both operands are typed (their
/// types re-read on a scratch collector): `None` when it can't be told.
fn pair_operator(
    op_name: &str,
    l: &protobuf::Node,
    r: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Option<crate::lookup::ResolvedOperator> {
    let mut scratch = params.clone();
    let lt = infer_expr(l, ctx, &mut scratch, TypeGoal::NONE).ok()?;
    let rt = infer_expr(r, ctx, &mut scratch, TypeGoal::NONE).ok()?;
    ctx.snapshot
        .find_operator(op_name, Some(lt.type_oid), rt.type_oid)
}

/// PG's `make_row_comparison_op` past the pairwise operators: a row of
/// more than one column is compared with the semantics (`=`, `<>`, `<`, …)
/// every operator shares as a member of some btree operator family — or,
/// for `<>`, as the negator of a btree equality
/// (`get_op_index_interpretation`). With no common one, PG can't tell what
/// the comparison means.
pub(crate) fn check_row_comparison_interpretation(
    snapshot: &PgCatalog,
    op_name: &str,
    ops: &[Option<crate::lookup::ResolvedOperator>],
    location: i32,
) -> Result<(), AnalyzeError> {
    if ops.len() < 2 || ops.iter().any(Option::is_none) {
        return Ok(());
    }
    // Comparison types as bits: btree strategies 1-5 (`<` … `>`), and 6
    // for `<>` (PG's CompareType numbering).
    const NE: u8 = 6;
    let interpretations = |opr: crate::oid::PgOperatorOid| -> u8 {
        let btree = |o: crate::oid::PgOperatorOid| {
            snapshot
                .pg_amop
                .iter()
                .filter(move |a| a.amopopr == o && a.amopmethod == "btree")
        };
        let mut bits = btree(opr)
            .filter(|a| (1..=5).contains(&a.amopstrategy))
            .fold(0u8, |acc, a| acc | (1 << a.amopstrategy));
        if bits == 0
            && let Some(neg) = snapshot.pg_operator.get(&opr).and_then(|o| o.oprnegate)
            && btree(neg).any(|a| a.amopstrategy == 3)
        {
            bits = 1 << NE;
        }
        bits
    };
    let common = ops
        .iter()
        .flatten()
        .fold(u8::MAX, |acc, o| acc & interpretations(o.oid));
    if common != 0 {
        return Ok(());
    }
    // PG names the operator without its schema (`strVal(llast(opname))`).
    let name = op_name.rsplit('.').next().unwrap_or(op_name);
    Err(crate::error::RawError::new(
        AnalyzeError::FeatureNotSupported(format!(
            "could not determine interpretation of row comparison operator {name}"
        )),
        crate::error::SourceSpan::from_location(location),
        Some("Row comparison operators must be associated with btree operator families.".into()),
    )
    .finalize_implicit())
}

/// `ROW(...)` compared against a sub-SELECT: PG counts columns at the subquery
/// boundary (the inner ROW stays a single record column), so the LHS arity
/// must equal the subquery's column count. Mirror PG's `subquery has too
/// few/many columns` for the mismatch case.
fn handle_row_subselect(
    expr: &protobuf::AExpr,
    op_name: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    let Ctx {
        scope, snapshot, ..
    } = ctx;
    if !matches!(op_name, "=" | "<>" | "<" | ">" | "<=" | ">=") {
        return Ok(None);
    }
    let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Ok(None);
    };
    let (Some(node::Node::RowExpr(lrow)), Some(node::Node::SubLink(sub))) =
        (lexpr.node.as_ref(), rexpr.node.as_ref())
    else {
        return Ok(None);
    };
    if !matches!(
        protobuf::SubLinkType::try_from(sub.sub_link_type),
        Ok(protobuf::SubLinkType::ExprSublink)
    ) {
        return Ok(None);
    }
    let Some(subselect) = sub.subselect.as_ref() else {
        return Ok(None);
    };
    let Some(node::Node::SelectStmt(sel)) = subselect.node.as_ref() else {
        return Ok(None);
    };

    // transformExpressionList expands the ROW's `t.*` / `(expr).*` items.
    let largs = expand_row_args(&lrow.args, ctx, params);
    for la in largs.iter() {
        let _ = infer_expr(la, ctx, params, TypeGoal::NONE);
    }
    let (cols, _) =
        crate::resolve::analyze_correlated_select(sel, snapshot, params, scope, ctx.null_ctx)?;
    if cols.len() != largs.len() {
        let pg_msg = if cols.len() < largs.len() {
            "subquery has too few columns"
        } else {
            "subquery has too many columns"
        };
        return Err(AnalyzeError::Invalid(format!(
            "{pg_msg} (subquery has {}, lhs has {})",
            cols.len(),
            largs.len(),
        )));
    }
    // make_row_comparison_op over the row and the subquery's columns.
    let ops: Vec<Option<crate::lookup::ResolvedOperator>> = largs
        .iter()
        .zip(&cols)
        .map(|(la, col)| {
            let mut scratch = params.clone();
            let lt = infer_expr(la, ctx, &mut scratch, TypeGoal::NONE).ok()?;
            snapshot.find_operator(op_name, Some(lt.type_oid), col.type_oid)
        })
        .collect();
    check_row_comparison_interpretation(snapshot, op_name, &ops, expr.location)?;
    Ok(Some(ExprType::scalar(oid::BOOL, true)))
}

/// Generic binary operator resolution (PG chapter 10.2): infer both sides
/// bottom-up, back-fill UNKNOWN sides from the concrete peer, then look up the
/// operator and emit PG-exact errors when it doesn't exist.
fn infer_generic_binary_op(
    expr: &protobuf::AExpr,
    op_name: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    // Pass 1: infer both sides bottom-up.
    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;

    let left_oid = left.as_ref().map(|l| l.type_oid);
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    // PG step 2: if one side is unknown and the other is concrete, assume
    // unknown = the other side's type. Re-infer to propagate into params.
    //
    // Smash a domain to its base first: operators resolve against the base
    // type (`find_operator` unwraps domains), so a parameter compared against
    // a domain column (`email_col <= $1`) is inferred by PG as the base
    // (`text`), not the domain. Pinning the param to the raw domain would
    // diverge from PG's Describe.
    // No pre-resolution "assume the unknown side is the other side's type"
    // walk happens here: that guess pinned `$1` in `prefs -> $1` to jsonb
    // and then failed resolution, while PG resolves the operator *with* the
    // unknown (`jsonb -> text` wins via the string-category rule) and only
    // then coerces. `find_operator`'s unknown handling covers the
    // homogeneous probe (`T OP T`) and the category/text fallbacks; the
    // post-resolution back-fill below pins params and validates literal
    // content against the operator's *declared* argument types — the
    // coercion PG actually performs.
    let left_oid_resolved = left_oid;
    let right_oid_resolved = right_oid;

    // Operator lookup with the bottom-up types — UNKNOWN sides are resolved
    // by `find_operator`'s own rules (homogeneous probe, category and text
    // fallbacks), exactly like PG; the unknown side is *not* pre-pinned to
    // the concrete peer's type.
    match snapshot.find_operator_detailed(op_name, left_oid_resolved, right_oid_resolved) {
        crate::lookup::OperatorMatch::Found(op) => {
            ctx.note_proc(op.code);
            ctx.note_strict(
                expr.location,
                crate::nonnull::StrictNode::Op,
                ctx.proc_is_strict(op.code)
                    && left_oid_resolved
                        .zip(op.left_type_oid)
                        .is_none_or(|(a, d)| ctx.coercion_is_strict(a, d))
                    && ctx.coercion_is_strict(right_oid_resolved, op.right_type_oid),
            );
            if ctx.strict_log.is_some() {
                note_std_compare(expr.location, op_name, &op, &left, &right, ctx);
            }
            // The operands as the operator's function receives them, coerced
            // to its declared types: a cast function may map one (or an
            // array's element) to NULL.
            let mut left = left;
            let mut right = right;
            if let (Some(l), Some(d)) = (left.as_mut(), op.left_type_oid) {
                l.note_coerced_to(d, snapshot);
            }
            if let Some(r) = right.as_mut() {
                r.note_coerced_to(op.right_type_oid, snapshot);
            }
            let args_nullable: Vec<bool> = left
                .iter()
                .chain(right.iter())
                .map(|t| t.nullable)
                .collect();
            if let (Some(actual), Some(declared)) = (left_oid_resolved, op.left_type_oid) {
                ctx.note_coercion(actual, declared);
            }
            ctx.note_coercion(right_oid_resolved, op.right_type_oid);
            // Pass 2: back-fill still-UNKNOWN sides with the operator's
            // *declared* argument types — the coercion PG performs (this is
            // what pins `$1` in `prefs -> $1` to text, and validates literal
            // content).
            if left_oid_resolved == Some(oid::UNKNOWN)
                && let (Some(expected), Some(lexpr)) = (op.left_type_oid, &expr.lexpr)
            {
                coerce_unknown_to(lexpr, ctx, params, expected)?;
            }
            if right_oid_resolved == oid::UNKNOWN
                && let Some(rexpr) = &expr.rexpr
            {
                coerce_unknown_to(rexpr, ctx, params, op.right_type_oid)?;
            }
            let state = derive_collation(
                left.iter().chain(right.iter()),
                op.result_type_oid,
                snapshot,
            )?;
            // A regex match of two constants is folded by the planner
            // (eval_const_expressions), compiling the pattern on every
            // execution.
            if plan_time_checks()
                && let Some(icase) = regex_operator_icase(snapshot, op.code)
                && let (Some(l), Some(r)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref())
                && const_string(l, snapshot).is_some()
                && let Some(pattern) = const_regex_pattern(r, snapshot)
            {
                check_regex_pattern(pattern, icase)?;
            }
            // The operator is a call of its function: `box # box`
            // (box_intersect) is NULL for disjoint boxes, `jsonb @? jsonpath`
            // (jsonb_path_exists_opr) when the path evaluation fails.
            let nullable =
                functions::operator_result_nullable(snapshot, op_name, op.code, &args_nullable);
            // `||` on arrays appends, prepends or concatenates: the result's
            // elements are both sides' elements / values.
            let side = |t: &Option<ExprType>, array: bool| {
                t.as_ref().and_then(|t| {
                    if array {
                        t.elem_nullable
                    } else {
                        Some(t.nullable)
                    }
                })
            };
            let elem_nullable = match op
                .code
                .and_then(|c| snapshot.pg_proc.get(&c))
                .map(|p| p.proname.as_str())
            {
                Some("array_cat") => merge_elem_nullable([side(&left, true), side(&right, true)]),
                Some("array_append") => {
                    merge_elem_nullable([side(&left, true), side(&right, false)])
                }
                Some("array_prepend") => {
                    merge_elem_nullable([side(&left, false), side(&right, true)])
                }
                _ => None,
            };
            return Ok(ExprType::scalar(op.result_type_oid, nullable)
                .with_collation(state)
                .with_elem_nullable(elem_nullable));
        }
        crate::lookup::OperatorMatch::Ambiguous => {
            // PG (SQLSTATE 42725): `operator is not unique: <left> <op>
            // <right>` — several overloads survived the unknown-side
            // tiebreaks (`bday + $1`, `$1 + $2`, `NULL + NULL`).
            let right_pg = crate::ddl::util::format_type_for_message(snapshot, right_oid_resolved);
            let span = (expr.location >= 0).then(|| {
                crate::error::SourceSpan::at_length(expr.location as usize, op_name.len())
            });
            // A prefix operator has no left operand to render.
            let err = match left_oid_resolved {
                Some(l) => crate::pgmsg::operator_is_not_unique(
                    &crate::ddl::util::format_type_for_message(snapshot, l),
                    op_name,
                    &right_pg,
                    span,
                ),
                None => crate::pgmsg::prefix_operator_is_not_unique(op_name, &right_pg, span),
            };
            return Err(err.finalize_implicit());
        }
        crate::lookup::OperatorMatch::Error(e) => return Err(e),
        crate::lookup::OperatorMatch::NotFound => {}
    }

    // PG (SQLSTATE 42883): `operator does not exist: <left> <op> <right>`.
    // Use PG's user-facing type names (`integer`, `bigint`, …) so the
    // sanity-check prefix match passes.
    let right_pg = crate::ddl::util::format_type_for_message(snapshot, right_oid_resolved);
    // `AExpr.location` points at the operator token; cover its length
    // so the caret spans the operator symbol/name.
    let span = (expr.location >= 0)
        .then(|| crate::error::SourceSpan::at_length(expr.location as usize, op_name.len()));
    let err = match left_oid_resolved {
        Some(l) => with_cast_note(
            crate::pgmsg::operator_does_not_exist(
                &crate::ddl::util::format_type_for_message(snapshot, l),
                op_name,
                &right_pg,
                span,
            ),
            snapshot,
            op_name,
            l,
            right_oid_resolved,
        ),
        None => crate::pgmsg::prefix_operator_does_not_exist(op_name, &right_pg, span),
    };
    Err(err.finalize_implicit())
}

/// Add to an `operator does not exist: L op R` error the cast that makes
/// it resolve, when there is one: an `op` taking two `L`s, the right
/// operand castable to `L` (or the mirror image) — `name = 1::bigint` gets
/// "`text = text` exists: cast the right operand to text (`expr::text`)".
/// A cast either side is offered only when its operator exists, so the
/// note never sends the user to another `operator does not exist`.
pub(crate) fn with_cast_note(
    err: crate::error::RawError,
    snapshot: &PgCatalog,
    op_name: &str,
    left: crate::oid::PgTypeOid,
    right: crate::oid::PgTypeOid,
) -> crate::error::RawError {
    use crate::coerce::{CoercionContext, coercion_pathway};
    if left == oid::UNKNOWN || right == oid::UNKNOWN {
        return err;
    }
    let castable = |target, source| {
        coercion_pathway(target, source, CoercionContext::Explicit, snapshot).is_some()
            && snapshot
                .find_operator(op_name, Some(target), target)
                .is_some()
    };
    let (side, target) = if castable(left, right) {
        ("right", left)
    } else if castable(right, left) {
        ("left", right)
    } else {
        return err;
    };
    let t = crate::ddl::util::format_type_for_message(snapshot, target);
    err.with_note(format!(
        "`{t} {op_name} {t}` exists: cast the {side} operand to {t} (`expr::{t}`)"
    ))
}

// ──────────────────────────────────────────────────────────────────────────────
// Regex patterns the planner compiles
// ──────────────────────────────────────────────────────────────────────────────

/// `Some(icase)` when `code` is one of the POSIX regex match functions
/// behind `~`, `~*`, `!~`, `!~*` (and SIMILAR TO): `textregexeq`,
/// `nameicregexne`, …
fn regex_operator_icase(snapshot: &PgCatalog, code: Option<crate::oid::PgProcOid>) -> Option<bool> {
    let f = code.and_then(|c| snapshot.pg_proc.get(&c))?;
    if Some(f.pronamespace) != snapshot.pg_catalog_oid() {
        return None;
    }
    let base = ["text", "bpchar", "name"]
        .iter()
        .find_map(|p| f.proname.strip_prefix(p))?;
    match base {
        "regexeq" | "regexne" => Some(false),
        "icregexeq" | "icregexne" => Some(true),
        _ => None,
    }
}

/// The value of a string constant as the planner folds it: an untyped or
/// string-typed literal, possibly cast between string types or
/// concatenated with `||`. `None` for anything else.
fn const_string(node: &protobuf::Node, snapshot: &PgCatalog) -> Option<String> {
    match node.node.as_ref()? {
        node::Node::AConst(ac) if !ac.isnull => match &ac.val {
            Some(a_const::Val::Sval(sv)) => Some(sv.sval.clone()),
            _ => None,
        },
        node::Node::TypeCast(c) => {
            let tn = c.type_name.as_ref()?;
            if !tn.typmods.is_empty() || !tn.array_bounds.is_empty() {
                return None;
            }
            let names = extract_string_fields(&tn.names);
            let name = names.last()?;
            let t = snapshot
                .resolve_type_by_name((names.len() == 2).then(|| names[0].as_str()), name)?;
            (t.typcategory == TypCategory::String).then_some(())?;
            const_string(c.arg.as_deref()?, snapshot)
        }
        node::Node::AExpr(e)
            if protobuf::AExprKind::try_from(e.kind) == Ok(protobuf::AExprKind::AexprOp)
                && extract_string_fields(&e.name) == ["||"] =>
        {
            let l = const_string(e.lexpr.as_deref()?, snapshot)?;
            let r = const_string(e.rexpr.as_deref()?, snapshot)?;
            Some(l + &r)
        }
        _ => None,
    }
}

/// The regex a constant pattern operand folds to: a constant string, or
/// SIMILAR TO's `similar_to_escape(pattern [, escape])` over constants
/// (its own errors are plan-time errors too, hence the inner `Result`).
fn const_regex_pattern(
    node: &protobuf::Node,
    snapshot: &PgCatalog,
) -> Option<Result<String, AnalyzeError>> {
    if let Some(node::Node::FuncCall(fc)) = node.node.as_ref()
        && is_builtin_similar_to_escape(fc, snapshot)
    {
        let pattern = const_string(fc.args.first()?, snapshot)?;
        let escape = match fc.args.get(1) {
            Some(e) => Some(const_string(e, snapshot)?),
            None => None,
        };
        return Some(similar_escape(&pattern, escape.as_deref()));
    }
    const_string(node, snapshot).map(Ok)
}

/// The grammar's `pg_catalog.similar_to_escape`, or a call that can reach
/// only it — not a user function of that name.
fn is_builtin_similar_to_escape(fc: &protobuf::FuncCall, snapshot: &PgCatalog) -> bool {
    let parts = extract_string_fields(&fc.funcname);
    let schema = match parts.as_slice() {
        [n] if n == "similar_to_escape" => None,
        [s, n] if s == "pg_catalog" && n == "similar_to_escape" => Some("pg_catalog"),
        _ => return false,
    };
    snapshot
        .find_functions(schema, "similar_to_escape")
        .iter()
        .all(|p| snapshot.namespace_name(p.pronamespace) == Some("pg_catalog"))
}

fn check_regex_pattern(
    pattern: Result<String, AnalyzeError>,
    icase: bool,
) -> Result<(), AnalyzeError> {
    use crate::regex_input::{REG_ADVANCED, REG_ICASE};
    let cflags = if icase {
        REG_ADVANCED | REG_ICASE
    } else {
        REG_ADVANCED
    };
    crate::regex_input::check(&pattern?, cflags)
        .map_err(|msg| crate::error::RawError::invalid(msg, None, None).finalize_implicit())
}

/// PG's `similar_escape_internal` (regexp.c): the POSIX regex a SIMILAR TO
/// pattern is matched as. `escape` is `None` for the default backslash,
/// `Some("")` for no escape character.
fn similar_escape(pattern: &str, escape: Option<&str>) -> Result<String, AnalyzeError> {
    let e: Option<char> = match escape {
        None => Some('\\'),
        Some("") => None,
        Some(esc) => {
            let mut chars = esc.chars();
            let c = chars.next();
            if chars.next().is_some() {
                return Err(crate::error::RawError::invalid(
                    "invalid escape string".to_owned(),
                    None,
                    Some("Escape string must be empty or one character.".to_owned()),
                )
                .finalize_implicit());
            }
            c
        }
    };
    let mut r = String::from("^(?:");
    let mut afterescape = false;
    let mut nquotes = 0;
    let mut bracket_depth = 0;
    let mut charclass_pos = 0;
    for pchar in pattern.chars() {
        if afterescape {
            if pchar == '"' && bracket_depth < 1 {
                match nquotes {
                    0 => r.push_str("){1,1}?("),
                    1 => r.push_str("){1,1}(?:"),
                    _ => {
                        return Err(crate::error::RawError::invalid(
                            "SQL regular expression may not contain more than two \
                             escape-double-quote separators"
                                .to_owned(),
                            None,
                            None,
                        )
                        .finalize_implicit());
                    }
                }
                nquotes += 1;
            } else {
                r.push('\\');
                r.push(pchar);
                charclass_pos = 3;
            }
            afterescape = false;
        } else if e == Some(pchar) {
            afterescape = true;
        } else if bracket_depth > 0 {
            if pchar == '\\' {
                r.push('\\');
            }
            r.push(pchar);
            if pchar == ']' && charclass_pos > 2 {
                bracket_depth -= 1;
            } else if pchar == '[' {
                bracket_depth += 1;
                charclass_pos = 3;
            } else if pchar == '^' {
                charclass_pos += 1;
            } else {
                charclass_pos = 3;
            }
        } else if pchar == '[' {
            r.push(pchar);
            bracket_depth = 1;
            charclass_pos = 1;
        } else if pchar == '%' {
            r.push_str(".*");
        } else if pchar == '_' {
            r.push('.');
        } else if pchar == '(' {
            r.push_str("(?:");
        } else if matches!(pchar, '\\' | '.' | '^' | '$') {
            r.push('\\');
            r.push(pchar);
        } else {
            r.push(pchar);
        }
    }
    r.push_str(")$");
    Ok(r)
}

/// The planner's selectivity estimate for a WHERE / JOIN ON qual
/// `expr ~ 'pattern'` (`patternsel` → `regex_fixed_prefix`) compiles the
/// constant pattern whenever the other side is a restriction variable — an
/// expression over one relation's columns — so an invalid pattern fails
/// every execution, however many rows the relation has. Walks the qual's
/// AND / OR / NOT tree, the part `clauselist_selectivity` estimates.
pub(crate) fn check_regex_restrictions(
    node: &protobuf::Node,
    ctx: Ctx<'_>,
) -> Result<(), AnalyzeError> {
    if !plan_time_checks() {
        return Ok(());
    }
    match node.node.as_ref() {
        Some(node::Node::BoolExpr(b)) => {
            for a in &b.args {
                check_regex_restrictions(a, ctx)?;
            }
            Ok(())
        }
        Some(node::Node::AExpr(e)) => {
            let kind = protobuf::AExprKind::try_from(e.kind);
            let op = extract_string_fields(&e.name);
            let icase = match (kind, op.as_slice()) {
                (Ok(protobuf::AExprKind::AexprOp), [o]) if o == "~" || o == "!~" => false,
                (Ok(protobuf::AExprKind::AexprOp), [o]) if o == "~*" || o == "!~*" => true,
                (Ok(protobuf::AExprKind::AexprSimilar), _) => false,
                _ => return Ok(()),
            };
            let (Some(l), Some(r)) = (e.lexpr.as_deref(), e.rexpr.as_deref()) else {
                return Ok(());
            };
            let Some(pattern) = const_regex_pattern(r, ctx.snapshot) else {
                return Ok(());
            };
            if !is_restriction_variable(l, ctx) {
                return Ok(());
            }
            // Only the string types' regex operators (not, e.g., ltree's `~`).
            let mut scratch = ParamCollector::default();
            let Ok(t) = infer_expr(l, ctx, &mut scratch, TypeGoal::NONE) else {
                return Ok(());
            };
            let base = ctx.snapshot.unwrap_domain(t.type_oid);
            if !(base == oid::NAME
                || coerce::type_category(base, ctx.snapshot) == Some(TypCategory::String))
            {
                return Ok(());
            }
            check_regex_pattern(pattern, icase)
        }
        _ => Ok(()),
    }
}

/// `get_restriction_variable`'s test: the expression references columns
/// of exactly one relation of this query level (and nothing we can't
/// classify).
fn is_restriction_variable(node: &protobuf::Node, ctx: Ctx<'_>) -> bool {
    fn walk(n: &protobuf::Node, ctx: Ctx<'_>, rel: &mut Option<String>) -> bool {
        match n.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) => {
                let parts = extract_string_fields(&cr.fields);
                let (table, column) = match parts.as_slice() {
                    [c] if parts.len() == cr.fields.len() => (None, c.as_str()),
                    [t, c] if parts.len() == cr.fields.len() => (Some(t.as_str()), c.as_str()),
                    _ => return false,
                };
                let Ok(col) = ctx.scope.resolve_column(table, column, None) else {
                    return false;
                };
                if !ctx.scope.sources.iter().any(|s| s.alias == col.table_alias) {
                    return false;
                }
                match rel {
                    Some(r) if *r != col.table_alias => false,
                    _ => {
                        *rel = Some(col.table_alias.clone());
                        true
                    }
                }
            }
            Some(node::Node::AConst(_)) => true,
            Some(node::Node::TypeCast(c)) => c.arg.as_deref().is_some_and(|a| walk(a, ctx, rel)),
            Some(node::Node::CollateClause(c)) => {
                c.arg.as_deref().is_some_and(|a| walk(a, ctx, rel))
            }
            Some(node::Node::FuncCall(f)) => {
                f.agg_order.is_empty()
                    && f.agg_filter.is_none()
                    && f.over.is_none()
                    && f.args.iter().all(|a| walk(a, ctx, rel))
            }
            Some(node::Node::AExpr(e)) => {
                protobuf::AExprKind::try_from(e.kind) == Ok(protobuf::AExprKind::AexprOp)
                    && e.lexpr.as_deref().is_none_or(|a| walk(a, ctx, rel))
                    && e.rexpr.as_deref().is_none_or(|a| walk(a, ctx, rel))
            }
            _ => false,
        }
    }
    let mut rel = None;
    walk(node, ctx, &mut rel) && rel.is_some()
}

/// Record whether the comparison at `location` is one the NULL
/// substitution of [`crate::nonnull::subst`] may compute over constants:
/// a built-in `=` `<>` `<` `>` `<=` `>=` between integer, numeric or float
/// operands (or `=` / `<>` between booleans) — and, separately, a
/// built-in `text = text` / `text <> text` under a deterministic
/// collation, whose result is then byte equality.
fn note_std_compare(
    location: i32,
    op_name: &str,
    op: &crate::lookup::ResolvedOperator,
    left: &Option<ExprType>,
    right: &Option<ExprType>,
    ctx: Ctx<'_>,
) {
    let snapshot = ctx.snapshot;
    let builtin = snapshot.pg_operator.get(&op.oid).is_some_and(|o| {
        snapshot.namespace_name(o.oprnamespace) == Some("pg_catalog")
            && o.oprcode
                .and_then(|f| snapshot.pg_proc.get(&f))
                .is_some_and(|p| snapshot.namespace_name(p.pronamespace) == Some("pg_catalog"))
    });
    let equality = matches!(op_name, "=" | "<>");
    let ordering = equality || matches!(op_name, "<" | ">" | "<=" | ">=");
    let numeric = |t: PgTypeOid| {
        [
            oid::INT2,
            oid::INT4,
            oid::INT8,
            oid::NUMERIC,
            oid::FLOAT4,
            oid::FLOAT8,
        ]
        .contains(&t)
    };
    let (Some(l), r) = (op.left_type_oid, op.right_type_oid) else {
        ctx.note_strict(location, crate::nonnull::StrictNode::StdCompare, false);
        ctx.note_strict(location, crate::nonnull::StrictNode::TextEquality, false);
        return;
    };
    let std = builtin
        && op.result_type_oid == oid::BOOL
        && ((ordering && numeric(l) && numeric(r))
            || (equality && l == oid::BOOL && r == oid::BOOL));
    ctx.note_strict(location, crate::nonnull::StrictNode::StdCompare, std);
    // PG's database default (100), "C" (950) and "POSIX" (951) compare
    // bytes; a nondeterministic collation may equal different strings.
    let deterministic = derive_collation(left.iter().chain(right.iter()), oid::TEXT, snapshot)
        .is_ok_and(|(c, _)| c.is_none_or(|c| matches!(c.get(), 100 | 950 | 951)));
    let text = builtin
        && equality
        && op.result_type_oid == oid::BOOL
        && l == oid::TEXT
        && r == oid::TEXT
        && deterministic;
    ctx.note_strict(location, crate::nonnull::StrictNode::TextEquality, text);
}
