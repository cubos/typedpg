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
    /// Each argument's inferred type with its collation state, for the
    /// call's collation derivation.
    exprs: Vec<ExprType>,
    /// Number of *direct* args (`func.args`); for ordered-set aggregates the
    /// `WITHIN GROUP (ORDER BY …)` exprs are appended to `types` after these.
    direct_count: usize,
    /// Per argument (parallel to `nullable`), whether coercing it to the
    /// resolved parameter type can map a non-NULL value to NULL (see
    /// [`coerce_call_args`]).
    coercion_nullable: Vec<bool>,
}

pub(crate) fn infer_func_call(
    func: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let func_name_parts = extract_string_fields(&func.funcname);
    let (schema, name) = deconstruct_qualified_name(
        &func_name_parts,
        crate::error::SourceSpan::from_node_qname(func.location),
    )?;

    validate_within_group(func)?;

    // A window call reads the other rows of its partition: what only
    // holds of the row the value is read for (a CASE branch's WHEN, the
    // AND / OR arms before it) says nothing of them.
    if func.over.is_some()
        && let Some(level) = ctx.null_ctx.for_other_rows()
    {
        return infer_func_call(func, ctx.with_null_ctx(&level), params);
    }
    // An aggregate's arguments are evaluated for its input rows — rows
    // past FROM and WHERE, even where the level's own row may come from
    // no input (see `NullabilityContext::where_exprs_off`).
    if func.over.is_none() && ctx.null_ctx.where_exprs_off && only_aggregates_named(func, snapshot)
    {
        let mut rows = ctx.null_ctx.clone();
        rows.where_exprs_off = false;
        return infer_func_call(func, ctx.with_null_ctx(&rows), params);
    }

    // `x(t)` may be the column projection `(t).x` (tried before lookup).
    if let Some(field) = try_column_projection(func, ctx, params)? {
        return Ok(field);
    }

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
    let detail = functions::func_get_detail(
        snapshot,
        schema,
        name,
        &args.types,
        &notation,
        Some(unknown_const),
        crate::error::SourceSpan::from_node_qname(func.location),
    );
    let detail = match detail {
        Err(e) if e.sqlstate() == Some("42883") => {
            return Err(unmatched_projection_error(func, ctx, params).unwrap_or(e));
        }
        other => other?,
    };
    let resolved = match detail {
        functions::FuncDetail::Routine(r) => r,
        functions::FuncDetail::Coercion(target) => {
            check_call_shape(func, None, &args, &notation, ctx)?;
            // Same literal-content validation an explicit cast performs.
            if let Some(node::Node::AConst(ac)) = func.args[0].node.as_ref()
                && !ac.isnull
                && let Some(typedpg_pg_query::protobuf::a_const::Val::Sval(sv)) = &ac.val
                // A function-style cast carries no typmod.
                && let Err(msg) =
                    crate::literal_input::validate_with_typmod(&sv.sval, target, None, snapshot)
            {
                let span = crate::error::node_location(&func.args[0])
                    .and_then(crate::error::SourceSpan::from_node_token);
                return Err(crate::error::RawError::invalid_literal(msg, span).finalize_implicit());
            }
            return Ok(ExprType::scalar(target, args.nullable[0]));
        }
    };

    check_call_shape(func, Some(&resolved), &args, &notation, ctx)?;
    ctx.note_proc(Some(resolved.oid));
    // A call leaving parameters out runs their DEFAULT expressions — unless
    // the function is inlined (a simple SQL body, which CheckMutability
    // judges by itself).
    if snapshot
        .pg_proc
        .get(&resolved.oid)
        .is_some_and(|p| func.args.len() < p.proargtypes.len())
        && !snapshot.inline_sql_bodies.contains_key(&resolved.oid)
    {
        for &p in snapshot
            .proc_default_procs
            .get(&resolved.oid)
            .into_iter()
            .flatten()
        {
            ctx.note_proc(Some(p));
        }
    }
    // A plain call of a strict function is NULL for a NULL argument — not
    // an aggregate or window call, nor a variadic one, whose arguments are
    // packed into an array the function sees whole.
    ctx.note_strict(
        func.location,
        crate::nonnull::StrictNode::Func,
        resolved.is_strict
            && !resolved.is_aggregate
            && !resolved.is_window
            && func.over.is_none()
            && resolved.nvargs == 0
            && args
                .types
                .iter()
                .zip(&resolved.arg_types)
                .all(|(&a, &d)| ctx.coercion_is_strict(a, d)),
    );
    // `num_nulls` / `num_nonnulls` as the analyzer counts them: the
    // built-in ones, not a user function of the name the call resolved to.
    ctx.note_strict(
        func.location,
        crate::nonnull::StrictNode::NullCount,
        snapshot.pg_proc.get(&resolved.oid).is_some_and(|p| {
            matches!(p.proname.as_str(), "num_nulls" | "num_nonnulls")
                && snapshot.namespace_name(p.pronamespace) == Some("pg_catalog")
        }),
    );
    for (&actual, &declared) in args.types.iter().zip(&resolved.arg_types) {
        ctx.note_coercion(actual, declared);
    }

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
    let filter_log = crate::nonnull::StrictLog::default();
    walk_func_modifiers(func, ctx, params, &filter_log)?;
    if let Some(outer) = ctx.strict_log {
        outer.absorb(&filter_log);
    }
    let mut args = args;
    if resolved.is_aggregate {
        narrow_by_filter(func, &mut args, ctx, params, &filter_log);
    }
    // The result's collation derives from the arguments' (assign_collations).
    let (collation, explicit_collation) =
        derive_collation(&args.exprs, resolved.return_type_oid, snapshot)?;
    coerce_call_args(&mut args, &resolved, snapshot);

    // The rules below read arguments by parameter position.
    let (func, args) = in_declared_order(func, args, &resolved);
    let func: &protobuf::FuncCall = &func;
    let nullable = resolve_func_nullability(func, name, &resolved, ctx, params, &args);

    // SRFs / OUT-arg functions carry a static row shape — propagate it as
    // `record_fields` so downstream `(call(...)).field` / `(scope_col).field`
    // indirection sees the named columns with their substituted polymorphic
    // types (e.g. `_pg_expandarray(oid[]).x` → `oid`, not `anyelement`).
    let record_fields = if resolved.out_args.is_empty() {
        None
    } else {
        let mut fields = RecordField::from_out_args(&resolved.out_args);
        // The rows a strict catalog SRF emits (`jsonb_each(j)`: a key, and
        // a value never SQL NULL) — a NULL record (lockstep padding) reads
        // as NULL fields anyway.
        if let Some(nullable) = crate::resolve::srf_out_columns_nullable(&resolved)
            && nullable.len() == fields.len()
        {
            for (f, &n) in fields.iter_mut().zip(nullable) {
                f.ty.nullable = n;
            }
        }
        Some(fields.into())
    };
    Ok(ExprType {
        type_oid: resolved.return_type_oid,
        nullable,
        // Functions / aggregates / window calls never propagate the
        // argument's typmod (PG matching: `lower(varchar(20))` returns
        // varchar, not varchar(20)).
        typmod: None,
        collation,
        explicit_collation,
        record_fields,
        elem_nullable: builtin_array_elem_nullable(func, &resolved, &args),
    })
}

/// The call with its arguments in positional notation, for the rules that
/// read them by parameter position: named arguments (`string_agg(delimiter
/// => ',', value => x)`) moved to the parameter each binds to, with their
/// [`FuncArgs`] entries. A parameter left to its default before a later
/// named one is an empty node (matching no rule), its default taken as
/// non-NULL as an omitted trailing one is.
fn in_declared_order<'a>(
    func: &'a protobuf::FuncCall,
    args: FuncArgs,
    resolved: &functions::ResolvedFunction,
) -> (std::borrow::Cow<'a, protobuf::FuncCall>, FuncArgs) {
    if !func
        .args
        .iter()
        .any(|a| matches!(a.node.as_ref(), Some(node::Node::NamedArgExpr(_))))
    {
        return (std::borrow::Cow::Borrowed(func), args);
    }
    let values: Vec<protobuf::Node> = func
        .args
        .iter()
        .map(|a| functions::call_arg_value(a).clone())
        .collect();
    let mut positional = func.clone();
    positional.args = resolved.in_declared_order(&values, protobuf::Node::default());
    let args = FuncArgs {
        types: resolved.in_declared_order(&args.types, oid::UNKNOWN),
        nullable: resolved.in_declared_order(&args.nullable, false),
        any_nullable: args.any_nullable,
        exprs: resolved.in_declared_order(&args.exprs, ExprType::scalar(oid::UNKNOWN, false)),
        direct_count: positional.args.len(),
        coercion_nullable: resolved.in_declared_order(&args.coercion_nullable, false),
    };
    (std::borrow::Cow::Owned(positional), args)
}

/// Whether the elements of the array a `pg_catalog` routine returns can be
/// NULL, for the routines whose result says (`None` for the rest): an
/// aggregate or constructor of values has NULL elements exactly when a
/// value is NULL, an array transformed element-wise keeps its input's.
fn builtin_array_elem_nullable(
    func: &protobuf::FuncCall,
    resolved: &functions::ResolvedFunction,
    args: &FuncArgs,
) -> Option<bool> {
    if resolved.schema != "pg_catalog" {
        return None;
    }
    let value = |i: usize| args.nullable.get(i).copied();
    let elems = |i: usize| args.exprs.get(i).and_then(|e| e.elem_nullable);
    let is_null_const = |i: usize| {
        matches!(
            func.args.get(i).and_then(|a| a.node.as_ref()),
            Some(node::Node::AConst(c)) if c.isnull
        )
    };
    match resolved.signature.as_str() {
        // (`array_agg(anyarray)` builds a multidimensional array.)
        "array_agg(anynonarray)"
        | "array_fill(anyelement,_int4)"
        | "array_fill(anyelement,_int4,_int4)" => value(0),
        "array_append(anycompatiblearray,anycompatible)" => {
            merge_elem_nullable([elems(0), value(1)])
        }
        "array_prepend(anycompatible,anycompatiblearray)" => {
            merge_elem_nullable([value(0), elems(1)])
        }
        "array_cat(anycompatiblearray,anycompatiblearray)" => {
            merge_elem_nullable([elems(0), elems(1)])
        }
        "array_replace(anycompatiblearray,anycompatible,anycompatible)" => {
            merge_elem_nullable([elems(0), value(2)])
        }
        // Removing NULL removes every NULL element.
        "array_remove(anycompatiblearray,anycompatible)" if is_null_const(1) => Some(false),
        "array_remove(anycompatiblearray,anycompatible)"
        | "array_reverse(anyarray)"
        | "array_shuffle(anyarray)"
        | "array_sample(anyarray,int4)"
        | "array_sort(anyarray)"
        | "array_sort(anyarray,bool)"
        | "array_sort(anyarray,bool,bool)"
        | "trim_array(anyarray,int4)" => elems(0),
        // The fields equal to the null string become NULL.
        "string_to_array(text,text,text)" => Some(!is_null_const(2)),
        "string_to_array(text,text)"
        | "regexp_split_to_array(text,text)"
        | "regexp_split_to_array(text,text,text)"
        | "array_positions(anycompatiblearray,anycompatible)" => Some(false),
        // A capture group that doesn't take part in the match is NULL.
        "regexp_match(text,text)"
        | "regexp_match(text,text,text)"
        | "regexp_matches(text,text)"
        | "regexp_matches(text,text,text)" => Some(true),
        _ => None,
    }
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
                    &args.types,
                    notation,
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
/// locations and type-name spellings, or column references resolving to
/// the same column.
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
    crate::grouping::typed_fingerprint(a, ctx.scope, ctx.snapshot)
        == crate::grouping::typed_fingerprint(b, ctx.scope, ctx.snapshot)
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
                crate::error::expr_span(harg),
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
    let mut exprs = Vec::with_capacity(func.args.len());
    for arg in &func.args {
        let t = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
        any_nullable = any_nullable || t.nullable;
        nullable.push(t.nullable);
        types.push(t.type_oid);
        exprs.push(t);
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

    let coercion_nullable = vec![false; nullable.len()];
    Ok(FuncArgs {
        types,
        nullable,
        any_nullable,
        exprs,
        direct_count,
        coercion_nullable,
    })
}

/// Which call arguments coercing to their parameter types (`resolved`'s
/// `arg_types`, polymorphic ones resolved) can map a non-NULL value to
/// NULL: one whose implicit cast runs a cast function that can return NULL
/// (`color_to_text(color)` in `upper(color_col)`).
pub(crate) fn arg_coercions_nullable(
    actual: &[PgTypeOid],
    resolved: &functions::ResolvedFunction,
    snapshot: &PgCatalog,
) -> Vec<bool> {
    actual
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            resolved
                .arg_types
                .get(i)
                .is_some_and(|&d| super::coercion_can_return_null(a, d, snapshot))
        })
        .collect()
}

/// Apply the coercion of each argument to its parameter type to the
/// arguments' nullability, as the call sees them (PG's
/// `make_fn_arguments`): a value — or an array's element — the cast
/// function maps to NULL is a NULL argument.
fn coerce_call_args(
    args: &mut FuncArgs,
    resolved: &functions::ResolvedFunction,
    snapshot: &PgCatalog,
) {
    args.coercion_nullable = arg_coercions_nullable(&args.types, resolved, snapshot);
    for (i, &coerced) in args.coercion_nullable.iter().enumerate() {
        if coerced && let Some(n) = args.nullable.get_mut(i) {
            *n = true;
        }
    }
    for (e, &d) in args.exprs.iter_mut().zip(&resolved.arg_types) {
        e.note_coerced_to(d, snapshot);
    }
    args.any_nullable = args.nullable.iter().any(|&n| n);
}

/// An aggregate reads its arguments only over the rows its FILTER passes:
/// what the FILTER proves non-NULL is so in them (`array_agg(x) FILTER
/// (WHERE x IS NOT NULL)` has no NULL element). The arguments were typed
/// before the FILTER, as PG does; their nullability is read again under
/// its facts, on a scratch copy of the parameters (the types are settled).
fn narrow_by_filter(
    func: &protobuf::FuncCall,
    args: &mut FuncArgs,
    ctx: Ctx<'_>,
    params: &ParamCollector,
    log: &crate::nonnull::StrictLog,
) {
    let Some(filter) = func.agg_filter.as_deref() else {
        return;
    };
    let facts = crate::nonnull::nonnullable(filter, true, ctx.scope, log, ctx.snapshot)
        .restricted_to(&crate::nonnull::own_aliases(ctx.scope));
    let Some(narrowed) = ctx.null_ctx.with_local_facts(facts) else {
        return;
    };
    let narrowed_ctx = ctx.with_null_ctx(&narrowed);
    let mut scratch = params.clone();
    let sort_exprs = func.agg_order.iter().filter_map(|o| match o.node.as_ref() {
        Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
        _ => Some(o),
    });
    let inputs: Vec<&protobuf::Node> = if func.agg_within_group {
        func.args.iter().chain(sort_exprs).collect()
    } else {
        func.args.iter().collect()
    };
    for (i, arg) in inputs.into_iter().enumerate() {
        let Ok(t) = infer_expr(arg, narrowed_ctx, &mut scratch, TypeGoal::NONE) else {
            continue;
        };
        if let Some(n) = args.nullable.get_mut(i) {
            *n &= t.nullable;
        }
        if let Some(e) = args.exprs.get_mut(i) {
            e.nullable &= t.nullable;
            if let (Some(old), Some(new)) = (e.elem_nullable, t.elem_nullable) {
                e.elem_nullable = Some(old && new);
            }
        }
    }
    args.any_nullable = args.nullable.iter().any(|&n| n);
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
        // A GROUPING(…) inside an aggregate is an aggregate nesting too.
        if kinds.has_aggregate || kinds.has_grouping {
            return Err(crate::pgmsg::nested_aggregate(kinds.aggregate_span()).finalize_implicit());
        }
        if kinds.has_window {
            return Err(crate::pgmsg::window_in_aggregate(kinds.window_span()).finalize_implicit());
        }
    }
    Ok(())
}

/// [`backfill_func_args`] for a call analyzed outside [`infer_func_call`] —
/// a function in FROM (`transformRangeFunction` transforms it through the
/// same `ParseFuncOrColumn`, so `FROM generate_series(1, $1)` types `$1`
/// and validates `FROM generate_series(1, 'a')` exactly like the SELECT
/// list does). `arg_types` are the bottom-up types of `func.args`.
pub(crate) fn backfill_call_args(
    func: &protobuf::FuncCall,
    arg_types: &[PgTypeOid],
    resolved: &functions::ResolvedFunction,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let args = FuncArgs {
        types: arg_types.to_vec(),
        nullable: vec![false; arg_types.len()],
        any_nullable: false,
        exprs: Vec::new(),
        direct_count: arg_types.len(),
        coercion_nullable: vec![false; arg_types.len()],
    };
    backfill_func_args(func, &args, resolved, ctx, params)
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
    filter_log: &crate::nonnull::StrictLog,
) -> Result<(), AnalyzeError> {
    if let Some(filter) = &func.agg_filter {
        // FILTER is a boolean clause like WHERE — wording and ordering live
        // in the shared clause walker.
        crate::clause::coerce_clause_expr(
            filter,
            ctx.logging_strictness(filter_log),
            params,
            crate::clause::ClauseKind::Filter,
        )?;
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
    // An inline window (`OVER (…)`) is a window definition of its own; one
    // inheriting from a named window (`OVER (w …)`) is checked against it
    // with the SELECT's WINDOW clause ([`check_window_clause`]). `OVER w`
    // defines nothing.
    if let Some(over) = &func.over
        && over.name.is_empty()
    {
        if over.refname.is_empty() {
            transform_window_def(over, None, ctx, params)?;
        } else {
            infer_window_exprs(over, ctx, params)?;
        }
    }
    Ok(())
}

/// `EXTRACT(field FROM ts)` / `date_part('field', ts)` over a timestamp or
/// date is NULL only for an infinite input with a field that has no
/// infinite value (`month`, `day`, …); the fields PG's
/// `NonFiniteTimestampTzPart` maps to ±Infinity (`year`, `epoch`, … in any
/// of `datetime.c`'s spellings) never are. Over an interval the same holds
/// with `NonFiniteIntervalPart`'s fields: `epoch`, `hour`, `day`, `year`,
/// `decade`, `century` and `millennium` are ±Infinity for an infinite
/// interval, the others (`second`, `minute`, `week`, `month`, …) NULL
/// (checked spelling by spelling on PostgreSQL 18). With such a literal
/// field the call is NULL exactly when an argument is.
fn extract_unit_is_infinite_safe(
    func: &protobuf::FuncCall,
    resolved: &functions::ResolvedFunction,
) -> bool {
    const INTERVAL_INFINITE_FIELDS: &[&str] = &[
        "epoch",
        "h",
        "hour",
        "hours",
        "hr",
        "hrs",
        "d",
        "day",
        "days",
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
    let fields = match resolved.signature.as_str() {
        "extract(text,timestamp)"
        | "extract(text,timestamptz)"
        | "extract(text,date)"
        | "date_part(text,timestamp)"
        | "date_part(text,timestamptz)"
        | "date_part(text,date)" => INFINITE_FIELDS,
        "extract(text,interval)" | "date_part(text,interval)" => INTERVAL_INFINITE_FIELDS,
        _ => return false,
    };
    matches!(
        func.args.first().and_then(|a| a.node.as_ref()),
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(typedpg_pg_query::protobuf::a_const::Val::Sval(sv)),
            ..
        })) if fields.contains(&sv.sval.to_lowercase().as_str())
    )
}

/// Whether `node` (inferred as `t`) is a non-NULL `array_agg` of
/// non-array values: then it read rows, and `array_agg_finalfn` built a
/// one-dimensional array with lower bound 1 holding one element per row —
/// `[1]` is the first value, `array_length(…, 1)` is defined. (Over
/// arrays, `array_agg(anyarray)` stacks a dimension: `[1]` is NULL.)
pub(crate) fn is_nonempty_1d_array_agg(
    node: &protobuf::Node,
    t: &ExprType,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> bool {
    let Some(node::Node::FuncCall(fc)) = node.node.as_ref() else {
        return false;
    };
    if t.nullable
        || crate::having::builtin_aggregate_name(fc, ctx.snapshot) != Some("array_agg")
        || fc.args.len() != 1
    {
        return false;
    }
    let mut scratch = params.clone();
    infer_expr(&fc.args[0], ctx, &mut scratch, TypeGoal::NONE).is_ok_and(|a| {
        a.type_oid != oid::UNKNOWN
            && coerce::element_type(ctx.snapshot.unwrap_domain(a.type_oid), ctx.snapshot).is_none()
    })
}

/// Whether an aggregate always reads rows: with no FILTER (which may pass
/// none), a window frame holding the current row, or a group — every
/// GROUP BY group has rows, the empty grouping set's has them when the
/// input does (`input_not_empty`: a constant source, or HAVING proving it).
/// Every function the call's name may resolve to is an aggregate.
fn only_aggregates_named(func: &protobuf::FuncCall, snapshot: &PgCatalog) -> bool {
    let parts = extract_string_fields(&func.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return false,
    };
    let candidates = snapshot.find_functions(schema, name);
    !candidates.is_empty()
        && candidates
            .iter()
            .all(|p| matches!(p.prokind, crate::pg_catalog::ProKind::Aggregate))
}

pub(crate) fn aggregate_reads_rows(
    has_filter: bool,
    over: Option<&protobuf::WindowDef>,
    null_ctx: &NullabilityContext,
) -> bool {
    if has_filter {
        return false;
    }
    match over {
        // `ROWS … 1 PRECEDING`, `… FOLLOWING`-only frames and `EXCLUDE
        // CURRENT ROW / GROUP` can leave a frame empty.
        Some(over) => window_frame(over, null_ctx).is_some_and(frame_contains_current_row),
        None => {
            null_ctx.input_not_empty || (null_ctx.has_group_by && !null_ctx.has_empty_grouping_set)
        }
    }
}

/// The frame options a window call runs over (see
/// [`effective_frame_options`]): its own, or — for `OVER w` — those of
/// the WINDOW clause's `w` (`None` when unknown).
fn window_frame(over: &protobuf::WindowDef, null_ctx: &NullabilityContext) -> Option<i32> {
    if over.name.is_empty() {
        Some(effective_frame_options(over))
    } else {
        null_ctx.window_frame_options(over)
    }
}

/// A window definition's frame options with a constant zero offset bound
/// (`0 PRECEDING`, `0 FOLLOWING`, `INTERVAL '0 day' PRECEDING`) read as
/// CURRENT ROW, which it is in every mode: the current row (ROWS), its
/// peer group (GROUPS), the rows whose key is the current one's (RANGE).
pub(crate) fn effective_frame_options(wd: &protobuf::WindowDef) -> i32 {
    const START_CURRENT_ROW: i32 = 0x200;
    const END_CURRENT_ROW: i32 = 0x400;
    const START_OFFSET: i32 = 0x800 | 0x2000;
    const END_OFFSET: i32 = 0x1000 | 0x4000;
    let mut options = wd.frame_options;
    if options & START_OFFSET != 0 && wd.start_offset.as_deref().is_some_and(is_zero_offset) {
        options = (options & !START_OFFSET) | START_CURRENT_ROW;
    }
    if options & END_OFFSET != 0 && wd.end_offset.as_deref().is_some_and(is_zero_offset) {
        options = (options & !END_OFFSET) | END_CURRENT_ROW;
    }
    options
}

/// The integer literal `0`.
fn is_zero_constant(n: &protobuf::Node) -> bool {
    matches!(
        n.node.as_ref(),
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)),
            ..
        })) if i.ival == 0
    )
}

/// A frame offset that is the constant zero: `0`, `0.0`, or a literal
/// such as `'0'` / `'0 day'` cast to the offset type.
fn is_zero_offset(n: &protobuf::Node) -> bool {
    use typedpg_pg_query::protobuf::a_const::Val;
    match n.node.as_ref() {
        Some(node::Node::AConst(c)) if !c.isnull => match &c.val {
            Some(Val::Ival(i)) => i.ival == 0,
            Some(Val::Fval(f)) => is_zero_decimal(&f.fval),
            Some(Val::Sval(s)) => is_zero_string(&s.sval),
            _ => false,
        },
        Some(node::Node::TypeCast(c)) => c.arg.as_deref().is_some_and(is_zero_offset),
        _ => false,
    }
}

/// Plain decimal digits with at most one point, all zeros (`0`, `00`,
/// `0.`, `.0`).
fn is_zero_decimal(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit())
        && s.chars().all(|c| c == '0' || c == '.')
        && s.matches('.').count() <= 1
}

/// A string literal every offset type reads as zero: a zero quantity
/// (`'0'`, `' 0.0 '`), or for an interval one followed, after a space, by
/// a unit (`'0 day'`, `'0 hours'`). A unit glued to the number is refused:
/// `int8` reads `'0xa'` as 10, `'0o7'` as 7 and `'0b1'` as 1.
fn is_zero_string(s: &str) -> bool {
    /// `datetime.c`'s `deltatktbl` units (any of them over zero is zero).
    const UNITS: &[&str] = &[
        "c",
        "cent",
        "centuries",
        "century",
        "d",
        "day",
        "days",
        "dec",
        "decade",
        "decades",
        "decs",
        "h",
        "hour",
        "hours",
        "hr",
        "hrs",
        "m",
        "microsecon",
        "microsecond",
        "microseconds",
        "mil",
        "millennia",
        "millennium",
        "millisecon",
        "millisecond",
        "milliseconds",
        "mils",
        "min",
        "mins",
        "minute",
        "minutes",
        "mon",
        "mons",
        "month",
        "months",
        "ms",
        "msec",
        "msecs",
        "s",
        "sec",
        "second",
        "seconds",
        "secs",
        "us",
        "usec",
        "usecs",
        "w",
        "week",
        "weeks",
        "y",
        "year",
        "years",
        "yr",
        "yrs",
    ];
    let mut words = s.split_ascii_whitespace();
    let (Some(num), unit, None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    is_zero_decimal(num) && unit.is_none_or(|u| UNITS.contains(&u.to_ascii_lowercase().as_str()))
}

/// Built-ins whose only NULL results (beyond NULL arguments) depend on
/// argument values the call spells out — so that, with those arguments as
/// written, the call is NULL exactly when an argument is:
///
/// - `jsonb_path_exists[_tz]` is NULL for a path error only when `silent`
///   (jsonpath_exec.c): without it, or with a literal `false`, errors are
///   raised instead;
/// - `to_char(timestamp[tz] | interval, fmt)` is NULL for an empty format
///   or a non-finite value: a non-empty literal format over a source that
///   is always finite (`now()`, `CURRENT_TIMESTAMP`, a literal interval
///   other than infinity);
/// - `extract` / `date_part` over such a finite timestamp or date has no
///   infinite input to be NULL for;
/// - `array_length` / `array_lower` / `array_upper` (dimension literal 1),
///   `array_ndims` and `array_dims` of a non-empty `ARRAY[…]` of scalars,
///   a one-dimensional array.
fn value_gated_non_null(
    func: &protobuf::FuncCall,
    resolved: &functions::ResolvedFunction,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> bool {
    let proname = resolved.signature.split('(').next().unwrap_or_default();
    // (In positional notation: see [`in_declared_order`].)
    let positional: Vec<&protobuf::Node> = func.args.iter().collect();
    let declared_types = resolved.in_declared_order(&resolved.arg_types, oid::UNKNOWN);
    let first_type = declared_types.first().copied();
    match proname {
        "jsonb_path_exists" | "jsonb_path_exists_tz" => {
            let silent = positional.get(3).copied();
            silent.is_none_or(|s| {
                matches!(
                    s.node.as_ref(),
                    Some(node::Node::AConst(protobuf::AConst {
                        isnull: false,
                        val: Some(typedpg_pg_query::protobuf::a_const::Val::Boolval(b)),
                        ..
                    })) if !b.boolval
                )
            })
        }
        "to_char" if func.args.len() == 2 && positional.len() == 2 => {
            let fmt_non_empty = matches!(
                positional[1].node.as_ref(),
                Some(node::Node::AConst(protobuf::AConst {
                    isnull: false,
                    val: Some(typedpg_pg_query::protobuf::a_const::Val::Sval(s)),
                    ..
                })) if !s.sval.is_empty()
            );
            let source_finite = match first_type {
                Some(t) if t == oid::TIMESTAMP || t == oid::TIMESTAMPTZ => {
                    finite_time_source(positional[0], ctx)
                }
                Some(t) if t == INTERVAL => finite_interval_literal(positional[0], ctx),
                _ => false,
            };
            fmt_non_empty && source_finite
        }
        "extract" | "date_part" if func.args.len() == 2 && positional.len() == 2 => {
            matches!(first_type, Some(t) if t == oid::TEXT)
                && declared_types.get(1).is_some_and(|&t| {
                    t == oid::TIMESTAMP || t == oid::TIMESTAMPTZ || t == oid::DATE
                })
                && finite_time_source(positional[1], ctx)
        }
        "array_length" | "array_lower" | "array_upper"
            if func.args.len() == 2 && positional.len() == 2 =>
        {
            matches!(
                positional[1].node.as_ref(),
                Some(node::Node::AConst(protobuf::AConst {
                    isnull: false,
                    val: Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)),
                    ..
                })) if i.ival == 1
            ) && scalar_array_constructor(positional[0], ctx, params)
        }
        "array_ndims" | "array_dims" if func.args.len() == 1 && positional.len() == 1 => {
            scalar_array_constructor(positional[0], ctx, params)
        }
        _ => false,
    }
}

/// PG's `interval` type.
const INTERVAL: PgTypeOid = PgTypeOid::from_raw(1186);

/// A timestamp source that is never infinite: the current time.
fn finite_time_source(n: &protobuf::Node, ctx: Ctx<'_>) -> bool {
    use protobuf::SqlValueFunctionOp as Op;
    match n.node.as_ref() {
        Some(node::Node::SqlvalueFunction(f)) => matches!(
            Op::try_from(f.op),
            Ok(Op::SvfopCurrentDate
                | Op::SvfopCurrentTimestamp
                | Op::SvfopCurrentTimestampN
                | Op::SvfopLocaltimestamp
                | Op::SvfopLocaltimestampN)
        ),
        Some(node::Node::FuncCall(f)) if f.args.is_empty() && f.over.is_none() => {
            let parts = extract_string_fields(&f.funcname);
            let (schema, name) = match parts.as_slice() {
                [n] => (None, n.as_str()),
                [s, n] if s == "pg_catalog" => (Some("pg_catalog"), n.as_str()),
                _ => return false,
            };
            let candidates = ctx.snapshot.find_functions(schema, name);
            matches!(
                name,
                "now" | "statement_timestamp" | "transaction_timestamp" | "clock_timestamp"
            ) && !candidates.is_empty()
                && candidates
                    .iter()
                    .all(|p| ctx.snapshot.namespace_name(p.pronamespace) == Some("pg_catalog"))
        }
        _ => false,
    }
}

/// An interval literal that isn't (minus) infinity.
fn finite_interval_literal(n: &protobuf::Node, ctx: Ctx<'_>) -> bool {
    let Some(node::Node::TypeCast(tc)) = n.node.as_ref() else {
        return false;
    };
    let is_interval = tc
        .type_name
        .as_ref()
        .and_then(|t| crate::ddl::util::resolve_type_name(t, ctx.snapshot))
        == Some(INTERVAL);
    is_interval
        && matches!(
            tc.arg.as_deref().and_then(|a| a.node.as_ref()),
            Some(node::Node::AConst(protobuf::AConst {
                isnull: false,
                val: Some(typedpg_pg_query::protobuf::a_const::Val::Sval(s)),
                ..
            })) if !s.sval.to_ascii_lowercase().contains("infinity")
        )
}

/// `ARRAY[e1, …, en]` (n ≥ 1, possibly cast to an array type) over
/// scalar elements: a one-dimensional array of n elements. The elements
/// are re-inferred on a throwaway collector.
fn scalar_array_constructor(n: &protobuf::Node, ctx: Ctx<'_>, params: &ParamCollector) -> bool {
    match n.node.as_ref() {
        Some(node::Node::TypeCast(tc)) => tc
            .arg
            .as_deref()
            .is_some_and(|a| scalar_array_constructor(a, ctx, params)),
        Some(node::Node::AArrayExpr(arr)) => {
            !arr.elements.is_empty()
                && arr.elements.iter().all(|e| {
                    !matches!(e.node.as_ref(), Some(node::Node::AArrayExpr(_)))
                        && infer_expr(e, ctx, &mut params.clone(), TypeGoal::NONE).is_ok_and(|t| {
                            t.type_oid != oid::UNKNOWN
                                && array_element_type(ctx.snapshot, t.type_oid).is_none()
                        })
                })
        }
        _ => false,
    }
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

// ──────────────────────────────────────────────────────────────────────────────
// Window definitions (PG's transformWindowDefinitions / transformFrameOffset)
// ──────────────────────────────────────────────────────────────────────────────

/// `FRAMEOPTION_*` bits (parsenodes.h).
const FRAME_RANGE: i32 = 0x2;
const FRAME_ROWS: i32 = 0x4;
const FRAME_GROUPS: i32 = 0x8;
const FRAME_OFFSET: i32 = 0x800 | 0x1000 | 0x2000 | 0x4000;
/// `FRAMEOPTION_DEFAULTS`: RANGE UNBOUNDED PRECEDING … CURRENT ROW, what
/// the grammar stores when no frame clause is written.
const FRAME_DEFAULTS: i32 = 0x2 | 0x20 | 0x400;

/// The expressions of a window's `ORDER BY` items.
fn window_order_exprs(wd: &protobuf::WindowDef) -> Vec<&protobuf::Node> {
    wd.order_clause
        .iter()
        .filter_map(|o| match o.node.as_ref() {
            Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
            _ => None,
        })
        .collect()
}

/// Infer a window definition's PARTITION BY / ORDER BY expressions, so
/// parameters are typed and column references validated.
fn infer_window_exprs(
    wd: &protobuf::WindowDef,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for e in wd.partition_clause.iter().chain(window_order_exprs(wd)) {
        infer_expr(e, ctx, params, TypeGoal::NONE)?;
    }
    Ok(())
}

/// PG's per-window checks in `transformWindowDefinitions` plus
/// `transformFrameOffset` (parse_clause.c), for a window definition `wd`
/// — a WINDOW clause entry or an inline `OVER (…)` — optionally inheriting
/// from the named window `base` (`OVER (w …)`, `WINDOW w2 AS (w …)`):
///
/// - no window functions inside it (42P20);
/// - inheriting copies `base`'s PARTITION BY (it can't add one), may add
///   an ORDER BY only if `base` has none, and `base` must have no frame
///   clause (42P20);
/// - a RANGE frame with an offset needs exactly one ORDER BY column (42P20)
///   whose type has `in_range` support for the offset's type (0A000) — the
///   offset is coerced to that support function's offset type, which is
///   what types `$1` in `RANGE $1 PRECEDING` (integer for an integer key,
///   interval for a timestamp);
/// - GROUPS needs an ORDER BY (42P20); ROWS / GROUPS offsets are bigint;
/// - no offset may reference a column (42P10).
fn transform_window_def(
    wd: &protobuf::WindowDef,
    base: Option<&protobuf::WindowDef>,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    use crate::pgmsg;
    let span = crate::error::SourceSpan::from_node_qname(wd.location);
    let fail = |e: crate::error::RawError| Err(e.finalize_implicit());
    let offsets: Vec<&protobuf::Node> = wd
        .start_offset
        .as_deref()
        .into_iter()
        .chain(wd.end_offset.as_deref())
        .collect();
    let own_exprs = wd
        .partition_clause
        .iter()
        .chain(window_order_exprs(wd))
        .chain(offsets.iter().copied());
    for e in own_exprs {
        if detect_func_kinds(e, ctx.snapshot).has_window {
            return fail(pgmsg::window_in_window_definition(span));
        }
    }
    infer_window_exprs(wd, ctx, params)?;

    let mut order = window_order_exprs(wd);
    if let Some(base) = base {
        if !wd.partition_clause.is_empty() {
            return fail(pgmsg::cannot_override_window_clause(
                "PARTITION BY",
                &wd.refname,
                span,
            ));
        }
        if !order.is_empty() && !base.order_clause.is_empty() {
            return fail(pgmsg::cannot_override_window_clause(
                "ORDER BY",
                &wd.refname,
                span,
            ));
        }
        if base.frame_options != FRAME_DEFAULTS {
            // A bare `OVER (w)` gets a hint to drop the parentheses.
            let bare_over =
                wd.name.is_empty() && order.is_empty() && wd.frame_options == FRAME_DEFAULTS;
            return fail(pgmsg::cannot_copy_window_with_frame(
                &wd.refname,
                bare_over,
                span,
            ));
        }
        if order.is_empty() {
            order = window_order_exprs(base);
        }
    }

    let options = wd.frame_options;
    let mut range_key: Option<PgTypeOid> = None;
    if options & FRAME_RANGE != 0 && options & FRAME_OFFSET != 0 {
        let [key] = order.as_slice() else {
            return fail(pgmsg::range_offset_needs_one_order_by(span));
        };
        range_key = Some(infer_expr(key, ctx, params, TypeGoal::NONE)?.type_oid);
    }
    if options & FRAME_GROUPS != 0 && order.is_empty() {
        return fail(pgmsg::groups_needs_order_by(span));
    }

    for offset in offsets {
        let construct = if options & FRAME_ROWS != 0 {
            "ROWS"
        } else if options & FRAME_GROUPS != 0 {
            "GROUPS"
        } else {
            "RANGE"
        };
        match range_key {
            Some(key) if construct == "RANGE" => {
                let actual = infer_expr(offset, ctx, params, TypeGoal::NONE)?.type_oid;
                crate::clause::check_level_calls(offset, ctx, "window RANGE", true)?;
                let target = in_range_offset_type(key, actual, offset, ctx)?;
                if actual == oid::UNKNOWN {
                    coerce_unknown_to(offset, ctx, params, target)?;
                }
            }
            // ROWS / GROUPS: coerce_to_specific_type(…, INT8OID, …), an
            // assignment coercion with the construct's wording.
            _ => {
                let mode = if construct == "ROWS" {
                    "ROWS"
                } else {
                    "GROUPS"
                };
                crate::clause::coerce_clause_expr(
                    offset,
                    ctx,
                    params,
                    crate::clause::ClauseKind::FrameOffset(mode),
                )?;
            }
        }
        if contains_column_ref(offset) {
            return fail(pgmsg::frame_offset_has_variables(
                construct,
                crate::error::node_location(offset)
                    .and_then(crate::error::SourceSpan::from_node_qname),
            ));
        }
    }
    Ok(())
}

/// `transformFrameOffset`'s RANGE branch: among the btree `in_range`
/// support functions for the sort key's opclass input type (in
/// `pg_catalog`, each `in_range(key, key, offset, bool, bool)` is one), keep
/// those whose offset type the offset coerces to implicitly and pick one —
/// preferring the offset's own type, or the key's type for an unknown
/// offset.
///
/// PG takes that input type (`opcintype`) from the ORDER BY's sort operator
/// and also names it in its errors; without `pg_opclass` it is the declared
/// left operand of the key's `<` operator — `text` for a `varchar` or a
/// domain over it, `anyarray` for an array.
fn in_range_offset_type(
    key: PgTypeOid,
    offset_type: PgTypeOid,
    offset: &protobuf::Node,
    ctx: Ctx<'_>,
) -> Result<PgTypeOid, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let key = snapshot.unwrap_domain(key);
    let key = snapshot
        .find_operator("<", Some(key), key)
        .and_then(|op| op.declared_left_type_oid)
        .unwrap_or(key);
    let span =
        crate::error::node_location(offset).and_then(crate::error::SourceSpan::from_node_qname);
    let fmt = |t| crate::ddl::util::format_type_for_message(snapshot, t);
    let candidates: Vec<PgTypeOid> = snapshot
        .find_functions(Some("pg_catalog"), "in_range")
        .iter()
        .filter(|f| f.proargtypes.len() == 5 && f.proargtypes[0] == key)
        .map(|f| f.proargtypes[2])
        .collect();
    if candidates.is_empty() {
        return Err(
            crate::pgmsg::range_offset_unsupported(&fmt(key), None, span).finalize_implicit(),
        );
    }
    let preferred = if offset_type == oid::UNKNOWN {
        key
    } else {
        offset_type
    };
    let mut selected: Option<PgTypeOid> = None;
    let mut matches = 0;
    for t in candidates {
        if !crate::coerce::can_coerce_types(&[offset_type], &[t], snapshot) {
            continue;
        }
        matches += 1;
        if selected != Some(preferred) {
            selected = Some(t);
        }
    }
    match selected {
        None => {
            Err(
                crate::pgmsg::range_offset_unsupported(&fmt(key), Some(&fmt(offset_type)), span)
                    .finalize_implicit(),
            )
        }
        Some(t) if matches != 1 && t != preferred => {
            Err(
                crate::pgmsg::range_offset_ambiguous(&fmt(key), &fmt(offset_type), span)
                    .finalize_implicit(),
            )
        }
        Some(t) => Ok(t),
    }
}

/// Whether an expression references a column of the current query (PG's
/// `contain_vars_of_level(…, 0)` as used by `checkExprIsVarFree`).
/// Subqueries are not descended into.
fn contains_column_ref(node: &protobuf::Node) -> bool {
    let Some(inner) = node.node.as_ref() else {
        return false;
    };
    let any = |nodes: &[protobuf::Node]| nodes.iter().any(contains_column_ref);
    let opt = |n: &Option<Box<protobuf::Node>>| n.as_deref().is_some_and(contains_column_ref);
    match inner {
        node::Node::ColumnRef(_) => true,
        node::Node::AExpr(e) => opt(&e.lexpr) || opt(&e.rexpr),
        node::Node::FuncCall(f) => any(&f.args),
        node::Node::TypeCast(c) => opt(&c.arg),
        node::Node::BoolExpr(b) => any(&b.args),
        node::Node::CoalesceExpr(c) => any(&c.args),
        node::Node::MinMaxExpr(m) => any(&m.args),
        node::Node::NullTest(t) => opt(&t.arg),
        node::Node::CaseExpr(c) => opt(&c.arg) || any(&c.args) || opt(&c.defresult),
        node::Node::CaseWhen(w) => opt(&w.expr) || opt(&w.result),
        node::Node::AArrayExpr(a) => any(&a.elements),
        node::Node::RowExpr(r) => any(&r.args),
        node::Node::AIndirection(i) => opt(&i.arg),
        node::Node::List(l) => any(&l.items),
        _ => false,
    }
}

/// PG's `transformWindowDefinitions` over a SELECT's WINDOW clause and
/// the inline windows inheriting from it: WINDOW entries are checked in
/// order — a name may be defined once (42P20), and a reference must name
/// an *earlier* entry (42704) — then every `OVER (w …)` in the target list
/// and ORDER BY is checked against `w` (see [`transform_window_def`]).
pub(crate) fn check_window_clause(
    sel: &protobuf::SelectStmt,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let defs: Vec<&protobuf::WindowDef> = sel
        .window_clause
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::WindowDef(w)) => Some(&**w),
            _ => None,
        })
        .collect();
    for (i, wd) in defs.iter().enumerate() {
        let earlier = &defs[..i];
        let span = crate::error::SourceSpan::from_node_qname(wd.location);
        if earlier.iter().any(|d| d.name == wd.name) {
            return Err(crate::pgmsg::window_already_defined(&wd.name, span).finalize_implicit());
        }
        let base = if wd.refname.is_empty() {
            None
        } else {
            Some(
                *earlier
                    .iter()
                    .find(|d| d.name == wd.refname)
                    .ok_or_else(|| {
                        crate::pgmsg::window_does_not_exist(&wd.refname, span).finalize_implicit()
                    })?,
            )
        };
        transform_window_def(wd, base, ctx, params)?;
    }

    let mut inline: Vec<&protobuf::WindowDef> = Vec::new();
    for t in &sel.target_list {
        if let Some(node::Node::ResTarget(rt)) = t.node.as_ref()
            && let Some(val) = rt.val.as_deref()
        {
            collect_inheriting_windows(val, &mut inline);
        }
    }
    for s in &sel.sort_clause {
        if let Some(node::Node::SortBy(sb)) = s.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
        {
            collect_inheriting_windows(inner, &mut inline);
        }
    }
    for wd in inline {
        if let Some(base) = defs.iter().find(|d| d.name == wd.refname) {
            transform_window_def(wd, Some(base), ctx, params)?;
        }
    }
    Ok(())
}

/// Inline `OVER (w …)` windows of the window calls in an expression
/// (subqueries excluded — their windows belong to the inner query).
fn collect_inheriting_windows<'a>(
    node: &'a protobuf::Node,
    out: &mut Vec<&'a protobuf::WindowDef>,
) {
    let Some(inner) = node.node.as_ref() else {
        return;
    };
    fn each<'a>(nodes: &'a [protobuf::Node], out: &mut Vec<&'a protobuf::WindowDef>) {
        for n in nodes {
            collect_inheriting_windows(n, out);
        }
    }
    match inner {
        node::Node::FuncCall(f) => {
            if let Some(over) = f.over.as_deref()
                && over.name.is_empty()
                && !over.refname.is_empty()
            {
                out.push(over);
            }
            each(&f.args, out);
        }
        node::Node::AExpr(e) => {
            for n in [&e.lexpr, &e.rexpr].into_iter().flatten() {
                collect_inheriting_windows(n, out);
            }
        }
        node::Node::TypeCast(c) => {
            if let Some(a) = &c.arg {
                collect_inheriting_windows(a, out);
            }
        }
        node::Node::BoolExpr(b) => each(&b.args, out),
        node::Node::CoalesceExpr(c) => each(&c.args, out),
        node::Node::CaseExpr(c) => {
            each(&c.args, out);
            if let Some(d) = &c.defresult {
                collect_inheriting_windows(d, out);
            }
        }
        node::Node::CaseWhen(w) => {
            for n in [&w.expr, &w.result].into_iter().flatten() {
                collect_inheriting_windows(n, out);
            }
        }
        _ => {}
    }
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
    ctx: Ctx<'_>,
    params: &ParamCollector,
    args: &FuncArgs,
) -> bool {
    let null_ctx = ctx.null_ctx;
    let arg_is_nullable = |i: usize| args.nullable.get(i).copied().unwrap_or(false);

    // Value window functions (`lag`/`lead`/`first_value`/`last_value`/
    // `nth_value`) can return NULL at partition/frame edges even when the
    // source column is NOT NULL — `lag(title) OVER (ORDER BY id)` produces
    // NULL for the first row of each partition. A 3-arg `lag(col, offset,
    // default)`/`lead(...)` replaces the boundary NULL with `default`, so
    // the result is only nullable when the value, the offset (a NULL
    // offset gives NULL) or the default are. Only the `pg_catalog` window
    // functions behave so: an aggregate named `lag` or `first_value` used
    // with OVER is whatever its definition makes it.
    let is_value_window = func.over.is_some()
        && resolved.is_window
        && resolved.schema == "pg_catalog"
        && matches!(
            name,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        );

    if resolved.is_set_returning && null_ctx.srfs_in_lockstep {
        // Several select-list SRFs run in lockstep; the shorter ones are
        // padded with NULL (see `NullabilityContext::srfs_in_lockstep`).
        true
    } else if let Some(nullable) =
        crate::resolve::srf_elements_nullable(resolved, name, &func.args, ctx, params)
    {
        nullable
    } else if is_value_window {
        let frame_has_current_row = || {
            func.over
                .as_deref()
                .and_then(|over| window_frame(over, null_ctx))
                .is_some_and(frame_contains_current_row)
        };
        match name {
            // An offset of 0 is the current row itself, whatever the
            // default (`WinGetFuncArgInPartition` at relpos 0).
            "lag" | "lead" if func.args.get(1).is_some_and(is_zero_constant) => arg_is_nullable(0),
            "lag" | "lead" if func.args.len() >= 3 => {
                arg_is_nullable(0) || arg_is_nullable(1) || arg_is_nullable(2)
            }
            // The first / last row of the frame: one exists whenever the
            // frame holds the current row — and so does `nth_value(x, 1)`.
            "first_value" | "last_value" => arg_is_nullable(0) || !frame_has_current_row(),
            "nth_value"
                if func.args.get(1).is_some_and(|n| {
                    matches!(
                        n.node.as_ref(),
                        Some(node::Node::AConst(protobuf::AConst {
                            val: Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)),
                            ..
                        })) if i.ival == 1
                    )
                }) =>
            {
                arg_is_nullable(0) || !frame_has_current_row()
            }
            _ => true,
        }
    } else if resolved.is_aggregate {
        let builtin = resolved.schema == "pg_catalog";
        let class = crate::having::aggregate_class(name);
        // The arguments whose values the aggregate reads: an ordered-set
        // aggregate's ordering values (its direct arguments are fixed per
        // group), and `string_agg`'s value (a NULL delimiter appends
        // nothing).
        let value_args: Vec<usize> = if func.agg_within_group {
            (args.direct_count..args.nullable.len()).collect()
        } else if builtin && name == "string_agg" {
            vec![0]
        } else {
            (0..args.nullable.len()).collect()
        };
        let direct_nullable = func.agg_within_group && (0..args.direct_count).any(arg_is_nullable);
        // A column HAVING proves non-NULL in some row of the group, read
        // by a plain aggregate (no FILTER, no window: all the group's rows).
        let plain = func.agg_filter.is_none() && func.over.is_none();
        let proven = |i: usize| {
            let node = if i < args.direct_count {
                func.args.get(i).map(functions::call_arg_value)
            } else {
                func.agg_order
                    .get(i - args.direct_count)
                    .and_then(|o| match o.node.as_ref() {
                        Some(node::Node::SortBy(sb)) => sb.node.as_deref(),
                        _ => None,
                    })
            };
            plain
                && builtin
                && class == crate::having::AggregateClass::Strict
                && node
                    .and_then(|n| crate::having::column_of(n, ctx.scope))
                    .is_some_and(|c| null_ctx.nonnull_agg_inputs.contains(&c))
        };
        // A column proven non-NULL in some row is still a NULL value there
        // when its coercion to the parameter type can make it one.
        let proven_value =
            |i: usize| proven(i) && !args.coercion_nullable.get(i).copied().unwrap_or(false);
        // A row whose every value is non-NULL — one a strict transition
        // function reads. HAVING proves each column present in *some* row
        // of the group: for an aggregate of several values (`regr_sxx(b,
        // c)`, `covar_pop(b, c)`), `count(b) > 0 AND count(c) > 0` holds of
        // a group whose rows are (1, NULL) and (NULL, 1). So a proof counts
        // only when the other values are never NULL.
        let proven_row = value_args
            .iter()
            .any(|&i| proven_value(i) && value_args.iter().all(|&j| j == i || !arg_is_nullable(j)));
        // An aggregate over a non-empty set of rows: NULL only when every
        // value it reads is NULL (for the strict ones), never for those
        // keeping NULL inputs (`array_agg`, `json_agg`, …) — except for
        // the builtins that are NULL for some non-empty inputs (a single
        // row, zero variance) and user-defined aggregates, whose
        // final/transition functions may return NULL at will.
        let over_rows = || {
            !builtin
                || crate::builtin_nullability::NULLABLE_AGGREGATES_OVER_ROWS.contains(&name)
                || match class {
                    crate::having::AggregateClass::Strict => {
                        direct_nullable
                            || (value_args.iter().any(|&i| arg_is_nullable(i)) && !proven_row)
                    }
                    _ => false,
                }
        };
        let hypothetical = matches!(
            resolved.aggregate,
            Some((crate::pg_catalog::AggKind::Hypothetical, _))
        );
        if builtin && (class == crate::having::AggregateClass::Count || hypothetical) {
            // COUNT / regr_count are 0 over no rows (even with FILTER); a
            // hypothetical-set aggregate ranks its hypothetical row among
            // whatever rows there are, a NULL one included.
            false
        } else {
            // A proven-present value is a row. An aggregate of an outer
            // level (over its columns only: `(SELECT max(t.h))`) reads
            // that level's rows, which this level knows nothing of.
            let rows =
                func.over.is_some() || crate::grouping::aggregate_levels_up(func, ctx.scope) == 0;
            let rows = rows
                && (aggregate_reads_rows(
                    func.agg_filter.is_some(),
                    func.over.as_deref(),
                    null_ctx,
                ) || value_args.iter().any(|&i| proven(i)));
            !rows || over_rows()
        }
    } else if resolved.schema == "pg_catalog"
        && (extract_unit_is_infinite_safe(func, resolved)
            || value_gated_non_null(func, resolved, ctx, params))
    {
        args.any_nullable
    } else if resolved.schema == "pg_catalog"
        && matches!(
            resolved.signature.as_str(),
            "array_length(anyarray,int4)"
                | "array_lower(anyarray,int4)"
                | "array_upper(anyarray,int4)"
        )
        && func.args.get(1).is_some_and(|n| {
            matches!(
                n.node.as_ref(),
                Some(node::Node::AConst(protobuf::AConst {
                    val: Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)),
                    ..
                })) if i.ival == 1
            )
        })
        && args
            .exprs
            .first()
            .is_some_and(|t| is_nonempty_1d_array_agg(&func.args[0], t, ctx, params))
    {
        // The one dimension of a non-empty `array_agg`.
        false
    } else if resolved.schema == "pg_catalog" {
        functions::builtin_result_nullable(resolved, &args.nullable, func.func_variadic)
    } else {
        // A user-defined function can return NULL whatever its inputs.
        true
    }
}
