use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Literals
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_a_const(a_const: &protobuf::AConst) -> Result<ExprType, AnalyzeError> {
    if a_const.isnull {
        return Ok(ExprType::scalar(oid::UNKNOWN, true));
    }

    let type_oid = match &a_const.val {
        Some(a_const::Val::Ival(_)) => oid::INT4,
        Some(a_const::Val::Fval(f)) => fval_const_type(&f.fval),
        Some(a_const::Val::Boolval(_)) => oid::BOOL,
        Some(a_const::Val::Sval(_)) => oid::UNKNOWN, // untyped string literal
        // `B'…'` / `X'…'`: PG's make_const types a T_BitString as `bit` and
        // runs `bit_in` on it right away, so a bad digit is a parse-time
        // 22P02. The lexer keeps the radix marker (`b101` / `x1F`), which is
        // exactly the form bit_in accepts.
        Some(a_const::Val::Bsval(b)) => {
            if let Err(msg) = crate::literal_input::validate_bit(&b.bsval) {
                let span = crate::error::SourceSpan::from_node_token(a_const.location);
                return Err(crate::error::RawError::invalid_literal(msg, span).finalize_implicit());
            }
            BIT
        }
        None => oid::UNKNOWN,
    };

    Ok(ExprType::scalar(type_oid, false))
}

/// Type of an `Fval` (PG `T_Float`) constant, mirroring PG's `make_const`.
///
/// libpg_query stores any integer literal too large for a C `int` as a
/// `Float` (the textual form), so an `Fval` is not necessarily a real float.
/// PG re-parses it: an all-integer value is `int4`/`int8` (by magnitude, or
/// `numeric` if it overflows `int8`); anything with a decimal point or
/// exponent is `numeric`. So `9999999999` is `bigint`, not `numeric`.
///
/// The re-parse is `pg_strtoint64_safe`, which accepts the `0x`/`0o`/`0b`
/// prefixes and `_` digit separators, so `0x80000000` and `10_000_000_000`
/// are `bigint` and `-0x80000000` (negation folds into the token) `integer`.
fn fval_const_type(fval: &str) -> PgTypeOid {
    match crate::literal_input::parse_pg_integer(fval) {
        Some(v) if (i32::MIN as i128..=i32::MAX as i128).contains(&v) => oid::INT4,
        Some(v) if (i64::MIN as i128..=i64::MAX as i128).contains(&v) => oid::INT8,
        _ => oid::NUMERIC,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Type casts
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn infer_type_cast(
    cast: &protobuf::TypeCast,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let inner = cast
        .arg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("TypeCast without arg".into()))?;

    // `typename $1` (like `int4 '1'`, with the parameter in the constant's
    // place) is a libpg_query grammar extension for query normalization;
    // PG's own grammar only takes a string constant there. That form is
    // the only TypeCast without a location whose type name precedes a
    // parameter operand.
    if let Some(node::Node::ParamRef(p)) = inner.node.as_ref()
        && cast.location < 0
        && cast
            .type_name
            .as_ref()
            .is_some_and(|tn| tn.location >= 0 && tn.location < p.location)
    {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError(format!("syntax error at or near \"${}\"", p.number)),
            crate::error::SourceSpan::from_node_token(p.location),
            None,
        )
        .finalize_implicit());
    }

    let target_oid = resolve_type_name(cast.type_name.as_ref(), snapshot)?;
    // typenameTypeIdAndMod: the target's typmod is resolved (and validated)
    // before the operand is transformed.
    let written_typmod = match cast.type_name.as_ref() {
        Some(tn) if !tn.typmods.is_empty() => {
            crate::typmod::encode(snapshot, target_oid, &tn.typmods)
                .map_err(|e| AnalyzeError::Invalid(e.to_string()))?
        }
        _ => None,
    };

    // PG validates the *content* of an untyped string literal against the
    // target's input function at parse time (`'x'::int` fails prepare with
    // `invalid input syntax for type integer: "x"`). Mirror it for the types
    // we model — see `literal_input`.
    if let Some(node::Node::AConst(ac)) = inner.node.as_ref()
        && !ac.isnull
        && let Some(a_const::Val::Sval(sv)) = &ac.val
        && let Err(msg) = crate::literal_input::validate(&sv.sval, target_oid, snapshot)
    {
        let span =
            crate::error::node_location(inner).and_then(crate::error::SourceSpan::from_node_token);
        return Err(crate::error::RawError::invalid_literal(msg, span).finalize_implicit());
    }

    // An explicit cast (::type / CAST) overrides type checking — we do NOT
    // check compatibility of the inner expression against the target type.
    // The inner expression is normally inferred with NONE to avoid false
    // TypeMismatch errors (e.g. age::text where int4→text has no implicit
    // cast). The one exception is a `ROW(...)::composite` shape: PG uses
    // the cast target as the composite goal so each ROW element gets
    // pinned against the matching field type — without that propagation,
    // params inside the ROW would remain indeterminate. Mirror it.
    let inner_goal = match (
        inner.node.as_ref(),
        snapshot
            .get_type(snapshot.unwrap_domain(target_oid))
            .map(|t| t.typtype),
    ) {
        (Some(node::Node::RowExpr(_)), Some(TypType::Composite)) => {
            TypeGoal::assignment(target_oid)
        }
        _ => TypeGoal::NONE,
    };
    // transformTypeCast: an `ARRAY[…]` operand of a cast to an array type
    // (or a domain over one) is transformed against the target's element
    // type, so its untyped elements and params take that type
    // (`ARRAY[$1]::int[]` makes `$1` an integer).
    let target_base = snapshot.unwrap_domain(target_oid);
    let array_target = match inner.node.as_ref() {
        Some(node::Node::AArrayExpr(arr)) => snapshot
            .get_type(target_base)
            .filter(|t| t.typcategory == TypCategory::Array)
            .and_then(|t| t.typelem)
            .map(|element_type| {
                (
                    arr,
                    ArrayTarget {
                        array_type: target_base,
                        element_type,
                        typmod: if target_base == target_oid {
                            written_typmod
                        } else {
                            snapshot.effective_typmod(target_oid, None)
                        },
                    },
                )
            }),
        _ => None,
    };
    let inner_type = match array_target {
        Some((arr, target)) => transform_array_expr(arr, ctx, params, Some(target))?,
        None => infer_expr(inner, ctx, params, inner_goal)?,
    };

    if let Some(node::Node::ParamRef(p)) = inner.node.as_ref()
        && params.get(p.number) == oid::UNKNOWN
    {
        params.record(p.number, target_oid);
    }

    // PG rejects an explicit cast with no legal path (e.g. boolean → double
    // precision) at parse time — `cannot cast type X to Y`. Mirror that, but
    // only after the inner expression's own type is known.
    if !coerce::can_cast_explicit(inner_type.type_oid, target_oid, snapshot) {
        let from = crate::ddl::util::format_type_for_message(snapshot, inner_type.type_oid);
        let to = crate::ddl::util::format_type_for_message(snapshot, target_oid);
        let span = crate::error::node_location(inner).and_then(|loc| {
            crate::error::SourceSpan::from_node_qname(loc)
                .or_else(|| crate::error::SourceSpan::from_node_token(loc))
        });
        return Err(crate::error::RawError::invalid(
            format!("cannot cast type {from} to {to}"),
            span,
            None,
        )
        .with_primary_label(format!("this is {from}"))
        .finalize_implicit());
    }

    // PG: the cast result has exactly the written typmod — `x::T(n)` is
    // T(n), and `x::T` is T with typmod -1 even when x already was a T(n)
    // (coerce_type_typmod relabels to the target typmod).
    Ok(ExprType::scalar_with_typmod(
        target_oid,
        inner_type.nullable,
        written_typmod,
    ))
}
