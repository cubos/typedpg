//! `CALL procedure(args)`.

use super::*;
use crate::pg_catalog::{ArgMode, ProKind};

/// `CALL proc(args)` — PG's `transformCallStmt` / `ParseFuncOrColumn` with
/// `proc_call`: the name must resolve to a *procedure* whose full parameter
/// list (IN, INOUT and — since PG 14 — OUT, which take a placeholder
/// argument) fits the arguments. A plain function is 42809 `… is not a
/// procedure`, no fit is 42883 `procedure … does not exist`. The result row
/// is the procedure's INOUT / OUT parameters.
///
/// Overload resolution is PG's first two passes (exact or implicitly
/// coercible arguments, untyped literals / parameters matching anything);
/// polymorphic procedures are not modelled.
pub(crate) fn analyze_call(
    call: &protobuf::CallStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    let fc = call
        .funccall
        .as_deref()
        .ok_or_else(|| AnalyzeError::Unsupported("CALL without a procedure call".into()))?;
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedFunction(format!(
                "invalid procedure name: {parts:?}"
            )));
        }
    };
    let span = crate::error::SourceSpan::from_node_qname(fc.location);

    let scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
    let args: Vec<&protobuf::Node> = fc.args.iter().map(functions::call_arg_value).collect();
    let mut arg_types = Vec::with_capacity(args.len());
    for a in &args {
        arg_types.push(expr::infer_expr(a, ctx, params, TypeGoal::NONE)?.type_oid);
    }
    let signature_text = arg_types
        .iter()
        .map(|&t| crate::ddl::util::format_type_for_message(snapshot, t))
        .collect::<Vec<_>>()
        .join(", ");
    let written = parts.join(".");

    // Every parameter a CALL passes an argument for.
    let call_signature = |p: &crate::pg_catalog::PgProc| -> Vec<PgTypeOid> {
        if p.proargmodes.is_empty() {
            p.proargtypes.clone()
        } else {
            p.proallargtypes.clone()
        }
    };
    let found = snapshot.find_functions(schema, name);
    let fits = |sig: &[PgTypeOid], exact: bool| {
        sig.len() == arg_types.len()
            && sig.iter().zip(&arg_types).all(|(&want, &got)| {
                got == oid::UNKNOWN
                    || snapshot.unwrap_domain(got) == snapshot.unwrap_domain(want)
                    || (!exact
                        && crate::coerce::can_coerce(
                            got,
                            want,
                            crate::coerce::CoercionContext::Implicit,
                            snapshot,
                        ))
            })
    };
    let procs: Vec<&crate::pg_catalog::PgProc> = found
        .iter()
        .copied()
        .filter(|p| matches!(p.prokind, ProKind::Procedure))
        .collect();
    let mut matches: Vec<&crate::pg_catalog::PgProc> = procs
        .iter()
        .copied()
        .filter(|p| fits(&call_signature(p), true))
        .collect();
    if matches.is_empty() {
        matches = procs
            .iter()
            .copied()
            .filter(|p| fits(&call_signature(p), false))
            .collect();
    }
    let proc = match matches.as_slice() {
        [p] => *p,
        [] => {
            // A function (not a procedure) that fits: 42809.
            let is_function = functions::resolve_function(
                snapshot,
                schema,
                name,
                &arg_types,
                &functions::CallNotation::of(fc)?,
                false,
                None,
            )
            .is_ok();
            if is_function {
                return Err(crate::error::RawError::new(
                    AnalyzeError::WrongObjectType(format!(
                        "{written}({signature_text}) is not a procedure"
                    )),
                    span,
                    Some("To call a function, use SELECT.".into()),
                )
                .finalize_implicit());
            }
            return Err(crate::error::RawError::new(
                AnalyzeError::UndefinedFunction(format!(
                    "procedure {written}({signature_text}) does not exist"
                )),
                span,
                Some(
                    "No procedure matches the given name and argument types. You might need to \
                     add explicit type casts."
                        .into(),
                ),
            )
            .finalize_implicit());
        }
        _ => {
            return Err(crate::error::RawError::new(
                AnalyzeError::AmbiguousFunction(format!(
                    "procedure {written}({signature_text}) is not unique"
                )),
                span,
                None,
            )
            .finalize_implicit());
        }
    };

    // Coerce each argument to its parameter: pins bare parameters and
    // validates untyped literals (`CALL p('x')` → 22P02 for an integer).
    let sig = call_signature(proc);
    for (a, &want) in args.iter().zip(&sig) {
        expr::coerce_unknown_to(a, ctx, params, want)?;
    }

    let columns = proc
        .proargmodes
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, ArgMode::Out | ArgMode::InOut))
        .map(|(i, _)| RawColumn {
            name: proc.proargnames.get(i).cloned().unwrap_or_default(),
            type_oid: proc.proallargtypes.get(i).copied().unwrap_or(oid::UNKNOWN),
            nullable: true,
            typmod: None,
            collation: None,
            record_fields: None,
        })
        .collect();
    Ok((columns, None))
}
