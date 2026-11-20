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
use typedpg_pg_query::ast;

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

fn format_type_of(format: Option<&ast::JsonFormat>) -> ast::JsonFormatType {
    format
        .map(|f| f.format_type)
        .unwrap_or(ast::JsonFormatType::JsFormatDefault)
}

fn has_encoding(format: Option<&ast::JsonFormat>) -> bool {
    format.is_some_and(|f| !matches!(f.encoding, ast::JsonEncoding::JsEncDefault))
}

/// `transformJsonOutput`: the `RETURNING type [FORMAT JSON …]` clause, or
/// `None` when absent. SETOF and pseudo-types are rejected.
/// `allow_format_for_non_strings` is checkJsonOutputFormat's flag: false
/// for JSON_QUERY, whose `FORMAT JSON` needs a string / json / bytea result.
fn json_output(
    output: Option<&ast::JsonOutput>,
    snapshot: &PgCatalog,
    allow_format_for_non_strings: bool,
) -> Result<Option<Returning>, AnalyzeError> {
    let Some(tn) = output.and_then(|o| o.type_name.as_deref()) else {
        return Ok(None);
    };
    let format = output
        .and_then(|o| o.returning.as_deref())
        .and_then(|r| r.format.as_deref());
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
    let typmod =
        crate::typmod::encode(snapshot, t, tn).map_err(|e| AnalyzeError::Invalid(e.to_string()))?;
    // checkJsonOutputFormat.
    let explicit = format_type_of(format);
    if !allow_format_for_non_strings
        && explicit != ast::JsonFormatType::JsFormatDefault
        && t != BYTEA
        && t != JSON
        && t != JSONB
        && !is_string_category(snapshot, t)
    {
        return Err(AnalyzeError::Invalid(
            "cannot use JSON format with non-string output types".into(),
        ));
    }
    if explicit == ast::JsonFormatType::JsFormatJson && has_encoding(format) {
        if t != BYTEA {
            return Err(AnalyzeError::Invalid(
                "cannot set JSON encoding for non-bytea output types".into(),
            ));
        }
        if !format.is_some_and(|f| f.encoding == ast::JsonEncoding::JsEncUtf8) {
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
    ve: &ast::JsonValueExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    default_format: ast::JsonFormatType,
    target: Option<PgTypeOid>,
    is_arg: bool,
) -> Result<ExprType, AnalyzeError> {
    use ast::JsonFormatType as F;
    let snapshot = ctx.snapshot;
    let raw = &ve.raw_expr;
    raw.as_ref()
        .ok_or_else(|| AnalyzeError::Internal("JSON value expression without expr".into()))?;
    let mut t = infer_expr(raw, ctx, params, TypeGoal::NONE)?;
    if t.type_oid == oid::UNKNOWN {
        coerce_unknown_to(raw, ctx, params, oid::TEXT)?;
        t.type_oid = oid::TEXT;
    }
    let ty = snapshot.unwrap_domain(t.type_oid);
    let explicit = format_type_of(ve.format.as_deref());
    let format = if explicit != F::JsFormatDefault {
        if has_encoding(ve.format.as_deref()) && ty != BYTEA {
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
    output: Option<&ast::JsonOutput>,
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
    key: &Option<ast::Node>,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let t = infer_expr(key, ctx, params, TypeGoal::NONE)?;
    if let Some(p) = super::bare_param(key, ctx.snapshot) {
        params.mark_indeterminate_locked(p.number);
    }
    Ok(t)
}

/// `JSON_OBJECT(k VALUE v, …)` — `transformJsonObjectConstructor`.
pub(crate) fn infer_json_object(
    c: &ast::JsonObjectConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    for kv in &c.exprs {
        let Some(ast::Node::JsonKeyValue(kv)) = kv.as_ref() else {
            continue;
        };
        if let key @ Some(_) = &kv.key {
            args.push(object_key(key, ctx, params)?.type_oid);
        }
        if let Some(v) = kv.value.as_deref() {
            let t = json_value_expr(
                v,
                ctx,
                params,
                ast::JsonFormatType::JsFormatDefault,
                None,
                false,
            )?;
            args.push(t.type_oid);
        }
    }
    constructor_result(c.output.as_deref(), &args, false, ctx.snapshot)
}

/// `JSON_ARRAY(v, …)` — `transformJsonArrayConstructor`.
pub(crate) fn infer_json_array(
    c: &ast::JsonArrayConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    for e in &c.exprs {
        if let Some(ast::Node::JsonValueExpr(v)) = e.as_ref() {
            let t = json_value_expr(
                v,
                ctx,
                params,
                ast::JsonFormatType::JsFormatDefault,
                None,
                false,
            )?;
            args.push(t.type_oid);
        }
    }
    constructor_result(c.output.as_deref(), &args, false, ctx.snapshot)
}

/// `JSON_ARRAY(SELECT …)` — `transformJsonArrayQueryConstructor` rewrites
/// it into `(SELECT JSON_ARRAYAGG(a) FROM (query) q(a))`: the query needs
/// exactly one column, and the result is NULL when it returns no rows.
pub(crate) fn infer_json_array_query(
    c: &ast::JsonArrayQueryConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    if let Some(ast::Node::SelectStmt(sel)) = c.query.as_ref() {
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
    constructor_result(c.output.as_deref(), &args, true, ctx.snapshot)
}

/// FILTER / ORDER BY / OVER of a JSON aggregate, walked like a regular
/// aggregate call's modifiers.
fn json_agg_modifiers(
    ac: &ast::JsonAggConstructor,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    if let filter @ Some(_) = &ac.agg_filter {
        crate::clause::coerce_clause_expr(filter, ctx, params, crate::clause::ClauseKind::Filter)?;
    }
    for item in &ac.agg_order {
        if let Some(ast::Node::SortBy(sb)) = item.as_ref()
            && let inner @ Some(_) = &sb.node
        {
            infer_expr(inner, ctx, params, TypeGoal::NONE)?;
            // transformJsonAggConstructor's transformSortClause.
            crate::expr::resolve_unknown_key(inner, ctx.snapshot, params)?;
        }
    }
    // Its window, like a function's, is transformed with the query's
    // windows (`check_window_clause`).
    Ok(())
}

/// A JSON aggregate is NULL only over no rows: `NULL ON NULL` appends a
/// JSON null, `ABSENT ON NULL` skips the value but still builds `[]` /
/// `{}` (`json[b]_agg[_strict]` / `json[b]_object_agg*` underneath).
/// One of an outer level (over its columns only) reads that level's rows,
/// which this level knows nothing of.
fn json_agg_nullable(
    ac: Option<&ast::JsonAggConstructor>,
    values: &[&Option<ast::Node>],
    ctx: Ctx<'_>,
) -> bool {
    let over = ac.and_then(|c| c.over.as_deref());
    if over.is_none() {
        let mut aggregated = values.to_vec();
        if let Some(ac) = ac {
            aggregated.extend(ac.agg_order.iter().filter_map(|o| match o.as_ref() {
                Some(ast::Node::SortBy(sb)) => sb.node.is_some().then_some(&sb.node),
                _ => Some(o),
            }));
            aggregated.extend(ac.agg_filter.is_some().then_some(&ac.agg_filter));
        }
        if crate::grouping::levels_up_of(&aggregated, ctx.scope) > 0 {
            return true;
        }
    }
    !crate::expr::aggregate_reads_rows(
        ac.is_some_and(|c| c.agg_filter.is_some()),
        over,
        ctx.null_ctx,
    )
}

/// `JSON_OBJECTAGG(k VALUE v)` — `transformJsonObjectAgg`. NULL over an
/// empty input.
pub(crate) fn infer_json_objectagg(
    a: &ast::JsonObjectAgg,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    if let Some(kv) = a.arg.as_deref() {
        if let key @ Some(_) = &kv.key {
            args.push(object_key(key, ctx, params)?.type_oid);
        }
        if let Some(v) = kv.value.as_deref() {
            let t = json_value_expr(
                v,
                ctx,
                params,
                ast::JsonFormatType::JsFormatDefault,
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
    let values: Vec<&Option<ast::Node>> = a
        .arg
        .as_deref()
        .into_iter()
        .flat_map(|kv| {
            std::iter::once(&kv.key).filter(|k| k.is_some()).chain(
                kv.value
                    .as_deref()
                    .map(|v| &v.raw_expr)
                    .filter(|e| e.is_some()),
            )
        })
        .collect();
    constructor_result(
        ac.and_then(|c| c.output.as_deref()),
        &args,
        json_agg_nullable(ac, &values, ctx),
        ctx.snapshot,
    )
}

/// `JSON_ARRAYAGG(v)` — `transformJsonArrayAgg`. NULL over an empty input.
pub(crate) fn infer_json_arrayagg(
    a: &ast::JsonArrayAgg,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let mut args = Vec::new();
    if let Some(v) = a.arg.as_deref() {
        let t = json_value_expr(
            v,
            ctx,
            params,
            ast::JsonFormatType::JsFormatDefault,
            None,
            false,
        )?;
        args.push(t.type_oid);
    }
    let ac = a.constructor.as_deref();
    if let Some(ac) = ac {
        json_agg_modifiers(ac, ctx, params)?;
    }
    let values: Vec<&Option<ast::Node>> = a
        .arg
        .as_deref()
        .map(|v| &v.raw_expr)
        .filter(|e| e.is_some())
        .into_iter()
        .collect();
    constructor_result(
        ac.and_then(|c| c.output.as_deref()),
        &args,
        json_agg_nullable(ac, &values, ctx),
        ctx.snapshot,
    )
}

/// `JSON(expr)` — `transformJsonParseExpr`: the input is cast to json
/// (text-like or bytea input, json passes). With `WITH UNIQUE KEYS` the
/// input is instead read as text by the executor, which checks the keys:
/// it must be a string, bytea or untyped (`transformJsonParseArg`), so a
/// json / jsonb input is rejected too. NULL in, NULL out.
pub(crate) fn infer_json_parse(
    p: &ast::JsonParseExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let ve = p
        .expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("JSON() without argument".into()))?;
    let returning = json_output(p.output.as_deref(), ctx.snapshot, true)?;
    let arg = if p.unique_keys {
        let raw = &ve.raw_expr;
        raw.as_ref()
            .ok_or_else(|| AnalyzeError::Internal("JSON value expression without expr".into()))?;
        let (ty, t) = json_parse_arg(raw, ve.format.as_deref(), ctx, params)?;
        if ty != oid::TEXT {
            return Err(AnalyzeError::DatatypeMismatch(
                "cannot use non-string types with WITH UNIQUE KEYS clause".into(),
            ));
        }
        t
    } else {
        json_value_expr(
            ve,
            ctx,
            params,
            ast::JsonFormatType::JsFormatJson,
            Some(JSON),
            false,
        )?
    };
    match returning {
        Some((t, typmod)) => Ok(ExprType::scalar_with_typmod(t, arg.nullable, typmod)),
        None => Ok(ExprType::scalar(JSON, arg.nullable)),
    }
}

/// `JSON_SCALAR(expr)` — `transformJsonScalarExpr`: any input (an untyped
/// one is text) becomes a json scalar. NULL in, NULL out.
pub(crate) fn infer_json_scalar(
    s: &ast::JsonScalarExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let e = &s.expr;
    e.as_ref()
        .ok_or_else(|| AnalyzeError::Internal("JSON_SCALAR() without argument".into()))?;
    let t = infer_expr(e, ctx, params, TypeGoal::NONE)?;
    if t.type_oid == oid::UNKNOWN {
        coerce_unknown_to(e, ctx, params, oid::TEXT)?;
    }
    match json_output(s.output.as_deref(), ctx.snapshot, true)? {
        Some((ty, typmod)) => Ok(ExprType::scalar_with_typmod(ty, t.nullable, typmod)),
        None => Ok(ExprType::scalar(JSON, t.nullable)),
    }
}

/// `JSON_SERIALIZE(expr [RETURNING T])` — `transformJsonSerializeExpr`: the
/// input takes the JSON format (non-string, non-json inputs are rejected);
/// the result is text unless RETURNING names a string type or bytea.
pub(crate) fn infer_json_serialize(
    s: &ast::JsonSerializeExpr,
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
        ast::JsonFormatType::JsFormatJson,
        None,
        false,
    )?;
    match json_output(s.output.as_deref(), snapshot, true)? {
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
    p: &ast::JsonIsPredicate,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let e = &p.expr;
    e.as_ref()
        .ok_or_else(|| AnalyzeError::Internal("IS JSON without operand".into()))?;
    let (ty, t) = json_parse_arg(e, p.format.as_deref(), ctx, params)?;
    if ty != oid::TEXT && ty != JSON && ty != JSONB {
        return Err(AnalyzeError::DatatypeMismatch(format!(
            "cannot use type {} in IS JSON predicate",
            type_name(snapshot, ty)
        )));
    }
    Ok(ExprType::scalar(oid::BOOL, t.nullable))
}

/// `transformJsonParseArg`: the input of `IS JSON` and of `JSON(… WITH
/// UNIQUE KEYS)`, both parsed as text by the executor. Bytea and string
/// inputs (an untyped one included) are read as text; any other type is
/// returned as its base type, for the caller to judge. Returns that type
/// and the input's own inference.
fn json_parse_arg(
    e: &Option<ast::Node>,
    format: Option<&ast::JsonFormat>,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(PgTypeOid, ExprType), AnalyzeError> {
    let snapshot = ctx.snapshot;
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
        if has_encoding(format) {
            return Err(AnalyzeError::FeatureNotSupported(
                "cannot use JSON FORMAT ENCODING clause for non-bytea input types".into(),
            ));
        }
    }
    Ok((ty, t))
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
    f: &ast::JsonFuncExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    use ast::JsonExprOp as Op;
    let snapshot = ctx.snapshot;
    let op = f.op;
    if !matches!(op, Op::JsonExistsOp | Op::JsonQueryOp | Op::JsonValueOp) {
        return Err(AnalyzeError::Unsupported(
            "JSON_TABLE is only valid in FROM".into(),
        ));
    }

    let context = f
        .context_item
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("SQL/JSON function without context".into()))?;
    let ctx_t = json_value_expr(
        context,
        ctx,
        params,
        ast::JsonFormatType::JsFormatJsonb,
        Some(JSONB),
        false,
    )?;

    let mut path_nullable = false;
    if let path @ Some(_) = &f.pathspec {
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
        if let Some(ast::Node::JsonArgument(a)) = arg.as_ref()
            && let Some(v) = a.val.as_deref()
        {
            json_value_expr(
                v,
                ctx,
                params,
                ast::JsonFormatType::JsFormatJson,
                None,
                true,
            )?;
        }
    }

    // JSON_VALUE's result is a scalar: no FORMAT in its RETURNING.
    if op == Op::JsonValueOp
        && format_type_of(
            f.output
                .as_deref()
                .and_then(|o| o.returning.as_deref())
                .and_then(|r| r.format.as_deref()),
        ) != ast::JsonFormatType::JsFormatDefault
    {
        return Err(AnalyzeError::SyntaxError(
            "cannot specify FORMAT JSON in RETURNING clause of JSON_VALUE()".into(),
        ));
    }
    // A wrapper already quotes scalars; OMIT QUOTES can't apply.
    if matches!(f.quotes, ast::JsonQuotes::JsQuotesOmit)
        && matches!(
            f.wrapper,
            ast::JsonWrapper::JswConditional | ast::JsonWrapper::JswUnconditional
        )
    {
        return Err(AnalyzeError::SyntaxError(
            "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used".into(),
        ));
    }
    let (ret, typmod) = match json_output(f.output.as_deref(), snapshot, op != Op::JsonQueryOp)? {
        Some(r) => r,
        None => match op {
            Op::JsonExistsOp => (oid::BOOL, None),
            Op::JsonQueryOp => (JSONB, None),
            _ => (oid::TEXT, None),
        },
    };

    let unknown_on_error = json_behaviors(
        op,
        f.on_empty.as_deref(),
        f.on_error.as_deref(),
        None,
        ret,
        ctx,
        params,
    )?;

    let nullable = match op {
        Op::JsonExistsOp => ctx_t.nullable || path_nullable || unknown_on_error,
        _ => true,
    };
    Ok(ExprType::scalar_with_typmod(ret, nullable, typmod))
}

/// The ON EMPTY / ON ERROR behaviors of a JSON_VALUE / JSON_QUERY /
/// JSON_EXISTS call, or of the JSON_TABLE column (`column`) that runs as
/// one (`transformJsonFuncExpr`): each must be one the function allows —
/// a column's message names it — and a DEFAULT expression must be coercible
/// to `ret`. Returns whether ON ERROR is UNKNOWN (JSON_EXISTS's NULL).
pub(crate) fn json_behaviors(
    op: ast::JsonExprOp,
    on_empty: Option<&ast::JsonBehavior>,
    on_error: Option<&ast::JsonBehavior>,
    column: Option<&str>,
    ret: PgTypeOid,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<bool, AnalyzeError> {
    use ast::JsonBehaviorType as B;
    use ast::JsonExprOp as Op;
    let mut unknown_on_error = false;
    for (behavior, when) in [(on_empty, "ON EMPTY"), (on_error, "ON ERROR")] {
        let Some(b) = behavior else {
            continue;
        };
        let btype = b.btype;
        let (allowed, list, fname, columns): (&[B], &str, &str, &str) = match op {
            Op::JsonExistsOp => (
                &[
                    B::JsonBehaviorError,
                    B::JsonBehaviorTrue,
                    B::JsonBehaviorFalse,
                    B::JsonBehaviorUnknown,
                ],
                "ERROR, TRUE, FALSE, or UNKNOWN",
                "JSON_EXISTS()",
                "EXISTS columns",
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
                "JSON_QUERY()",
                "formatted columns",
            ),
            _ => (
                &[
                    B::JsonBehaviorError,
                    B::JsonBehaviorNull,
                    B::JsonBehaviorDefault,
                ],
                "ERROR, NULL, or DEFAULT expression",
                "JSON_VALUE()",
                "scalar columns",
            ),
        };
        // `EMPTY` is the older spelling of EMPTY ARRAY.
        let effective = if btype == B::JsonBehaviorEmpty {
            B::JsonBehaviorEmptyArray
        } else {
            btype
        };
        if !allowed.contains(&effective) {
            // PG prints the column name raw: `invalid %s behavior for
            // column \"%s\"`.
            let (message, target) = match column {
                Some(name) => (
                    format!("invalid {when} behavior for column \"{name}\""),
                    columns,
                ),
                None => (format!("invalid {when} behavior"), fname),
            };
            return Err(crate::error::RawError::new(
                AnalyzeError::SyntaxError(message),
                crate::error::SourceSpan::from_location(b.location),
                Some(format!("Only {list} is allowed in {when} for {target}.")),
            )
            .finalize_implicit());
        }
        if when == "ON ERROR" && btype == B::JsonBehaviorUnknown {
            unknown_on_error = true;
        }
        if btype == B::JsonBehaviorDefault
            && let e @ Some(_) = &b.expr
        {
            json_behavior_default(e, ret, ctx, params)?;
        }
    }
    Ok(unknown_on_error)
}

/// A `DEFAULT expr ON EMPTY/ERROR` (transformJsonBehavior): only a constant,
/// a (non-aggregate) function or an operator expression, with no column
/// references, cast (explicitly) to the function's result type.
fn json_behavior_default(
    e: &Option<ast::Node>,
    ret: PgTypeOid,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    // The transformed node PG checks: a cast of a constant is a constant,
    // any other cast (of a column / parameter) is not.
    let shape_ok = |n: &Option<ast::Node>| -> bool {
        fn go(n: &Option<ast::Node>, snapshot: &PgCatalog) -> bool {
            match n.as_ref() {
                Some(ast::Node::AConst(_) | ast::Node::AExpr(_)) => true,
                // An aggregate / window call is not a FuncExpr.
                Some(ast::Node::FuncCall(_)) => {
                    let kinds = detect_func_kinds(n, snapshot);
                    !kinds.has_aggregate && !kinds.has_window
                }
                Some(ast::Node::TypeCast(c)) => go(&c.arg, snapshot),
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
        return coerce_unknown_to(e, ctx, params, ret);
    }
    if t.type_oid == ret {
        return Ok(());
    }
    // A NULL constant (a cast of one folds into a typed NULL constant), a
    // jsonb value and a boolean — unless the result is (a domain over)
    // integer, which has a cast from boolean — are coerced at run time,
    // by json_populate_type.
    fn null_const(n: &Option<ast::Node>) -> bool {
        match n.as_ref() {
            Some(ast::Node::AConst(c)) => c.isnull,
            Some(ast::Node::TypeCast(c)) => null_const(&c.arg),
            _ => false,
        }
    }
    if null_const(e)
        || t.type_oid == JSONB
        || (t.type_oid == oid::BOOL && snapshot.unwrap_domain(ret) != oid::INT4)
    {
        return Ok(());
    }
    // Any other value needs a cast: an assignment one to a string or bit
    // string type (so a too-long input errors instead of being
    // truncated), an explicit one otherwise.
    let castable = match coerce::type_category(ret, snapshot) {
        Some(TypCategory::String | TypCategory::BitString) => coerce::can_coerce(
            t.type_oid,
            ret,
            coerce::CoercionContext::Assignment,
            snapshot,
        ),
        _ => coerce::can_cast_explicit(t.type_oid, ret, snapshot),
    };
    if !castable {
        return Err(crate::pgmsg::cannot_cast_behavior_expression(
            &type_name(snapshot, t.type_oid),
            &type_name(snapshot, ret),
        )
        .finalize_implicit());
    }
    Ok(())
}
