use super::*;
use typedpg_pg_query::ast;

// ──────────────────────────────────────────────────────────────────────────────
// Indirection (`(expr).field`, `(expr)[i]`)
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve `(expr).field1.field2…` chains. Each step either names a field in
/// a composite (String) or subscripts an array / `jsonb` (`AIndices`).
/// Array subscripting handles both element access (`arr[n]`) and slicing
/// (`arr[1:3]`, which keeps the array type). `jsonb` / `json` subscripting
/// (`data['key']`, `data[0]`, chained) yields `jsonb` at every step.
pub(crate) fn infer_indirection(
    ind: &ast::AIndirection,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let Ctx {
        scope, snapshot, ..
    } = ctx;
    let arg = &ind.arg;
    arg.as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("indirection without arg".into()))?;

    // A `record` arg's fields come with its inferred type: a function's OUT
    // / TABLE args, a ROW constructor's, a column's (carried from the
    // subquery or CTE that produced it).
    let mut current = infer_expr(arg.as_ref(), ctx, params, TypeGoal::NONE)?;
    let arg_is_column = matches!(
        arg.as_ref(),
        Some(ast::Node::ColumnRef(cr)) if column_ref_is_column(cr, scope)
    );

    // The `(alias).field` shape: arg is a FROM entry's whole row (see
    // [`whole_row_entry`]). Only the first indirection step is at the
    // relation boundary.
    let entry = whole_row_entry(ind, scope);

    let steps = &ind.indirection[..];
    let mut i = 0;
    while i < steps.len() {
        match steps[i].as_ref() {
            Some(ast::Node::String(s)) => {
                if i == 0 && arg_is_column {
                    record_var_shape_visible(&current, snapshot)?;
                }
                // `(t).f` is the column `t.f` (see [`entry_column`]); a name
                // that is no column of the entry is the call `f(t)` (below).
                let selected = match entry.filter(|_| i == 0) {
                    Some((alias, src)) => {
                        match entry_column(alias, src, steps[i].as_ref(), arg.as_ref()) {
                            Some(column) => {
                                Ok(infer_expr(Some(&column), ctx, params, TypeGoal::NONE)?)
                            }
                            // unknown_attribute: errmsg("column %s.%s does not
                            // exist", rte->eref->aliasname, attname) — the names
                            // as they are, neither quoted nor escaped.
                            None => Err(AnalyzeError::UndefinedColumn(format!(
                                "column {alias}.{} does not exist",
                                s.sval
                            ))),
                        }
                    }
                    None => resolve_composite_field(&current, &s.sval, snapshot),
                };
                current = match selected {
                    Ok(field) => field,
                    // No such field: `(x).f` is the call `f(x)`, on the
                    // value selected so far (transformIndirection).
                    Err(e) => {
                        let value = if i == 0 {
                            arg.clone()
                        } else {
                            Some(ast::Node::AIndirection(NodeBox::new(ast::AIndirection {
                                arg: ind.arg.clone(),
                                indirection: steps[..i].to_vec(),
                            })))
                        };
                        let location = crate::error::node_location(arg.as_ref()).unwrap_or(-1);
                        let bound =
                            crate::ddl::policies::bound_indirection_function(location, &s.sval);
                        if let Some(oid) = bound
                            && let Some(call) = super::func_call::infer_bound_column_notation_call(
                                oid,
                                value.clone(),
                                location,
                                ctx,
                                params,
                            )
                        {
                            call?
                        } else {
                            match super::func_call::infer_column_notation_call(
                                &s.sval, value, location, ctx, params,
                            ) {
                                Some(call) => call?,
                                None => return Err(e),
                            }
                        }
                    }
                };
                i += 1;
            }
            Some(ast::Node::AIndices(_)) => {
                // PG's transformIndirection gathers every consecutive
                // subscript into one list and hands the whole run to
                // transformContainerSubscripts, so e.g. a slice anywhere in
                // `arr[1:2][1]` makes the entire run a slice.
                let run = subscript_run(&steps[i..]);
                // `(array_agg(x))[1]` over rows: the first value.
                let first_aggregated = i == 0
                    && current.elem_nullable == Some(false)
                    && matches!(run.as_slice(), [ai] if !ai.is_slice
                    && ai.lidx.is_none()
                    && matches!(
                        ai.uidx.as_ref(),
                        Some(ast::Node::AConst(c)) if matches!(
                            &**c,
                            ast::AConst {
                                val: Some(ast::AConstValue::Integer(one)),
                                ..
                            } if one.ival == 1
                        )
                    ))
                    && super::func_call::is_nonempty_1d_array_agg(
                        arg.as_ref(),
                        &current,
                        ctx,
                        params,
                    );
                i += run.len();
                current = transform_container_subscripts(&current, &run, ctx, params)?;
                if first_aggregated {
                    current.nullable = false;
                }
            }
            // `(expr).*` only expands in a SELECT list (see
            // `expand_indirection_star`); anywhere else PG refuses it.
            Some(ast::Node::AStar(_)) => {
                return Err(AnalyzeError::Invalid(
                    "row expansion via \"*\" is not supported here".into(),
                ));
            }
            _ => {
                return Err(AnalyzeError::Unsupported(format!(
                    "typedpg does not support {} in an indirection (`expr.field`, \
                     `expr[i]`) yet",
                    steps[i]
                        .as_ref()
                        .map_or_else(|| "an empty step".to_owned(), crate::error::node_kind)
                )));
            }
        }
    }

    Ok(current)
}

/// The FROM entry whose whole row the argument of `ind` is — a
/// single-name `ColumnRef` (or `t.*`) naming an entry rather than a
/// column (transformColumnRef prefers a column) — with the alias it goes
/// by. PG reports a missing field of such a row as `column alias.field
/// does not exist` (whereas `(c.col).field` gives `column "field" not
/// found in data type T`).
pub(crate) fn whole_row_entry<'a>(
    ind: &'a ast::AIndirection,
    scope: &'a Scope,
) -> Option<(&'a str, &'a crate::scope::TableSource)> {
    let Some(ast::Node::ColumnRef(cr)) = ind.arg.as_ref() else {
        return None;
    };
    if column_ref_is_column(cr, scope) {
        return None;
    }
    let alias = match cr.fields.as_slice() {
        [name] | [name, Some(ast::Node::AStar(_))] => match name.as_ref()? {
            ast::Node::String(s) => s.sval.as_str(),
            _ => return None,
        },
        _ => return None,
    };
    Some((alias, scope.find_source(alias)?))
}

/// The column `alias.field` a field selection `(alias).field` on entry
/// `src`'s whole row (`arg`) stands for, if the entry has one of that
/// name: ParseComplexProjection looks the name up among the entry's
/// columns (scanNSItemForColumn) — by the names it exposes (an alias
/// list's), a relation's system columns included — and makes it that
/// column's Var: `(t).a` is `t.a`, `(t).ctid` is `t.ctid`. Nothing else
/// of the row's type is reachable that way.
pub(crate) fn entry_column(
    alias: &str,
    src: &crate::scope::TableSource,
    field: Option<&ast::Node>,
    arg: Option<&ast::Node>,
) -> Option<ast::Node> {
    let Some(ast::Node::String(f)) = field else {
        return None;
    };
    if !src
        .columns
        .iter()
        .chain(&src.system_columns)
        .any(|c| c.name == f.sval)
    {
        return None;
    }
    let location = match arg {
        Some(ast::Node::ColumnRef(cr)) => cr.location,
        _ => -1,
    };
    Some(ast::Node::ColumnRef(NodeBox::new(ast::ColumnRef {
        fields: vec![
            Some(ast::Node::String(NodeBox::new(ast::String {
                sval: alias.to_owned(),
            }))),
            field.cloned(),
        ],
        location,
    })))
}

/// The column `t.f` that `ind`, a single field selection `(t).f` on a FROM
/// entry's whole row, is ([`entry_column`]) — `None` for any other
/// indirection.
pub(crate) fn whole_row_field_column(ind: &ast::AIndirection, scope: &Scope) -> Option<ast::Node> {
    let [field] = ind.indirection.as_slice() else {
        return None;
    };
    let (alias, src) = whole_row_entry(ind, scope)?;
    ind.arg.as_ref()?;
    entry_column(alias, src, field.as_ref(), ind.arg.as_ref())
}

/// Whether `cr` names a column (a Var in PG) rather than a FROM entry's
/// whole row.
pub(crate) fn column_ref_is_column(cr: &ast::ColumnRef, scope: &Scope) -> bool {
    let parts = extract_string_fields(&cr.fields);
    if parts.len() != cr.fields.len() {
        return false; // `t.*`
    }
    scope
        .resolve_column_names(&parts, None)
        .is_some_and(|r| r.is_ok())
}

/// PG's `expandRecordVariable`: the row shape of a `record` column (of a
/// subquery, CTE, VALUES list or function) is the shape of the expression
/// behind it, and when PG has none (`(ROW(1, ROW(2, 3))).f2`, a CASE, a
/// VALUES cell) it fails outright with `record type has not been
/// registered` (42809) — before looking at any field name.
pub(crate) fn record_var_shape_visible(
    value: &ExprType,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    if snapshot.unwrap_domain(value.type_oid) == oid::RECORD
        && value.record_fields.as_ref().is_none_or(|s| s.hidden)
    {
        return Err(AnalyzeError::WrongObjectType(
            "record type has not been registered".into(),
        ));
    }
    Ok(())
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
/// A field of a FROM entry's whole row (`(alias).field`) is resolved as
/// that entry's column instead ([`entry_column`]).
pub(super) fn resolve_composite_field(
    current: &ExprType,
    field_name: &str,
    snapshot: &PgCatalog,
) -> Result<ExprType, AnalyzeError> {
    // Domain-over-composite needs unwrapping to see the composite fields.
    let base_oid = snapshot.unwrap_domain(current.type_oid);
    // A shape PG can't see (see `RecordShape`) has no field PG could find.
    if base_oid == oid::RECORD
        && let Some(shape) = current.record_fields.as_ref().filter(|s| !s.hidden)
    {
        let field = shape.iter().find(|f| f.name == field_name).ok_or_else(|| {
            AnalyzeError::UndefinedColumn(format!(
                "could not identify column \"{field_name}\" in record data type"
            ))
        })?;
        // Field's full ExprType (including any nested record shape) is
        // already on `field.ty`; just OR the enclosing nullability in. A
        // record selected out of a record is a FieldSelect PG types as
        // plain `record`: its shape is hidden from PG from here on.
        let record_fields = field.ty.record_fields.clone().map(|s| {
            if field.ty.type_oid == oid::RECORD {
                RecordShape::hidden(s.fields)
            } else {
                s
            }
        });
        return Ok(ExprType {
            type_oid: field.ty.type_oid,
            nullable: current.nullable || field.ty.nullable,
            typmod: field.ty.typmod,
            collation: field.ty.collation,
            explicit_collation: false,
            // A field of a FROM entry's row is its column, a Var (PG's
            // ParseComplexProjection): none when the column has none.
            no_collation: field.ty.no_collation.map(|_| super::NoCollation::Unset),
            // A FieldSelect keeps its collation when planned (it folds only
            // into a field of that collation), but a FROM entry's column
            // may not: one planning replaces may change it.
            planned: match field.ty.planned {
                super::Planned::AsParsed => super::Planned::AsParsed,
                _ => super::Planned::Unknown,
            },
            record_fields,
            elem_nullable: field.ty.elem_nullable,
            elem_refine: field.ty.elem_refine.clone(),
            refine: field.ty.refine.clone(),
            // A FieldSelect: an `unknown` field (over an untyped literal,
            // `(ROW(1, 'x')).f2`) is no literal any more.
            unknown_value: field.ty.type_oid == oid::UNKNOWN,
        });
    }

    // A `record` whose row shape is unknown here (a `RETURNS record`
    // function without a column definition list): ParseFuncOrColumn finds
    // no field and reports the column, not the type (42703).
    if base_oid == oid::RECORD {
        return Err(AnalyzeError::UndefinedColumn(format!(
            "could not identify column \"{field_name}\" in record data type"
        )));
    }
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
            AnalyzeError::UndefinedColumn(format!(
                "column \"{field_name}\" not found in data type {}",
                type_entry.typname
            ))
        })?;
    crate::ddl::depend::note_column(relid, field_name);

    Ok(ExprType::scalar_with_typmod(
        field.atttypid,
        current.nullable || composite_field_nullable(current, field_name),
        field.atttypmod,
    ))
}

/// Whether field `name` of a non-NULL composite value `value` may be NULL.
/// A composite type's fields have no NOT NULL of their own: even a table's
/// row type takes `ROW(NULL)::t`, and a column of type `t` holds whatever
/// was stored. Only a row read from a relation (whose whole-row reference
/// carries the relation's columns as its shape) keeps their NOT NULL.
pub(crate) fn composite_field_nullable(value: &ExprType, name: &str) -> bool {
    value
        .record_fields
        .as_deref()
        .and_then(|shape| shape.iter().find(|f| f.name == name))
        .is_none_or(|f| f.ty.nullable)
}

/// `ARRAY[expr1, expr2, …]` with no target type (see [`transform_array_expr`]).
pub(crate) fn infer_array_expr(
    arr: &ast::AArrayExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    transform_array_expr(arr, ctx, params, None)
}

/// The array type an `ARRAY[…]` is being cast to (`ARRAY[…]::T[]`): the
/// (domain-unwrapped) array type, its element type, and the target typmod.
#[derive(Clone, Copy)]
pub(crate) struct ArrayTarget {
    pub array_type: PgTypeOid,
    pub element_type: PgTypeOid,
    pub typmod: Option<i32>,
}

/// PG's `transformArrayExpr` (parse_expr.c). Elements that are themselves
/// `ARRAY[…]` (or any array-typed value, except `int2vector`/`oidvector`)
/// make the result multi-dimensional, and the element type is then the
/// sub-arrays' array type.
///
/// - With a `target` (the array is the operand of a cast to an array type,
///   see `transformTypeCast`), every element is coerced *explicitly* to the
///   target element type (the target array type for sub-arrays): an
///   untyped element or `$N` takes that type, a typed one needs an explicit
///   cast path (`cannot cast type X to Y`). `ARRAY[]::int[]` is fine.
/// - Without one, the element type is the common type of the elements
///   (`ARRAY types X and Y cannot be matched` / `ARRAY could not convert
///   type X to Y`), and an empty `ARRAY[]` is `cannot determine type of
///   empty array`.
///
/// The result's typmod is the one every element agrees on (PG's
/// `exprTypmod` of an ArrayExpr), or the target's.
pub(crate) fn transform_array_expr(
    arr: &ast::AArrayExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    target: Option<ArrayTarget>,
) -> Result<ExprType, AnalyzeError> {
    transform_array_expr_typed(arr, ctx, params, target).map(|(t, _)| t)
}

/// [`transform_array_expr`], and whether the array was already of the
/// target's type without it: it has elements, and each is of the type the
/// target converts it to on its own (no untyped literal, parameter or
/// `ROW(…)` the target types). Without a target, whether it has elements.
pub(crate) fn transform_array_expr_typed(
    arr: &ast::AArrayExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    target: Option<ArrayTarget>,
) -> Result<(ExprType, bool), AnalyzeError> {
    let Ctx { snapshot, .. } = ctx;
    let mut elems = Vec::with_capacity(arr.elements.len());
    // Per element: `Some(typed)` for a sub-array, `None` for an element
    // typed on its own (judged against the converted-to type below), and
    // `Some(false)` for one the target types.
    let mut typed_alone: Vec<Option<bool>> = Vec::with_capacity(arr.elements.len());
    let mut multidims = false;
    for elem in &arr.elements {
        let t = if let Some(ast::Node::AArrayExpr(sub)) = elem.as_ref() {
            multidims = true;
            let (t, typed) = transform_array_expr_typed(sub, ctx, params, target)?;
            typed_alone.push(Some(typed));
            t
        } else {
            // coerce_record_to_complex: a `ROW(...)` element of a cast to
            // an array of composites is converted field by field, so its
            // arity and fields are checked against the target's
            // (`ARRAY[ROW(1, 2, 3)]::t[]` is `cannot cast type record to t`).
            let goal = match (elem.as_ref(), target) {
                (Some(ast::Node::RowExpr(_)), Some(tg))
                    if coerce::is_complex(tg.element_type, snapshot) =>
                {
                    TypeGoal {
                        coercion: CoercionContext::Explicit,
                        ..TypeGoal::assignment(tg.element_type)
                    }
                }
                _ => TypeGoal::NONE,
            };
            let by_target = goal.type_oid != TypeGoal::NONE.type_oid
                || matches!(elem.as_ref(), Some(ast::Node::ParamRef(_)));
            let t = infer_expr(elem.as_ref(), ctx, params, goal)?;
            typed_alone.push(
                if by_target || t.unknown_value || t.type_oid == oid::UNKNOWN {
                    Some(false)
                } else {
                    None
                },
            );
            if is_sub_array_type(t.type_oid, snapshot) {
                multidims = true;
            }
            t
        };
        elems.push(t);
    }

    let name = |t: PgTypeOid| crate::ddl::util::format_type_for_message(snapshot, t);
    let (array_type, coerce_type, hard) = match target {
        Some(tg) => (
            tg.array_type,
            if multidims {
                tg.array_type
            } else {
                tg.element_type
            },
            true,
        ),
        None => {
            if elems.is_empty() {
                return Err(crate::pgmsg::cannot_determine_type_of_empty_array(
                    crate::error::SourceSpan::from_location(arr.location),
                )
                .finalize_implicit());
            }
            let types: Vec<PgTypeOid> = elems.iter().map(|t| t.type_oid).collect();
            let common = match coerce::select_common_type(&types, snapshot) {
                Ok(t) => t,
                Err(coerce::CommonTypeError::Mismatch(a, b)) => {
                    let nodes: Vec<Option<&ast::Node>> =
                        arr.elements.iter().map(Option::as_ref).collect();
                    let span = super::conditional::failing_input_span(&types, &nodes, b, snapshot);
                    let (a, b) = (name(a), name(b));
                    return Err(crate::pgmsg::types_cannot_be_matched(
                        "ARRAY",
                        &a,
                        &b,
                        "",
                        Some(format!(
                            "cast the elements to a common type, e.g. `elem::{a}`"
                        )),
                        span,
                    )
                    .finalize_implicit());
                }
                Err(coerce::CommonTypeError::CannotConvert { from, to }) => {
                    let nodes: Vec<Option<&ast::Node>> =
                        arr.elements.iter().map(Option::as_ref).collect();
                    let span =
                        super::conditional::failing_input_span(&types, &nodes, from, snapshot);
                    return Err(crate::pgmsg::could_not_convert_type(
                        "ARRAY",
                        &name(from),
                        &name(to),
                        span,
                    )
                    .finalize_implicit());
                }
            };
            let array_type = if multidims {
                if snapshot.get_type(common).and_then(|t| t.typelem).is_none() {
                    return Err(AnalyzeError::UndefinedObject(format!(
                        "could not find element type for data type {}",
                        name(common)
                    )));
                }
                common
            } else {
                snapshot
                    .array_type_of(common)
                    .ok_or_else(|| crate::pgmsg::no_array_type_for(&name(common)))?
            };
            (array_type, common, false)
        }
    };

    for (elem, t) in arr.elements.iter().zip(&elems) {
        if t.type_oid == oid::UNKNOWN && hard {
            // transformArrayExpr coerces each element to the cast's element
            // type with coerce_to_target_type(..., COERCION_EXPLICIT): an
            // `unknown` value that is no literal reaches a string type
            // through I/O (`ARRAY['a'::text::unknown]::text[]`).
            swallow_unless_literal(infer_expr(
                elem.as_ref(),
                ctx,
                params,
                TypeGoal {
                    coercion: CoercionContext::Explicit,
                    ..TypeGoal::implicit(coerce_type)
                },
            ))?;
        } else if t.type_oid == oid::UNKNOWN {
            coerce_unknown_to(elem.as_ref(), ctx, params, coerce_type)?;
        } else if hard {
            if t.type_oid == oid::RECORD && coerce::is_complex(coerce_type, snapshot) {
                // ... and a `record` element to a composite goes through
                // coerce_record_to_complex, field by field, explicitly:
                // `ARRAY[ROW(1, 'x')]::pair[]` checks each field's
                // coercion, and its column count.
                super::coerce_record_to_complex(
                    elem.as_ref(),
                    coerce_type,
                    CoercionContext::Explicit,
                    ctx,
                    params,
                )?;
            } else if !coerce::can_cast_explicit(t.type_oid, coerce_type, snapshot) {
                let span = crate::error::node_location(elem.as_ref())
                    .and_then(crate::error::SourceSpan::from_node_qname);
                return Err(crate::error::RawError::invalid(
                    format!(
                        "cannot cast type {} to {}",
                        name(t.type_oid),
                        name(coerce_type)
                    ),
                    span,
                    None,
                )
                .finalize_implicit());
            }
        } else if t.type_oid != coerce_type
            && !can_coerce(t.type_oid, coerce_type, CoercionContext::Implicit, snapshot)
        {
            return Err(crate::pgmsg::could_not_convert_type(
                "ARRAY",
                &name(t.type_oid),
                &name(coerce_type),
                crate::error::expr_span(elem.as_ref()),
            )
            .finalize_implicit());
        }
    }

    // ExecEvalArrayExpr builds a multidimensional array from sub-arrays of
    // one shape; a NULL or empty sub-array is allowed only when every one
    // is (the result is then empty). Sub-arrays the query fixes the shape
    // of that can't agree fail every evaluation — and, when they are all
    // constants, `eval_const_expressions` evaluates the array while
    // planning: then even a statement that is planned but not run
    // (EXPLAIN) fails, where the planner folds it.
    let executes = crate::resolve::executes();
    let folded = !executes
        && plan_time_checks()
        && arr
            .elements
            .iter()
            .all(|e| certainly_constant(e.as_ref(), ctx.snapshot));
    if multidims && (executes || folded) {
        let shapes: Vec<SubArrayShape> = arr
            .elements
            .iter()
            .map(|e| sub_array_shape(e.as_ref(), coerce_type, ctx, params))
            .collect();
        let mut dims = shapes.iter().filter_map(|s| match s {
            SubArrayShape::Dims(d) => Some(d),
            _ => None,
        });
        if let Some(first) = dims.next()
            && (dims.any(|d| d != first) || shapes.contains(&SubArrayShape::Absent))
        {
            let e = crate::error::RawError::invalid(
                "multidimensional arrays must have array expressions with matching dimensions"
                    .to_string(),
                crate::error::SourceSpan::from_location(arr.location),
                None,
            )
            .finalize_implicit();
            if !folded {
                return Err(e);
            }
            super::plan_time_result(Some(arr.location), Err(e))?;
        }
    }

    let typmod = match (target, elems.first()) {
        (_, None) => None,
        (Some(tg), _) => tg.typmod,
        // Coercing an element to a different type drops its typmod, so only
        // elements already of the common type can agree on one.
        (None, Some(first)) => elems
            .iter()
            .all(|t| t.type_oid == coerce_type && t.typmod == first.typmod)
            .then_some(first.typmod)
            .flatten(),
    };
    // An ARRAY[...] constructor is never NULL itself — it's always at least
    // an empty array.
    let state = derive_collation(&elems, array_type, snapshot)?;
    // Its elements are the listed values (unless it is multidimensional,
    // where they are the sub-arrays' elements), each converted to the
    // element type — which may map one to NULL (`int4(jsonb)` for a JSON
    // null, a user's non-strict cast).
    let elem_nullable = (!multidims).then(|| {
        elems.iter().any(|t| {
            t.nullable
                || (t.type_oid != oid::UNKNOWN
                    && t.type_oid != coerce_type
                    && super::literals::cast_function_can_return_null(
                        t.type_oid,
                        coerce_type,
                        snapshot,
                    ))
        })
    });
    // What its non-NULL elements are: each listed value's (a sub-array's
    // elements'), converted to the element type; a NULL constant is none.
    let parts: Vec<crate::refine::Refinement> = arr
        .elements
        .iter()
        .zip(&elems)
        .filter(|(e, _)| !matches!(e.as_ref(), Some(ast::Node::AConst(c)) if c.isnull))
        .map(|(_, t)| {
            if multidims {
                t.elem_refine_converted(coerce_type, snapshot)
            } else {
                t.refine
                    .converted(t.type_oid, coerce_type, snapshot)
                    .and(&snapshot.domain_refinement(coerce_type))
            }
        })
        .collect();
    let elem_refine = crate::refine::Refinement::either(&parts);
    // A target's typmod converts every element (`ARRAY[…]::varchar(3)[]`).
    let typed = !elems.is_empty()
        && target.is_none_or(|tg| tg.typmod.is_none())
        && typed_alone
            .iter()
            .zip(&elems)
            .all(|(alone, t)| alone.unwrap_or(t.type_oid == coerce_type));
    Ok((
        ExprType::scalar_with_typmod(array_type, false, typmod)
            .with_collation(state)
            .with_elem_nullable(elem_nullable)
            .with_elem_refine(elem_refine),
        typed,
    ))
}

/// Whether an `ARRAY[…]` element of type `t` is a sub-array, making the
/// constructor multidimensional (transformArrayExpr: an array type, other
/// than `int2vector` / `oidvector`).
fn is_sub_array_type(t: PgTypeOid, snapshot: &PgCatalog) -> bool {
    snapshot.get_type(t).is_some_and(|e| {
        !matches!(e.typname.as_str(), "int2vector" | "oidvector")
            && e.typcategory == TypCategory::Array
            && e.typelem.is_some()
    })
}

/// What the query fixes of a sub-array of a multidimensional `ARRAY[…]`.
#[derive(PartialEq, Eq)]
enum SubArrayShape {
    /// NULL or an empty array, whatever the data.
    Absent,
    /// A non-empty array of these dimensions, as `(lower bound, length)`
    /// pairs, whatever the data.
    Dims(Vec<(i64, i64)>),
    /// Depends on the data.
    Unknown,
}

/// Whether `node`, an element of an `ARRAY[...]`, is certainly a constant
/// once parsed and folded: a literal (NULL included), an untyped one cast
/// to a type that is no domain (`'{1}'::int[]`, `NULL::int[]` — the
/// literal is read as that type; a domain's check stays a node), or an
/// `ARRAY[...]` of such, which `eval_const_expressions` evaluates.
fn certainly_constant(node: Option<&ast::Node>, snapshot: &PgCatalog) -> bool {
    match node {
        Some(ast::Node::AConst(_)) => true,
        Some(ast::Node::TypeCast(c)) => {
            let untyped = match c.arg.as_ref() {
                Some(ast::Node::AConst(a)) => {
                    a.isnull || matches!(a.val, Some(ast::AConstValue::String(_)))
                }
                _ => false,
            };
            untyped
                && c.type_name
                    .as_ref()
                    .and_then(|tn| crate::ddl::util::lookup_type_name(tn, snapshot).ok())
                    .is_some_and(|t| snapshot.unwrap_domain(t) == t)
        }
        Some(ast::Node::AArrayExpr(a)) => a
            .elements
            .iter()
            .all(|e| certainly_constant(e.as_ref(), snapshot)),
        _ => false,
    }
}

/// The [`SubArrayShape`] of `node`, a sub-array of a multidimensional
/// `ARRAY[…]` of type `array_type`: an `ARRAY[…]` constructor (cast or not)
/// over values — or over sub-arrays all of one fixed shape — a NULL, or an
/// array literal (untyped, read as `array_type`, or cast to an array type).
fn sub_array_shape(
    node: Option<&ast::Node>,
    array_type: PgTypeOid,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> SubArrayShape {
    match node {
        Some(ast::Node::AConst(c)) if c.isnull => SubArrayShape::Absent,
        Some(ast::Node::AConst(c)) => match &c.val {
            Some(ast::AConstValue::String(sv)) => {
                array_literal_shape(&sv.sval, array_type, ctx.snapshot)
            }
            _ => SubArrayShape::Unknown,
        },
        Some(ast::Node::TypeCast(c)) => match &c.arg {
            // A literal is read by the input function of the type it is
            // cast to — `array_in` for an array type.
            arg @ Some(ast::Node::AConst(ac))
                if matches!(ac.val, Some(ast::AConstValue::String(_))) =>
            {
                match c
                    .type_name
                    .as_ref()
                    .and_then(|tn| crate::ddl::util::lookup_type_name(tn, ctx.snapshot).ok())
                {
                    Some(t) => sub_array_shape(arg.as_ref(), t, ctx, params),
                    None => SubArrayShape::Unknown,
                }
            }
            arg @ Some(_) => sub_array_shape(arg.as_ref(), array_type, ctx, params),
            None => SubArrayShape::Unknown,
        },
        Some(ast::Node::AArrayExpr(a)) => {
            if a.elements.is_empty() {
                return SubArrayShape::Absent;
            }
            let n = a.elements.len() as i64;
            // transformArrayExpr: a sub-ARRAY, or any element of an array
            // type, makes it multidimensional.
            let sub_array = |e: Option<&ast::Node>| {
                matches!(e, Some(ast::Node::AArrayExpr(_))) || {
                    let mut scratch = params.clone();
                    infer_expr(e, ctx, &mut scratch, TypeGoal::NONE)
                        .is_ok_and(|t| is_sub_array_type(t.type_oid, ctx.snapshot))
                }
            };
            if a.elements.iter().any(|e| sub_array(e.as_ref())) {
                // Its own sub-arrays: one shape for all of them, or (all
                // NULL / empty) an empty array.
                let inner: Vec<SubArrayShape> = a
                    .elements
                    .iter()
                    .map(|e| sub_array_shape(e.as_ref(), array_type, ctx, params))
                    .collect();
                if inner.iter().all(|s| *s == SubArrayShape::Absent) {
                    return SubArrayShape::Absent;
                }
                return match inner.first() {
                    Some(SubArrayShape::Dims(d)) if inner.iter().all(|s| *s == inner[0]) => {
                        SubArrayShape::Dims(
                            std::iter::once((1, n)).chain(d.iter().copied()).collect(),
                        )
                    }
                    _ => SubArrayShape::Unknown,
                };
            }
            // Values (not arrays, which would make it multidimensional with
            // their own shapes) make a one-dimensional array of `n`.
            let scalar = |e: Option<&ast::Node>| {
                let mut scratch = params.clone();
                infer_expr(e, ctx, &mut scratch, TypeGoal::NONE).is_ok_and(|t| {
                    t.type_oid != oid::UNKNOWN
                        && array_element_type(ctx.snapshot, t.type_oid).is_none()
                }) || matches!(e, Some(ast::Node::AConst(_)))
            };
            if a.elements.iter().all(|e| scalar(e.as_ref())) {
                SubArrayShape::Dims(vec![(1, n)])
            } else {
                SubArrayShape::Unknown
            }
        }
        _ => SubArrayShape::Unknown,
    }
}

/// The [`SubArrayShape`] of the literal `content` read as `array_type`: by
/// `array_in` when that is a true array type (no domain, whose CHECK may
/// fail first; no `int2vector`, read by its own input function).
fn array_literal_shape(
    content: &str,
    array_type: PgTypeOid,
    snapshot: &PgCatalog,
) -> SubArrayShape {
    let Some(elem) = snapshot.get_type(array_type).and_then(|t| t.typelem) else {
        return SubArrayShape::Unknown;
    };
    if snapshot.array_type_of(elem) != Some(array_type) {
        return SubArrayShape::Unknown;
    }
    let dims = crate::literal_input::array_delimiter(elem, snapshot)
        .and_then(|delim| crate::array_input::array_dimensions(content, delim));
    match dims {
        Some(d) if d.is_empty() => SubArrayShape::Absent,
        Some(d) => SubArrayShape::Dims(d),
        // A malformed literal fails on its own.
        None => SubArrayShape::Unknown,
    }
}

/// PG's `ExpandIndirectionStar` for a SELECT-list `(expr).*`: one output
/// column per field of the composite value `expr` (named after the field),
/// each typed like `(expr).field`. `Ok(None)` when the indirection does not
/// end in `.*`. The value must have a known row shape: a composite type
/// (domains unwrapped) or an anonymous record whose fields are known.
pub(crate) fn expand_indirection_star(
    ind: &ast::AIndirection,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<Vec<(String, ExprType)>>, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let Some((last, prefix)) = ind.indirection.split_last() else {
        return Ok(None);
    };
    if !matches!(last.as_ref(), Some(ast::Node::AStar(_))) {
        return Ok(None);
    }
    let arg = &ind.arg;
    arg.as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("indirection without arg".into()))?;
    let container = if prefix.is_empty() {
        infer_expr(arg.as_ref(), ctx, params, TypeGoal::NONE)?
    } else {
        let inner = ast::AIndirection {
            arg: ind.arg.clone(),
            indirection: prefix.to_vec(),
        };
        infer_indirection(&inner, ctx, params)?
    };

    let base = snapshot.unwrap_domain(container.type_oid);
    // A shape hidden from PG fails below like an unknown one (see
    // `RecordShape`).
    if base == oid::RECORD
        && let Some(fields) = container.record_fields.as_ref().filter(|s| !s.hidden)
    {
        return Ok(Some(
            fields
                .iter()
                .map(|f| {
                    let mut ty = f.ty.clone();
                    ty.nullable |= container.nullable;
                    // A leaf as to its collation: none stays none, a
                    // conflict below it is just none.
                    ty.no_collation = ty.no_collation.map(|_| super::NoCollation::Unset);
                    // Each column is a FieldSelect: a record one hides
                    // its shape from PG.
                    if ty.type_oid == oid::RECORD {
                        ty.record_fields = ty.record_fields.map(|s| RecordShape::hidden(s.fields));
                    }
                    (f.name.clone(), ty)
                })
                .collect(),
        ));
    }
    let relid = snapshot
        .get_type(base)
        .filter(|t| t.typtype == TypType::Composite)
        .and_then(|t| t.typrelid);
    let Some(relid) = relid else {
        // get_expr_result_tupdesc: an anonymous record of unknown shape vs.
        // a scalar.
        let msg = if base == oid::RECORD {
            "record type has not been registered".to_string()
        } else {
            format!(
                "type {} is not composite",
                crate::ddl::util::format_type_for_message(snapshot, base)
            )
        };
        return Err(AnalyzeError::WrongObjectType(msg));
    };
    let mut out = Vec::new();
    for attr in snapshot
        .attributes_of(relid)
        .iter()
        .filter(|a| a.attnum > 0)
    {
        let mut ty = resolve_composite_field(&container, &attr.attname, snapshot)?;
        ty.typmod = attr.atttypmod;
        out.push((attr.attname.clone(), ty));
    }
    Ok(Some(out))
}

/// The argument and name of a call PG may read as a column projection:
/// one argument, an unqualified name, no aggregate / window / VARIADIC
/// decoration and no argument name.
fn projection_candidate(func: &ast::FuncCall) -> Option<(Option<&ast::Node>, &ast::String)> {
    let [arg] = func.args.as_slice() else {
        return None;
    };
    let [name] = func.funcname.as_slice() else {
        return None;
    };
    let Some(ast::Node::String(name)) = name.as_ref() else {
        return None;
    };
    if !func.agg_order.is_empty()
        || func.agg_filter.is_some()
        || func.agg_star
        || func.agg_distinct
        || func.agg_within_group
        || func.over.is_some()
        || func.func_variadic
        || matches!(arg.as_ref(), Some(ast::Node::NamedArgExpr(_)))
    {
        return None;
    }
    Some((arg.as_ref(), name))
}

/// For a call no function matched: PG's last try reads it as a column
/// projection, and on a `record` column whose shape it can't see that
/// fails in `expandRecordVariable` (see [`record_var_shape_visible`])
/// rather than as a missing function. `None` when that doesn't apply.
pub(crate) fn unmatched_projection_error(
    func: &ast::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Option<AnalyzeError> {
    let (arg, _) = projection_candidate(func)?;
    let Some(ast::Node::ColumnRef(cr)) = arg else {
        return None;
    };
    if !column_ref_is_column(cr, ctx.scope) {
        return None;
    }
    let t = infer_expr(arg, ctx, params, TypeGoal::NONE).ok()?;
    record_var_shape_visible(&t, ctx.snapshot).err()
}

/// PG's column-projection reading of a one-argument call
/// (`ParseFuncOrColumn` → `ParseComplexProjection`): `x(t)` with an
/// unqualified name, no decoration and a composite / record argument is
/// `(t).x` when the argument's row type has a field `x` — tried before any
/// function lookup. When the call doesn't qualify or no such field exists,
/// the caller goes on to function resolution.
pub(crate) fn try_column_projection(
    func: &ast::FuncCall,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ColumnProjection, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let Some((arg, name)) = projection_candidate(func) else {
        return Ok(ColumnProjection::Not(None));
    };
    let t = infer_expr(arg, ctx, params, TypeGoal::NONE)?;
    let base = snapshot.unwrap_domain(t.type_oid);
    let complex = base == oid::RECORD
        || snapshot
            .get_type(base)
            .is_some_and(|e| e.typtype == TypType::Composite);
    if !complex {
        return Ok(ColumnProjection::Not(Some(t)));
    }
    let has_field = match &t.record_fields {
        // A hidden shape has no field PG could find.
        Some(fields) if base == oid::RECORD => {
            !fields.hidden && fields.iter().any(|f| f.name == name.sval)
        }
        _ => snapshot
            .get_type(base)
            .and_then(|e| e.typrelid)
            .is_some_and(|relid| {
                snapshot
                    .attributes_of(relid)
                    .iter()
                    .any(|a| a.attnum > 0 && a.attname == name.sval)
            }),
    };
    if !has_field {
        return Ok(ColumnProjection::Not(Some(t)));
    }
    resolve_composite_field(&t, &name.sval, snapshot).map(ColumnProjection::Field)
}

/// What [`try_column_projection`] found a one-argument call to be.
pub(crate) enum ColumnProjection {
    /// The projection `(t).x`: the field.
    Field(ExprType),
    /// A call: the type its argument was inferred to, if it was — the call
    /// takes it as is, as inferring the argument again would, at each level
    /// of nested calls, double the work (2^depth).
    Not(Option<ExprType>),
}

/// The leading run of consecutive `[…]` steps of an indirection list.
pub(crate) fn subscript_run(steps: &[Option<ast::Node>]) -> Vec<&ast::AIndices> {
    steps
        .iter()
        .map_while(|s| match s.as_ref() {
            Some(ast::Node::AIndices(ai)) => Some(&**ai),
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
    subscripts: &[&ast::AIndices],
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

    // hstore_subscript_transform (hstore_subs.c): one text subscript, no
    // slices; the result is text.
    if snapshot
        .pg_type
        .get(&base)
        .and_then(|t| t.typsubscript)
        .and_then(|h| snapshot.pg_proc.get(&h))
        .is_some_and(|h| h.proname == "hstore_subscript_handler")
    {
        if is_slice || subscripts.len() != 1 {
            return Err(AnalyzeError::FeatureNotSupported(
                "hstore allows only one subscript".into(),
            ));
        }
        if let bound @ Some(_) = &subscripts[0].uidx {
            match infer_expr(bound.as_ref(), ctx, params, TypeGoal::assignment(oid::TEXT)) {
                Ok(_) => {}
                Err(AnalyzeError::TypeMismatch { .. }) => {
                    return Err(AnalyzeError::DatatypeMismatch(
                        "hstore subscript must have type text".into(),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        return Ok(ExprType::scalar(oid::TEXT, true));
    }
    let bound_span = |bound: Option<&ast::Node>| {
        crate::error::node_location(bound).and_then(crate::error::SourceSpan::from_node_qname)
    };

    if entry.is_some_and(|t| {
        t.typname == "jsonb" && snapshot.namespace_name(t.typnamespace) == Some("pg_catalog")
    }) {
        for ai in subscripts {
            if is_slice {
                let span = bound_span(if ai.uidx.is_some() {
                    ai.uidx.as_ref()
                } else {
                    ai.lidx.as_ref()
                });
                return Err(
                    crate::pgmsg::jsonb_subscript_does_not_support_slices(span).finalize_implicit()
                );
            }
            let bound = &ai.uidx;
            if bound.is_none() {
                continue;
            }
            let t = infer_expr(bound.as_ref(), ctx, params, TypeGoal::NONE)?;
            if t.type_oid == oid::UNKNOWN {
                coerce_unknown_to(bound.as_ref(), ctx, params, oid::TEXT)?;
            } else if !can_coerce(t.type_oid, oid::INT4, CoercionContext::Implicit, snapshot)
                && !can_coerce(t.type_oid, oid::TEXT, CoercionContext::Implicit, snapshot)
            {
                let name = crate::ddl::util::format_type_for_message(snapshot, t.type_oid);
                return Err(crate::pgmsg::subscript_type_not_supported(
                    &name,
                    bound_span(bound.as_ref()),
                )
                .finalize_implicit());
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
        let lower = if is_slice { Some(&ai.lidx) } else { None };
        for bound in [lower, Some(&ai.uidx)]
            .into_iter()
            .flatten()
            .filter(|b| b.is_some())
        {
            let t = match infer_expr(bound.as_ref(), ctx, params, TypeGoal::assignment(oid::INT4)) {
                Ok(t) => t,
                Err(AnalyzeError::TypeMismatch { .. }) => {
                    return Err(crate::pgmsg::array_subscript_must_be_integer(bound_span(
                        bound.as_ref(),
                    ))
                    .with_primary_label("this is not an integer")
                    .finalize_implicit());
                }
                Err(e) => return Err(e),
            };
            // transformContainerSubscripts converts the subscript to
            // `integer`, a conversion the planner folds.
            if plan_time_checks()
                && let Some(e) =
                    crate::fold::conversion_error(bound.as_ref(), oid::INT4, None, snapshot)
                && let Some(e) = super::plan_time_error(
                    crate::error::node_location(bound.as_ref()),
                    e.at(bound.as_ref()),
                )
            {
                return Err(e);
            }
            any_bound_nullable = any_bound_nullable || t.nullable;
        }
    }
    if subscripts.len() > MAXDIM {
        return Err(AnalyzeError::Invalid(format!(
            "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
            subscripts.len()
        )));
    }

    // A SubscriptingRef derives its collation from the container's (its
    // subscripts are integers).
    if is_slice {
        // `arr[lo:hi]` keeps the (base) array type. NULL iff the array or a
        // bound is NULL — out-of-range bounds yield an empty array.
        Ok(
            ExprType::scalar_with_typmod(base, container.nullable || any_bound_nullable, typmod)
                .with_collation(derive_collation([container], base, snapshot)?)
                // Some of its elements.
                .with_elem_refine(container.elem_refine.clone()),
        )
    } else {
        // `arr[i]` is always nullable (out-of-bounds → NULL, even with a
        // non-null array and index).
        Ok(ExprType::scalar_with_typmod(elem, true, typmod)
            .with_collation(derive_collation([container], elem, snapshot)?)
            // One of its elements.
            .with_refine(container.elem_refine.clone()))
    }
}
