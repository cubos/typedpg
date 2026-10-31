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
    // A number, a boolean or a bit string is never an infinity; a string
    // is when it spells one.
    let refine = match &a_const.val {
        Some(a_const::Val::Sval(s)) => crate::refine::Refinement::of_string_literal(&s.sval),
        Some(a_const::Val::Ival(i)) => crate::refine::Refinement {
            values: Some([i.ival.to_string()].into()),
            range: Some(crate::refine::IntRange::exactly(i.ival.into())),
            ..crate::refine::Refinement::FINITE
        },
        Some(a_const::Val::Fval(f)) if type_oid != oid::NUMERIC => {
            let int = crate::literal_input::parse_pg_integer(&f.fval);
            crate::refine::Refinement {
                values: int.map(|i| [i.to_string()].into()),
                range: int.map(crate::refine::IntRange::exactly),
                ..crate::refine::Refinement::FINITE
            }
        }
        Some(a_const::Val::Boolval(b)) => crate::refine::Refinement {
            values: Some([b.boolval.to_string()].into()),
            ..crate::refine::Refinement::FINITE
        },
        _ => crate::refine::Refinement::FINITE,
    };

    Ok(ExprType::scalar(type_oid, false).with_refine(refine))
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

/// coerce_type hands an `unknown` operand of a cast to `"any"`,
/// `anyelement`, `anynonarray`, `anycompatible`, `anycompatiblenonarray` or
/// `unknown` itself back untouched — no input function runs, no type is
/// assigned — so such a cast of an untyped literal or a still-untyped
/// parameter *is* that literal or parameter: `SELECT '1'::anyelement` is a
/// `text` column, `$1::anyelement` a `text` parameter, `WHERE
/// $1::anyelement IS NULL` leaves `$1` untyped.
/// Returns the operand for such a cast (through nested ones), `None` for any
/// other node. (A typed operand goes through [`infer_type_cast`], which
/// returns it as it is too.)
pub(crate) fn passthrough_cast_operand<'a>(
    node: &'a protobuf::Node,
    snapshot: &PgCatalog,
    params: &ParamCollector,
) -> Option<&'a protobuf::Node> {
    let mut operand = node;
    let mut peeled = false;
    let mut to_unknown = false;
    while let Some(node::Node::TypeCast(cast)) = operand.node.as_ref() {
        let tn = cast.type_name.as_ref()?;
        if !tn.typmods.is_empty() || !tn.array_bounds.is_empty() {
            return None;
        }
        let target = resolve_type_name(Some(tn), snapshot).ok()?;
        if !keeps_unknown_operand(target) {
            return None;
        }
        to_unknown |= target == oid::UNKNOWN;
        operand = cast.arg.as_deref()?;
        peeled = true;
    }
    let untyped = match operand.node.as_ref()? {
        node::Node::AConst(ac) => ac.isnull || matches!(ac.val, Some(a_const::Val::Sval(_))),
        // A cast to `unknown` of a typed value fails (`cannot cast type
        // integer to unknown`): unless this occurrence was transformed
        // untyped, and a later clause typed it.
        node::Node::ParamRef(p) => {
            params.get(p.number) == oid::UNKNOWN
                || (to_unknown && params.is_untyped_cast_operand(p.location))
        }
        _ => false,
    };
    (peeled && untyped).then_some(operand)
}

/// Whether coerce_type hands an `unknown` operand of a cast to `target`
/// back untouched: the pass-through pseudo-types, and `unknown` itself
/// (`targetTypeId == inputTypeId`), so `$1::unknown` is the parameter node
/// `$1` while `$1` is untyped.
fn keeps_unknown_operand(target: PgTypeOid) -> bool {
    target == oid::UNKNOWN || crate::polymorphic::passes_operand_through(target)
}

/// The parameter an output column is, through the casts
/// [`passthrough_cast_operand`] sees through: `$1`, `$1::anyelement` and
/// `$1::unknown` alike are a bare parameter PG's `resolveTargetListUnknowns`
/// may coerce.
pub(crate) fn bare_param<'a>(
    node: &'a protobuf::Node,
    snapshot: &PgCatalog,
) -> Option<&'a protobuf::ParamRef> {
    let mut operand = node;
    loop {
        match operand.node.as_ref()? {
            node::Node::ParamRef(p) => return Some(p),
            node::Node::TypeCast(cast) => {
                let tn = cast.type_name.as_ref()?;
                if !tn.typmods.is_empty()
                    || !tn.array_bounds.is_empty()
                    || !resolve_type_name(Some(tn), snapshot).is_ok_and(keeps_unknown_operand)
                {
                    return None;
                }
                operand = cast.arg.as_deref()?;
            }
            _ => return None,
        }
    }
}

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
    // `('a'::anyelement)::int` casts the literal itself.
    let inner = passthrough_cast_operand(inner, snapshot, params).unwrap_or(inner);

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
        Some(tn) if !tn.typmods.is_empty() => crate::typmod::encode(snapshot, target_oid, tn)
            .map_err(|e| AnalyzeError::Invalid(e.to_string()))?,
        _ => None,
    };

    // coerce_type returns the operand of a cast to `"any"`, `anyelement`,
    // `anynonarray`, `anycompatible` or `anycompatiblenonarray` as it is —
    // its own type, not the pseudo-type (`1::anyelement` is an integer; an
    // untyped literal or parameter stays unknown, to resolve as text).
    const PASS_THROUGH: [PgTypeOid; 5] = [
        PgTypeOid::from_raw(2276), // "any"
        PgTypeOid::from_raw(2283), // anyelement
        PgTypeOid::from_raw(2776), // anynonarray
        PgTypeOid::from_raw(5077), // anycompatible
        PgTypeOid::from_raw(5079), // anycompatiblenonarray
    ];
    let pass_through = PASS_THROUGH.contains(&target_oid);

    if let Some(node::Node::AConst(ac)) = inner.node.as_ref()
        && !pass_through
        && !ac.isnull
        && matches!(ac.val, Some(a_const::Val::Sval(_)))
    {
        ctx.note_literal_type(ac.location, target_oid);
    }
    // stringTypeDatum runs the target's input function on an untyped
    // literal, which a pseudo-type with a dummy one refuses — on NULL too
    // when it is not strict (`NULL::trigger`; `NULL::anyarray` is a NULL).
    if let Some(node::Node::AConst(ac)) = inner.node.as_ref()
        && (ac.isnull || matches!(ac.val, Some(a_const::Val::Sval(_))))
        && let Some(strict) = dummy_input_pseudo_type(target_oid, snapshot)
        && (!ac.isnull || !strict)
    {
        let name = crate::ddl::util::format_type_for_message(snapshot, target_oid);
        let span =
            crate::error::node_location(inner).and_then(crate::error::SourceSpan::from_node_token);
        return Err(crate::pgmsg::cannot_accept_a_value_of_type(&name, span).finalize_implicit());
    }
    // PG validates the *content* of an untyped string literal against the
    // target's input function at parse time (`'x'::int` fails prepare with
    // `invalid input syntax for type integer: "x"`). Mirror it for the types
    // we model — see `literal_input`.
    if let Some(node::Node::AConst(ac)) = inner.node.as_ref()
        && !ac.isnull
        && let Some(a_const::Val::Sval(sv)) = &ac.val
        && let Err(msg) = crate::literal_input::validate_with_typmod(
            &sv.sval,
            target_oid,
            written_typmod,
            snapshot,
        )
    {
        let span =
            crate::error::node_location(inner).and_then(crate::error::SourceSpan::from_node_token);
        return Err(crate::error::RawError::invalid_literal(msg, span).finalize_implicit());
    }
    // The cast then applies the target's typmod (a domain's: its base
    // type's), `numeric(p, s)`'s `apply_typmod` failing every execution
    // for a literal out of range (`'123.45'::numeric(4,2)`).
    let cast_typmod = if snapshot.unwrap_domain(target_oid) == target_oid {
        written_typmod
    } else {
        snapshot.effective_typmod(target_oid, None)
    };
    if let Some(msg) = crate::typmod::numeric_literal_overflow(
        snapshot,
        snapshot.unwrap_domain(target_oid),
        cast_typmod,
        inner,
    ) {
        let span =
            crate::error::node_location(inner).and_then(crate::error::SourceSpan::from_node_token);
        return Err(crate::error::RawError::invalid(msg, span, None).finalize_implicit());
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
        // (An explicit cast: coerce_record_to_complex converts each field
        // in the explicit context.)
        (Some(node::Node::RowExpr(_)), Some(TypType::Composite)) => TypeGoal {
            coercion: CoercionContext::Explicit,
            ..TypeGoal::assignment(target_oid)
        },
        _ => TypeGoal::NONE,
    };
    // transformTypeCast: an `ARRAY[…]` operand of a cast to an array type
    // (or a domain over one) is transformed against the target's element
    // type, so its untyped elements and params take that type
    // (`ARRAY[$1]::int[]` makes `$1` an integer).
    let target_base = snapshot.unwrap_domain(target_oid);
    let array_target = match inner.node.as_ref() {
        // (`get_element_type`: also `record[]`, a pseudo-type array.)
        Some(node::Node::AArrayExpr(arr)) => {
            array_element_type(snapshot, target_base).map(|element_type| {
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
            })
        }
        _ => None,
    };
    let built_for_target = array_target.is_some();
    let inner_type = match array_target {
        Some((arr, target)) => transform_array_expr(arr, ctx, params, Some(target))?,
        None => infer_expr(inner, ctx, params, inner_goal)?,
    };
    // coerce_type: any other `record` cast to a composite is rebuilt field
    // by field from the row it names (a whole-row reference to a subquery,
    // CTE, VALUES list, …) — or rejected when it names none.
    let inner_type = if inner_type.type_oid == oid::RECORD
        && coerce::is_complex(target_oid, snapshot)
    {
        super::coerce_record_to_complex(inner, target_oid, CoercionContext::Explicit, ctx, params)?
    } else {
        inner_type
    };

    if let Some(node::Node::ParamRef(p)) = inner.node.as_ref()
        && !pass_through
        && params.get(p.number) == oid::UNKNOWN
    {
        params.record(p.number, target_oid);
    }

    // An `unknown` value that is no literal casts through I/O to a string
    // type only, and passes through `unknown` itself and the pseudo-types
    // coerce_type hands it to untouched.
    if inner_type.unknown_value {
        super::unknown_value_coercion(target_oid, CoercionContext::Explicit, snapshot, inner)?;
        if target_oid == oid::UNKNOWN || crate::polymorphic::passes_operand_through(target_oid) {
            return Ok(inner_type);
        }
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

    // An operand still untyped here (no literal or parameter, which
    // `passthrough_cast_operand` handles) is handed back as it is too.
    if pass_through && inner_type.type_oid == oid::UNKNOWN {
        return Ok(inner_type);
    }

    // coerce_type: a composite value cast to `record` (or an array of one to
    // `record[]`) is left as it is — the result keeps the composite type.
    // (An `ARRAY[...]` operand was built as the target array type above.)
    const RECORDARRAY: PgTypeOid = PgTypeOid::from_raw(2287);
    if (target_oid == oid::RECORD && coerce::is_complex(inner_type.type_oid, snapshot))
        || (target_oid == RECORDARRAY
            && !built_for_target
            && coerce::element_type(inner_type.type_oid, snapshot)
                .is_some_and(|e| coerce::is_complex(e, snapshot)))
    {
        return Ok(inner_type);
    }

    // coerce_type: a typed value cast to `"any"` or a polymorphic
    // pseudo-type is left as it is (`1::anyelement` is an integer) — to an
    // array, enum or range one, relabeled to its domain's base type (typmod
    // -1). coerce_type_typmod then relabels a value that still carries a
    // typmod to the target itself, typmod -1: `v::anyelement` over a
    // `varchar(3)` column is an `anyelement`, which no row can be output as
    // (`cannot display a value of type anyelement`).
    const ANY: PgTypeOid = PgTypeOid::from_raw(2276);
    if target_oid != inner_type.type_oid
        && inner_type.type_oid != oid::UNKNOWN
        && (target_oid == ANY || crate::polymorphic::is_polymorphic(target_oid))
    {
        let mut result = inner_type;
        let base = snapshot.unwrap_domain(result.type_oid);
        if !crate::polymorphic::passes_operand_through(target_oid) && base != result.type_oid {
            let (collation, explicit_collation) = derive_collation([&result], base, snapshot)?;
            result = ExprType {
                type_oid: base,
                typmod: None,
                collation,
                explicit_collation,
                ..result
            };
        }
        if result.typmod.is_some_and(|t| t >= 0) {
            let (collation, explicit_collation) =
                derive_collation([&result], target_oid, snapshot)?;
            result = ExprType {
                type_oid: target_oid,
                typmod: None,
                collation,
                explicit_collation,
                record_fields: None,
                elem_nullable: None,
                refine: Default::default(),
                ..result
            };
        }
        return Ok(result);
    }

    // PG: the cast result has exactly the written typmod — `x::T(n)` is
    // T(n), and `x::T` is T with typmod -1 even when x already was a T(n)
    // (coerce_type_typmod relabels to the target typmod).
    let cast_func = snapshot
        .cast_by_pair
        .get(&(
            snapshot.unwrap_domain(inner_type.type_oid),
            snapshot.unwrap_domain(target_oid),
        ))
        .and_then(|oid| snapshot.pg_cast.get(oid))
        .and_then(|c| c.castfunc);
    ctx.note_proc(cast_func);
    // A cast with no function (binary-coercible, I/O conversion) or a
    // strict one maps NULL to NULL.
    ctx.note_strict(
        cast.location,
        crate::nonnull::StrictNode::Cast,
        cast_func.is_none() || ctx.proc_is_strict(cast_func),
    );
    let state = derive_collation([&inner_type], target_oid, snapshot)?;
    let nullable = inner_type.nullable
        || cast_function_can_return_null(inner_type.type_oid, target_oid, snapshot);
    // The elements of an array cast to an array type: an array literal
    // says (`'{1,2}'::int[]` has no NULL element), an `ARRAY[…]` built
    // for the target or an array of the same type keeps its own (a
    // relabeling or typmod coercion maps no element to NULL); an element
    // conversion (`jsonb[]` → `int[]`, …) may.
    // A domain whose CHECK keeps NULL elements out checks the value.
    let elem_nullable = if array_element_type(snapshot, target_base).is_none() {
        None
    } else if snapshot.domain_null_free_elements(target_oid) {
        Some(false)
    } else if let Some(node::Node::AConst(ac)) = inner.node.as_ref() {
        match &ac.val {
            Some(a_const::Val::Sval(sv)) if !ac.isnull => Some(
                crate::literal_input::array_literal_may_contain_null(&sv.sval),
            ),
            _ => None,
        }
    } else if built_for_target || snapshot.unwrap_domain(inner_type.type_oid) == target_base {
        inner_type.elem_nullable
    } else {
        None
    };
    // `ROW(a, b)::pair` is the row built for `pair` (and a cast to the
    // composite a value already is changes nothing): its fields keep
    // their nullability.
    let record_fields = inner_type.record_fields.filter(|_| {
        coerce::is_complex(target_oid, snapshot)
            && snapshot.unwrap_domain(inner_type.type_oid) == snapshot.unwrap_domain(target_oid)
    });
    Ok(ExprType {
        record_fields,
        // A typed value cast to `unknown` (a CoerceViaIO) is no literal.
        unknown_value: target_oid == oid::UNKNOWN && inner_type.type_oid != oid::UNKNOWN,
        ..ExprType::scalar_with_typmod(target_oid, nullable, written_typmod)
            .with_collation(state)
            .with_elem_nullable(elem_nullable)
            .with_refine({
                let mut refine = inner_type
                    .refine
                    .converted(inner_type.type_oid, target_oid, snapshot)
                    .and(&snapshot.domain_refinement(target_oid));
                // An explicit cast to `varchar(n)` truncates.
                if cast_typmod.is_some() {
                    refine.values = None;
                }
                // A parameter cast to a type the generator binds finite
                // (`$at::timestamptz`) is of that type.
                if matches!(inner.node.as_ref(), Some(node::Node::ParamRef(_)))
                    && snapshot.finite_param(target_oid)
                {
                    refine.finite = true;
                }
                refine
            })
    })
}

/// Whether a value typed `t` may be NULL once assignment-coerced to a
/// column of type `target` (`coerce_to_target_type`, as INSERT, UPDATE,
/// a DEFAULT or a generation expression store it): when it is NULL
/// itself, or the coercion runs a cast function that can map a non-NULL
/// value to NULL (`time(timestamp)` on `infinity`, a user's cast). A value
/// of the column's own type (an untyped literal read by its input
/// function) needs no cast.
pub(crate) fn assignment_nullable(t: &ExprType, target: PgTypeOid, snapshot: &PgCatalog) -> bool {
    t.nullable
        || (t.type_oid != oid::UNKNOWN
            && snapshot.unwrap_domain(t.type_oid) != snapshot.unwrap_domain(target)
            && cast_function_can_return_null(t.type_oid, target, snapshot))
}

/// Whether the cast from `source` to `target` runs a cast function that can
/// return NULL for a non-NULL input — `int4(jsonb)` and the other jsonb →
/// scalar casts yield NULL for a JSON null. PG names a built-in cast
/// function after its target type (`pg_cast.castfunc` isn't in the
/// snapshot), and its nullability comes from the per-overload table.
pub(crate) fn cast_function_can_return_null(
    source: PgTypeOid,
    target: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    let (source, target) = (
        snapshot.unwrap_domain(source),
        snapshot.unwrap_domain(target),
    );
    let Some(cast) = snapshot
        .cast_by_pair
        .get(&(source, target))
        .and_then(|oid| snapshot.pg_cast.get(oid))
    else {
        return false;
    };
    if cast.castmethod != crate::pg_catalog::CastMethod::Function {
        return false;
    }
    // A user-defined cast function may return NULL like any user-defined
    // function (`functions::operator_result_nullable` has the same rule):
    // one in SQL, PL/pgSQL, … for any input — `STRICT` only says a NULL
    // argument skips the call — and a compiled (C / internal) one unless
    // it is STRICT.
    if let Some(f) = cast.castfunc.and_then(|oid| snapshot.pg_proc.get(&oid))
        && snapshot.namespace_name(f.pronamespace) != Some("pg_catalog")
    {
        let compiled = matches!(
            f.prolang,
            crate::pg_catalog::C_LANGUAGE | crate::pg_catalog::INTERNAL_LANGUAGE
        );
        return !f.proisstrict || !compiled;
    }
    let (Some(src), Some(tgt)) = (snapshot.get_type(source), snapshot.get_type(target)) else {
        return false;
    };
    snapshot
        .find_functions(Some("pg_catalog"), &tgt.typname)
        .iter()
        .find(|f| f.proargtypes.first() == Some(&source) && f.prorettype == target)
        .is_some_and(|f| {
            let sig = format!("{}({})", f.proname, src.typname);
            f.proisstrict && crate::builtin_nullability::NULLABLE_STRICT.contains(&sig.as_str())
        })
}

/// Whether implicitly coercing a non-NULL value of type `from` to `to` — a
/// call argument to its parameter's type, a CASE / COALESCE / UNION branch
/// to the common type — can yield NULL: the coercion runs a cast function
/// that can ([`cast_function_can_return_null`]). Nothing runs between equal
/// types, nor for an untyped literal (read by `to`'s input function).
pub(crate) fn coercion_can_return_null(
    from: PgTypeOid,
    to: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    from != to
        && from != oid::UNKNOWN
        && to != oid::UNKNOWN
        && cast_function_can_return_null(from, to, snapshot)
}

/// Whether that coercion can turn a non-NULL *element* of an array into
/// NULL: with no cast between the array types themselves, `find_coercion_pathway`
/// coerces an array to another array type element by element
/// (`ArrayCoerceExpr`), through the elements' cast.
pub(crate) fn coercion_can_null_elements(
    from: PgTypeOid,
    to: PgTypeOid,
    snapshot: &PgCatalog,
) -> bool {
    let (f, t) = (snapshot.unwrap_domain(from), snapshot.unwrap_domain(to));
    if f == t || from == oid::UNKNOWN || snapshot.cast_by_pair.contains_key(&(f, t)) {
        return false;
    }
    match (
        coerce::element_type(f, snapshot),
        coerce::element_type(t, snapshot),
    ) {
        (Some(fe), Some(te)) => coercion_can_return_null(fe, te, snapshot),
        _ => false,
    }
}

/// A pseudo-type whose input function is PSEUDOTYPE_DUMMY_INPUT_FUNC
/// (pseudotypes.c), with whether that function is strict (pg_proc.dat):
/// a string literal of the type always fails, a NULL one only when the
/// function is not strict. (Verified against PG 18.)
fn dummy_input_pseudo_type(t: PgTypeOid, snapshot: &PgCatalog) -> Option<bool> {
    let ty = snapshot.get_type(t)?;
    if ty.typtype != TypType::Pseudo
        || snapshot.namespace_name(ty.typnamespace) != Some("pg_catalog")
    {
        return None;
    }
    match ty.typname.as_str() {
        "anyarray"
        | "anyrange"
        | "anymultirange"
        | "anycompatiblearray"
        | "anycompatiblerange"
        | "anycompatiblemultirange" => Some(true),
        "trigger" | "event_trigger" | "language_handler" | "fdw_handler" | "table_am_handler"
        | "index_am_handler" | "tsm_handler" => Some(false),
        _ => None,
    }
}
