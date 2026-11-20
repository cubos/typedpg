//! `CALL procedure(args)`.

use super::*;
use typedpg_pg_query::ast;

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
    call: &ast::CallStmt,
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
        check_call_argument(a.as_ref(), snapshot)?;
        arg_types.push(expr::infer_expr(a.as_ref(), ctx, params, TypeGoal::NONE)?.type_oid);
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
    // ExecuteCallStmt: EXECUTE on the procedure, and the parameter DEFAULT
    // expressions the call is expanded with — those of the parameters it
    // leaves out.
    let certainty: Vec<crate::expr::plan_fold::Certain> = fc
        .args
        .iter()
        .map(|arg| crate::expr::plan_fold::certainly_in(arg.as_ref(), ctx))
        .collect();
    crate::privileges::note_call(
        snapshot,
        resolved.oid,
        false,
        crate::privileges::CallSite::Expression,
        &fc.args,
        &certainty,
    );
    for default in snapshot
        .routine_defaults
        .get(&resolved.oid)
        .into_iter()
        .flatten()
    {
        if !resolved.default_params.contains(&default.param) {
            continue;
        }
        for &p in &default.procs {
            crate::privileges::note_routine(p, crate::privileges::RoutineKind::Function);
        }
        for &(seq, any_of) in &default.sequences {
            crate::privileges::note_sequence(seq, any_of);
        }
    }

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
            no_collation: None,
            planned: crate::expr::Planned::AsParsed,
            planned_top: None,
            scan_stays: false,
            scan_var: None,
            record_fields: None,
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
            origin: None,
            unknown_value: false,
        })
        .collect();
    Ok((columns, None))
}

/// EXPR_KIND_CALL_ARGUMENT: no sub-select (transformSubLink, 0A000), no
/// aggregate, window or set-returning function.
fn check_call_argument(arg: Option<&ast::Node>, snapshot: &PgCatalog) -> Result<(), AnalyzeError> {
    let mut sublink = None;
    visit_same_level(arg, &mut |n| {
        if let Some(ast::Node::SubLink(sl)) = n {
            sublink.get_or_insert(sl.location);
        }
    });
    if let Some(location) = sublink {
        return Err(
            crate::pgmsg::call_argument_subquery(crate::error::SourceSpan::from_location(location))
                .finalize_implicit(),
        );
    }
    crate::clause::check_no_aggregates_or_windows(arg, snapshot, "CALL arguments")?;
    check_no_srf_in_clause(arg, snapshot, "CALL arguments")
}
