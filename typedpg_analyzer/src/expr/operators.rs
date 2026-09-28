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

/// `NULLIF(v1, v2)` — represented as an AExpr with op_name "=" and a special
/// kind. PG defines it as `CASE WHEN v1 = v2 THEN NULL ELSE v1 END`
/// (src/backend/parser/parse_expr.c:transformAExprNullIf), so the result type
/// is v1's type and the expression is always nullable. The generic path would
/// return `bool` (from the `=` operator's result type), silently corrupting
/// the result column, so handle it up front.
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

    // Both arms are inferred with NONE goal: a concrete-but-incompatible
    // RHS would otherwise trip the generic `cannot coerce X to Y` error
    // from the implicit goal before we get to the NULLIF-specific check
    // below, swallowing the chance to emit PG's exact wording.
    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;
    let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    // Back-fill UNKNOWN side with the concrete side via implicit goal so
    // params and bare unknowns get pinned. Errors here are non-fatal (a
    // genuinely incompatible pair falls through to the operator check) —
    // except a literal-content rejection, which is exactly the error PG
    // raises from this coercion (`NULLIF(1, 'x')` → `invalid input syntax
    // for type integer: "x"`).
    let left_oid_final = if left_oid == oid::UNKNOWN && right_oid != oid::UNKNOWN {
        if let Some(lexpr) = &expr.lexpr {
            coerce_unknown_to(lexpr, ctx, params, right_oid)?;
        }
        // The coerced side now carries the peer's type — PG resolves the
        // `=` over (peer, peer).
        right_oid
    } else {
        left_oid
    };
    let right_oid_final = if right_oid == oid::UNKNOWN && left_oid_final != oid::UNKNOWN {
        if let Some(rexpr) = &expr.rexpr {
            coerce_unknown_to(rexpr, ctx, params, left_oid_final)?;
        }
        left_oid_final
    } else {
        right_oid
    };

    // Validate: `=` must be defined between the two types.
    if left_oid_final != oid::UNKNOWN
        && right_oid_final != oid::UNKNOWN
        && snapshot
            .find_operator("=", Some(left_oid_final), right_oid_final)
            .is_none()
    {
        // PG's wording is `operator does not exist: A = B`. We append the
        // NULLIF context as a suffix so the macro caller still sees that
        // it was a NULLIF-shape mismatch.
        let l = crate::ddl::util::format_type_for_message(snapshot, left_oid_final);
        let r = crate::ddl::util::format_type_for_message(snapshot, right_oid_final);
        return Err(crate::pgmsg::nullif_types_mismatch(&l, &r));
    }

    // Result type is the first arg's type (never bool). If the first arg
    // is UNKNOWN and the second is concrete, use the second as a fallback
    // so the result isn't a bare UNKNOWN dangling into the output.
    let result_oid = if left_oid_final != oid::UNKNOWN {
        left_oid_final
    } else {
        right_oid_final
    };
    Ok(Some(ExprType::scalar(result_oid, true)))
}

/// `expr IS [NOT] DISTINCT FROM other` — shares op_name "=" with ordinary
/// equality but PG guarantees the result is ALWAYS bool NOT NULL (the whole
/// point of the construct is NULL-aware comparison). Handled up front so
/// operand nullability doesn't bleed into the result.
fn handle_distinct_from(
    expr: &protobuf::AExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    if !matches!(
        protobuf::AExprKind::try_from(expr.kind),
        Ok(protobuf::AExprKind::AexprDistinct) | Ok(protobuf::AExprKind::AexprNotDistinct)
    ) {
        return Ok(None);
    }
    let left = expr
        .lexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;
    let left_oid = left.as_ref().map(|l| l.type_oid).unwrap_or(oid::UNKNOWN);
    let right = expr
        .rexpr
        .as_ref()
        .map(|n| infer_expr(n, ctx, params, TypeGoal::NONE))
        .transpose()?;
    let right_oid = right.as_ref().map(|r| r.type_oid).unwrap_or(oid::UNKNOWN);

    // PG transforms the construct through the `=` operator's resolution:
    // an UNKNOWN side is assumed to be the concrete peer's type (re-infer to
    // pin params / validate literal content); two concrete sides must have
    // an actual `=` overload — a goal-driven coercion check would wrongly
    // reject comparable pairs like `int4 IS DISTINCT FROM numeric`.
    if left_oid != oid::UNKNOWN && right_oid == oid::UNKNOWN {
        if let Some(rexpr) = &expr.rexpr {
            coerce_unknown_to(rexpr, ctx, params, snapshot.unwrap_domain(left_oid))?;
        }
    } else if left_oid == oid::UNKNOWN && right_oid != oid::UNKNOWN {
        if let Some(lexpr) = &expr.lexpr {
            coerce_unknown_to(lexpr, ctx, params, snapshot.unwrap_domain(right_oid))?;
        }
    } else if left_oid != oid::UNKNOWN
        && right_oid != oid::UNKNOWN
        && snapshot
            .find_operator("=", Some(left_oid), right_oid)
            .is_none()
    {
        // PG: `operator does not exist: <left> = <right>` — domain names
        // are reported as-is (`email = integer`), not unwrapped.
        let l = crate::ddl::util::format_type_for_message(snapshot, left_oid);
        let r = crate::ddl::util::format_type_for_message(snapshot, right_oid);
        return Err(crate::pgmsg::operator_does_not_exist(&l, "=", &r, None).finalize_implicit());
    }
    Ok(Some(ExprType::scalar(oid::BOOL, false)))
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

/// `a IN (x, y, …)` / `a NOT IN (…)` (pg_query tags NOT IN with op `<>`) —
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
            }
            // `a op ANY(common[])` resolves `a op common`.
            infer_synthetic_op(op, a, &typed_null(common), expr.location, ctx, params)?;
        }
    }
    for (i, (item, _, _)) in items.iter().enumerate() {
        if !folded[i] {
            infer_synthetic_op(op, a, item, expr.location, ctx, params)?;
        }
    }
    Ok(Some(ExprType::scalar(oid::BOOL, nullable)))
}

/// Resolve `l op r` exactly as a written binary operator would be — the
/// rewrite target of BETWEEN / IN (PG's `makeSimpleA_Expr` + transform).
fn infer_synthetic_op(
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
        location,
    };
    infer_a_expr(&e, ctx, params)
}

/// A `NULL::T` node standing for "some value of type T" (the folded array
/// element of an IN list).
fn typed_null(t: PgTypeOid) -> protobuf::Node {
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
fn contains_level0_column_ref(n: &protobuf::Node) -> bool {
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
    if right_oid != oid::UNKNOWN
        && !snapshot
            .get_type(snapshot.unwrap_domain(right_oid))
            .is_some_and(|t| t.typcategory == TypCategory::Array)
    {
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
    if left_oid != oid::UNKNOWN && right_oid == oid::UNKNOWN {
        match snapshot.array_type_of(snapshot.unwrap_domain(left_oid)) {
            Some(arr_oid) => {
                if let Some(rexpr) = &expr.rexpr {
                    coerce_unknown_to(rexpr, ctx, params, arr_oid)?;
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
        && let Some(elem_oid) = snapshot.get_type(right_oid).and_then(|t| {
            if t.typcategory == TypCategory::Array {
                t.typelem
            } else {
                None
            }
        })
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
        && let Some(elem_oid) = snapshot
            .get_type(snapshot.unwrap_domain(right_oid))
            .and_then(|t| {
                if t.typcategory == TypCategory::Array {
                    t.typelem
                } else {
                    None
                }
            })
    {
        let op_name = extract_string_fields(&expr.name).join(".");
        if !op_name.is_empty()
            && !op_name.contains('.')
            && snapshot
                .find_operator(&op_name, Some(left_oid), elem_oid)
                .is_none()
        {
            let l = crate::ddl::util::format_type_for_message(snapshot, left_oid);
            let r = crate::ddl::util::format_type_for_message(snapshot, elem_oid);
            return Err(
                crate::pgmsg::operator_does_not_exist(&l, &op_name, &r, None).finalize_implicit(),
            );
        }
    }

    let any_nullable =
        left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);
    Ok(Some(ExprType::scalar(oid::BOOL, any_nullable)))
}

/// Record-record comparison pre-pass.
///
/// `ROW(a, b) = ROW(c, d)` and the implicit `(a, b) = (c, d)` both parse as
/// AExpr with two RowExpr children. The generic resolver can't handle them:
/// `find_operator` looks for a `record OP record` overload but neither side
/// carries enough type info for params to be pinned, so `$p1`/`$p2` fall
/// through as text. Instead, walk both rows once to collect shapes, then
/// back-fill each ROW element with the peer's concrete OID as a goal — exactly
/// mirroring how PG types each component before reaching the row-compare
/// operator.
fn handle_row_row(
    expr: &protobuf::AExpr,
    op_name: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<ExprType>, AnalyzeError> {
    if !matches!(op_name, "=" | "<>" | "<" | ">" | "<=" | ">=") {
        return Ok(None);
    }
    let (Some(lexpr), Some(rexpr)) = (expr.lexpr.as_deref(), expr.rexpr.as_deref()) else {
        return Ok(None);
    };
    let (Some(node::Node::RowExpr(lrow)), Some(node::Node::RowExpr(rrow))) =
        (lexpr.node.as_ref(), rexpr.node.as_ref())
    else {
        return Ok(None);
    };

    // PG (parse_analyze): `unequal number of entries in row expressions`
    // when the two ROWs have different arity. Catch it up front so the
    // back-fill loop below can assume aligned positions.
    if lrow.args.len() != rrow.args.len() {
        return Err(AnalyzeError::Invalid(
            "unequal number of entries in row expressions".to_owned(),
        ));
    }
    // Pass 1: collect element types for each side with no goal.
    let mut left_types = Vec::with_capacity(lrow.args.len());
    let mut right_types = Vec::with_capacity(rrow.args.len());
    let mut any_nullable = false;
    for la in &lrow.args {
        let t = infer_expr(la, ctx, params, TypeGoal::NONE)?;
        any_nullable = any_nullable || t.nullable;
        left_types.push(t);
    }
    for ra in &rrow.args {
        let t = infer_expr(ra, ctx, params, TypeGoal::NONE)?;
        any_nullable = any_nullable || t.nullable;
        right_types.push(t);
    }

    // Pass 2: back-fill — when one side is concrete and the other is
    // UNKNOWN at the same position, re-walk the unknown side with the
    // concrete OID as goal so embedded params get pinned.
    for (i, (l, r)) in left_types.iter().zip(right_types.iter()).enumerate() {
        if l.type_oid != oid::UNKNOWN && r.type_oid == oid::UNKNOWN {
            coerce_unknown_to(&rrow.args[i], ctx, params, l.type_oid)?;
        } else if r.type_oid != oid::UNKNOWN && l.type_oid == oid::UNKNOWN {
            coerce_unknown_to(&lrow.args[i], ctx, params, r.type_oid)?;
        }
    }

    Ok(Some(ExprType::scalar(oid::BOOL, any_nullable)))
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

    for la in &lrow.args {
        let _ = infer_expr(la, ctx, params, TypeGoal::NONE);
    }
    let (cols, _) = crate::resolve::analyze_correlated_select(sel, snapshot, params, scope)?;
    if cols.len() != lrow.args.len() {
        let pg_msg = if cols.len() < lrow.args.len() {
            "subquery has too few columns"
        } else {
            "subquery has too many columns"
        };
        return Err(AnalyzeError::Invalid(format!(
            "{pg_msg} (subquery has {}, lhs has {})",
            cols.len(),
            lrow.args.len(),
        )));
    }
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

    let any_nullable =
        left.as_ref().is_some_and(|l| l.nullable) || right.as_ref().is_some_and(|r| r.nullable);
    let op_always_nullable = functions::is_nullable_operator(op_name);
    let nullable = any_nullable || op_always_nullable;

    // Operator lookup with the bottom-up types — UNKNOWN sides are resolved
    // by `find_operator`'s own rules (homogeneous probe, category and text
    // fallbacks), exactly like PG; the unknown side is *not* pre-pinned to
    // the concrete peer's type.
    match snapshot.find_operator_detailed(op_name, left_oid_resolved, right_oid_resolved) {
        crate::lookup::OperatorMatch::Found(op) => {
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
            return Ok(ExprType::scalar(op.result_type_oid, nullable));
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
        Some(l) => crate::pgmsg::operator_does_not_exist(
            &crate::ddl::util::format_type_for_message(snapshot, l),
            op_name,
            &right_pg,
            span,
        ),
        None => crate::pgmsg::prefix_operator_does_not_exist(op_name, &right_pg, span),
    };
    Err(err.finalize_implicit())
}
