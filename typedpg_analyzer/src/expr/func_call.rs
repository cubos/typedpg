use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Function calls — two-pass (PG chapter 10.3)
// ──────────────────────────────────────────────────────────────────────────────

/// Inferred argument types collected in Pass 1, before overload resolution.
struct FuncArgs {
    /// Type OID of each argument (UNKNOWN for untyped literals/params).
    types: Vec<PgTypeOid>,
    /// Per-argument nullability, read later by `concat_ws`/`lag`/`lead`.
    nullable: Vec<bool>,
    /// `true` if any argument is nullable.
    any_nullable: bool,
    /// Number of *direct* args (`func.args`); for ordered-set aggregates the
    /// `WITHIN GROUP (ORDER BY …)` exprs are appended to `types` after these.
    direct_count: usize,
}

pub(crate) fn infer_func_call(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        null_ctx, snapshot, ..
    } = ctx;
    let func_name_parts = extract_string_fields(&func.funcname);
    let (schema, name) = match func_name_parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedFunction(format!(
                "invalid function name: {:?}",
                func_name_parts
            )));
        }
    };

    validate_within_group(func)?;

    // Pass 1: infer argument types bottom-up.
    let args = collect_arg_types(func, ctx, params)?;
    let notation = functions::CallNotation::of(func)?;

    // Resolve the call (PG's func_get_detail). A single-argument call named
    // after a type with no exact match may instead be a function-style cast
    // (FUNCDETAIL_COERCION) — `text(123)`, `pg_catalog."numeric"('1')`;
    // an untyped literal argument always makes it one.
    let unknown_const = args.types.first() == Some(&oid::UNKNOWN)
        && matches!(
            func.args.first().and_then(|a| a.node.as_ref()),
            Some(node::Node::AConst(_))
        );
    let resolved = match functions::func_get_detail(
        snapshot,
        schema,
        name,
        &args.types,
        &notation,
        Some(unknown_const),
        crate::error::SourceSpan::from_node_qname(func.location),
    )? {
        functions::FuncDetail::Routine(r) => r,
        functions::FuncDetail::Coercion(target) => {
            check_call_shape(func, None, &args, &notation, ctx)?;
            // Same literal-content validation an explicit cast performs.
            if let Some(node::Node::AConst(ac)) = func.args[0].node.as_ref()
                && !ac.isnull
                && let Some(pg_query::protobuf::a_const::Val::Sval(sv)) = &ac.val
                && let Err(msg) = crate::literal_input::validate(&sv.sval, target, snapshot)
            {
                let span = crate::error::node_location(&func.args[0])
                    .and_then(crate::error::SourceSpan::from_node_token);
                return Err(crate::error::RawError::invalid_literal(msg, span).finalize_implicit());
            }
            return Ok(ExprType::scalar(target, args.nullable[0]));
        }
    };

    check_call_shape(func, Some(&resolved), &args, &notation, ctx)?;

    if func.over.is_some() {
        check_no_nested_windows(func, snapshot)?;
    } else if resolved.is_aggregate {
        check_no_nested_aggregates(func, snapshot)?;
    }

    // The pseudo-type `"any"` (plain and VARIADIC, e.g. `concat`,
    // `format`, `jsonb_build_object`) gives PG nothing to infer a bare
    // `$N` from — `concat(name, $1)` fails prepare with `could not
    // determine data type of parameter $1`. Mirror the first-use lock.
    const ANY_PSEUDO: PgTypeOid = PgTypeOid::from_raw(2276);
    let declared_any = |i: usize| -> bool {
        match resolved.arg_types.get(i) {
            Some(&t) => t == ANY_PSEUDO,
            // Past the declared list — only reachable for variadic
            // matches, where the tail repeats the last declared type.
            None => resolved.arg_types.last() == Some(&ANY_PSEUDO),
        }
    };
    // (A hypothetical-set aggregate's `"any"` arguments are the exception:
    // `unify_hypothetical_args` types them below.)
    let hypothetical = matches!(
        resolved.aggregate,
        Some((crate::pg_catalog::AggKind::Hypothetical, _))
    );
    for (i, arg) in func.args.iter().enumerate() {
        if declared_any(i)
            && !hypothetical
            && let Some(node::Node::ParamRef(p)) = functions::call_arg_value(arg).node.as_ref()
        {
            params.mark_indeterminate_locked(p.number);
        }
    }

    // A hypothetical-set aggregate (`rank(3) WITHIN GROUP (ORDER BY x)`)
    // types each hypothetical direct argument like its ordering column.
    if matches!(
        resolved.aggregate,
        Some((crate::pg_catalog::AggKind::Hypothetical, _))
    ) {
        unify_hypothetical_args(func, &args, ctx, params)?;
    }

    // Pass 2: back-fill UNKNOWN args from the resolved signature.
    backfill_func_args(func, &args, &resolved, ctx, params)?;

    // Walk aggregate / window modifiers so embedded params and column refs
    // are inferred and validated.
    walk_func_modifiers(func, ctx, params)?;

    let nullable = resolve_func_nullability(func, name, &resolved, null_ctx, &args);

    // SRFs / OUT-arg functions carry a static row shape — propagate it as
    // `record_fields` so downstream `(call(...)).field` / `(scope_col).field`
    // indirection sees the named columns with their substituted polymorphic
    // types (e.g. `_pg_expandarray(oid[]).x` → `oid`, not `anyelement`).
    let record_fields = if resolved.out_args.is_empty() {
        None
    } else {
        Some(RecordField::from_out_args(&resolved.out_args))
    };
    Ok(ExprType {
        type_oid: resolved.return_type_oid,
        nullable,
        // Functions / aggregates / window calls never propagate the
        // argument's typmod (PG matching: `lower(varchar(20))` returns
        // varchar, not varchar(20)).
        typmod: None,
        // Collation derivation through function calls is PG's most
        // intricate area (see "collation derivation" in the docs). For
        // the common case of `lower(text_col)` / `upper(text_col)` the
        // input collation flows through, but exhaustive support
        // requires the per-function `proargcollation`/`procollation`
        // we don't model. Conservatively drop collation through
        // calls — the compiler still propagates COLLATE-decorated
        // column refs for the surrounding context.
        collation: None,
        record_fields,
    })
}

/// PG's checks in `ParseFuncOrColumn` / `transformAggregateCall` that the
/// call's modifiers (`(*)`, DISTINCT, ORDER BY, WITHIN GROUP, FILTER, OVER,
/// named arguments) suit the kind of routine it resolved to. `resolved` is
/// `None` for a function-style cast. Names render as written (PG's
/// `NameListToString`).
fn check_call_shape(
    func: &protobuf::FuncCall,
    resolved: Option<&functions::ResolvedFunction>,
    args: &FuncArgs,
    notation: &functions::CallNotation,
    ctx: Ctx<'_>,
) -> Result<(), AnalyzeError> {
    use crate::pg_catalog::AggKind;
    use crate::pgmsg;
    let w = extract_string_fields(&func.funcname).join(".");
    let span = crate::error::SourceSpan::from_node_qname(func.location);
    let fail = |e: crate::error::RawError| Err(e.finalize_implicit());
    let is_aggregate = resolved.is_some_and(|r| r.is_aggregate);
    let is_window = resolved.is_some_and(|r| r.is_window);

    if let Some(filter) = &func.agg_filter {
        let kinds = detect_func_kinds(filter, ctx.snapshot);
        if kinds.has_aggregate {
            return fail(pgmsg::aggregate_in_filter(span));
        }
        if kinds.has_window {
            return fail(pgmsg::window_in_filter(span));
        }
    }

    if !is_aggregate && !is_window {
        let modifier = if func.agg_star {
            Some(format!("{w}(*)"))
        } else if func.agg_distinct {
            Some("DISTINCT".into())
        } else if func.agg_within_group {
            Some("WITHIN GROUP".into())
        } else if !func.agg_order.is_empty() {
            Some("ORDER BY".into())
        } else if func.agg_filter.is_some() {
            Some("FILTER".into())
        } else {
            None
        };
        if let Some(m) = modifier {
            return fail(pgmsg::not_an_aggregate(&m, &w, span));
        }
        if func.over.is_some() {
            // PG classifies both placement failures as wrong_object_type.
            return fail(crate::error::RawError::new(
                AnalyzeError::WrongObjectType(format!(
                    "OVER specified, but {w} is not a window function nor an aggregate function"
                )),
                span,
                None,
            ));
        }
        return Ok(());
    }
    let Some(resolved) = resolved else {
        return Ok(());
    };

    if is_window {
        if func.over.is_none() {
            return fail(crate::error::RawError::new(
                AnalyzeError::WrongObjectType(format!(
                    "window function {w} requires an OVER clause"
                )),
                span,
                Some("add `OVER ()` (or a window definition) after the call".into()),
            ));
        }
        if func.agg_within_group {
            return fail(pgmsg::window_function_within_group(&w, span));
        }
    } else if let Some((kind, direct)) = resolved.aggregate {
        if matches!(kind, AggKind::OrderedSet | AggKind::Hypothetical) {
            if !func.agg_within_group {
                return fail(pgmsg::within_group_required(&w, span));
            }
            if func.over.is_some() {
                return fail(pgmsg::call_shape_not_implemented(
                    &format!("OVER is not supported for ordered-set aggregate {w}"),
                    span,
                ));
            }
            // func_get_detail matched the undifferentiated argument list;
            // the split into direct and aggregated arguments must fit too.
            let nargs = args.types.len();
            let aggregated = func.agg_order.len();
            let num_direct = nargs - aggregated;
            let direct = direct as usize;
            let fits = if resolved.provariadic.is_none() {
                num_direct == direct
            } else {
                let pronargs = nargs - resolved.nvargs.saturating_sub(1);
                if direct < pronargs {
                    num_direct == direct
                } else if kind == AggKind::Hypothetical {
                    resolved.nvargs == 2 * aggregated
                } else {
                    resolved.nvargs > aggregated
                }
            };
            if !fits {
                let sig = args
                    .types
                    .iter()
                    .map(|&t| crate::ddl::util::format_type_for_message(ctx.snapshot, t))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(functions::undefined_function_error(
                    ctx.snapshot,
                    None,
                    &w,
                    format!("function {w}({sig}) does not exist"),
                    span,
                ));
            }
        } else if func.agg_within_group {
            return fail(pgmsg::not_an_ordered_set_aggregate(&w, span));
        }
    }

    if func.over.is_none() {
        if func.args.is_empty() && !func.agg_star && !func.agg_within_group {
            return fail(pgmsg::parameterless_aggregate_needs_star(&w, span));
        }
        if !notation.names.is_empty() {
            return fail(pgmsg::call_shape_not_implemented(
                "aggregates cannot use named arguments",
                span,
            ));
        }
        // transformAggregateCall: with DISTINCT, every ORDER BY expression
        // must be one of the arguments.
        if func.agg_distinct && !func.agg_within_group {
            for item in &func.agg_order {
                if let Some(node::Node::SortBy(sb)) = item.node.as_ref()
                    && let Some(inner) = sb.node.as_deref()
                    && !func.args.iter().any(|a| same_expression(a, inner, ctx))
                {
                    return fail(pgmsg::distinct_aggregate_order_by_not_in_args(
                        crate::error::node_location(inner)
                            .and_then(crate::error::SourceSpan::from_node_qname),
                    ));
                }
            }
        }
    } else {
        let message = if func.agg_distinct {
            Some("DISTINCT is not implemented for window functions")
        } else if is_aggregate && !func.agg_order.is_empty() {
            Some("aggregate ORDER BY is not implemented for window functions")
        } else if !is_aggregate && func.agg_filter.is_some() {
            Some("FILTER is not implemented for non-aggregate window functions")
        } else {
            None
        };
        if let Some(m) = message {
            return fail(pgmsg::call_shape_not_implemented(m, span));
        }
        if is_aggregate && func.args.is_empty() && !func.agg_star {
            return fail(pgmsg::parameterless_aggregate_needs_star(&w, span));
        }
    }
    Ok(())
}

/// Whether two argument/sort expressions are the same (PG compares the
/// transformed trees with `equal()`): structurally equal ignoring source
/// locations, or column references resolving to the same column.
fn same_expression(a: &protobuf::Node, b: &protobuf::Node, ctx: Ctx<'_>) -> bool {
    if let (Some(node::Node::ColumnRef(ca)), Some(node::Node::ColumnRef(cb))) =
        (a.node.as_ref(), b.node.as_ref())
    {
        let resolve = |cr: &protobuf::ColumnRef| {
            let parts = extract_string_fields(&cr.fields);
            let (table, col) = match parts.as_slice() {
                [c] => (None, c.as_str()),
                [t, c] => (Some(t.as_str()), c.as_str()),
                _ => return None,
            };
            ctx.scope
                .resolve_column(table, col, None)
                .ok()
                .map(|c| (c.table_alias.clone(), c.name.clone()))
        };
        if let (Some(x), Some(y)) = (resolve(ca), resolve(cb)) {
            return x == y;
        }
    }
    crate::resolve::node_fingerprint(a) == crate::resolve::node_fingerprint(b)
}

/// PG's `unify_hypothetical_args` (parse_func.c): each hypothetical direct
/// argument of a hypothetical-set aggregate and its ordering column are
/// coerced to their common type — `rank($1) WITHIN GROUP (ORDER BY
/// int_col)` types `$1` as integer, `rank('a') …` validates `'a'` as one.
fn unify_hypothetical_args(
    func: &protobuf::FuncCall,
    args: &FuncArgs,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let aggregated = func.agg_order.len();
    let num_direct = args.direct_count;
    let Some(first_hypothetical) = num_direct.checked_sub(aggregated) else {
        return Ok(());
    };
    for (k, harg) in func.args[first_hypothetical..num_direct].iter().enumerate() {
        let (ht, at) = (
            args.types[first_hypothetical + k],
            args.types[num_direct + k],
        );
        // The aggregated argument's type is preferred: coerce the direct
        // argument once rather than every aggregated value.
        let Some(common) = crate::coerce::find_common_type(&[at, ht], ctx.snapshot) else {
            let name = |t| crate::ddl::util::format_type_for_message(ctx.snapshot, t);
            return Err(crate::pgmsg::types_cannot_be_matched(
                "WITHIN GROUP",
                &name(at),
                &name(ht),
                "",
                None,
            )
            .finalize_implicit());
        };
        if ht == oid::UNKNOWN {
            coerce_unknown_to(harg, ctx, params, common)?;
        }
    }
    Ok(())
}

/// `WITHIN GROUP (ORDER BY …)` marks an ordered-set aggregate. PG's grammar
/// forbids combining it with `DISTINCT`; reject that up front so the error
/// points at the actual conflict instead of a misleading overload-resolution
/// failure. (`OVER` is rejected after resolution, by [`check_call_shape`],
/// with the wording that depends on what the call resolved to.)
fn validate_within_group(func: &protobuf::FuncCall) -> Result<(), AnalyzeError> {
    if func.agg_within_group && func.agg_distinct {
        return Err(AnalyzeError::Invalid(
            "DISTINCT is not implemented for ordered-set aggregates".into(),
        ));
    }
    Ok(())
}

/// Pass 1: infer every direct argument with no goal, then (for ordered-set
/// aggregates) append the `WITHIN GROUP (ORDER BY …)` expression types so
/// overload resolution sees the full signature — PG records both direct args
/// and ordered args in `pg_proc.proargtypes`.
fn collect_arg_types(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<FuncArgs, AnalyzeError> {
    let mut types = Vec::with_capacity(func.args.len());
    let mut nullable = Vec::with_capacity(func.args.len());
    let mut any_nullable = false;
    for arg in &func.args {
        let t = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
        any_nullable = any_nullable || t.nullable;
        nullable.push(t.nullable);
        types.push(t.type_oid);
    }

    let direct_count = types.len();
    if func.agg_within_group {
        for order_item in &func.agg_order {
            // Each item is a `SortBy` wrapping the actual sort expression.
            let sort_inner = match order_item.node.as_ref() {
                Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                _ => Some(order_item),
            };
            if let Some(inner) = sort_inner {
                let t = infer_expr(inner, ctx, params, TypeGoal::NONE)?;
                any_nullable = any_nullable || t.nullable;
                nullable.push(t.nullable);
                types.push(t.type_oid);
            }
        }
    }

    Ok(FuncArgs {
        types,
        nullable,
        any_nullable,
        direct_count,
    })
}

/// A window function's arguments may hold aggregates — `sum(sum(x)) OVER
/// (…)` aggregates the grouped rows first — but not another window
/// function (PG's `transformWindowFuncCall`, SQLSTATE 42P20).
fn check_no_nested_windows(
    func: &protobuf::FuncCall,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    if func
        .args
        .iter()
        .any(|arg| detect_func_kinds(arg, snapshot).has_window)
    {
        return Err(AnalyzeError::WindowingError(
            "window function calls cannot be nested".into(),
        ));
    }
    Ok(())
}

/// PG forbids aggregates / window functions nested inside aggregate arguments
/// (`SUM(COUNT(*))`). Catch it after resolution, using each arg's AST.
fn check_no_nested_aggregates(
    func: &protobuf::FuncCall,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    for arg in &func.args {
        let kinds = detect_func_kinds(arg, snapshot);
        if kinds.has_aggregate {
            return Err(AnalyzeError::GroupingError(
                "aggregate function calls cannot be nested".into(),
            ));
        }
        if kinds.has_window {
            return Err(AnalyzeError::GroupingError(
                "aggregate function calls cannot contain window function calls".into(),
            ));
        }
    }
    Ok(())
}

/// Pass 2: back-fill UNKNOWN direct args with the expected types from the
/// resolved signature (equivalent to PG's `coerce_func_args`). Only the direct
/// args correspond to `func.args`; ordered args (`agg_within_group`) come from
/// `func.agg_order` and are handled by [`walk_func_modifiers`].
fn backfill_func_args(
    func: &protobuf::FuncCall,
    args: &FuncArgs,
    resolved: &functions::ResolvedFunction,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for (i, arg) in func.args.iter().enumerate() {
        if i >= args.direct_count {
            break;
        }
        if args.types[i] == oid::UNKNOWN
            && let Some(&expected) = resolved.arg_types.get(i)
            && expected != oid::UNKNOWN
            // A pseudo-type slot (`"any"`, anyelement, …) is not a concrete
            // coercion target — recording it would leak the pseudo OID into
            // a param's type.
            && ctx
                .snapshot
                .get_type(expected)
                .is_none_or(|t| t.typtype != TypType::Pseudo)
        {
            // Speculative re-walk: ordinary failures are swallowed, but a
            // literal-content rejection is the parse-time error PG itself
            // raises from this argument coercion (`sqrt('x')`).
            coerce_unknown_to(functions::call_arg_value(arg), ctx, params, expected)?;
        }
    }
    Ok(())
}

/// Walk aggregate modifiers so any `$N` placeholders they contain get their
/// types inferred and column refs validated. FILTER must be bool (like a WHERE
/// clause), per-aggregate ORDER BY items have no specific goal, and the WINDOW
/// `OVER (…)` clause's expressions are walked too. None of these positions can
/// reference a select-list alias — they're all row-level — so propagating
/// errors here matches PG.
fn walk_func_modifiers(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    if let Some(filter) = &func.agg_filter {
        // FILTER is a boolean clause like WHERE — wording and ordering live
        // in the shared clause walker.
        crate::clause::coerce_clause_expr(filter, ctx, params, crate::clause::ClauseKind::Filter)?;
    }
    // Per-aggregate `ORDER BY` (e.g. `array_agg(x ORDER BY y)`). For
    // ordered-set aggregates (`WITHIN GROUP`) the sort expressions were
    // already inferred in Pass 1 as part of the arg list, so skip to avoid
    // double inference / param recording. Items are `SortBy` nodes — unwrap
    // to the inner expression before inferring.
    if !func.agg_within_group {
        for order_item in &func.agg_order {
            if let Some(node::Node::SortBy(sb)) = order_item.node.as_ref()
                && let Some(inner) = sb.node.as_deref()
            {
                infer_expr(inner, ctx, params, TypeGoal::NONE)?;
            }
        }
    }
    if let Some(over) = &func.over {
        for item in &over.partition_clause {
            infer_expr(item, ctx, params, TypeGoal::NONE)?;
        }
        for item in &over.order_clause {
            // Window `ORDER BY` items are also `SortBy` nodes; unwrap.
            if let Some(node::Node::SortBy(sb)) = item.node.as_ref()
                && let Some(inner) = sb.node.as_deref()
            {
                infer_expr(inner, ctx, params, TypeGoal::NONE)?;
            }
        }
        // Frame offsets: ROWS / GROUPS offsets are int8 in PG (so a bare
        // `$N` there describes as bigint); RANGE offsets take the ORDER BY
        // key's "distance" type (interval for timestamps, …) — left
        // unconstrained. Bits from parsenodes.h: ROWS 0x4, GROUPS 0x8.
        let offset_goal = if over.frame_options & 0x4 != 0 || over.frame_options & 0x8 != 0 {
            TypeGoal::implicit(oid::INT8)
        } else {
            TypeGoal::NONE
        };
        if let Some(start) = &over.start_offset {
            infer_expr(start, ctx, params, offset_goal.clone())?;
        }
        if let Some(end) = &over.end_offset {
            infer_expr(end, ctx, params, offset_goal)?;
        }
    }
    Ok(())
}

/// `EXTRACT(field FROM ts)` / `date_part('field', ts)` over a timestamp or
/// date is NULL only for an infinite input with a field that has no
/// infinite value (`month`, `day`, …); the fields PG's
/// `NonFiniteTimestampTzPart` maps to ±Infinity (`year`, `epoch`, … in any
/// of `datetime.c`'s spellings) never are. With such a literal field the
/// call is NULL exactly when an argument is.
fn extract_unit_is_infinite_safe(
    func: &protobuf::FuncCall,
    resolved: &functions::ResolvedFunction,
) -> bool {
    const INFINITE_FIELDS: &[&str] = &[
        "epoch",
        "isoyear",
        "j",
        "jd",
        "julian",
        "y",
        "year",
        "years",
        "yr",
        "yrs",
        "c",
        "cent",
        "centuries",
        "century",
        "dec",
        "decade",
        "decades",
        "decs",
        "mil",
        "millennia",
        "millennium",
        "mils",
    ];
    let over_timestamp = matches!(
        resolved.signature.as_str(),
        "extract(text,timestamp)"
            | "extract(text,timestamptz)"
            | "extract(text,date)"
            | "date_part(text,timestamp)"
            | "date_part(text,timestamptz)"
            | "date_part(text,date)"
    );
    over_timestamp
        && matches!(
            func.args.first().and_then(|a| a.node.as_ref()),
            Some(node::Node::AConst(protobuf::AConst {
                val: Some(pg_query::protobuf::a_const::Val::Sval(sv)),
                ..
            })) if INFINITE_FIELDS.contains(&sv.sval.to_lowercase().as_str())
        )
}

/// Whether a window frame always includes the current row (PG's
/// `FRAMEOPTION_*` bits, parsenodes.h): it starts at or before it
/// (UNBOUNDED/offset PRECEDING, CURRENT ROW), ends at or after it (CURRENT
/// ROW, offset/UNBOUNDED FOLLOWING), and doesn't `EXCLUDE CURRENT ROW` or
/// `EXCLUDE GROUP`. The default frame (RANGE UNBOUNDED PRECEDING to CURRENT
/// ROW) does.
fn frame_contains_current_row(options: i32) -> bool {
    const NONDEFAULT: i32 = 0x1;
    const BETWEEN: i32 = 0x10;
    const START_UNBOUNDED_PRECEDING: i32 = 0x20;
    const END_UNBOUNDED_FOLLOWING: i32 = 0x100;
    const START_CURRENT_ROW: i32 = 0x200;
    const END_CURRENT_ROW: i32 = 0x400;
    const START_OFFSET_PRECEDING: i32 = 0x800;
    const END_OFFSET_FOLLOWING: i32 = 0x4000;
    const EXCLUDE_CURRENT_ROW: i32 = 0x8000;
    const EXCLUDE_GROUP: i32 = 0x10000;
    if options & NONDEFAULT == 0 {
        return true;
    }
    let starts_before =
        options & (START_UNBOUNDED_PRECEDING | START_CURRENT_ROW | START_OFFSET_PRECEDING) != 0;
    // Without BETWEEN the frame ends at the current row.
    let ends_after = options & BETWEEN == 0
        || options & (END_CURRENT_ROW | END_OFFSET_FOLLOWING | END_UNBOUNDED_FOLLOWING) != 0;
    starts_before && ends_after && options & (EXCLUDE_CURRENT_ROW | EXCLUDE_GROUP) == 0
}

/// Decide whether a function/aggregate/window call's result is nullable.
///
/// Covers value-window edge NULLs (`lag`/`lead`/…), aggregate emptiness
/// (FILTER, empty grouping sets, GROUP BY presence), strict-function
/// propagation, and the `concat_ws` separator special case.
fn resolve_func_nullability(
    func: &protobuf::FuncCall,
    name: &str,
    resolved: &functions::ResolvedFunction,
    null_ctx: &NullabilityContext,
    args: &FuncArgs,
) -> bool {
    let arg_is_nullable = |i: usize| args.nullable.get(i).copied().unwrap_or(false);

    // Value window functions (`lag`/`lead`/`first_value`/`last_value`/
    // `nth_value`) can return NULL at partition/frame edges even when the
    // source column is NOT NULL — `lag(title) OVER (ORDER BY id)` produces
    // NULL for the first row of each partition. A 3-arg `lag(col, offset,
    // default)`/`lead(...)` replaces the boundary NULL with `default`, so
    // the result is only nullable when the value, the offset (a NULL
    // offset gives NULL) or the default are.
    let is_value_window = func.over.is_some()
        && matches!(
            name,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        );

    if is_value_window {
        match name {
            "lag" | "lead" if func.args.len() >= 3 => {
                arg_is_nullable(0) || arg_is_nullable(1) || arg_is_nullable(2)
            }
            _ => true,
        }
    } else if resolved.is_aggregate {
        let builtin = resolved.schema == "pg_catalog";
        // An aggregate over a non-empty set of rows: NULL only on NULL
        // input, except for the builtins that are NULL for some non-empty
        // inputs (a single row, zero variance) and user-defined aggregates,
        // whose final/transition functions may return NULL at will.
        let over_rows = || {
            args.any_nullable
                || !builtin
                || crate::builtin_nullability::NULLABLE_AGGREGATES_OVER_ROWS.contains(&name)
        };
        if builtin && name == "count" {
            // COUNT is never NULL (returns 0 for empty input, even with FILTER).
            false
        } else if func.agg_filter.is_some() {
            // A FILTER clause can eliminate every row in the group.
            true
        } else if let Some(over) = &func.over {
            // A window aggregate sees its frame: never empty when the frame
            // contains the current row (every window input row exists), but
            // `ROWS … 1 PRECEDING`, `… FOLLOWING`-only frames and `EXCLUDE
            // CURRENT ROW / GROUP` can leave it empty. `OVER w` takes its
            // frame from the WINDOW clause, not visible here.
            !over.name.is_empty() || !frame_contains_current_row(over.frame_options) || over_rows()
        } else if null_ctx.has_empty_grouping_set {
            // GROUPING SETS / ROLLUP / CUBE include an empty grouping set
            // (or `GROUP BY ()` does explicitly). For that row the aggregate
            // sees the whole input — and an empty input still produces NULL
            // for non-COUNT aggregates.
            true
        } else if null_ctx.has_group_by {
            over_rows()
        } else {
            // Without GROUP BY, non-COUNT aggregates return NULL for empty tables.
            true
        }
    } else if resolved.schema == "pg_catalog" && extract_unit_is_infinite_safe(func, resolved) {
        args.any_nullable
    } else if resolved.schema == "pg_catalog" {
        functions::builtin_result_nullable(resolved, &args.nullable, func.func_variadic)
    } else {
        // A user-defined function can return NULL whatever its inputs.
        true
    }
}
