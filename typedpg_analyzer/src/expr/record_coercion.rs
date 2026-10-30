//! PG's `coerce_record_to_complex` (parse_coerce.c): a value of the pseudo
//! type `record` coerced to a composite type (or a domain over one).
//!
//! `can_coerce_type` lets `record` reach any composite in every context,
//! but `coerce_record_to_complex` can only rebuild the value when it knows
//! its fields: a `ROW(...)` constructor, or a whole-row reference to a FROM
//! entry without a row type of its own (a subquery, CTE, VALUES list, join
//! or function returning several columns — `makeWholeRowVar`'s `record`),
//! whose columns `expandNSItemVars` lists. Any other `record` (a column of a
//! subquery holding a row, a function returning `record`, a CASE, …) fails
//! with no detail. The fields are then coerced one by one, in the record's
//! own coercion context (explicit for a cast, assignment for a stored
//! value, implicit for a function argument), and the result is a row
//! constructor of the target type — never NULL itself.

use super::*;

/// `cannot cast type record to T`, with PG's DETAIL as the hint.
fn cannot_cast(
    target: PgTypeOid,
    detail: Option<String>,
    span: Option<crate::error::SourceSpan>,
    snapshot: &PgCatalog,
) -> AnalyzeError {
    crate::pgmsg::cannot_cast_record_to(
        &crate::ddl::util::format_type_for_message(snapshot, target),
        detail,
        span,
    )
    .finalize_implicit()
}

/// Where PG's `exprLocation` puts an error about the record `node`.
fn record_span(node: &protobuf::Node) -> Option<crate::error::SourceSpan> {
    match node.node.as_ref() {
        Some(node::Node::RowExpr(row)) => crate::error::SourceSpan::from_node_token(row.location),
        Some(node::Node::ColumnRef(cr)) => crate::error::SourceSpan::from_node_qname(cr.location),
        _ => crate::error::expr_span(node),
    }
}

/// The FROM entry the column reference `cr` names as a whole row of type
/// `record` (`s.*`, or a bare `s` no column claims — `transformColumnRef`'s
/// fallback to `transformWholeRowRef`): one with no row type of its own.
fn whole_row_source<'s>(
    cr: &protobuf::ColumnRef,
    scope: &'s Scope,
) -> Option<&'s crate::scope::TableSource> {
    let name = |n: &protobuf::Node| match n.node.as_ref() {
        Some(node::Node::String(s)) => Some(s.sval.clone()),
        _ => None,
    };
    let alias = match cr.fields.as_slice() {
        [.., qualifier, last] if matches!(last.node.as_ref(), Some(node::Node::AStar(_))) => {
            name(qualifier)?
        }
        [only] => {
            let only = name(only)?;
            if !matches!(
                scope.resolve_column(None, &only, None),
                Err(AnalyzeError::UndefinedColumn(_))
            ) {
                return None;
            }
            only
        }
        _ => return None,
    };
    scope
        .find_source(&alias)
        .filter(|src| src.source_qn.is_none() && src.whole_row == crate::scope::WholeRow::Record)
}

/// Coerce the `record`-typed expression `node` to the composite `target`
/// (or a domain over one) in `context`, as `coerce_record_to_complex`
/// does. Returns the rebuilt row, typed as the composite (domains
/// smashed), its fields carrying the coerced values' nullability.
pub(crate) fn coerce_record_to_complex(
    node: &protobuf::Node,
    target: PgTypeOid,
    context: CoercionContext,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    // coerce_type hands a `record` cast to `record` back as it is, so
    // `ROW(1, 'a')::record::t` and `s::record::t` still name their fields.
    let mut node = node;
    while let Some(node::Node::TypeCast(cast)) = node.node.as_ref()
        && let Some(tn) = cast.type_name.as_ref()
        && tn.array_bounds.is_empty()
        && crate::ddl::util::resolve_type_name(tn, snapshot) == Some(oid::RECORD)
        && let Some(arg) = cast.arg.as_deref()
    {
        node = arg;
    }
    let composite = snapshot.unwrap_domain(target);
    let fields = snapshot
        .get_type(composite)
        .and_then(|t| t.typrelid)
        .map(|relid| snapshot.attributes_of(relid).to_vec())
        .unwrap_or_default();
    let span = record_span(node);
    let too_few = || {
        cannot_cast(
            target,
            Some("Input has too few columns.".into()),
            span,
            snapshot,
        )
    };
    let too_many = || {
        cannot_cast(
            target,
            Some("Input has too many columns.".into()),
            span,
            snapshot,
        )
    };
    let mismatch = |from: PgTypeOid, to: PgTypeOid, column: usize| {
        cannot_cast(
            target,
            Some(format!(
                "Cannot cast type {} to {} in column {column}.",
                crate::ddl::util::format_type_for_message(snapshot, from),
                crate::ddl::util::format_type_for_message(snapshot, to),
            )),
            span,
            snapshot,
        )
    };
    let mut shape = Vec::with_capacity(fields.len());
    let whole_row = match node.node.as_ref() {
        Some(node::Node::ColumnRef(cr)) => whole_row_source(cr, ctx.scope).map(|s| (cr, s)),
        _ => None,
    };
    match (node.node.as_ref(), whole_row) {
        (Some(node::Node::RowExpr(row)), _) => {
            // transformRowExpr has expanded the stars already.
            let args = expand_row_args(&row.args, ctx, params)?;
            for (i, field) in fields.iter().enumerate() {
                let Some(arg) = args.get(i) else {
                    return Err(too_few());
                };
                let mut scratch = params.clone();
                let own = infer_expr(arg, ctx, &mut scratch, TypeGoal::NONE)?;
                // A nested `record` goes through coerce_record_to_complex
                // again (the goal below), failing for the nested type.
                let nested = own.type_oid == oid::RECORD
                    && crate::coerce::is_complex(field.atttypid, snapshot);
                if own.type_oid != oid::UNKNOWN
                    && !nested
                    && !crate::coerce::can_coerce(own.type_oid, field.atttypid, context, snapshot)
                {
                    return Err(mismatch(own.type_oid, field.atttypid, i + 1));
                }
                let t = infer_expr(
                    arg,
                    ctx,
                    params,
                    TypeGoal {
                        coercion: context,
                        ..TypeGoal::assignment(field.atttypid)
                    },
                )?;
                // Each value is kept, coerced to its field's type: still
                // non-NULL when no cast runs (same type), an untyped
                // literal is read by the field type's input function, or a
                // nested row is rebuilt. Any other cast may map a value to
                // NULL (`int4(jsonb)` on a JSON null).
                let literal = matches!(
                    arg.node.as_ref(),
                    Some(node::Node::AConst(c)) if !c.isnull
                );
                let kept = own.type_oid == field.atttypid
                    || (own.type_oid == oid::UNKNOWN && literal)
                    || nested;
                shape.push(RecordField {
                    name: field.attname.clone(),
                    ty: ExprType {
                        record_fields: t.record_fields.filter(|_| nested),
                        ..ExprType::scalar_with_typmod(
                            field.atttypid,
                            t.nullable || !kept,
                            field.atttypmod,
                        )
                    },
                });
            }
            if args.len() > fields.len() {
                return Err(too_many());
            }
        }
        (_, Some((cr, source))) => {
            // expandNSItemVars: one Var per column of the entry, each NULL
            // where a reference to that column would be (its own
            // nullability, an outer join's nullable side, a grouping set
            // leaving it out).
            let columns = &source.columns;
            crate::grouping::note_expanded_whole_row(
                cr.location,
                &source.alias,
                columns.iter().map(|c| c.name.clone()).collect(),
            );
            for (i, field) in fields.iter().enumerate() {
                let Some(column) = columns.get(i) else {
                    return Err(too_few());
                };
                let own = ExprType {
                    record_fields: column.record_fields.clone(),
                    ..ExprType::scalar_with_typmod(
                        column.type_oid,
                        ctx.null_ctx.is_nullable(
                            &column.table_alias,
                            &column.name,
                            column.base_not_null,
                        ),
                        column.typmod,
                    )
                };
                if own.type_oid == oid::RECORD
                    && crate::coerce::is_complex(field.atttypid, snapshot)
                {
                    // A Var of the entry's column, not a whole row: there
                    // is nothing to rebuild it from.
                    return Err(cannot_cast(field.atttypid, None, span, snapshot));
                }
                if own.type_oid != oid::UNKNOWN
                    && !crate::coerce::can_coerce(own.type_oid, field.atttypid, context, snapshot)
                {
                    return Err(mismatch(own.type_oid, field.atttypid, i + 1));
                }
                let nullable =
                    crate::expr::literals::assignment_nullable(&own, field.atttypid, snapshot);
                shape.push(RecordField {
                    name: field.attname.clone(),
                    ty: ExprType {
                        record_fields: own
                            .record_fields
                            .clone()
                            .filter(|_| own.type_oid == field.atttypid),
                        ..ExprType::scalar_with_typmod(field.atttypid, nullable, field.atttypmod)
                    },
                });
            }
            if columns.len() > fields.len() {
                return Err(too_many());
            }
        }
        _ => return Err(cannot_cast(target, None, span, snapshot)),
    }
    Ok(ExprType {
        record_fields: Some(shape.into()),
        ..ExprType::scalar(composite, false)
    })
}
