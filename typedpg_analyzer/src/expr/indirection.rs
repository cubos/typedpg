use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// Indirection (`(expr).field`, `(expr)[i]`)
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve `(expr).field1.field2…` chains. Each step either names a field in
/// a composite (String) or subscripts an array / `jsonb` (`AIndices`).
/// Array subscripting handles both element access (`arr[n]`) and slicing
/// (`arr[1:3]`, which keeps the array type). `jsonb` / `json` subscripting
/// (`data['key']`, `data[0]`, chained) yields `jsonb` at every step.
pub(crate) fn infer_indirection(
    ind: &protobuf::AIndirection,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        scope, snapshot, ..
    } = ctx;
    let arg = ind
        .arg
        .as_deref()
        .ok_or_else(|| AnalyzeError::Unsupported("indirection without arg".into()))?;

    // Two shortcut paths for `record`-typed args whose fields aren't stored
    // in a composite `TypeEntry`:
    //
    // 1. `(func(...)).field` — direct FuncCall with `out_args` (TABLE/OUT).
    // 2. `(alias.col).field` — ColumnRef whose scope entry carries
    //    `record_fields` (populated when the subquery's target expr was a
    //    FuncCall with out_args).
    //
    // Consume leading String steps against those named fields; fall through
    // to the generic walker for any remaining steps (e.g. nested composite
    // unwrap, subscript on a scalar out_arg).
    let from_direct_funccall = if let Some(node::Node::FuncCall(fc)) = arg.node.as_ref() {
        resolve_funccall_out_args(fc, ctx, params)?
    } else {
        None
    };
    let from_column_record = if from_direct_funccall.is_none() {
        if let Some(node::Node::ColumnRef(cr)) = arg.node.as_ref() {
            column_ref_record_fields(cr, scope)
        } else {
            None
        }
    } else {
        None
    };

    let leading_fields = from_direct_funccall
        .as_ref()
        .or(from_column_record.as_ref());
    let (start_step, mut current) = if let Some(fields) = leading_fields {
        let mut idx = 0usize;
        let mut current = None;
        while idx < ind.indirection.len() {
            let Some(node::Node::String(s)) = ind.indirection[idx].node.as_ref() else {
                break;
            };
            let field = fields.iter().find(|f| f.name == s.sval).ok_or_else(|| {
                AnalyzeError::UndefinedColumn(format!(
                    "could not identify column \"{}\" in record data type",
                    s.sval
                ))
            })?;
            current = Some(field.ty.clone());
            idx += 1;
        }
        (idx, current)
    } else {
        (0, None)
    };

    let mut current = match current.take() {
        Some(c) => c,
        None => infer_expr(arg, ctx, params, TypeGoal::NONE)?,
    };

    // Detect the `(alias).field` shape: arg is a single-identifier ColumnRef
    // whose identifier is a relation alias in scope (not a column). PG emits
    // `column alias.field does not exist` for this case (whereas
    // `(c.col).field` produces `column "field" not found in data type T`).
    // The alias hint only applies to the first indirection step — chained
    // accesses past that point are no longer at the relation boundary.
    let arg_is_bare_alias: Option<&str> = if let Some(node::Node::ColumnRef(cr)) = arg.node.as_ref()
    {
        let parts = extract_string_fields(&cr.fields);
        match parts.as_slice() {
            [single] if scope.find_source(single).is_some() => {
                cr.fields.iter().find_map(|f| match f.node.as_ref()? {
                    node::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                })
            }
            _ => None,
        }
    } else {
        None
    };

    let steps = &ind.indirection[start_step..];
    let mut i = 0;
    while i < steps.len() {
        match steps[i].node.as_ref() {
            Some(node::Node::String(s)) => {
                let alias_hint = if i == 0 { arg_is_bare_alias } else { None };
                current = resolve_composite_field(&current, &s.sval, snapshot, alias_hint)?;
                i += 1;
            }
            Some(node::Node::AIndices(_)) => {
                // PG's transformIndirection gathers every consecutive
                // subscript into one list and hands the whole run to
                // transformContainerSubscripts, so e.g. a slice anywhere in
                // `arr[1:2][1]` makes the entire run a slice.
                let run = subscript_run(&steps[i..]);
                i += run.len();
                current = transform_container_subscripts(&current, &run, ctx, params)?;
            }
            _ => {
                return Err(AnalyzeError::Unsupported(format!(
                    "unsupported indirection step: {:?}",
                    steps[i].node.as_ref().map(std::mem::discriminant)
                )));
            }
        }
    }

    Ok(current)
}

/// Look up `record_fields` for a `ColumnRef` that resolves to a scope column
/// carrying named output columns (set when its producing expression was a
/// FuncCall with `out_args`). Returns `None` if the ref doesn't resolve or
/// the column isn't a record.
fn column_ref_record_fields(cr: &protobuf::ColumnRef, scope: &Scope) -> Option<Vec<RecordField>> {
    let parts = extract_string_fields(&cr.fields);
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return None,
    };
    let col = scope.resolve_column(table, column, None).ok()?;
    col.record_fields.clone()
}

/// If `fc` names a function with declared `out_args` (TABLE/OUT args),
/// return them so indirection steps can match against named output columns.
/// Returns `Ok(None)` when the function has no out_args — the caller should
/// fall back to generic composite/record handling.
fn resolve_funccall_out_args(
    fc: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<Vec<RecordField>>, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let parts = extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Ok(None),
    };

    // Infer arg types against the caller's scope so column refs in the
    // arguments resolve to concrete types — needed for polymorphic
    // substitution (`anyelement` → element-of-array etc.) when the function
    // has polymorphic out args like `_pg_expandarray(anyarray) RETURNS
    // (x anyelement, n int)`.
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for arg in &fc.args {
        let t = infer_expr(arg, ctx, params, TypeGoal::NONE)
            .map(|e| e.type_oid)
            .unwrap_or(oid::UNKNOWN);
        arg_types.push(t);
    }

    let Ok(notation) = crate::functions::CallNotation::of(fc) else {
        return Ok(None);
    };
    let resolved = match crate::functions::resolve_function(
        snapshot, schema, name, &arg_types, &notation, false, None,
    ) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    if resolved.out_args.is_empty() {
        Ok(None)
    } else {
        Ok(Some(RecordField::from_out_args(&resolved.out_args)))
    }
}

/// Look up `field_name` inside a composite type's field list. The resulting
/// nullability is the combination of the enclosing value being nullable AND
/// the field's own `not_null` declaration — either one being nullable makes
/// the access nullable.
///
/// When the enclosing value carries an inline `record_fields` shape (e.g.
/// `(ROW(1, 'x'::text)).f2`), we use that directly — no snapshot lookup,
/// since pseudo `record` has no `TypeKind::Composite` to consult.
///
/// `relation_alias` is `Some(alias)` when the indirection's argument was a
/// bare relation reference (`(alias).field` form). PG emits a different
/// error wording in that case — `column alias.field does not exist` — so
/// the analyzer mirrors it to keep `pg_sanity` aligned. For chained or
/// composite-column accesses (`(c.col).field`, `((c).x).field`), pass
/// `None` and the wording switches to PG's `column "f" not found in data
/// type T`.
fn resolve_composite_field(
    current: &ExprType,
    field_name: &str,
    snapshot: &PgCatalog,
    relation_alias: Option<&str>,
) -> Result<ExprType, AnalyzeError> {
    if let Some(shape) = current.record_fields.as_deref() {
        let field = shape.iter().find(|f| f.name == field_name).ok_or_else(|| {
            AnalyzeError::UndefinedColumn(format!(
                "could not identify column \"{field_name}\" in record data type"
            ))
        })?;
        // Field's full ExprType (including any nested record shape) is
        // already on `field.ty`; just OR the enclosing nullability in.
        return Ok(ExprType {
            type_oid: field.ty.type_oid,
            nullable: current.nullable || field.ty.nullable,
            typmod: field.ty.typmod,
            collation: field.ty.collation,
            record_fields: field.ty.record_fields.clone(),
        });
    }

    // Domain-over-composite needs unwrapping to see the composite fields.
    let base_oid = snapshot.unwrap_domain(current.type_oid);
    let type_entry = snapshot.get_type(base_oid).ok_or_else(|| {
        AnalyzeError::UndefinedType(format!(
            "internal: composite field access .{field_name} over unknown type OID {}",
            base_oid.get()
        ))
    })?;

    let pg_type_name = crate::ddl::util::format_type_for_message(snapshot, base_oid);
    let Some(relid) = type_entry.typrelid else {
        return Err(AnalyzeError::Unsupported(format!(
            "column notation .{field_name} applied to type {pg_type_name}, \
             which is not a composite type"
        )));
    };
    if type_entry.typtype != TypType::Composite {
        return Err(AnalyzeError::Unsupported(format!(
            "column notation .{field_name} applied to type {pg_type_name}, \
             which is not a composite type"
        )));
    }
    let fields = snapshot.attributes_of(relid);
    let field = fields
        .iter()
        .find(|f| f.attname == field_name)
        .ok_or_else(|| {
            let msg = if let Some(alias) = relation_alias {
                format!(
                    "column {} does not exist",
                    crate::qualified_name::QualifiedName::new(alias, field_name),
                )
            } else {
                format!(
                    "column \"{field_name}\" not found in data type {}",
                    type_entry.typname
                )
            };
            AnalyzeError::UndefinedColumn(msg)
        })?;

    Ok(ExprType::scalar(
        field.atttypid,
        current.nullable || !field.attnotnull,
    ))
}

/// `ARRAY[expr1, expr2, …]` literal — result type is the common element type
/// promoted to its array. Empty arrays fall back to `UNKNOWN` so that the
/// enclosing cast (`ARRAY[]::text[]`) takes over.
pub(crate) fn infer_array_expr(
    arr: &protobuf::AArrayExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    if arr.elements.is_empty() {
        return Ok(ExprType::scalar(oid::UNKNOWN, false));
    }
    let mut element_types = Vec::with_capacity(arr.elements.len());
    let mut any_nullable = false;
    for elem in &arr.elements {
        let t = infer_expr(elem, ctx, params, TypeGoal::NONE)?;
        element_types.push(t.type_oid);
        any_nullable |= t.nullable;
    }
    let common = match coerce::find_common_type(&element_types, snapshot) {
        Some(t) => t,
        None => {
            // PG: `ARRAY types <X> and <Y> cannot be matched`. Use the
            // first two distinct concrete types in the message so the
            // diagnostic is stable regardless of ordering tie-breaks.
            let mut concrete: Vec<PgTypeOid> = element_types
                .iter()
                .copied()
                .filter(|&t| t != oid::UNKNOWN)
                .collect();
            concrete.dedup();
            let names: Vec<String> = concrete
                .iter()
                .take(2)
                .map(|&t| crate::ddl::util::format_type_for_message(snapshot, t))
                .collect();
            // PG (SQLSTATE 42804) emits this exactly as `ARRAY types A and
            // B cannot be matched`. We keep the same wording so the
            // `pglite_sanity` mirror passes; demote to `Invalid` so the
            // generic `type mismatch: …` prefix from `TypeMismatch::Display`
            // doesn't leak in front of it.
            let a = names.first().map(String::as_str).unwrap_or("?");
            let b = names.get(1).map(String::as_str).unwrap_or("?");
            return Err(crate::pgmsg::types_cannot_be_matched(
                "ARRAY",
                a,
                b,
                "",
                Some(format!(
                    "cast the elements to a common type, e.g. `elem::{a}`"
                )),
            )
            .finalize_implicit());
        }
    };
    // Pass 2: back-fill UNKNOWN elements with the resolved common type so
    // embedded params get pinned and string-literal contents are validated
    // (PG rejects `ARRAY[1, 'x']` at parse time with `invalid input syntax`).
    if common != oid::UNKNOWN {
        for (elem, &t) in arr.elements.iter().zip(&element_types) {
            if t == oid::UNKNOWN {
                coerce_unknown_to(elem, ctx, params, common)?;
            }
        }
    }
    // PG collapses array dimensions into the same type OID:
    // `ARRAY[ARRAY[1,2], ARRAY[3,4]]` is `int4[]`, not `int4[][]`. So if the
    // common element type is already an array, reuse it instead of trying to
    // wrap it (`array_type_of` on an array type returns `None`).
    let common_is_array = snapshot
        .get_type(common)
        .is_some_and(|t| t.typcategory == TypCategory::Array);
    let array_oid = if common_is_array {
        common
    } else {
        snapshot.array_type_of(common).unwrap_or(oid::UNKNOWN)
    };
    // An ARRAY[...] constructor is never NULL itself — it's always at least
    // an empty array. Element nullability is tracked separately by Rust's
    // `Option<T>` inside `Vec<T>`.
    let _ = any_nullable;
    Ok(ExprType::scalar(array_oid, false))
}

/// The leading run of consecutive `[…]` steps of an indirection list.
pub(crate) fn subscript_run(steps: &[protobuf::Node]) -> Vec<&protobuf::AIndices> {
    steps
        .iter()
        .map_while(|s| match s.node.as_ref() {
            Some(node::Node::AIndices(ai)) => Some(&**ai),
            _ => None,
        })
        .collect()
}

/// PG's `MAXDIM`: the most subscripts an array reference can carry.
const MAXDIM: usize = 6;

/// Apply one run of subscripts to a container value — PG's
/// `transformContainerSubscripts` (parse_node.c) followed by the type's
/// subscript handler (`array_subscript_transform` in arraysubs.c or
/// `jsonb_subscript_transform` in jsonbsubs.c). The same transform serves
/// fetches and assignments (`UPDATE … SET arr[1] = …`): the returned type is
/// both the fetched value's type and the type an assigned value must have.
///
/// - A domain subscripts as its base type (`transformContainerType`).
/// - A type is subscriptable iff it has a subscript handler: every type with
///   a `typelem` (true arrays, `int2vector`, `name`, `point`, …) uses the
///   array handler, and `jsonb` its own. Anything else is `cannot subscript
///   type T because it does not support subscripting` (42804).
/// - `is_slice` is decided for the whole run: one `lo:hi` makes every
///   subscript a slice (a plain `[i]` then means `[1:i]`).
/// - Arrays coerce each bound to int4 in assignment context; a slice keeps
///   the array type, an element fetch yields the element type, and both keep
///   the array's typmod.
/// - jsonb rejects slices and accepts a subscript coercible (implicitly) to
///   int4 or text, trying int4 first; an unknown one becomes text.
pub(crate) fn transform_container_subscripts(
    container: &ExprType,
    subscripts: &[&protobuf::AIndices],
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let base = snapshot.unwrap_domain(container.type_oid);
    let typmod = if base == container.type_oid {
        container.typmod
    } else {
        snapshot.effective_typmod(container.type_oid, None)
    };
    let entry = snapshot.get_type(base);
    let is_slice = subscripts.iter().any(|ai| ai.is_slice);
    let bound_span = |bound: &protobuf::Node| {
        crate::error::node_location(bound).and_then(crate::error::SourceSpan::from_node_qname)
    };

    if entry.is_some_and(|t| {
        t.typname == "jsonb" && snapshot.namespace_name(t.typnamespace) == Some("pg_catalog")
    }) {
        for ai in subscripts {
            if is_slice {
                let span = ai
                    .uidx
                    .as_deref()
                    .or(ai.lidx.as_deref())
                    .and_then(bound_span);
                return Err(
                    crate::pgmsg::jsonb_subscript_does_not_support_slices(span).finalize_implicit()
                );
            }
            let Some(bound) = ai.uidx.as_deref() else {
                continue;
            };
            let t = infer_expr(bound, ctx, params, TypeGoal::NONE)?;
            if t.type_oid == oid::UNKNOWN {
                coerce_unknown_to(bound, ctx, params, oid::TEXT)?;
            } else if !can_coerce(t.type_oid, oid::INT4, CoercionContext::Implicit, snapshot)
                && !can_coerce(t.type_oid, oid::TEXT, CoercionContext::Implicit, snapshot)
            {
                let name = crate::ddl::util::format_type_for_message(snapshot, t.type_oid);
                return Err(
                    crate::pgmsg::subscript_type_not_supported(&name, bound_span(bound))
                        .finalize_implicit(),
                );
            }
        }
        return Ok(ExprType::scalar(base, true));
    }

    let Some(elem) = entry.and_then(|t| t.typelem) else {
        let name = crate::ddl::util::format_type_for_message(snapshot, base);
        return Err(crate::pgmsg::cannot_subscript_type(&name).finalize_implicit());
    };

    let mut any_bound_nullable = false;
    for ai in subscripts {
        // A non-slice subscript inside a slice run means `[1:i]`: its
        // implicit lower bound is the constant 1, so only `uidx` is walked.
        let lower = if is_slice { ai.lidx.as_deref() } else { None };
        for bound in [lower, ai.uidx.as_deref()].into_iter().flatten() {
            let t = match infer_expr(bound, ctx, params, TypeGoal::assignment(oid::INT4)) {
                Ok(t) => t,
                Err(AnalyzeError::TypeMismatch { .. }) => {
                    return Err(
                        crate::pgmsg::array_subscript_must_be_integer(bound_span(bound))
                            .with_primary_label("this is not an integer")
                            .finalize_implicit(),
                    );
                }
                Err(e) => return Err(e),
            };
            any_bound_nullable = any_bound_nullable || t.nullable;
        }
    }
    if subscripts.len() > MAXDIM {
        return Err(AnalyzeError::Invalid(format!(
            "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
            subscripts.len()
        )));
    }

    if is_slice {
        // `arr[lo:hi]` keeps the (base) array type. NULL iff the array or a
        // bound is NULL — out-of-range bounds yield an empty array.
        Ok(ExprType::scalar_with_collation(
            base,
            container.nullable || any_bound_nullable,
            typmod,
            container.collation,
        ))
    } else {
        // `arr[i]` is always nullable (out-of-bounds → NULL, even with a
        // non-null array and index).
        Ok(ExprType::scalar_with_collation(
            elem,
            true,
            typmod,
            container.collation,
        ))
    }
}
