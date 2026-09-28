//! SQL/XML expressions: `XMLCONCAT`, `XMLELEMENT`, `XMLFOREST`,
//! `XMLPARSE`, `XMLPI`, `XMLROOT`, `IS DOCUMENT` (an `XmlExpr`) and
//! `XMLSERIALIZE` — ports of PG's `transformXmlExpr` / `transformXmlSerialize`
//! (parse_expr.c). The plain-function members of the family (`xpath`,
//! `xmlagg`, `xmlcomment`, …) resolve through the ordinary function path.

use super::*;

const XML: PgTypeOid = PgTypeOid::from_raw(142);

/// `coerce_to_specific_type` (parse_coerce.c): an untyped input takes the
/// type; a typed one needs an assignment cast, else `argument of X must be
/// type T, not type U` (42804).
fn coerce_to_specific_type(
    e: &protobuf::Node,
    target: PgTypeOid,
    construct: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let t = infer_expr(e, ctx, params, TypeGoal::NONE)?;
    if t.type_oid == oid::UNKNOWN {
        coerce_unknown_to(e, ctx, params, target)?;
    } else if t.type_oid != target
        && !can_coerce(t.type_oid, target, CoercionContext::Assignment, snapshot)
    {
        let name = |t| crate::ddl::util::format_type_for_message(snapshot, t);
        return Err(AnalyzeError::DatatypeMismatch(format!(
            "argument of {construct} must be type {}, not type {}",
            name(target),
            name(t.type_oid)
        )));
    }
    Ok(t)
}

/// `transformXmlExpr`. XMLELEMENT's attributes and XMLFOREST's elements
/// (`named_args`) are transformed without coercion and must be named — by
/// `AS name` or by being a column reference — and XMLELEMENT's attribute
/// names must be distinct; the other arguments are coerced per function
/// (xml, text, boolean, int4). `IS DOCUMENT` is boolean, everything else
/// xml.
pub(crate) fn infer_xml_expr(
    x: &protobuf::XmlExpr,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    use protobuf::XmlExprOp as Op;
    let op = protobuf::XmlExprOp::try_from(x.op).unwrap_or(Op::Undefined);

    let mut named_nullable = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for na in &x.named_args {
        let Some(node::Node::ResTarget(rt)) = na.node.as_ref() else {
            continue;
        };
        let Some(val) = rt.val.as_deref() else {
            continue;
        };
        let t = infer_expr(val, ctx, params, TypeGoal::NONE)?;
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref() {
            params.mark_indeterminate_locked(p.number);
        }
        named_nullable.push(t.nullable);
        let name = if !rt.name.is_empty() {
            rt.name.clone()
        } else if let Some(node::Node::ColumnRef(cr)) = val.node.as_ref()
            && let Some(col) = extract_string_fields(&cr.fields).pop()
        {
            col
        } else {
            return Err(AnalyzeError::SyntaxError(
                if op == Op::IsXmlelement {
                    "unnamed XML attribute value must be a column reference"
                } else {
                    "unnamed XML element value must be a column reference"
                }
                .into(),
            ));
        };
        if op == Op::IsXmlelement && names.contains(&name) {
            return Err(AnalyzeError::SyntaxError(format!(
                "XML attribute name \"{name}\" appears more than once"
            )));
        }
        names.push(name);
    }

    let mut args = Vec::with_capacity(x.args.len());
    for (i, e) in x.args.iter().enumerate() {
        let t = match op {
            Op::IsXmlconcat => coerce_to_specific_type(e, XML, "XMLCONCAT", ctx, params)?,
            Op::IsXmlforest => coerce_to_specific_type(e, XML, "XMLFOREST", ctx, params)?,
            Op::IsXmlparse if i == 0 => {
                coerce_to_specific_type(e, oid::TEXT, "XMLPARSE", ctx, params)?
            }
            // The grammar's PRESERVE / STRIP WHITESPACE boolean constant.
            Op::IsXmlparse => infer_expr(e, ctx, params, TypeGoal::NONE)?,
            Op::IsXmlpi => coerce_to_specific_type(e, oid::TEXT, "XMLPI", ctx, params)?,
            // XMLROOT(xml, VERSION text, STANDALONE int4 constant).
            Op::IsXmlroot => {
                let target = match i {
                    0 => XML,
                    1 => oid::TEXT,
                    _ => oid::INT4,
                };
                coerce_to_specific_type(e, target, "XMLROOT", ctx, params)?
            }
            Op::IsDocument => coerce_to_specific_type(e, XML, "IS DOCUMENT", ctx, params)?,
            // XMLELEMENT content: no coercion.
            _ => {
                let t = infer_expr(e, ctx, params, TypeGoal::NONE)?;
                if let Some(node::Node::ParamRef(p)) = e.node.as_ref() {
                    params.mark_indeterminate_locked(p.number);
                }
                t
            }
        };
        args.push(t);
    }

    let any_null = |ts: &[ExprType]| ts.iter().any(|t| t.nullable);
    let (type_oid, nullable) = match op {
        Op::IsDocument => (oid::BOOL, any_null(&args)),
        // Always builds an element.
        Op::IsXmlelement => (XML, false),
        // NULL inputs are skipped; NULL only when every input is.
        Op::IsXmlforest => (XML, named_nullable.iter().all(|&n| n)),
        Op::IsXmlconcat => (XML, args.iter().all(|t| t.nullable)),
        // XMLPARSE / XMLPI / XMLROOT: a NULL input gives NULL.
        _ => (XML, any_null(&args)),
    };
    Ok(ExprType::scalar(type_oid, nullable))
}

/// `transformXmlSerialize`: the value is coerced to xml; the result is the
/// target type, which must be implicitly castable from text (`cannot cast
/// XMLSERIALIZE result to integer`, 42846).
pub(crate) fn infer_xml_serialize(
    xs: &protobuf::XmlSerialize,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<ExprType, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let e = xs
        .expr
        .as_deref()
        .ok_or_else(|| AnalyzeError::Internal("XMLSERIALIZE without argument".into()))?;
    let arg = coerce_to_specific_type(e, XML, "XMLSERIALIZE", ctx, params)?;
    let target = resolve_type_name(xs.type_name.as_ref(), snapshot)?;
    let typmod = match xs.type_name.as_ref() {
        Some(tn) => crate::typmod::encode(snapshot, target, &tn.typmods)
            .map_err(|e| AnalyzeError::Invalid(e.to_string()))?,
        None => None,
    };
    if target != oid::TEXT && !can_coerce(oid::TEXT, target, CoercionContext::Implicit, snapshot) {
        return Err(AnalyzeError::Invalid(format!(
            "cannot cast XMLSERIALIZE result to {}",
            crate::ddl::util::format_type_for_message(snapshot, target)
        )));
    }
    Ok(ExprType::scalar_with_typmod(target, arg.nullable, typmod))
}
