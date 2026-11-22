//! CREATE FUNCTION / CREATE PROCEDURE, ALTER FUNCTION, and the routine
//! lookups other commands share (`LookupFuncWithArgs`).
//!
//! `create_function` follows `CreateFunction` (functioncmds.c) step by step:
//! the options (`compute_function_attributes`), the language, the
//! parameters (`interpret_function_parameter_list`), the result type, the
//! body (`interpret_AS_clause`), then `ProcedureCreate` (pg_proc.c) — the
//! signature rules, CREATE OR REPLACE's rules — and the language's validator.

use typedpg_pg_query::ast::{
    self, CreateFunctionStmt, DefElem, FunctionParameterMode, ObjectType, ObjectWithArgs, TypeName,
    VariableSetKind,
};

use crate::oid::{PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{ArgMode, DepType, PgProc, ProKind, ProVolatile, oid as builtin_oid};

use super::DdlError;
use super::util::{
    ensure_qualified_name, format_type_for_message, lookup_type_name, node_string,
    type_name_to_string,
};
use crate::pg_catalog::PgCatalog;

const VOID: PgTypeOid = PgTypeOid::from_raw(2278);
const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);

/// The OID of a routine row [`procedure_create`] hasn't stored yet (it
/// assigns a new one, or keeps the replaced routine's).
pub(crate) const PENDING_OID: PgProcOid = PgProcOid::from_raw(u32::MAX);

/// What the analyzer keeps of a routine the migrations create, beyond its
/// `pg_proc` row: what deciding whether a role may run its body needs.
#[derive(Clone, Debug)]
pub(crate) struct RoutineBody {
    /// `prolang`'s name, lowercased (`sql`, `plpgsql`, `c`, ...).
    pub(crate) language: String,
    /// `prosecdef`: the body runs as the routine's owner, not its caller.
    pub(crate) security_definer: bool,
    /// The schema names a `SET search_path` clause gives the body, when it
    /// has one. `None`: the caller's session search path.
    pub(crate) search_path: Option<Vec<String>>,
    pub(crate) source: RoutineSource,
}

/// Where a routine's body text lives.
#[derive(Clone, Debug)]
pub(crate) enum RoutineSource {
    /// A `LANGUAGE sql` body given as a string (`AS $$ ... $$`).
    SqlText(String),
    /// A SQL-standard body (`BEGIN ATOMIC ... END` / `RETURN ...`),
    /// resolved at CREATE FUNCTION: PG stores it analyzed, and what it
    /// requires is fixed then (by OID).
    SqlStandard(Box<crate::privileges::Requirements>),
    /// A `LANGUAGE plpgsql` body: the `CREATE FUNCTION` statement as
    /// written, which PL/pgSQL's compiler reads (through
    /// `parse_plpgsql`) for its parameter and variable declarations.
    Plpgsql { create_sql: String },
}

/// `errorConflictingDefElem` (define.c).
fn conflicting_options() -> DdlError {
    DdlError::Parse("conflicting or redundant options".into())
}

/// `compute_common_attribute`'s error for an option a procedure can't take.
fn procedure_attribute_error() -> DdlError {
    DdlError::Parse("invalid attribute in procedure definition".into())
}

/// The options CREATE FUNCTION and ALTER FUNCTION share
/// (`compute_common_attribute`), each at most once.
#[derive(Default)]
struct CommonAttributes<'a> {
    volatility: Option<&'a DefElem>,
    strict: Option<&'a DefElem>,
    security: Option<&'a DefElem>,
    leakproof: Option<&'a DefElem>,
    set_items: Vec<&'a ast::Node>,
    cost: Option<&'a DefElem>,
    rows: Option<&'a DefElem>,
    support: Option<&'a DefElem>,
    parallel: Option<&'a DefElem>,
}

impl<'a> CommonAttributes<'a> {
    /// `compute_common_attribute`: `Ok(false)` for an option it doesn't
    /// handle.
    fn take(&mut self, is_procedure: bool, de: &'a DefElem) -> Result<bool, DdlError> {
        let slot = match de.defname.as_str() {
            "volatility" => &mut self.volatility,
            "strict" => &mut self.strict,
            "security" => {
                if self.security.replace(de).is_some() {
                    return Err(conflicting_options());
                }
                return Ok(true);
            }
            "leakproof" => &mut self.leakproof,
            "set" => {
                if let Some(arg) = de.arg.as_ref() {
                    self.set_items.push(arg);
                }
                return Ok(true);
            }
            "cost" => &mut self.cost,
            "rows" => &mut self.rows,
            "support" => &mut self.support,
            "parallel" => &mut self.parallel,
            _ => return Ok(false),
        };
        if is_procedure {
            return Err(procedure_attribute_error());
        }
        if slot.replace(de).is_some() {
            return Err(conflicting_options());
        }
        Ok(true)
    }

    /// The checks `compute_function_attributes` / `AlterFunction` make once
    /// every option is read, in their order: the SET items
    /// (`update_proconfig_value`), COST, ROWS, SUPPORT, PARALLEL. Returns
    /// the support function named.
    fn validate(&self, interp: &PgCatalog) -> Result<Option<PgProcOid>, DdlError> {
        for item in &self.set_items {
            check_set_item(interp, item)?;
        }
        if let Some(cost) = self.cost
            && def_numeric(cost).is_some_and(|v| v <= 0.0)
        {
            return Err(DdlError::UnsupportedDdl("COST must be positive".into()));
        }
        if let Some(rows) = self.rows
            && def_numeric(rows).is_some_and(|v| v <= 0.0)
        {
            return Err(DdlError::UnsupportedDdl("ROWS must be positive".into()));
        }
        let support = match self.support {
            Some(support) => Some(interpret_func_support(interp, support)?),
            None => None,
        };
        if let Some(parallel) = self.parallel {
            interpret_func_parallel(parallel)?;
        }
        Ok(support)
    }

    fn volatility(&self) -> Option<ProVolatile> {
        let de = self.volatility?;
        Some(match def_string(de)?.as_str() {
            "immutable" => ProVolatile::Immutable,
            "stable" => ProVolatile::Stable,
            _ => ProVolatile::Volatile,
        })
    }

    fn strict(&self) -> Option<bool> {
        self.strict.and_then(def_bool)
    }
}

/// `compute_function_attributes`: CREATE FUNCTION's options.
#[derive(Default)]
struct FunctionAttributes<'a> {
    as_clause: Option<&'a DefElem>,
    language: Option<String>,
    transform: Option<&'a DefElem>,
    window: bool,
    common: CommonAttributes<'a>,
    /// `prosupport`.
    support: Option<PgProcOid>,
}

fn compute_function_attributes<'a>(
    interp: &PgCatalog,
    is_procedure: bool,
    options: &'a [Option<typedpg_pg_query::ast::Node>],
) -> Result<FunctionAttributes<'a>, DdlError> {
    let mut attrs = FunctionAttributes::default();
    let mut language_item: Option<&DefElem> = None;
    let mut window_item: Option<&DefElem> = None;
    for option in options {
        let Some(ast::Node::DefElem(de)) = option.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "as" => {
                if attrs.as_clause.replace(de).is_some() {
                    return Err(conflicting_options());
                }
            }
            "language" => {
                if language_item.replace(de).is_some() {
                    return Err(conflicting_options());
                }
            }
            "transform" => {
                if attrs.transform.replace(de).is_some() {
                    return Err(conflicting_options());
                }
            }
            "window" => {
                if window_item.is_some() {
                    return Err(conflicting_options());
                }
                if is_procedure {
                    return Err(procedure_attribute_error());
                }
                window_item = Some(de);
            }
            _ => {
                if !attrs.common.take(is_procedure, de)? {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "option \"{}\" not recognized",
                        de.defname
                    )));
                }
            }
        }
    }
    attrs.language = language_item.and_then(def_string);
    attrs.window = window_item.and_then(def_bool).unwrap_or(false);
    attrs.support = attrs.common.validate(interp)?;
    Ok(attrs)
}

/// `defGetString` of an option's value, for the forms these options take.
fn def_string(de: &DefElem) -> Option<String> {
    match de.arg.as_ref()? {
        ast::Node::String(s) => Some(s.sval.clone()),
        ast::Node::Integer(i) => Some(i.ival.to_string()),
        ast::Node::Float(f) => Some(f.fval.clone()),
        ast::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.into()),
        ast::Node::TypeName(tn) => Some(type_name_to_string(tn)),
        ast::Node::List(l) => Some(
            l.items
                .iter()
                .filter_map(|n| node_string(n.as_ref()))
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

/// `defGetBoolean`: no value means true.
fn def_bool(de: &DefElem) -> Option<bool> {
    match de.arg.as_ref() {
        None => Some(true),
        Some(ast::Node::Boolean(b)) => Some(b.boolval),
        Some(ast::Node::Integer(i)) => Some(i.ival != 0),
        Some(ast::Node::String(s)) => super::reloptions::parse_bool(&s.sval),
        _ => None,
    }
}

/// `defGetNumeric`.
fn def_numeric(de: &DefElem) -> Option<f64> {
    match de.arg.as_ref()? {
        ast::Node::Integer(i) => Some(f64::from(i.ival)),
        ast::Node::Float(f) => f.fval.parse().ok(),
        _ => None,
    }
}

/// `interpret_func_parallel`.
fn interpret_func_parallel(de: &DefElem) -> Result<(), DdlError> {
    match def_string(de).as_deref() {
        Some("safe" | "restricted" | "unsafe") => Ok(()),
        _ => Err(DdlError::Parse(
            "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE".into(),
        )),
    }
}

/// `interpret_func_support`: `SUPPORT name` names a `name(internal)`
/// returning `internal`.
fn interpret_func_support(interp: &PgCatalog, de: &DefElem) -> Result<PgProcOid, DdlError> {
    let names: Vec<String> = match de.arg.as_ref() {
        Some(ast::Node::List(l)) => l
            .items
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .map(str::to_owned)
            .collect(),
        Some(ast::Node::TypeName(tn)) => tn
            .names
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .map(str::to_owned)
            .collect(),
        Some(ast::Node::String(s)) => vec![s.sval.clone()],
        _ => Vec::new(),
    };
    let written = names.join(".");
    let found = lookup_func_name(interp, &names, Some(&[INTERNAL]))?;
    let Some(oid) = found else {
        return Err(DdlError::TypeNotFound(format!(
            "function {written}(internal) does not exist"
        )));
    };
    if interp.pg_proc.get(&oid).map(|p| p.prorettype) != Some(INTERNAL) {
        return Err(DdlError::UnsupportedDdl(format!(
            "support function {written} must return type internal"
        )));
    }
    Ok(oid)
}

/// A `SET name = value` / `RESET name` item of a routine
/// (`update_proconfig_value` → `validate_option_array_item`): the
/// parameter must exist (or be a custom `prefix.name` one), be settable,
/// and a new value valid.
fn check_set_item(interp: &PgCatalog, item: &ast::Node) -> Result<(), DdlError> {
    let ast::Node::VariableSetStmt(set) = item else {
        return Ok(());
    };
    let kind = set.kind;
    if matches!(kind, VariableSetKind::VarResetAll) {
        return Ok(());
    }
    if kind == VariableSetKind::VarSetValue && super::guc::null_arg(&set.name, &set.args)? {
        return super::guc::check_settable(interp, &set.name).map(|_| ());
    }
    let Some(setting) = super::guc::check_settable(interp, &set.name)?.cloned() else {
        return Ok(());
    };
    if kind == VariableSetKind::VarSetValue
        && let [arg] = set.args.as_slice()
        && let Some(ast::Node::AConst(c)) = arg.as_ref()
    {
        use typedpg_pg_query::ast::AConstValue;
        let value = match c.val.as_ref() {
            Some(AConstValue::Integer(i)) => Some(i.ival.to_string()),
            Some(AConstValue::Float(f)) => Some(f.fval.clone()),
            Some(AConstValue::String(s)) => Some(s.sval.clone()),
            Some(AConstValue::Boolean(b)) => Some(b.boolval.to_string()),
            _ => None,
        };
        if let Some(value) = value {
            super::guc::check_value(interp, &setting, &value)?;
        }
    }
    Ok(())
}

/// What a routine's parameter DEFAULT expression runs, and which parameter
/// it fills: `add_function_defaults` / `reorder_function_arguments` insert
/// only the defaults of the parameters a call leaves out.
#[derive(Clone, Debug, Default)]
pub(crate) struct RoutineDefault {
    /// The parameter's position in `proargtypes` (0-based).
    pub param: usize,
    /// The functions the DEFAULT expression runs.
    pub procs: Vec<crate::oid::PgProcOid>,
    /// The sequences its `nextval` / `currval` / `setval` name, with the
    /// privileges each needs.
    pub sequences: Vec<crate::privileges::SequencePrivileges>,
    /// The expression certainly is the NULL constant: a strict function
    /// called with it is NULL before its body or its other arguments run
    /// (`evaluate_function`).
    pub null: bool,
    /// The routines and sequences the expression certainly runs: `procs`
    /// and `sequences` keep the functions of code the planner folds (a
    /// dead CASE branch), which `add_function_defaults` drops.
    pub live_procs: Vec<crate::oid::PgProcOid>,
    pub live_sequences: Vec<crate::privileges::SequencePrivileges>,
    /// Everything the expression's analysis required: a call that never
    /// evaluates the default still simplifies it (an `IMMUTABLE` call over
    /// constants in it runs, see `privileges::keep_default_simplified`).
    pub live: crate::privileges::Requirements,
}

/// The parameter list of a routine as `interpret_function_parameter_list`
/// reads it.
pub(crate) struct ParameterList {
    /// Input parameter types (`proargtypes`).
    pub in_types: Vec<PgTypeOid>,
    /// Every parameter's type, mode and name, in declaration order.
    pub all_types: Vec<PgTypeOid>,
    pub modes: Vec<ArgMode>,
    pub names: Vec<String>,
    /// Types of the DEFAULT expressions, for the trailing input parameters
    /// that have one.
    pub default_types: Vec<PgTypeOid>,
    /// `provariadic`: the element type of the VARIADIC parameter.
    pub variadic: Option<PgTypeOid>,
    /// The result type the OUT parameters imply (`requiredResultType`).
    pub required_result: Option<PgTypeOid>,
    /// What the DEFAULT expressions refer to.
    pub default_refs: Vec<super::depend::Reference>,
    /// The functions the DEFAULT expressions run.
    pub default_procs: Vec<crate::oid::PgProcOid>,
    /// Per defaulted parameter, in declaration order: what inserting its
    /// default runs.
    pub defaults_data: Vec<RoutineDefault>,
    /// Whether any parameter has an OUT or VARIADIC mode (PG then stores
    /// `proallargtypes` / `proargmodes`).
    pub has_modes: bool,
}

/// `interpret_function_parameter_list` (functioncmds.c). `language` is the
/// routine's language (`None` for an aggregate).
pub(crate) fn interpret_function_parameter_list(
    interp: &PgCatalog,
    parameters: &[Option<typedpg_pg_query::ast::Node>],
    language: Option<&str>,
    objtype: ObjectType,
) -> Result<ParameterList, DdlError> {
    let mut list = ParameterList {
        in_types: Vec::new(),
        all_types: Vec::new(),
        modes: Vec::new(),
        names: Vec::new(),
        default_types: Vec::new(),
        variadic: None,
        required_result: None,
        default_refs: Vec::new(),
        default_procs: Vec::new(),
        defaults_data: Vec::new(),
        has_modes: false,
    };
    let mut out_count = 0;
    let mut var_count = 0;
    let mut have_defaults = false;
    let params: Vec<&typedpg_pg_query::ast::FunctionParameter> = parameters
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::FunctionParameter(fp)) => Some(fp.as_ref()),
            _ => None,
        })
        .collect();
    for (i, fp) in params.iter().enumerate() {
        let mode = match fp.mode {
            FunctionParameterMode::FuncParamOut => ArgMode::Out,
            FunctionParameterMode::FuncParamInout => ArgMode::InOut,
            FunctionParameterMode::FuncParamVariadic => ArgMode::Variadic,
            FunctionParameterMode::FuncParamTable => ArgMode::Table,
            _ => ArgMode::In,
        };
        let Some(tn) = fp.arg_type.as_ref() else {
            continue;
        };
        // LookupTypeName; the message names the type unquoted here.
        let toid = lookup_type_name(tn, interp).map_err(|e| match e {
            DdlError::TypeNotFound(msg) if msg.starts_with("type \"") => {
                DdlError::TypeNotFound(format!("type {} does not exist", type_name_to_string(tn)))
            }
            other => other,
        })?;
        // LookupTypeName computes the typmod too (typenameTypeMod), though
        // a function doesn't keep it.
        crate::typmod::encode(interp, toid, tn)?;
        if interp.pg_type.get(&toid).is_some_and(|t| !t.typisdefined) {
            if language == Some("sql") {
                return Err(DdlError::Parse(format!(
                    "SQL function cannot accept shell type {}",
                    type_name_to_string(tn)
                )));
            } else if objtype == ObjectType::ObjectAggregate {
                return Err(DdlError::Parse(format!(
                    "aggregate cannot accept shell type {}",
                    type_name_to_string(tn)
                )));
            }
        }
        if tn.setof {
            return Err(DdlError::Parse(
                match objtype {
                    ObjectType::ObjectAggregate => "aggregates cannot accept set arguments",
                    ObjectType::ObjectProcedure => "procedures cannot accept set arguments",
                    _ => "functions cannot accept set arguments",
                }
                .into(),
            ));
        }
        let is_input = !matches!(mode, ArgMode::Out | ArgMode::Table);
        if is_input {
            // Other input parameters can't follow a VARIADIC one.
            if var_count > 0 {
                return Err(DdlError::Parse(
                    "VARIADIC parameter must be the last input parameter".into(),
                ));
            }
            list.in_types.push(toid);
        }
        if !matches!(mode, ArgMode::In | ArgMode::Variadic) {
            if objtype == ObjectType::ObjectProcedure {
                if var_count > 0 {
                    return Err(DdlError::Parse(
                        "VARIADIC parameter must be the last parameter".into(),
                    ));
                }
                list.required_result = Some(builtin_oid::RECORD);
            } else if out_count == 0 {
                list.required_result = Some(toid);
            }
            out_count += 1;
        }
        if mode == ArgMode::Variadic {
            var_count += 1;
            list.variadic = Some(
                crate::polymorphic::variadic_element_type(toid, interp)
                    .ok_or_else(|| DdlError::Parse("VARIADIC parameter must be an array".into()))?,
            );
        }
        // Two input or two output parameters can't share a name.
        if !fp.name.is_empty() {
            let pure_in = |m: ArgMode| matches!(m, ArgMode::In | ArgMode::Variadic);
            let pure_out = |m: ArgMode| matches!(m, ArgMode::Out | ArgMode::Table);
            for (j, prev) in list.names.iter().enumerate().take(i) {
                let prev_mode = list.modes[j];
                if (pure_in(mode) && pure_out(prev_mode)) || (pure_in(prev_mode) && pure_out(mode))
                {
                    continue;
                }
                if *prev == fp.name {
                    return Err(DdlError::Parse(format!(
                        "parameter name \"{}\" used more than once",
                        fp.name
                    )));
                }
            }
        }
        match &fp.defexpr {
            Some(_) if !is_input => {
                return Err(DdlError::Parse(
                    "only input parameters can have default values".into(),
                ));
            }
            expr @ Some(_) => {
                let polymorphic = crate::polymorphic::is_polymorphic(toid);
                let used = std::cell::RefCell::new(Vec::new());
                let (default_type, refs) = super::depend::collect(|| {
                    super::defaults::check_function_default(
                        interp,
                        expr.as_ref(),
                        toid,
                        polymorphic,
                        &used,
                    )
                });
                list.default_types.push(default_type?);
                list.default_refs.extend(refs);
                // An untyped literal becomes a constant of the parameter's
                // type.
                list.default_refs
                    .extend(super::depend::literal_coercion_references(
                        interp,
                        expr.as_ref(),
                        toid,
                    ));
                let procs = used.into_inner();
                let mut sequences = Vec::new();
                super::defaults::sequence_function_refs(interp, expr.as_ref(), &mut sequences);
                let null = matches!(
                    crate::expr::plan_fold::certainly(expr.as_ref(), interp),
                    crate::expr::plan_fold::Certain::Null
                );
                // What the default runs for real: the planner drops the
                // functions of its dead code, and those need no EXECUTE.
                let (_, live) = crate::privileges::collect(|| {
                    let scope = crate::scope::Scope::default();
                    let null_ctx = crate::nullability::NullabilityContext::default();
                    let mut params = crate::param_collector::ParamCollector::default();
                    let _ = crate::expr::infer_expr(
                        expr.as_ref(),
                        crate::expr::Ctx::new(&scope, &null_ctx, interp),
                        &mut params,
                        crate::expr::TypeGoal::NONE,
                    );
                });
                list.defaults_data.push(RoutineDefault {
                    param: list.in_types.len() - 1,
                    procs: procs.clone(),
                    sequences,
                    null,
                    live_procs: live.routines.iter().map(|(oid, _)| *oid).collect(),
                    live_sequences: live.sequences.clone(),
                    live,
                });
                list.default_procs.extend(procs);
                have_defaults = true;
            }
            None => {
                if is_input && have_defaults {
                    return Err(DdlError::Parse(
                        "input parameters after one with a default value must also have defaults"
                            .into(),
                    ));
                }
                if objtype == ObjectType::ObjectProcedure && have_defaults {
                    return Err(DdlError::Parse(
                        "procedure OUT parameters cannot appear after one with a default value"
                            .into(),
                    ));
                }
            }
        }
        list.all_types.push(toid);
        list.modes.push(mode);
        list.names.push(fp.name.clone());
    }
    list.has_modes = out_count > 0 || var_count > 0;
    if out_count > 1 {
        list.required_result = Some(builtin_oid::RECORD);
    }
    Ok(list)
}

pub fn create_function(interp: &mut PgCatalog, stmt: &CreateFunctionStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.funcname)?;
    let is_procedure = stmt.is_procedure;
    let attrs = compute_function_attributes(interp, is_procedure, &stmt.options)?;

    // The language: an inline body without LANGUAGE is SQL.
    let language = match attrs.language.as_deref() {
        Some(l) => l.to_ascii_lowercase(),
        None if stmt.sql_body.is_some() => "sql".to_owned(),
        None => return Err(DdlError::Parse("no language specified".into())),
    };
    let prolang = super::languages::check(interp, &language)?;

    // TRANSFORM FOR TYPE t: a transform for t (or its element type) and
    // the language must exist.
    if let Some(transform) = attrs.transform
        && let Some(ast::Node::List(types)) = transform.arg.as_ref()
    {
        for item in &types.items {
            let Some(ast::Node::TypeName(tn)) = item.as_ref() else {
                continue;
            };
            let typeid = typename_type_id(interp, tn)?;
            let typeid =
                crate::coerce::element_type(interp.unwrap_domain(typeid), interp).unwrap_or(typeid);
            if !super::languages::transform_exists(interp, typeid, &language) {
                return Err(DdlError::TypeNotFound(format!(
                    "transform for type {} language \"{language}\" does not exist",
                    format_type_for_message(interp, typeid)
                )));
            }
        }
    }

    let objtype = if is_procedure {
        ObjectType::ObjectProcedure
    } else {
        ObjectType::ObjectFunction
    };
    let params =
        interpret_function_parameter_list(interp, &stmt.parameters, Some(&language), objtype)?;

    // The result type.
    let (prorettype, proretset) = if is_procedure {
        (params.required_result.unwrap_or(VOID), false)
    } else if let Some(tn) = stmt.return_type.as_ref() {
        let rettype = compute_return_type(interp, tn, &language)?;
        if let Some(required) = params.required_result
            && required != rettype
        {
            return Err(DdlError::Parse(format!(
                "function result type must be {} because of OUT parameters",
                format_type_for_message(interp, required)
            )));
        }
        (rettype, tn.setof)
    } else if let Some(required) = params.required_result {
        (required, false)
    } else {
        return Err(DdlError::Parse(
            "function result type must be specified".into(),
        ));
    };

    interpret_as_clause(stmt, &language, attrs.as_clause, &params.in_types)?;

    if attrs.common.rows.is_some() && !proretset {
        return Err(DdlError::UnsupportedDdl(
            "ROWS is not applicable when function does not return a set".into(),
        ));
    }

    let prokind = if is_procedure {
        ProKind::Procedure
    } else if attrs.window {
        ProKind::Window
    } else {
        ProKind::Function
    };

    // PG stores proallargtypes / proargmodes only when some parameter is
    // OUT or VARIADIC, and proargnames when some parameter is named; the
    // analyzer keeps the three together whenever either applies.
    let keep_all = params.has_modes || params.names.iter().any(|n| !n.is_empty());
    let proc = PgProc {
        oid: PENDING_OID,
        proname: name,
        pronamespace: nsoid,
        prokind,
        proargtypes: params.in_types.clone(),
        prorettype,
        proretset,
        provariadic: params.variadic,
        proisstrict: attrs.common.strict().unwrap_or(false),
        pronargdefaults: params.default_types.len() as i16,
        proallargtypes: if keep_all {
            params.all_types.clone()
        } else {
            Vec::new()
        },
        proargmodes: if keep_all {
            params.modes.clone()
        } else {
            Vec::new()
        },
        proargnames: if keep_all {
            params.names.clone()
        } else {
            Vec::new()
        },
        provolatile: attrs.common.volatility().unwrap_or(ProVolatile::Volatile),
        proargdefaulttypes: params.default_types.clone(),
        prolang,
        prosupport: attrs.support,
    };
    let proc = procedure_create(interp, proc, stmt.replace)?;
    record_routine_body(interp, stmt, &attrs, &proc, &language);
    // ProcedureCreate: the routine depends on its language (the pinned
    // built-in ones record nothing).
    super::depend::record(
        interp,
        super::depend::ObjectAddress::proc(proc.oid),
        [super::depend::ObjectAddress::language(prolang)],
        DepType::Normal,
    );

    super::function_body::check_pseudo_types(interp, Some(&language), &proc)?;
    interp
        .proc_default_procs
        .insert(proc.oid, params.default_procs.clone());
    if params.defaults_data.is_empty() {
        interp.routine_defaults.remove(&proc.oid);
    } else {
        interp
            .routine_defaults
            .insert(proc.oid, params.defaults_data.clone());
    }
    // A replaced body replaces what was inlined of the old one.
    match super::function_body::inlinable_body(stmt, &proc) {
        Some(body) => {
            interp
                .inline_body_trust
                .insert(proc.oid, std::sync::OnceLock::new());
            interp.inline_sql_bodies.insert(proc.oid, Some(body))
        }
        None => {
            interp.inline_body_trust.remove(&proc.oid);
            interp.inline_sql_bodies.remove(&proc.oid)
        }
    };
    if attrs.common.set_items.is_empty() {
        interp.procs_with_config.remove(&proc.oid);
    } else {
        interp.procs_with_config.insert(proc.oid);
    }
    // The language's validator, with the routine already in the catalog (a
    // recursive SQL function resolves).
    if language == "internal" {
        fmgr_internal_validator(attrs.as_clause, &proc)?;
    }
    let settings = super::session::RoutineSettings::of(interp, &attrs.common.set_items);
    // An inline body was parsed (interpret_AS_clause) before the routine's
    // settings apply; only the validator runs under them.
    let body_refs = if stmt.sql_body.is_some() {
        super::function_body::validate_sql_function(interp, stmt, &proc, settings.check_bodies)?
    } else {
        settings.apply(interp, |interp| {
            let refs = super::function_body::validate_sql_function(
                interp,
                stmt,
                &proc,
                settings.check_bodies,
            )?;
            super::function_body::validate_plpgsql_function(interp, stmt, settings.check_bodies)?;
            Ok::<_, DdlError>(refs)
        })?
    };
    // What the inline body and the parameter defaults refer to.
    let addr = super::depend::ObjectAddress::proc(proc.oid);
    super::depend::record_references(interp, addr, &body_refs, DepType::Normal);
    super::depend::record_references(interp, addr, &params.default_refs, DepType::Normal);
    let mut refers = super::depend::type_references(&body_refs);
    refers.extend(super::depend::type_references(&params.default_refs));
    super::depend::note_expr_type_use(
        interp,
        addr,
        super::depend::ExprTypeUse {
            refers,
            results: Vec::new(),
        },
    );

    Ok(())
}

/// Record what a routine the migrations create runs, for the privilege
/// check of its body (`privileges`). Built-in routines and aggregates keep
/// none: their bodies belong to the bootstrap superuser, or aren't SQL.
fn record_routine_body(
    interp: &mut PgCatalog,
    stmt: &CreateFunctionStmt,
    attrs: &FunctionAttributes,
    proc: &PgProc,
    language: &str,
) {
    let security_definer = attrs.common.security.and_then(def_bool).unwrap_or(false);
    let search_path = routine_search_path(interp, &attrs.common.set_items, None);
    let source = if let Some(body) = stmt.sql_body.as_ref() {
        // PG stores an SQL-standard body analyzed (`prosqlbody`): what it
        // runs is what the creation `search_path` resolved now.
        let statements = super::function_body::atomic_body_statements(body);
        Some(RoutineSource::SqlStandard(Box::new(
            crate::privileges::resolve_sql_standard_body(interp, &statements, proc),
        )))
    } else {
        let text = attrs.as_clause.and_then(|de| {
            match de.arg.as_ref()? {
                ast::Node::List(l) => l.items.first().and_then(|n| node_string(n.as_ref())),
                ast::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            }
            .map(str::to_owned)
        });
        match text {
            Some(_) if language == "plpgsql" => interp
                .statement_sql
                .clone()
                .map(|create_sql| RoutineSource::Plpgsql { create_sql }),
            Some(text) => Some(RoutineSource::SqlText(text)),
            None => None,
        }
    };
    match source {
        Some(source) => {
            interp.routine_bodies.insert(
                proc.oid,
                RoutineBody {
                    language: language.to_owned(),
                    security_definer,
                    search_path,
                    source,
                },
            );
        }
        None => {
            interp.routine_bodies.remove(&proc.oid);
        }
    }
}

/// The `search_path` a routine's `SET` clauses leave: the last `SET
/// search_path` wins; `SET search_path TO DEFAULT` / `RESET search_path`
/// remove it. `current` is what the routine had before (ALTER FUNCTION).
fn routine_search_path(
    interp: &PgCatalog,
    set_items: &[&ast::Node],
    current: Option<Vec<String>>,
) -> Option<Vec<String>> {
    let mut path = current;
    for item in set_items {
        let ast::Node::VariableSetStmt(set) = item else {
            continue;
        };
        let kind = set.kind;
        // `RESET ALL` clears every setting, `search_path` included.
        if kind == VariableSetKind::VarResetAll {
            path = None;
            continue;
        }
        if !set.name.eq_ignore_ascii_case("search_path") {
            continue;
        }
        path = match kind {
            VariableSetKind::VarSetValue => super::session::search_path_args(set),
            VariableSetKind::VarSetCurrent => Some(interp.search_path_guc.effective().to_vec()),
            VariableSetKind::VarSetDefault | VariableSetKind::VarReset => None,
            _ => path,
        };
    }
    path
}

/// `typenameTypeId`: like LookupTypeName, but a shell type is an error.
pub(crate) fn typename_type_id(interp: &PgCatalog, tn: &TypeName) -> Result<PgTypeOid, DdlError> {
    let oid = lookup_type_name(tn, interp)?;
    if interp.pg_type.get(&oid).is_some_and(|t| !t.typisdefined) {
        return Err(DdlError::TypeNotFound(format!(
            "type \"{}\" is only a shell",
            type_name_to_string(tn)
        )));
    }
    Ok(oid)
}

/// `compute_return_type`: the RETURNS type. A shell type can't be the
/// result of a SQL function; a C / internal function may name a type that
/// doesn't exist yet, which PG creates as a shell (`NOTICE: type "x" is
/// not yet defined`) — how extension scripts declare a type's I/O
/// functions before the type itself.
fn compute_return_type(
    interp: &mut PgCatalog,
    tn: &TypeName,
    language: &str,
) -> Result<PgTypeOid, DdlError> {
    match lookup_type_name(tn, interp) {
        Ok(oid) => {
            // LookupTypeName's typenameTypeMod.
            crate::typmod::encode(interp, oid, tn)?;
            if language == "sql" && interp.pg_type.get(&oid).is_some_and(|t| !t.typisdefined) {
                return Err(DdlError::Parse(format!(
                    "SQL function cannot return shell type {}",
                    type_name_to_string(tn)
                )));
            }
            Ok(oid)
        }
        // Not for a name with `[]`, `%TYPE` or a type modifier (PostgreSQL
        // 18.7): it's no input function's.
        Err(DdlError::TypeNotFound(msg))
            if msg.starts_with("type \"")
                && tn.array_bounds.is_empty()
                && !tn.pct_type
                && tn.typmods.is_empty()
                && matches!(language, "c" | "internal") =>
        {
            let (nsoid, name) = ensure_qualified_name(interp, &tn.names)?;
            super::types::create_base_type(interp, nsoid, &name)
        }
        Err(e) => Err(e),
    }
}

/// `interpret_AS_clause`: exactly one body; an inline (`BEGIN ATOMIC` /
/// `RETURN`) body only for SQL, and without polymorphic arguments.
fn interpret_as_clause(
    stmt: &CreateFunctionStmt,
    language: &str,
    as_clause: Option<&DefElem>,
    in_types: &[PgTypeOid],
) -> Result<(), DdlError> {
    let invalid = |msg: &str| Err(DdlError::Parse(msg.to_owned()));
    match (stmt.sql_body.is_some(), as_clause.is_some()) {
        (false, false) => return invalid("no function body specified"),
        (true, true) => return invalid("duplicate function body specified"),
        _ => {}
    }
    if stmt.sql_body.is_some() {
        if language != "sql" {
            return invalid("inline SQL function body only valid for language SQL");
        }
        if in_types
            .iter()
            .any(|&t| crate::polymorphic::is_polymorphic(t))
        {
            return invalid(
                "SQL function with unquoted function body cannot have polymorphic arguments",
            );
        }
    }
    Ok(())
}

/// The C function names of the built-in (`LANGUAGE internal`) functions,
/// sorted (`SELECT DISTINCT prosrc FROM pg_proc WHERE prolang = 12` on
/// PostgreSQL 19.0 — the entries of `fmgr_builtins`).
const BUILTIN_PROSRC: &str = include_str!("builtin_prosrc.txt");

/// `fmgr_internal_validator` (pg_proc.c): a `LANGUAGE internal` function's
/// body names a built-in C function.
fn fmgr_internal_validator(as_clause: Option<&DefElem>, proc: &PgProc) -> Result<(), DdlError> {
    let prosrc = match as_clause.and_then(|de| de.arg.as_ref()) {
        Some(ast::Node::List(l)) => l
            .items
            .first()
            .and_then(|n| node_string(n.as_ref()))
            .map(str::to_owned),
        Some(ast::Node::String(s)) => Some(s.sval.clone()),
        _ => None,
    }
    .unwrap_or_else(|| proc.proname.clone());
    let prosrc = prosrc.trim();
    let mut names = BUILTIN_PROSRC.lines();
    if names.any(|n| n == prosrc) {
        return Ok(());
    }
    Err(DdlError::TypeNotFound(format!(
        "there is no built-in function named \"{prosrc}\""
    )))
}

/// `ProcedureCreate` (pg_proc.c) from the point the row is built: the
/// signature rules, then — when a routine of the same name and input types
/// exists — CREATE OR REPLACE's rules, keeping the existing OID. Returns the
/// stored row.
pub(crate) fn procedure_create(
    interp: &mut PgCatalog,
    mut proc: PgProc,
    replace: bool,
) -> Result<PgProc, DdlError> {
    // A polymorphic result (or OUT parameter) needs a polymorphic input to
    // be deduced from.
    let outs: Vec<PgTypeOid> = proc
        .proargmodes
        .iter()
        .zip(&proc.proallargtypes)
        .filter(|(m, _)| !matches!(m, ArgMode::In | ArgMode::Variadic))
        .map(|(_, &t)| t)
        .collect();
    for result in std::iter::once(proc.prorettype).chain(outs.iter().copied()) {
        check_valid_polymorphic_signature(result, &proc.proargtypes)?;
    }

    let existing = interp
        .proc_by_qname
        .get(&(proc.pronamespace, proc.proname.clone()))
        .into_iter()
        .flatten()
        .filter_map(|oid| interp.pg_proc.get(oid))
        .find(|p| p.proargtypes == proc.proargtypes)
        .cloned();
    // CREATE OR REPLACE keeps the routine's owner and ACL
    // (`ProcedureCreate` updates the row in place); removing and re-inserting
    // it would otherwise reset them.
    let mut preserved_owner = None;
    let mut preserved_acl = None;
    let oid = match existing {
        Some(existing) => {
            if !replace {
                return Err(DdlError::DuplicateObject(format!(
                    "function \"{}\" already exists with same argument types",
                    proc.proname
                )));
            }
            check_replacement(&existing, &proc)?;
            let object = super::acl::AclObject::Function(existing.oid);
            preserved_owner = Some(interp.owner_of(object));
            preserved_acl = interp.acls.explicit(object).cloned();
            interp.remove_pg_proc(existing.oid);
            super::depend::forget_dependencies_of(
                interp,
                super::depend::ObjectAddress::proc(existing.oid),
            );
            existing.oid
        }
        None => PgProcOid::from_nonzero(interp.alloc_oid()?),
    };
    proc.oid = oid;
    interp.insert_pg_proc(proc.clone());
    match preserved_owner {
        Some(owner) => {
            let object = super::acl::AclObject::Function(oid);
            interp.set_object_owner(object, owner);
            if let Some(acl) = preserved_acl {
                interp.acls.set_explicit(object, acl);
            }
        }
        None => {
            // ProcedureCreate: a new routine is the current role's, with the
            // function default privileges.
            let owner = super::roles::current_role(interp);
            super::acl::function_created(interp, oid, proc.pronamespace, owner);
        }
    }
    Ok(proc)
}

/// ProcedureCreate's rules for CREATE OR REPLACE of an existing routine:
/// same kind, same result (including SETOF and the OUT-parameter row, names
/// included), the input parameters keep their names, and existing defaults
/// stay (with their types).
fn check_replacement(existing: &PgProc, new: &PgProc) -> Result<(), DdlError> {
    if existing.prokind != new.prokind {
        return Err(DdlError::DuplicateObject(
            "cannot change routine kind".into(),
        ));
    }
    if existing.prorettype != new.prorettype || existing.proretset != new.proretset {
        return Err(DdlError::DuplicateObject(
            if new.prokind == ProKind::Procedure {
                "cannot change whether a procedure has output parameters"
            } else {
                "cannot change return type of existing function"
            }
            .into(),
        ));
    }
    // The row type the OUT parameters define: names and types.
    let out_row = |p: &PgProc| -> Vec<(String, PgTypeOid)> {
        p.proargmodes
            .iter()
            .zip(&p.proallargtypes)
            .enumerate()
            .filter(|(_, (m, _))| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
            .map(|(i, (_, &t))| (p.proargnames.get(i).cloned().unwrap_or_default(), t))
            .collect()
    };
    if new.prorettype == builtin_oid::RECORD && out_row(existing) != out_row(new) {
        return Err(DdlError::DuplicateObject(
            "cannot change return type of existing function (Row type defined by OUT parameters \
             is different.)"
                .into(),
        ));
    }
    // Input parameter names, in call order.
    let input_names = |p: &PgProc| -> Vec<String> {
        if p.proargmodes.is_empty() {
            return p.proargnames.clone();
        }
        p.proargmodes
            .iter()
            .zip(&p.proargnames)
            .filter(|(m, _)| matches!(m, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|(_, n)| n.clone())
            .collect()
    };
    let old_names = input_names(existing);
    let new_names = input_names(new);
    for (i, old) in old_names.iter().enumerate() {
        if !old.is_empty() && new_names.get(i) != Some(old) {
            return Err(DdlError::DuplicateObject(format!(
                "cannot change name of input parameter \"{old}\""
            )));
        }
    }
    let old_defaults = existing.pronargdefaults.max(0) as usize;
    if old_defaults > 0 {
        if new.proargdefaulttypes.len() < old_defaults {
            return Err(DdlError::DuplicateObject(
                "cannot remove parameter defaults from existing function".into(),
            ));
        }
        // The overlapping (trailing) defaults must keep their types.
        let new_tail = &new.proargdefaulttypes[new.proargdefaulttypes.len() - old_defaults..];
        let old_tail = &existing.proargdefaulttypes[existing
            .proargdefaulttypes
            .len()
            .saturating_sub(old_defaults)..];
        if !old_tail.is_empty() && new_tail != old_tail {
            return Err(DdlError::DuplicateObject(
                "cannot change data type of existing parameter default value".into(),
            ));
        }
    }
    Ok(())
}

/// `check_valid_polymorphic_signature` (parse_coerce.c): a polymorphic
/// result type is only allowed when an input can determine it.
pub(crate) fn check_valid_polymorphic_signature(
    result: PgTypeOid,
    inputs: &[PgTypeOid],
) -> Result<(), DdlError> {
    if valid_polymorphic_signature(result, inputs) {
        Ok(())
    } else {
        Err(DdlError::Parse("cannot determine result data type".into()))
    }
}

/// Whether an input of `inputs` determines polymorphic `result` (always,
/// for a non-polymorphic one).
pub(crate) fn valid_polymorphic_signature(result: PgTypeOid, inputs: &[PgTypeOid]) -> bool {
    use crate::polymorphic::is_polymorphic;
    if !is_polymorphic(result) {
        return true;
    }
    // Type OIDs of the polymorphic pseudo-types (pg_type.dat).
    let is = |t: PgTypeOid, oid: u32| t.get() == oid;
    let range_family1 = |t: PgTypeOid| is(t, 3831) || is(t, 4537); // anyrange, anymultirange
    let range_family2 = |t: PgTypeOid| is(t, 5080) || is(t, 4538); // anycompatible(multi)range
    let family2 = |t: PgTypeOid| is(t, 5077) || is(t, 5078) || is(t, 5079) || range_family2(t);
    if range_family1(result) {
        // A range / multirange result needs a range or multirange input.
        inputs.iter().any(|t| range_family1(*t))
    } else if range_family2(result) {
        inputs.iter().any(|t| range_family2(*t))
    } else if family2(result) {
        inputs.iter().any(|t| family2(*t))
    } else {
        inputs.iter().any(|t| is_polymorphic(*t) && !family2(*t))
    }
}

// ─── Routine lookup ─────────────────────────────────────────────────────────

/// `func_signature_string`: `name(type, type)` with the name as written.
pub(crate) fn func_signature_string(
    interp: &PgCatalog,
    names: &[String],
    args: &[PgTypeOid],
) -> String {
    format!(
        "{}({})",
        names.join("."),
        args.iter()
            .map(|&t| format_type_for_message(interp, t))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// One routine `FuncnameGetCandidates` offers: its OID and argument types
/// (input types, or every parameter when OUT arguments count).
struct FuncCandidate {
    oid: PgProcOid,
    args: Vec<PgTypeOid>,
}

/// `FuncnameGetCandidates` for a routine named by `names`, of any argument
/// count: the routines of that name in the named schema, or along the
/// search path — one routine hiding another of the same arguments in a
/// later schema. A missing schema is an error unless `missing_ok`.
fn funcname_get_candidates(
    interp: &PgCatalog,
    names: &[String],
    include_out_arguments: bool,
    missing_ok: bool,
) -> Result<Vec<FuncCandidate>, DdlError> {
    let (schema, name) = match names {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => {
            return Err(DdlError::Parse(format!(
                "improper qualified name (too many dotted names): {}",
                names.join(".")
            )));
        }
    };
    if let Some(s) = schema
        && interp.namespace_oid(s).is_none()
    {
        if missing_ok {
            return Ok(Vec::new());
        }
        return Err(DdlError::TypeNotFound(format!(
            "schema \"{s}\" does not exist"
        )));
    }
    let mut out: Vec<FuncCandidate> = Vec::new();
    for nsoid in interp.schemas_for_function_lookup(schema) {
        let Some(oids) = interp.proc_by_qname.get(&(nsoid, name.to_owned())) else {
            continue;
        };
        let mut here: Vec<FuncCandidate> = oids
            .iter()
            .filter_map(|oid| interp.pg_proc.get(oid))
            .map(|p| FuncCandidate {
                oid: p.oid,
                args: if include_out_arguments && !p.proallargtypes.is_empty() {
                    p.proallargtypes.clone()
                } else {
                    p.proargtypes.clone()
                },
            })
            .collect();
        here.sort_by_key(|c| c.oid);
        for c in here {
            if !out.iter().any(|o| o.args == c.args) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// `LookupFuncName` with an exact argument list (`None` for "any"): the
/// unique match, `None` when there's none.
pub(crate) fn lookup_func_name(
    interp: &PgCatalog,
    names: &[String],
    args: Option<&[PgTypeOid]>,
) -> Result<Option<PgProcOid>, DdlError> {
    let candidates = funcname_get_candidates(interp, names, false, true)?;
    let mut found = candidates
        .iter()
        .filter(|c| args.is_none_or(|a| c.args == a));
    let first = found.next().map(|c| c.oid);
    if first.is_some() && found.next().is_some() {
        return Err(DdlError::Parse(format!(
            "function name \"{}\" is not unique",
            names.join(".")
        )));
    }
    Ok(first)
}

enum LookupError {
    NoSuchFunc,
    Ambiguous,
}

/// `LookupFuncNameInternal`: the routine of `objtype` among the candidates
/// whose arguments are `args` (any, when `None`).
fn lookup_func_name_internal(
    interp: &PgCatalog,
    objtype: ObjectType,
    names: &[String],
    args: Option<&[PgTypeOid]>,
    include_out_arguments: bool,
    missing_ok: bool,
) -> Result<Result<PgProcOid, LookupError>, DdlError> {
    let candidates = funcname_get_candidates(interp, names, include_out_arguments, missing_ok)?;
    let mut result = None;
    for c in candidates {
        if args.is_some_and(|a| c.args != a) {
            continue;
        }
        let kind = interp.pg_proc.get(&c.oid).map(|p| p.prokind);
        let wanted = match objtype {
            ObjectType::ObjectFunction | ObjectType::ObjectAggregate => {
                kind != Some(ProKind::Procedure)
            }
            ObjectType::ObjectProcedure => kind == Some(ProKind::Procedure),
            _ => true,
        };
        if !wanted {
            continue;
        }
        if result.is_some() {
            return Ok(Err(LookupError::Ambiguous));
        }
        result = Some(c.oid);
    }
    Ok(result.ok_or(LookupError::NoSuchFunc))
}

/// The name of an `ObjectWithArgs` as written.
pub(crate) fn object_name(owa: &ObjectWithArgs) -> Vec<String> {
    owa.objname
        .iter()
        .filter_map(|n| node_string(n.as_ref()))
        .map(str::to_owned)
        .collect()
}

/// `LookupFuncWithArgs` (parse_func.c): the routine an `ObjectWithArgs`
/// names — by its input argument types, or by every argument for a
/// PROCEDURE / ROUTINE written without parameter modes, or by name alone
/// when no argument list is given (then it must be unique). The routine's
/// kind must fit `objtype`. `Ok(None)` only when `missing_ok`.
pub(crate) fn lookup_func_with_args(
    interp: &PgCatalog,
    objtype: ObjectType,
    owa: &ObjectWithArgs,
    missing_ok: bool,
) -> Result<Option<PgProcOid>, DdlError> {
    let names = object_name(owa);
    let mut argoids: Vec<PgTypeOid> = Vec::new();
    for arg in &owa.objargs {
        let Some(ast::Node::TypeName(tn)) = arg.as_ref() else {
            continue;
        };
        match lookup_type_name(tn, interp) {
            Ok(oid) => argoids.push(oid),
            Err(_) if missing_ok => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    let args = (!owa.args_unspecified).then_some(argoids.as_slice());
    let first_objtype = if owa.args_unspecified {
        objtype
    } else {
        ObjectType::ObjectRoutine
    };
    let mut result =
        lookup_func_name_internal(interp, first_objtype, &names, args, false, missing_ok)?;
    // A PROCEDURE / ROUTINE argument list without parameter modes may list
    // the OUT arguments too.
    if matches!(
        objtype,
        ObjectType::ObjectProcedure | ObjectType::ObjectRoutine
    ) && !owa.objfuncargs.is_empty()
        && !matches!(result, Err(LookupError::Ambiguous))
    {
        let have_param_mode = owa.objfuncargs.iter().any(|a| {
            matches!(a.as_ref(), Some(ast::Node::FunctionParameter(fp))
                if fp.mode != FunctionParameterMode::FuncParamDefault)
        });
        if !have_param_mode {
            let with_out = lookup_func_name_internal(
                interp,
                objtype,
                &names,
                Some(&argoids),
                true,
                missing_ok,
            )?;
            result = match (result, with_out) {
                (Ok(a), Ok(b)) if a != b => Err(LookupError::Ambiguous),
                (_, Ok(b)) => Ok(b),
                (_, Err(LookupError::Ambiguous)) => Err(LookupError::Ambiguous),
                (r, Err(LookupError::NoSuchFunc)) => r,
            };
        }
    }
    let signature = || func_signature_string(interp, &names, &argoids);
    let written = names.join(".");
    match result {
        Ok(oid) => {
            let kind = interp.pg_proc.get(&oid).map(|p| p.prokind);
            let wrong = |msg: String| Err(DdlError::UnsupportedDdl(msg));
            match objtype {
                ObjectType::ObjectFunction if kind == Some(ProKind::Procedure) => {
                    return wrong(format!("{} is not a function", signature()));
                }
                ObjectType::ObjectProcedure if kind != Some(ProKind::Procedure) => {
                    return wrong(format!("{} is not a procedure", signature()));
                }
                ObjectType::ObjectAggregate if kind != Some(ProKind::Aggregate) => {
                    return wrong(format!("function {} is not an aggregate", signature()));
                }
                _ => {}
            }
            Ok(Some(oid))
        }
        Err(LookupError::NoSuchFunc) if missing_ok => Ok(None),
        Err(LookupError::NoSuchFunc) => {
            let unspecified = owa.args_unspecified;
            Err(DdlError::TypeNotFound(match objtype {
                ObjectType::ObjectProcedure if unspecified => {
                    format!("could not find a procedure named \"{written}\"")
                }
                ObjectType::ObjectProcedure => format!("procedure {} does not exist", signature()),
                ObjectType::ObjectAggregate if unspecified => {
                    format!("could not find an aggregate named \"{written}\"")
                }
                ObjectType::ObjectAggregate if argoids.is_empty() => {
                    format!("aggregate {written}(*) does not exist")
                }
                ObjectType::ObjectAggregate => format!("aggregate {} does not exist", signature()),
                _ if unspecified => format!("could not find a function named \"{written}\""),
                _ => format!("function {} does not exist", signature()),
            }))
        }
        Err(LookupError::Ambiguous) => {
            let kind = match objtype {
                ObjectType::ObjectProcedure => "procedure",
                ObjectType::ObjectAggregate => "aggregate",
                ObjectType::ObjectRoutine => "routine",
                _ => "function",
            };
            let hint = if owa.args_unspecified {
                format!(" (Specify the argument list to select the {kind} unambiguously.)")
            } else {
                String::new()
            };
            Err(DdlError::Parse(format!(
                "{kind} name \"{written}\" is not unique{hint}"
            )))
        }
    }
}

/// `IsThereFunctionInNamespace` (functioncmds.c): renaming or moving a
/// routine must not collide with one of the same name and input types.
pub(crate) fn check_no_function_in_namespace(
    interp: &PgCatalog,
    name: &str,
    argtypes: &[PgTypeOid],
    nsoid: PgNamespaceOid,
) -> Result<(), DdlError> {
    let exists = interp
        .proc_by_qname
        .get(&(nsoid, name.to_owned()))
        .into_iter()
        .flatten()
        .filter_map(|oid| interp.pg_proc.get(oid))
        .any(|p| p.proargtypes == argtypes);
    if exists {
        let args = argtypes
            .iter()
            .map(|&t| format_type_for_message(interp, t))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DdlError::DuplicateObject(format!(
            "function {name}({args}) already exists in schema \"{}\"",
            interp.namespace_name(nsoid).unwrap_or_default()
        )));
    }
    Ok(())
}

/// `ALTER FUNCTION / PROCEDURE / ROUTINE name[(args)] action ...`
/// (`AlterFunction`, functioncmds.c): the routine must not be an aggregate,
/// each option appears once and fits the routine's kind; the volatility and
/// strictness actions update the pg_proc row (the rest don't affect static
/// analysis).
pub fn alter_function(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterFunctionStmt,
) -> Result<(), DdlError> {
    let Some(func) = stmt.func.as_ref() else {
        return Ok(());
    };
    let objtype = stmt.objtype;
    let Some(oid) = lookup_func_with_args(interp, objtype, func, false)? else {
        return Ok(());
    };
    let Some(proc) = interp.pg_proc.get(&oid).cloned() else {
        return Ok(());
    };
    if proc.prokind == ProKind::Aggregate {
        return Err(DdlError::UnsupportedDdl(format!(
            "\"{}\" is an aggregate function",
            object_name(func).join(".")
        )));
    }
    let is_procedure = proc.prokind == ProKind::Procedure;
    let mut common = CommonAttributes::default();
    for action in &stmt.actions {
        let Some(ast::Node::DefElem(de)) = action.as_ref() else {
            continue;
        };
        if !common.take(is_procedure, de)? {
            return Err(DdlError::UnsupportedDdl(format!(
                "option \"{}\" not recognized",
                de.defname
            )));
        }
    }
    // AlterFunction's order: COST, ROWS (also against the result), SUPPORT,
    // PARALLEL, then the SET items.
    let deferred_sets = CommonAttributes {
        set_items: std::mem::take(&mut common.set_items),
        ..CommonAttributes::default()
    };
    if let Some(cost) = common.cost
        && def_numeric(cost).is_some_and(|v| v <= 0.0)
    {
        return Err(DdlError::UnsupportedDdl("COST must be positive".into()));
    }
    if let Some(rows) = common.rows.take() {
        if def_numeric(rows).is_some_and(|v| v <= 0.0) {
            return Err(DdlError::UnsupportedDdl("ROWS must be positive".into()));
        }
        if !proc.proretset {
            return Err(DdlError::UnsupportedDdl(
                "ROWS is not applicable when function does not return a set".into(),
            ));
        }
    }
    common.cost = None;
    let support = common.validate(interp)?;
    deferred_sets.validate(interp)?;
    if !deferred_sets.set_items.is_empty() {
        interp.procs_with_config.insert(oid);
    }
    let volatility = common.volatility();
    let strict = common.strict();
    // What the routine's body sees, for the privilege check: its
    // `SECURITY DEFINER` flag and its `SET search_path` are part of
    // `proconfig` too.
    let security = common.security.and_then(def_bool);
    if let Some(mut body) = interp.routine_bodies.get(&oid).cloned() {
        if let Some(security) = security {
            body.security_definer = security;
        }
        if !deferred_sets.set_items.is_empty() {
            body.search_path =
                routine_search_path(interp, &deferred_sets.set_items, body.search_path);
        }
        interp.routine_bodies.insert(oid, body);
    }
    if let Some(p) = interp.pg_proc.get_mut(&oid) {
        if let Some(v) = volatility {
            p.provolatile = v;
        }
        if let Some(s) = strict {
            p.proisstrict = s;
        }
        if support.is_some() {
            p.prosupport = support;
        }
    }
    Ok(())
}

/// `ALTER FUNCTION / PROCEDURE / ROUTINE / AGGREGATE ... RENAME TO`
/// (`AlterObjectRename_internal`): the new name must be free for the
/// routine's input types in its schema.
pub(crate) fn rename_routine(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::RenameStmt,
    objtype: ObjectType,
) -> Result<(), DdlError> {
    let Some(ast::Node::ObjectWithArgs(owa)) = stmt.object.as_ref() else {
        return Ok(());
    };
    let Some(oid) = lookup_func_with_args(interp, objtype, owa, stmt.missing_ok)? else {
        return Ok(());
    };
    let Some(proc) = interp.pg_proc.get(&oid).cloned() else {
        return Ok(());
    };
    check_no_function_in_namespace(interp, &stmt.newname, &proc.proargtypes, proc.pronamespace)?;
    // A stored policy expression calls the routine by name: follow it.
    let schema = interp.namespace_name(proc.pronamespace).map(str::to_owned);
    crate::ddl::policies::rewrite_function_refs(interp, oid, schema.as_deref(), &stmt.newname);
    interp.rename_pg_proc(oid, stmt.newname.clone(), proc.pronamespace);
    Ok(())
}

/// `ALTER FUNCTION / ... SET SCHEMA` (`AlterObjectNamespace_internal`): the
/// routine's name must be free for its input types in the new schema.
pub(crate) fn set_routine_schema(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterObjectSchemaStmt,
    new_schema: &str,
    objtype: ObjectType,
) -> Result<(), DdlError> {
    let Some(ast::Node::ObjectWithArgs(owa)) = stmt.object.as_ref() else {
        return Ok(());
    };
    let Some(oid) = lookup_func_with_args(interp, objtype, owa, stmt.missing_ok)? else {
        return Ok(());
    };
    let Some(proc) = interp.pg_proc.get(&oid).cloned() else {
        return Ok(());
    };
    let Some(new_nsoid) = super::util::set_schema_target(interp, proc.pronamespace, new_schema)?
    else {
        return Ok(());
    };
    check_no_function_in_namespace(interp, &proc.proname, &proc.proargtypes, new_nsoid)?;
    // A stored policy expression calls the routine by name: follow it.
    let schema = interp.namespace_name(new_nsoid).map(str::to_owned);
    crate::ddl::policies::rewrite_function_refs(interp, oid, schema.as_deref(), &proc.proname);
    interp.rename_pg_proc(oid, proc.proname.clone(), new_nsoid);
    Ok(())
}

/// `IsBinaryCoercible` (parse_coerce.c): `src` is `target`, a domain over
/// it, fits a polymorphic target (any, anyelement, an array for anyarray,
/// ...), a composite for record, or has an implicit binary-method cast.
pub(crate) fn is_binary_coercible(interp: &PgCatalog, src: PgTypeOid, target: PgTypeOid) -> bool {
    use crate::pg_catalog::{CastContext, CastMethod, TypType};
    let raw = |oid: u32| PgTypeOid::from_raw(oid);
    if src == target || [raw(2276), raw(2283), raw(5077)].contains(&target) {
        return true;
    }
    let src = interp.unwrap_domain(src);
    if src == target {
        return true;
    }
    let typtype = interp.pg_type.get(&src).map(|t| t.typtype);
    let is_array = crate::coerce::element_type(src, interp).is_some();
    let fits = match target.get() {
        2277 | 5078 => is_array,                // anyarray, anycompatiblearray
        2776 | 5079 => !is_array,               // anynonarray, anycompatiblenonarray
        3500 => typtype == Some(TypType::Enum), // anyenum
        3831 | 5080 => typtype == Some(TypType::Range), // anyrange, anycompatiblerange
        4537 | 4538 => typtype == Some(TypType::Multirange),
        2249 => crate::coerce::is_complex(src, interp), // record
        2287 => crate::coerce::element_type(src, interp)
            .is_some_and(|e| crate::coerce::is_complex(e, interp)),
        _ => false,
    };
    if fits {
        return true;
    }
    interp
        .cast_by_pair
        .get(&(src, target))
        .and_then(|oid| interp.pg_cast.get(oid))
        .is_some_and(|c| {
            c.castmethod == CastMethod::Binary && c.castcontext == CastContext::Implicit
        })
}
