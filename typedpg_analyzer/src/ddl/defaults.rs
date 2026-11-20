//! Type-checking of column / domain `DEFAULT` expressions.
//!
//! Mirrors `cookDefault` (`catalog/heap.c`): the expression is transformed
//! in `EXPR_KIND_COLUMN_DEFAULT`, which forbids column references,
//! subqueries, aggregates and window functions, and must then be coercible
//! to the column's type under assignment rules.

use typedpg_pg_query::ast;

use super::DdlError;
use crate::coerce::{CoercionContext, can_coerce};
use crate::expr::{TypeGoal, infer_expr};
use crate::nullability::NullabilityContext;
use crate::oid::PgTypeOid;
use crate::param_collector::ParamCollector;
use crate::pg_catalog::{PgCatalog, ProKind, oid};
use crate::scope::Scope;

/// Check `expr` as the DEFAULT of column (or domain) `name` of type
/// `type_oid`. Returns the type the stored default has once its implicit
/// coercion to the column is stripped (the expression's own type; the
/// column type for an untyped literal).
pub(crate) fn check_default(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    name: &str,
    type_oid: PgTypeOid,
) -> Result<PgTypeOid, DdlError> {
    check_default_kind(interp, expr, true)?;

    let scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let ctx = || crate::expr::Ctx::new(&scope, &null_ctx, interp);
    let wrap = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(format!("{e}"));

    let result = infer_expr(expr, ctx(), &mut params, TypeGoal::NONE).map_err(wrap)?;
    if result.type_oid == oid::UNKNOWN {
        // An untyped literal is coerced to the column type, which runs the
        // type's input function (`invalid input syntax for type ...`).
        let mut goal = TypeGoal::assignment(type_oid);
        goal.source_col_name = Some(name.to_owned());
        infer_expr(expr, ctx(), &mut params, goal).map_err(wrap)?;
        return Ok(type_oid);
    }
    if record_to_complex(interp, result.type_oid, type_oid) {
        infer_expr(expr, ctx(), &mut params, TypeGoal::assignment(type_oid)).map_err(wrap)?;
        return Ok(type_oid);
    }
    if !can_coerce(
        result.type_oid,
        type_oid,
        CoercionContext::Assignment,
        interp,
    ) {
        return Err(DdlError::UnsupportedDdl(format!(
            "column \"{name}\" is of type {} but default expression is of type {}",
            super::util::format_type_for_message(interp, type_oid),
            super::util::format_type_for_message(interp, result.type_oid),
        )));
    }
    Ok(result.type_oid)
}

/// Check `expr` as the DEFAULT of a function parameter of type
/// `param_type` (`interpret_function_parameter_list`): transformed in
/// `EXPR_KIND_FUNCTION_DEFAULT`, then coerced to the parameter's type
/// (`coerce_to_specific_type`, assignment rules) — unless the parameter is
/// polymorphic, where the default keeps its own type. Returns the stored
/// default's type.
pub(crate) fn check_function_default(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    param_type: PgTypeOid,
    polymorphic: bool,
    used: &std::cell::RefCell<Vec<crate::oid::PgProcOid>>,
) -> Result<PgTypeOid, DdlError> {
    check_default_kind(interp, expr, false)?;
    let scope = Scope::default();
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let ctx = || crate::expr::Ctx::new(&scope, &null_ctx, interp).recording(used);
    let wrap = |e: crate::error::AnalyzeError| DdlError::UnsupportedDdl(format!("{e}"));
    let result = infer_expr(expr, ctx(), &mut params, TypeGoal::NONE).map_err(wrap)?;
    if polymorphic {
        return Ok(result.type_oid);
    }
    if result.type_oid == oid::UNKNOWN {
        infer_expr(expr, ctx(), &mut params, TypeGoal::assignment(param_type)).map_err(wrap)?;
        return Ok(param_type);
    }
    if record_to_complex(interp, result.type_oid, param_type) {
        infer_expr(expr, ctx(), &mut params, TypeGoal::assignment(param_type)).map_err(wrap)?;
        return Ok(param_type);
    }
    if !can_coerce(
        result.type_oid,
        param_type,
        CoercionContext::Assignment,
        interp,
    ) {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of DEFAULT must be type {}, not type {}",
            super::util::format_type_for_message(interp, param_type),
            super::util::format_type_for_message(interp, result.type_oid),
        )));
    }
    Ok(param_type)
}

/// Whether a stored expression of type `source` reaches `target` through
/// `can_coerce_type`'s rule that `record` coerces to any composite (or
/// domain over one): `coerce_record_to_complex` then converts a row
/// constructor field by field, raising PG's `cannot cast type record to …`
/// itself — what inferring it toward the composite does.
pub(crate) fn record_to_complex(interp: &PgCatalog, source: PgTypeOid, target: PgTypeOid) -> bool {
    source == oid::RECORD && crate::coerce::is_complex(target, interp)
}

/// Whether the DEFAULT `expr` of a column of type `type_oid` / `typmod`
/// cooks to a null constant, which AddRelationNewConstraints doesn't store:
/// the column gets no default, so ADD COLUMN rewrites nothing.
pub(crate) fn is_null_default(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
) -> bool {
    null_const_coerced(interp, null_const(interp, expr), type_oid, typmod).is_some()
}

/// The `(type, typmod)` of the null `Const` that `expr` transforms to:
/// `NULL`, or `NULL` under casts that each stay a constant.
fn null_const(interp: &PgCatalog, expr: Option<&ast::Node>) -> Option<(PgTypeOid, i32)> {
    match expr? {
        ast::Node::AConst(c) if c.isnull => Some((oid::UNKNOWN, -1)),
        ast::Node::TypeCast(tc) => {
            let tn = tc.type_name.as_ref()?;
            let target = super::util::resolve_type_name(tn, interp)?;
            let typmod = crate::typmod::encode(interp, target, tn).ok()?;
            tc.arg.as_ref()?;
            null_const_coerced(interp, null_const(interp, tc.arg.as_ref()), target, typmod)
        }
        _ => None,
    }
}

/// `coerce_to_target_type` of a null `Const` of `input` to `target` /
/// `typmod`, if the result is still a `Const`: `coerce_type` turns an
/// unknown-type literal into a `Const` of the target (applying its typmod
/// only for interval, whose input routine takes it) and leaves a same-type
/// node alone — any other conversion is a RelabelType, a function call or
/// a CoerceToDomain. Then `coerce_type_typmod` adds a length coercion for a
/// typmod the constant doesn't already have.
fn null_const_coerced(
    interp: &PgCatalog,
    input: Option<(PgTypeOid, i32)>,
    target: PgTypeOid,
    typmod: Option<i32>,
) -> Option<(PgTypeOid, i32)> {
    const INTERVAL: PgTypeOid = PgTypeOid::from_raw(1186);
    let (input_type, input_typmod) = input?;
    let typmod = typmod.unwrap_or(-1);
    if interp
        .pg_type
        .get(&target)
        .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Domain)
    {
        return None;
    }
    let const_typmod = if input_type == target {
        input_typmod
    } else if input_type == oid::UNKNOWN {
        if target == INTERVAL { typmod } else { -1 }
    } else {
        return None;
    };
    if typmod >= 0 && typmod != const_typmod {
        return None;
    }
    Some((target, const_typmod))
}

/// The `EXPR_KIND_COLUMN_DEFAULT` / `EXPR_KIND_FUNCTION_DEFAULT`
/// restrictions (`transformColumnRef`, `transformSubLink`,
/// `check_agglevels_and_constraints`, `transformWindowFuncCall`,
/// `check_srf_call_placement`).
fn check_default_kind(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    column_default: bool,
) -> Result<(), DdlError> {
    check_default_kind_node(interp, expr, column_default)?;
    crate::resolve::check_no_srf_in_clause(expr, interp, "DEFAULT expressions")
        .map_err(|e| DdlError::UnsupportedDdl(e.to_string()))
}

fn check_default_kind_node(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    column_default: bool,
) -> Result<(), DdlError> {
    let check_default_kind =
        |e: Option<&ast::Node>| check_default_kind_node(interp, e, column_default);
    let Some(inner) = expr else {
        return Ok(());
    };
    match inner {
        // A parameter default has no columns in scope at all: the
        // reference is simply `column "x" does not exist`.
        ast::Node::ColumnRef(_) if column_default => {
            return Err(DdlError::UnsupportedDdl(
                "cannot use column reference in DEFAULT expression".into(),
            ));
        }
        ast::Node::SubLink(_) => {
            return Err(DdlError::UnsupportedDdl(
                "cannot use subquery in DEFAULT expression".into(),
            ));
        }
        ast::Node::FuncCall(fc) => {
            if fc.over.is_some() {
                return Err(DdlError::UnsupportedDdl(
                    "window functions are not allowed in DEFAULT expressions".into(),
                ));
            }
            let parts: Vec<&str> = fc
                .funcname
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()))
                .collect();
            let (schema, name) = match parts.as_slice() {
                [name] => (None, *name),
                [schema, name] => (Some(*schema), *name),
                _ => (None, ""),
            };
            let candidates = interp.find_functions(schema, name);
            let aggregate = fc.agg_star
                || (!candidates.is_empty()
                    && candidates
                        .iter()
                        .all(|p| matches!(p.prokind, ProKind::Aggregate)));
            if aggregate {
                return Err(DdlError::UnsupportedDdl(
                    "aggregate functions are not allowed in DEFAULT expressions".into(),
                ));
            }
            for arg in &fc.args {
                check_default_kind(arg.as_ref())?;
            }
        }
        ast::Node::AExpr(e) => {
            for side in [&e.lexpr, &e.rexpr] {
                check_default_kind(side.as_ref())?;
            }
        }
        ast::Node::BoolExpr(b) => {
            for arg in &b.args {
                check_default_kind(arg.as_ref())?;
            }
        }
        ast::Node::TypeCast(tc) => {
            check_default_kind(tc.arg.as_ref())?;
        }
        ast::Node::CoalesceExpr(c) => {
            for arg in &c.args {
                check_default_kind(arg.as_ref())?;
            }
        }
        ast::Node::CaseExpr(c) => {
            for part in [&c.arg, &c.defresult] {
                check_default_kind(part.as_ref())?;
            }
            for when in &c.args {
                if let Some(ast::Node::CaseWhen(w)) = when.as_ref() {
                    for part in [&w.expr, &w.result] {
                        check_default_kind(part.as_ref())?;
                    }
                }
            }
        }
        ast::Node::ArrayExpr(a) => {
            for elem in &a.elements {
                check_default_kind(elem.as_ref())?;
            }
        }
        ast::Node::RowExpr(r) => {
            for arg in &r.args {
                check_default_kind(arg.as_ref())?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Record what a column default depends on (PG's pg_attrdef dependencies
/// that DDL consults): the sequences its `nextval` / `currval` / `setval`
/// calls name through a regclass literal. Replaces any earlier record for
/// the column.
pub(crate) fn record_default_dependencies(
    interp: &mut PgCatalog,
    relid: crate::oid::PgClassOid,
    attnum: i16,
    expr: Option<&ast::Node>,
) {
    forget_default_dependencies(interp, relid, attnum);
    let mut sequences = Vec::new();
    if let Some(expr) = expr {
        collect_sequence_refs(interp, Some(expr), &mut sequences);
    }
    for seq in sequences {
        record_default_sequence(interp, relid, attnum, seq);
    }
    // The privileges each sequence function of the default needs when the
    // default runs (`nextval`: USAGE or UPDATE, `currval`: SELECT or USAGE,
    // `setval`: UPDATE) — a bare `'s'::regclass` constant names no function.
    let mut perms = Vec::new();
    if let Some(expr) = expr {
        sequence_function_refs(interp, Some(expr), &mut perms);
    }
    if perms.is_empty() {
        interp.default_sequence_perms.remove(&(relid, attnum));
    } else {
        interp.default_sequence_perms.insert((relid, attnum), perms);
    }
    // Functions, operators, types, ... the expression refers to.
    if let Some(expr) = expr {
        let target = interp
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.atttypid);
        let refs =
            super::depend::coerced_expression_references(interp, Some(expr), Some(relid), target);
        super::depend::record_references(
            interp,
            super::depend::ObjectAddress::column(relid, attnum),
            &refs,
            crate::pg_catalog::DepType::Normal,
        );
    }
}

/// The default of `relid.attnum` depends on sequence `seq` (serial's
/// `nextval(...)`, or a written `nextval('s')`).
pub(crate) fn record_default_sequence(
    interp: &mut PgCatalog,
    relid: crate::oid::PgClassOid,
    attnum: i16,
    seq: crate::oid::PgClassOid,
) {
    use crate::oid::PgGenericOid;
    use crate::pg_catalog::{DepType, PG_CLASS_RELID, PgDepend};
    interp.add_dependency(PgDepend {
        classid: PG_CLASS_RELID,
        objid: PgGenericOid::from_nonzero(relid.into_nonzero()),
        objsubid: attnum,
        refclassid: PG_CLASS_RELID,
        refobjid: PgGenericOid::from_nonzero(seq.into_nonzero()),
        refobjsubid: 0,
        deptype: DepType::Normal,
    });
}

/// Drop the recorded dependencies of the default of `relid.attnum`.
pub(crate) fn forget_default_dependencies(
    interp: &mut PgCatalog,
    relid: crate::oid::PgClassOid,
    attnum: i16,
) {
    use crate::pg_catalog::{DepType, PG_CLASS_RELID};
    let rel_obj = crate::oid::PgGenericOid::from_nonzero(relid.into_nonzero());
    interp.pg_depend.retain(|d| {
        !(d.classid == PG_CLASS_RELID
            && d.objid == rel_obj
            && d.objsubid == attnum
            && d.deptype == DepType::Normal)
    });
    interp.default_sequence_perms.remove(&(relid, attnum));
}

/// The sequences the `nextval` / `currval` / `setval` calls of `expr` name,
/// with the privileges each needs (`do_setval` requires UPDATE). The
/// expression runs through `expression_planner` first: a call in code the
/// planner certainly drops (a dead CASE branch, a COALESCE argument after a
/// constant) never runs.
pub(crate) fn sequence_function_refs(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    out: &mut Vec<crate::privileges::SequencePrivileges>,
) {
    if let Some(inner) = expr {
        walk_sequence_functions(inner.to_ref(), interp, false, out);
    }
}

fn walk_sequence_functions(
    n: typedpg_pg_query::NodeRef<'_>,
    interp: &PgCatalog,
    dead: bool,
    out: &mut Vec<crate::privileges::SequencePrivileges>,
) {
    use typedpg_pg_query::NodeRef;
    if dead {
        return;
    }
    match n {
        NodeRef::FuncCall(fc) => {
            note_sequence_call(interp, fc, out);
            for arg in &fc.args {
                walk_sequence_node(arg.as_ref(), interp, false, out);
            }
        }
        NodeRef::BoolExpr(b) => {
            let deciding = match b.boolop {
                typedpg_pg_query::ast::BoolExprType::AndExpr => Some(false),
                typedpg_pg_query::ast::BoolExprType::OrExpr => Some(true),
                _ => None,
            };
            let decided = deciding.filter(|&value| {
                b.args.iter().any(|a| {
                    crate::expr::plan_fold::certainly(a.as_ref(), interp)
                        == crate::expr::plan_fold::Certain::Bool(value)
                })
            });
            for arg in &b.args {
                let dead = decided.is_some_and(|value| {
                    crate::expr::plan_fold::certainly(arg.as_ref(), interp)
                        != crate::expr::plan_fold::Certain::Bool(value)
                });
                walk_sequence_node(arg.as_ref(), interp, dead, out);
            }
        }
        NodeRef::CoalesceExpr(c) => {
            let mut stopped = false;
            for arg in &c.args {
                walk_sequence_node(arg.as_ref(), interp, stopped, out);
                stopped |=
                    crate::expr::plan_fold::certainly_non_null_constant(arg.as_ref(), interp);
            }
        }
        NodeRef::CaseExpr(c) => {
            // A simple CASE compares its test with each WHEN value: the
            // WHEN is a value, not a condition. A searched CASE tests each
            // WHEN directly.
            if let Some(test) = c.arg.as_ref() {
                walk_sequence_node(Some(test), interp, false, out);
            }
            let mut stopped = false;
            for when in &c.args {
                let Some(typedpg_pg_query::ast::Node::CaseWhen(when)) = when.as_ref() else {
                    break;
                };
                if stopped {
                    for part in when.expr.iter().chain(when.result.iter()) {
                        walk_sequence_node(Some(part), interp, true, out);
                    }
                    continue;
                }
                if let Some(cond) = when.expr.as_ref() {
                    walk_sequence_node(Some(cond), interp, false, out);
                }
                if let Some(test) = c.arg.as_ref() {
                    match when.expr.as_ref().and_then(|v| {
                        crate::expr::plan_fold::simple_case_equal(Some(test), Some(v))
                    }) {
                        Some(true) => {
                            if let Some(result) = when.result.as_ref() {
                                walk_sequence_node(Some(result), interp, false, out);
                            }
                            stopped = true;
                        }
                        Some(false) => {
                            if let Some(result) = when.result.as_ref() {
                                walk_sequence_node(Some(result), interp, true, out);
                            }
                        }
                        None => {
                            if let Some(result) = when.result.as_ref() {
                                walk_sequence_node(Some(result), interp, false, out);
                            }
                        }
                    }
                    continue;
                }
                match when
                    .expr
                    .as_ref()
                    .map(|e| crate::expr::plan_fold::certainly(Some(e), interp))
                {
                    Some(
                        crate::expr::plan_fold::Certain::Bool(false)
                        | crate::expr::plan_fold::Certain::Null,
                    ) => {
                        if let Some(result) = when.result.as_ref() {
                            walk_sequence_node(Some(result), interp, true, out);
                        }
                    }
                    Some(crate::expr::plan_fold::Certain::Bool(true)) => {
                        if let Some(result) = when.result.as_ref() {
                            walk_sequence_node(Some(result), interp, false, out);
                        }
                        stopped = true;
                    }
                    _ => {
                        if let Some(result) = when.result.as_ref() {
                            walk_sequence_node(Some(result), interp, false, out);
                        }
                    }
                }
            }
            if let Some(default) = c.defresult.as_ref() {
                walk_sequence_node(Some(default), interp, stopped, out);
            }
        }
        _ => {
            for child in n.children() {
                walk_sequence_functions(child, interp, false, out);
            }
        }
    }
}

fn walk_sequence_node(
    node: Option<&ast::Node>,
    interp: &PgCatalog,
    dead: bool,
    out: &mut Vec<crate::privileges::SequencePrivileges>,
) {
    if let Some(inner) = node {
        walk_sequence_functions(inner.to_ref(), interp, dead, out);
    }
}

/// Record the sequence `fc` names, if it is a `nextval` / `currval` /
/// `setval` call over a regclass constant.
fn note_sequence_call(
    interp: &PgCatalog,
    fc: &typedpg_pg_query::ast::FuncCall,
    out: &mut Vec<crate::privileges::SequencePrivileges>,
) {
    let Some(needed) = sequence_function_privileges(interp, fc) else {
        return;
    };
    if let Some(first) = fc.args.first()
        && let Some(text) = regclass_literal(first.as_ref())
        && let Some(seq) = resolve_regclass(interp, text)
        && !out.contains(&(seq, needed))
    {
        out.push((seq, needed));
    }
}

/// The privileges the function `fc` calls needs of the sequence it names,
/// when it resolves to `pg_catalog`'s `nextval` / `currval` / `setval` —
/// `None` for another function (a user's same-named one, which the search
/// path may put first) and for anything else.
fn sequence_function_privileges(
    interp: &PgCatalog,
    fc: &typedpg_pg_query::ast::FuncCall,
) -> Option<u32> {
    use crate::ddl::acl::bits;
    let parts: Vec<&str> = fc
        .funcname
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    let (schema, name) = match parts.as_slice() {
        [name] => (None, *name),
        [schema, name] => (Some(*schema), *name),
        _ => return None,
    };
    let needed = match name {
        "nextval" => bits::USAGE | bits::UPDATE,
        "currval" => bits::SELECT | bits::USAGE,
        "setval" => bits::UPDATE,
        _ => return None,
    };
    const REGCLASS: PgTypeOid = PgTypeOid::from_raw(2205);
    let mut arg_types = Vec::with_capacity(fc.args.len());
    for (i, arg) in fc.args.iter().enumerate() {
        if i == 0 {
            // The sequence argument is a regclass constant (an untyped
            // literal, or a cast to regclass).
            regclass_literal(arg.as_ref())?;
            arg_types.push(REGCLASS);
        } else {
            arg_types.push(literal_argument_type(interp, arg.as_ref()).unwrap_or(oid::UNKNOWN));
        }
    }
    let notation = crate::functions::CallNotation::of(fc).ok()?;
    let resolved =
        crate::functions::func_get_detail(interp, schema, name, &arg_types, &notation, None, None)
            .ok()?;
    let crate::functions::FuncDetail::Routine(routine) = resolved else {
        return None;
    };
    let proc = interp.pg_proc.get(&routine.oid)?;
    (Some(proc.pronamespace) == interp.pg_catalog_oid()).then_some(needed)
}

/// The type a literal argument of a sequence call has, as the parse tree
/// writes it: an integer, a numeric, a boolean, a string (unknown) or an
/// explicit cast.
fn literal_argument_type(interp: &PgCatalog, arg: Option<&ast::Node>) -> Option<PgTypeOid> {
    match arg? {
        ast::Node::AConst(c) => {
            if c.isnull {
                return None;
            }
            match c.val.as_ref()? {
                ast::AConstValue::Integer(_) => Some(oid::INT4),
                ast::AConstValue::Float(_) => Some(oid::NUMERIC),
                ast::AConstValue::Boolean(_) => Some(oid::BOOL),
                ast::AConstValue::String(_) => Some(oid::UNKNOWN),
                ast::AConstValue::BitString(_) => None,
            }
        }
        ast::Node::TypeCast(tc) => super::util::resolve_type_name(tc.type_name.as_ref()?, interp),
        _ => None,
    }
}

/// The sequences `expr` names in a regclass literal, anywhere in the tree:
/// what `pg_depend` records (dead branches included).
fn collect_sequence_refs(
    interp: &PgCatalog,
    expr: Option<&ast::Node>,
    out: &mut Vec<crate::oid::PgClassOid>,
) {
    let Some(inner) = expr else {
        return;
    };
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        if let typedpg_pg_query::NodeRef::FuncCall(fc) = n {
            let is_seq_fn = sequence_function_privileges(interp, fc).is_some();
            if is_seq_fn
                && let Some(first) = fc.args.first()
                && let Some(text) = regclass_literal(first.as_ref())
                && let Some(seq) = resolve_regclass(interp, text)
            {
                out.push(seq);
            }
        }
        todo.extend(n.children());
    }
}

/// The text of `'name'` or `'name'::regclass`: a regclass constant, the
/// only argument find_expr_references_walker records a relation for. A
/// literal cast to another type first (`'name'::text`) reaches the
/// function through a run-time cast, which names no relation.
fn regclass_literal(node: Option<&ast::Node>) -> Option<&str> {
    match node? {
        ast::Node::AConst(c) => match c.val.as_ref()? {
            typedpg_pg_query::ast::AConstValue::String(s) => Some(s.sval.as_str()),
            _ => None,
        },
        ast::Node::TypeCast(tc) => {
            let type_name = tc.type_name.as_ref()?;
            if !type_name.array_bounds.is_empty() {
                return None;
            }
            let names: Vec<&str> = type_name
                .names
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()))
                .collect();
            match names.as_slice() {
                ["regclass"] | ["pg_catalog", "regclass"] => regclass_literal(tc.arg.as_ref()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `regclassin`: a possibly schema-qualified, possibly quoted relation name.
fn resolve_regclass(interp: &PgCatalog, text: &str) -> Option<crate::oid::PgClassOid> {
    let mut parts: Vec<String> = Vec::new();
    let mut chars = text.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.next_if_eq(&'"').is_some() {
                        part.push('"');
                    } else {
                        break;
                    }
                } else {
                    part.push(c);
                }
            }
        } else {
            while let Some(c) = chars.next_if(|&c| c != '.') {
                part.push(c.to_ascii_lowercase());
            }
        }
        parts.push(part);
        if chars.next_if_eq(&'.').is_none() {
            break;
        }
    }
    let (schema, name) = match parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => return None,
    };
    interp.resolve_table(schema, name).map(|c| c.oid)
}
