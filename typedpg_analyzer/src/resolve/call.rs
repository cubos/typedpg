//! `CALL procedure(args)`.

use super::*;

/// `CALL proc(args)` — PG's `transformCallStmt`: the arguments are
/// transformed, then resolved by `ParseFuncOrColumn` with `proc_call`,
/// i.e. ordinary function resolution (named notation, defaults, VARIADIC,
/// polymorphism, the unknown-literal preference rules) over candidates
/// whose OUT parameters take arguments too (`include_out_arguments`, PG
/// 14+). The winner must be a procedure (a plain function is 42809 `… is not
/// a procedure`); no fit is 42883 `procedure … does not exist`. The result
/// row is the procedure's INOUT / OUT parameters, with polymorphic ones
/// resolved from the arguments.
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
    let (schema, name) = expr::deconstruct_qualified_name(
        &parts,
        crate::error::SourceSpan::from_node_qname(fc.location),
    )?;

    let scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for a in &fc.args {
        arg_types.push(expr::infer_expr(a, ctx, params, TypeGoal::NONE)?.type_oid);
    }
    let notation = functions::CallNotation {
        proc_call: true,
        ..functions::CallNotation::of(fc)?
    };
    let resolved = match functions::func_get_detail(
        snapshot,
        schema,
        name,
        &arg_types,
        &notation,
        None,
        crate::error::SourceSpan::from_node_qname(fc.location),
    )? {
        functions::FuncDetail::Routine(r) => r,
        functions::FuncDetail::Coercion(_) => unreachable!("coercion interpretation not requested"),
    };
    ctx.note_proc(Some(resolved.oid));

    // Coerce each argument to its parameter: pins bare parameters and
    // validates untyped literals (`CALL p('x')` → 22P02 for an integer).
    expr::backfill_call_args(fc, &arg_types, &resolved, ctx, params)?;

    let columns = resolved
        .out_args
        .iter()
        .map(|a| RawColumn {
            name: a.name.clone(),
            type_oid: a.type_oid,
            nullable: true,
            typmod: None,
            collation: None,
            record_fields: None,
            elem_nullable: None,
            origin: None,
        })
        .collect();
    Ok((columns, None))
}
