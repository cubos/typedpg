//! SQL/JSON expressions (PG 16/17): `JSON_VALUE` / `JSON_QUERY` /
//! `JSON_EXISTS`, `JSON()` / `JSON_SCALAR` / `JSON_SERIALIZE`, the
//! `JSON_OBJECT` / `JSON_ARRAY` constructors, the `JSON_OBJECTAGG` /
//! `JSON_ARRAYAGG` aggregates and the `IS JSON` predicate.
//!
//! Each arm ports the matching `transformJson*` routine of PG's
//! `parse_expr.c`: the input coercions (`transformJsonValueExpr`), the
//! `RETURNING` clause (`transformJsonOutput`), the default result types and
//! the parse-time errors, with PG's wording.

use super::*;

const JSON: PgTypeOid = PgTypeOid::from_raw(114);
const JSONB: PgTypeOid = PgTypeOid::from_raw(3802);
const JSONPATH: PgTypeOid = PgTypeOid::from_raw(4072);
const BYTEA: PgTypeOid = PgTypeOid::from_raw(17);

/// A resolved `RETURNING` clause: type, typmod.
type Returning = (PgTypeOid, Option<i32>);

fn type_name(snapshot: &PgCatalog, t: PgTypeOid) -> String {
    crate::ddl::util::format_type_for_message(snapshot, t)
}

fn is_string_category(snapshot: &PgCatalog, t: PgTypeOid) -> bool {
    snapshot
        .get_type(t)
        .is_some_and(|e| e.typcategory == TypCategory::String)
}

fn format_type_of(format: Option<&protobuf::JsonFormat>) -> protobuf::JsonFormatType {
    format
        .and_then(|f| protobuf::JsonFormatType::try_from(f.format_type).ok())
        .unwrap_or(protobuf::JsonFormatType::JsFormatDefault)
}

fn has_encoding(format: Option<&protobuf::JsonFormat>) -> bool {
    format.is_some_and(|f| {
        !matches!(
            protobuf::JsonEncoding::try_from(f.encoding),
            Ok(protobuf::JsonEncoding::JsEncDefault | protobuf::JsonEncoding::Undefined) | Err(_)
        )
    })
}

/// `transformJsonOutput`: the `RETURNING type [FORMAT JSON …]` clause, or
/// `None` when absent. SETOF and pseudo-types are rejected.
/// `allow_format_for_non_strings` is checkJsonOutputFormat's flag: false
/// for JSON_QUERY, whose `FORMAT JSON` needs a string / json / bytea result.
fn json_output(
    output: Option<&protobuf::JsonOutput>,
    snapshot: &PgCatalog,
    allow_format_for_non_strings: bool,
) -> Result<Option<Returning>, AnalyzeError> {
    let Some(tn) = output.and_then(|o| o.type_name.as_ref()) else {
        return Ok(None);
    };
    let format = output
        .and_then(|o| o.returning.as_ref())
        .and_then(|r| r.format.as_ref());
    if tn.setof {
        return Err(AnalyzeError::Invalid(
            "returning SETOF types is not supported in SQL/JSON functions".into(),
        ));
    }
    let t = resolve_type_name(Some(tn), snapshot)?;
    if snapshot
        .get_type(t)
        .is_some_and(|e| e.typtype == TypType::Pseudo)
    {
        return Err(AnalyzeError::Invalid(
            "returning pseudo-types is not supported in SQL/JSON functions".into(),
        ));
    }
    let typmod = crate::typmod::encode(snapshot, t, &tn.typmods)
        .map_err(|e| AnalyzeError::Invalid(e.to_string()))?;
    // checkJsonOutputFormat.
    let explicit = format_type_of(format);
    if !allow_format_for_non_strings
        && explicit != protobuf::JsonFormatType::JsFormatDefault
        && t != BYTEA
        && t != JSON
        && t != JSONB
        && !is_string_category(snapshot, t)
    {
        return Err(AnalyzeError::Invalid(
            "cannot use JSON format with non-string output types".into(),
        ));
    }
    if explicit == protobuf::JsonFormatType::JsFormatJson && has_encoding(format) {
        if t != BYTEA {
            return Err(AnalyzeError::Invalid(
                "cannot set JSON encoding for non-bytea output types".into(),
            ));
        }
        if !format.is_some_and(|f| f.encoding == protobuf::JsonEncoding::JsEncUtf8 as i32) {
            return Err(AnalyzeError::Invalid("unsupported JSON encoding".into()));
        }
    }
    Ok(Some((t, typmod)))
}

/// `transformJsonValueExpr`: an input of a SQL/JSON construct. An untyped
/// input is text; `json`/`jsonb` pass through unformatted; otherwise, when
/// a JSON format applies (the construct's default or an explicit `FORMAT
/// JSON`), a string or bytea input is converted to `target` (json/jsonb),
/// and any other type is rejected — unless the caller only allows a cast to
/// its fixed `target` (`only_cast`), in which case a missing cast is
/// `cannot cast type X to Y`. PASSING arguments (`is_arg`) of the types a
/// jsonpath variable can hold are passed through as they are.
fn json_value_expr(
    ve: &protobuf::JsonValueExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    default_format: protobuf::JsonFormatType,
    target: Option<PgTypeOid>,
    is_arg: bool,
) -> Result<ExprType, AnalyzeError> {
    use protobuf::JsonFormatType as F;
    let snapshot = ctx.snapshot;
    let raw = ve
        .raw_expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("JSON value expression without expr".into()))?;
    let mut t = infer_expr(raw, ctx, params, TypeGoal::NONE)?;
    if t.type_oid == oid::UNKNOWN {
        coerce_unknown_to(raw, ctx, params, oid::TEXT)?;
        t.type_oid = oid::TEXT;
    }
    let ty = snapshot.unwrap_domain(t.type_oid);
    let explicit = format_type_of(ve.format.as_ref());
    let format = if explicit != F::JsFormatDefault {
        if has_encoding(ve.format.as_ref()) && ty != BYTEA {
            return Err(AnalyzeError::DatatypeMismatch(
                "JSON ENCODING clause is only allowed for bytea input type".into(),
            ));
        }
        if ty == JSON || ty == JSONB {
            F::JsFormatDefault
        } else {
            explicit
        }
    } else if is_arg {
        let passes = matches!(
            snapshot.get_type(ty).map(|e| e.typname.as_str()),
            Some(
                "bool"
                    | "numeric"
                    | "int2"
                    | "int4"
                    | "int8"
                    | "float4"
                    | "float8"
                    | "date"
                    | "time"
                    | "timetz"
                    | "timestamp"
                    | "timestamptz"
            )
        ) || is_string_category(snapshot, ty);
        if passes {
            return Ok(t);
        }
        default_format
    } else if ty == JSON || ty == JSONB {
        F::JsFormatDefault
    } else {
        default_format
    };

    if format == F::JsFormatDefault && target.is_none_or(|tg| tg == ty) {
        return Ok(t);
    }
    let only_cast = target.is_some();
    if !is_arg && !only_cast && ty != BYTEA && !is_string_category(snapshot, ty) {
        return Err(AnalyzeError::DatatypeMismatch(format!(
            "cannot use non-string types with {} FORMAT JSON clause",
            if explicit == F::JsFormatDefault {
                "implicit"
            } else {
                "explicit"
            }
        )));
    }
    // Encoded JSON text in bytea is only decoded under FORMAT JSON.
    let source = if ty == BYTEA && format == F::JsFormatJson {
        oid::TEXT
    } else {
        ty
    };
    let target = target.unwrap_or(if format == F::JsFormatJsonb {
        JSONB
    } else {
        JSON
    });
    if only_cast && source != target && !coerce::can_cast_explicit(source, target, snapshot) {
        return Err(crate::error::RawError::invalid(
            format!(
                "cannot cast type {} to {}",
                type_name(snapshot, source),
                type_name(snapshot, target)
            ),
            crate::error::node_location(raw).and_then(crate::error::SourceSpan::from_node_qname),
            None,
        )
        .finalize_implicit());
    }
    // Otherwise the value is cast, or wrapped in to_json()/to_jsonb().
    Ok(ExprType::scalar(target, t.nullable))
}

/// The result of a JSON constructor / aggregate
/// (`transformJsonConstructorOutput`): without RETURNING it is jsonb when
/// any argument is of type jsonb, json otherwise; with RETURNING T the
/// constructor builds json or jsonb and is then coerced to T, which must
/// have an explicit cast from it (bytea goes through convert_to).
fn constructor_result(
    output: Option<&protobuf::JsonOutput>,
    args: &[PgTypeOid],
    nullable: bool,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    let Some((t, typmod)) = json_output(output, snapshot, true)? else {
        let t = if args.contains(&JSONB) { JSONB } else { JSON };
        return Ok(ExprType::scalar(t, nullable));
    };
    let built = if t == JSONB { JSONB } else { JSON };
    if t != built && t != BYTEA && !coerce::can_cast_explicit(built, t, snapshot) {
        return Err(AnalyzeError::Invalid(format!(
            "cannot cast type {} to {}",
            type_name(snapshot, built),
            type_name(snapshot, t)
        )));
    }
    Ok(ExprType::scalar_with_typmod(t, nullable, typmod))
}

/// An object key (`key VALUE value` / `key : value`): PG transforms it with
/// no coercion — it is an `"any"` argument of the builder — so a bare `$N`
/// stays untypable.
fn object_key(
    key: &protobuf::Node,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let t = infer_expr(key, ctx, params, TypeGoal::NONE)?;
    if let Some(node::Node::ParamRef(p)) = key.node.as_ref() {
        params.mark_indeterminate_locked(p.number);
    }
    Ok(t)
}

/// `JSON_OBJECT(k VALUE v, …)` — `transformJsonObjectConstructor`.
pub(crate) fn infer_json_object(
    c: &protobuf::JsonObjectConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    for kv in &c.exprs {
        let Some(node::Node::JsonKeyValue(kv)) = kv.node.as_ref() else {
            continue;
        };
        if let Some(key) = kv.key.as_deref() {
            args.push(object_key(key, ctx, params)?.type_oid);
        }
        if let Some(v) = kv.value.as_deref() {
            let t = json_value_expr(
                v,
                ctx,
                params,
                protobuf::JsonFormatType::JsFormatDefault,
                None,
                false,
            )?;
            args.push(t.type_oid);
        }
    }
    constructor_result(c.output.as_ref(), &args, false, ctx.snapshot)
}

/// `JSON_ARRAY(v, …)` — `transformJsonArrayConstructor`.
pub(crate) fn infer_json_array(
    c: &protobuf::JsonArrayConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    for e in &c.exprs {
        if let Some(node::Node::JsonValueExpr(v)) = e.node.as_ref() {
            let t = json_value_expr(
                v,
                ctx,
                params,
                protobuf::JsonFormatType::JsFormatDefault,
                None,
                false,
            )?;
            args.push(t.type_oid);
        }
    }
    constructor_result(c.output.as_ref(), &args, false, ctx.snapshot)
}

/// `JSON_ARRAY(SELECT …)` — `transformJsonArrayQueryConstructor` rewrites
/// it into `(SELECT JSON_ARRAYAGG(a) FROM (query) q(a))`: the query needs
/// exactly one column, and the result is NULL when it returns no rows.
pub(crate) fn infer_json_array_query(
    c: &protobuf::JsonArrayQueryConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    if let Some(node::Node::SelectStmt(sel)) = c.query.as_deref().and_then(|q| q.node.as_ref()) {
        let (cols, _) = crate::resolve::analyze_correlated_select(
            sel,
            ctx.snapshot,
            params,
            ctx.scope,
            ctx.null_ctx,
        )?;
        if cols.len() != 1 {
            return Err(crate::pgmsg::subquery_must_return_one_column(
                crate::error::SourceSpan::from_location(c.location),
            )
            .finalize_implicit());
        }
        args.push(cols[0].type_oid);
    }
    constructor_result(c.output.as_ref(), &args, true, ctx.snapshot)
}

/// FILTER / ORDER BY / OVER of a JSON aggregate, walked like a regular
/// aggregate call's modifiers.
fn json_agg_modifiers(
    ac: &protobuf::JsonAggConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    if let Some(filter) = ac.agg_filter.as_deref() {
        crate::clause::coerce_clause_expr(filter, ctx, params, crate::clause::ClauseKind::Filter)?;
    }
    let sort_items = ac.agg_order.iter().chain(
        ac.over
            .as_deref()
            .map(|w| w.order_clause.as_slice())
            .unwrap_or_default(),
    );
    for item in sort_items {
        if let Some(node::Node::SortBy(sb)) = item.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
        {
            infer_expr(inner, ctx, params, TypeGoal::NONE)?;
        }
    }
    if let Some(over) = ac.over.as_deref() {
        for item in &over.partition_clause {
            infer_expr(item, ctx, params, TypeGoal::NONE)?;
        }
    }
    Ok(())
}

/// An aggregate's argument may not itself contain an aggregate.
fn check_not_nested(n: &protobuf::Node, snapshot: &PgCatalog) -> Result<(), AnalyzeError> {
    let kinds = detect_func_kinds(n, snapshot);
    if kinds.has_aggregate || kinds.has_grouping {
        return Err(crate::pgmsg::nested_aggregate(kinds.aggregate_span()).finalize_implicit());
    }
    Ok(())
}

/// `JSON_OBJECTAGG(k VALUE v)` — `transformJsonObjectAgg`. NULL over an
/// empty input.
pub(crate) fn infer_json_objectagg(
    a: &protobuf::JsonObjectAgg,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    if let Some(kv) = a.arg.as_deref() {
        if let Some(key) = kv.key.as_deref() {
            check_not_nested(key, ctx.snapshot)?;
            args.push(object_key(key, ctx, params)?.type_oid);
        }
        if let Some(v) = kv.value.as_deref() {
            if let Some(raw) = v.raw_expr.as_deref() {
                check_not_nested(raw, ctx.snapshot)?;
            }
            let t = json_value_expr(
                v,
                ctx,
                params,
                protobuf::JsonFormatType::JsFormatDefault,
                None,
                false,
            )?;
            args.push(t.type_oid);
        }
    }
    let ac = a.constructor.as_deref();
    if let Some(ac) = ac {
        json_agg_modifiers(ac, ctx, params)?;
    }
    constructor_result(
        ac.and_then(|c| c.output.as_ref()),
        &args,
        true,
        ctx.snapshot,
    )
}

/// `JSON_ARRAYAGG(v)` — `transformJsonArrayAgg`. NULL over an empty input.
pub(crate) fn infer_json_arrayagg(
    a: &protobuf::JsonArrayAgg,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    if let Some(v) = a.arg.as_deref() {
        if let Some(raw) = v.raw_expr.as_deref() {
            check_not_nested(raw, ctx.snapshot)?;
        }
        let t = json_value_expr(
            v,
            ctx,
            params,
            protobuf::JsonFormatType::JsFormatDefault,
            None,
            false,
        )?;
        args.push(t.type_oid);
    }
    let ac = a.constructor.as_deref();
    if let Some(ac) = ac {
        json_agg_modifiers(ac, ctx, params)?;
    }
    constructor_result(
        ac.and_then(|c| c.output.as_ref()),
        &args,
        true,
        ctx.snapshot,
    )
}

/// `JSON(expr)` — `transformJsonParseExpr`: the input is cast to json
/// (text-like or bytea input, json passes). NULL in, NULL out.
pub(crate) fn infer_json_parse(
    p: &protobuf::JsonParseExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let ve = p
        .expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("JSON() without argument".into()))?;
    let arg = json_value_expr(
        ve,
        ctx,
        params,
        protobuf::JsonFormatType::JsFormatJson,
        Some(JSON),
        false,
    )?;
    match json_output(p.output.as_ref(), ctx.snapshot, true)? {
        Some((t, typmod)) => Ok(ExprType::scalar_with_typmod(t, arg.nullable, typmod)),
        None => Ok(ExprType::scalar(JSON, arg.nullable)),
    }
}

/// `JSON_SCALAR(expr)` — `transformJsonScalarExpr`: any input (an untyped
/// one is text) becomes a json scalar. NULL in, NULL out.
pub(crate) fn infer_json_scalar(
    s: &protobuf::JsonScalarExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let e = s
        .expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("JSON_SCALAR() without argument".into()))?;
    let t = infer_expr(e, ctx, params, TypeGoal::NONE)?;
    if t.type_oid == oid::UNKNOWN {
        coerce_unknown_to(e, ctx, params, oid::TEXT)?;
    }
    match json_output(s.output.as_ref(), ctx.snapshot, true)? {
        Some((ty, typmod)) => Ok(ExprType::scalar_with_typmod(ty, t.nullable, typmod)),
        None => Ok(ExprType::scalar(JSON, t.nullable)),
    }
}

/// `JSON_SERIALIZE(expr [RETURNING T])` — `transformJsonSerializeExpr`: the
/// input takes the JSON format (non-string, non-json inputs are rejected);
/// the result is text unless RETURNING names a string type or bytea.
pub(crate) fn infer_json_serialize(
    s: &protobuf::JsonSerializeExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let ve = s
        .expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("JSON_SERIALIZE() without argument".into()))?;
    let arg = json_value_expr(
        ve,
        ctx,
        params,
        protobuf::JsonFormatType::JsFormatJson,
        None,
        false,
    )?;
    match json_output(s.output.as_ref(), snapshot, true)? {
        Some((t, typmod)) => {
            if t != BYTEA && !is_string_category(snapshot, t) {
                return Err(crate::error::RawError::new(
                    AnalyzeError::DatatypeMismatch(format!(
                        "cannot use type {} in RETURNING clause of JSON_SERIALIZE()",
                        type_name(snapshot, t)
                    )),
                    None,
                    Some("Try returning a string type or bytea.".into()),
                )
                .finalize_implicit());
            }
            Ok(ExprType::scalar_with_typmod(t, arg.nullable, typmod))
        }
        None => Ok(ExprType::scalar(oid::TEXT, arg.nullable)),
    }
}

/// `expr IS [NOT] JSON [VALUE|OBJECT|ARRAY|SCALAR] [WITH UNIQUE KEYS]` —
/// `transformJsonIsPredicate` / `transformJsonParseArg`: bytea and string
/// inputs (an untyped one included) are read as text; anything but text,
/// json and jsonb is rejected.
pub(crate) fn infer_json_is_predicate(
    p: &protobuf::JsonIsPredicate,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let e = p
        .expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("IS JSON without operand".into()))?;
    let t = infer_expr(e, ctx, params, TypeGoal::NONE)?;
    let base = snapshot.unwrap_domain(t.type_oid);
    let mut ty = base;
    if base == BYTEA {
        ty = oid::TEXT;
    } else {
        if base == oid::UNKNOWN {
            coerce_unknown_to(e, ctx, params, oid::TEXT)?;
            ty = oid::TEXT;
        } else if is_string_category(snapshot, base) {
            ty = oid::TEXT;
        }
        if has_encoding(p.format.as_ref()) {
            return Err(AnalyzeError::DatatypeMismatch(
                "cannot use JSON FORMAT ENCODING clause for non-bytea input types".into(),
            ));
        }
    }
    if ty != oid::TEXT && ty != JSON && ty != JSONB {
        return Err(AnalyzeError::DatatypeMismatch(format!(
            "cannot use type {} in IS JSON predicate",
            type_name(snapshot, ty)
        )));
    }
    Ok(ExprType::scalar(oid::BOOL, t.nullable))
}

/// `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS` — `transformJsonFuncExpr`.
///
/// The context item is cast to jsonb; the path spec must be (castable to)
/// jsonpath; PASSING arguments are jsonpath variables. The result is
/// RETURNING's type, else text (JSON_VALUE), jsonb (JSON_QUERY) or boolean
/// (JSON_EXISTS). ON EMPTY / ON ERROR behaviors are checked against what each
/// function allows, and a DEFAULT expression must be a constant, function or
/// operator expression without column references, cast to the result type.
pub(crate) fn infer_json_func_expr(
    f: &protobuf::JsonFuncExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    use protobuf::JsonExprOp as Op;
    let snapshot = ctx.snapshot;
    let op = protobuf::JsonExprOp::try_from(f.op).unwrap_or(Op::Undefined);
    let fname = match op {
        Op::JsonExistsOp => "JSON_EXISTS()",
        Op::JsonQueryOp => "JSON_QUERY()",
        Op::JsonValueOp => "JSON_VALUE()",
        _ => {
            return Err(AnalyzeError::Unsupported(
                "JSON_TABLE is only valid in FROM".into(),
            ));
        }
    };

    let context = f
        .context_item
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("SQL/JSON function without context".into()))?;
    let ctx_t = json_value_expr(
        context,
        ctx,
        params,
        protobuf::JsonFormatType::JsFormatJsonb,
        Some(JSONB),
        false,
    )?;

    let mut path_nullable = false;
    if let Some(path) = f.pathspec.as_deref() {
        let t = infer_expr(path, ctx, params, TypeGoal::NONE)?;
        path_nullable = t.nullable;
        if t.type_oid == oid::UNKNOWN {
            coerce_unknown_to(path, ctx, params, JSONPATH)?;
        } else if t.type_oid != JSONPATH
            && !coerce::can_cast_explicit(t.type_oid, JSONPATH, snapshot)
        {
            return Err(AnalyzeError::DatatypeMismatch(format!(
                "JSON path expression must be of type jsonpath, not of type {}",
                type_name(snapshot, t.type_oid)
            )));
        }
    }

    for arg in &f.passing {
        if let Some(node::Node::JsonArgument(a)) = arg.node.as_ref()
            && let Some(v) = a.val.as_deref()
        {
            json_value_expr(
                v,
                ctx,
                params,
                protobuf::JsonFormatType::JsFormatJson,
                None,
                true,
            )?;
        }
    }

    // JSON_VALUE's result is a scalar: no FORMAT in its RETURNING.
    if op == Op::JsonValueOp
        && format_type_of(
            f.output
                .as_ref()
                .and_then(|o| o.returning.as_ref())
                .and_then(|r| r.format.as_ref()),
        ) != protobuf::JsonFormatType::JsFormatDefault
    {
        return Err(AnalyzeError::SyntaxError(
            "cannot specify FORMAT JSON in RETURNING clause of JSON_VALUE()".into(),
        ));
    }
    // A wrapper already quotes scalars; OMIT QUOTES can't apply.
    if matches!(
        protobuf::JsonQuotes::try_from(f.quotes),
        Ok(protobuf::JsonQuotes::JsQuotesOmit)
    ) && matches!(
        protobuf::JsonWrapper::try_from(f.wrapper),
        Ok(protobuf::JsonWrapper::JswConditional | protobuf::JsonWrapper::JswUnconditional)
    ) {
        return Err(AnalyzeError::SyntaxError(
            "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used".into(),
        ));
    }
    let (ret, typmod) = match json_output(f.output.as_ref(), snapshot, op != Op::JsonQueryOp)? {
        Some(r) => r,
        None => match op {
            Op::JsonExistsOp => (oid::BOOL, None),
            Op::JsonQueryOp => (JSONB, None),
            _ => (oid::TEXT, None),
        },
    };

    let mut unknown_on_error = false;
    for (behavior, when) in [(&f.on_empty, "ON EMPTY"), (&f.on_error, "ON ERROR")] {
        let Some(b) = behavior.as_deref() else {
            continue;
        };
        use protobuf::JsonBehaviorType as B;
        let btype = protobuf::JsonBehaviorType::try_from(b.btype).unwrap_or(B::Undefined);
        let (allowed, list): (&[B], &str) = match op {
            Op::JsonExistsOp => (
                &[
                    B::JsonBehaviorError,
                    B::JsonBehaviorTrue,
                    B::JsonBehaviorFalse,
                    B::JsonBehaviorUnknown,
                ],
                "ERROR, TRUE, FALSE, or UNKNOWN",
            ),
            Op::JsonQueryOp => (
                &[
                    B::JsonBehaviorError,
                    B::JsonBehaviorNull,
                    B::JsonBehaviorEmptyArray,
                    B::JsonBehaviorEmptyObject,
                    B::JsonBehaviorDefault,
                ],
                "ERROR, NULL, EMPTY ARRAY, EMPTY OBJECT, or DEFAULT expression",
            ),
            _ => (
                &[
                    B::JsonBehaviorError,
                    B::JsonBehaviorNull,
                    B::JsonBehaviorDefault,
                ],
                "ERROR, NULL, or DEFAULT expression",
            ),
        };
        // `EMPTY` is the older spelling of EMPTY ARRAY.
        let effective = if btype == B::JsonBehaviorEmpty {
            B::JsonBehaviorEmptyArray
        } else {
            btype
        };
        if !allowed.contains(&effective) {
            return Err(crate::error::RawError::new(
                AnalyzeError::SyntaxError(format!("invalid {when} behavior")),
                crate::error::SourceSpan::from_location(b.location),
                Some(format!("Only {list} is allowed in {when} for {fname}.")),
            )
            .finalize_implicit());
        }
        if when == "ON ERROR" && btype == B::JsonBehaviorUnknown {
            unknown_on_error = true;
        }
        if btype == B::JsonBehaviorDefault
            && let Some(e) = b.expr.as_deref()
        {
            json_behavior_default(e, ret, ctx, params)?;
        }
    }

    let nullable = match op {
        Op::JsonExistsOp => ctx_t.nullable || path_nullable || unknown_on_error,
        _ => true,
    };
    Ok(ExprType::scalar_with_typmod(ret, nullable, typmod))
}

/// A `DEFAULT expr ON EMPTY/ERROR` (transformJsonBehavior): only a constant,
/// a (non-aggregate) function or an operator expression, with no column
/// references, cast (explicitly) to the function's result type.
fn json_behavior_default(
    e: &protobuf::Node,
    ret: PgTypeOid,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    // The transformed node PG checks: a cast of a constant is a constant,
    // any other cast (of a column / parameter) is not.
    let shape_ok = |n: &protobuf::Node| -> bool {
        fn go(n: &protobuf::Node, snapshot: &PgCatalog) -> bool {
            match n.node.as_ref() {
                Some(node::Node::AConst(_) | node::Node::AExpr(_)) => true,
                // An aggregate / window call is not a FuncExpr.
                Some(node::Node::FuncCall(_)) => {
                    let kinds = detect_func_kinds(n, snapshot);
                    !kinds.has_aggregate && !kinds.has_window
                }
                Some(node::Node::TypeCast(c)) => c.arg.as_deref().is_some_and(|a| go(a, snapshot)),
                _ => false,
            }
        }
        go(n, snapshot)
    };
    if !shape_ok(e) {
        return Err(AnalyzeError::DatatypeMismatch(
            "can only specify a constant, non-aggregate function, or operator expression for DEFAULT"
                .into(),
        ));
    }
    if super::operators::contains_level0_column_ref(e) {
        return Err(AnalyzeError::DatatypeMismatch(
            "DEFAULT expression must not contain column references".into(),
        ));
    }
    let t = infer_expr(e, ctx, params, TypeGoal::NONE)?;
    if t.type_oid == oid::UNKNOWN {
        coerce_unknown_to(e, ctx, params, ret)?;
    } else if t.type_oid != ret && !coerce::can_cast_explicit(t.type_oid, ret, snapshot) {
        return Err(AnalyzeError::DatatypeMismatch(format!(
            "cannot cast behavior expression of type {} to {}",
            type_name(snapshot, t.type_oid),
            type_name(snapshot, ret)
        )));
    }
    Ok(())
}
