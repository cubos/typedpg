//! CREATE FUNCTION handler (signature registration only).

use typedpg_pg_query::protobuf::{CreateFunctionStmt, FunctionParameterMode, node};

use crate::oid::{PgProcOid, PgTypeOid};
use crate::pg_catalog::{ArgMode, PgProc, ProKind, oid as builtin_oid};

use super::DdlError;
use super::util::{ensure_qualified_name, lookup_type_name, type_name_to_string};
use crate::pg_catalog::PgCatalog;

pub fn create_function(interp: &mut PgCatalog, stmt: &CreateFunctionStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.funcname)?;
    // CreateFunction: the language must exist.
    if let Some(language) = function_language(stmt) {
        super::languages::check(interp, &language)?;
    }

    // Walk parameters once, splitting IN/INOUT/VARIADIC into the call
    // signature and OUT/TABLE/INOUT into the named output columns.
    let mut proargtypes: Vec<PgTypeOid> = Vec::new();
    let mut proallargtypes: Vec<PgTypeOid> = Vec::new();
    let mut proargmodes: Vec<ArgMode> = Vec::new();
    let mut proargnames: Vec<String> = Vec::new();
    let mut variadic_oid: Option<PgTypeOid> = None;
    // Input parameters declared with a DEFAULT — PG only allows them as a
    // trailing run, so the count alone locates them.
    let mut pronargdefaults: i16 = 0;
    let mut proargdefaulttypes: Vec<PgTypeOid> = Vec::new();
    for param_node in &stmt.parameters {
        let Some(node::Node::FunctionParameter(fp)) = param_node.node.as_ref() else {
            continue;
        };
        let mode =
            FunctionParameterMode::try_from(fp.mode).unwrap_or(FunctionParameterMode::FuncParamIn);
        let Some(tn) = fp.arg_type.as_ref() else {
            continue;
        };
        // `interpret_function_parameter_list` (functioncmds.c) reports an
        // unknown parameter type with the name unquoted, unlike every other
        // `type "x" does not exist` site.
        let resolved_oid = lookup_type_name(tn, interp).map_err(|e| match e {
            DdlError::TypeNotFound(msg) if msg.starts_with("type \"") => {
                DdlError::TypeNotFound(format!("type {} does not exist", type_name_to_string(tn)))
            }
            other => other,
        })?;

        let arg_mode = match mode {
            FunctionParameterMode::FuncParamIn
            | FunctionParameterMode::FuncParamDefault
            | FunctionParameterMode::Undefined => ArgMode::In,
            FunctionParameterMode::FuncParamVariadic => ArgMode::Variadic,
            FunctionParameterMode::FuncParamInout => ArgMode::InOut,
            FunctionParameterMode::FuncParamOut => ArgMode::Out,
            FunctionParameterMode::FuncParamTable => ArgMode::Table,
        };

        // interpret_function_parameter_list: only input parameters take a
        // DEFAULT, and once one does every later input parameter must too.
        let is_input = !matches!(arg_mode, ArgMode::Out | ArgMode::Table);
        match fp.defexpr.as_deref() {
            Some(_) if !is_input => {
                return Err(DdlError::Parse(
                    "only input parameters can have default values".into(),
                ));
            }
            Some(expr) => {
                pronargdefaults += 1;
                let polymorphic = crate::polymorphic::is_polymorphic(resolved_oid);
                proargdefaulttypes.push(super::defaults::check_function_default(
                    interp,
                    expr,
                    resolved_oid,
                    polymorphic,
                )?);
            }
            None if is_input && pronargdefaults > 0 => {
                return Err(DdlError::Parse(
                    "input parameters after one with a default value must also have defaults"
                        .into(),
                ));
            }
            None => {}
        }
        match arg_mode {
            ArgMode::In => proargtypes.push(resolved_oid),
            ArgMode::Variadic => {
                proargtypes.push(resolved_oid);
                variadic_oid = Some(
                    crate::polymorphic::variadic_element_type(resolved_oid, interp).ok_or_else(
                        || DdlError::UnsupportedDdl("VARIADIC parameter must be an array".into()),
                    )?,
                );
            }
            ArgMode::InOut => proargtypes.push(resolved_oid),
            ArgMode::Out | ArgMode::Table => {}
        }
        proallargtypes.push(resolved_oid);
        proargmodes.push(arg_mode);
        proargnames.push(fp.name.clone());
    }

    // Drop proallargtypes/modes/names if every entry is an IN with no name —
    // PG only stores them when there's something interesting to record.
    let all_simple_in = proargmodes.iter().all(|m| matches!(m, ArgMode::In))
        && proargnames.iter().all(|n| n.is_empty());
    if all_simple_in {
        proallargtypes.clear();
        proargmodes.clear();
        proargnames.clear();
    }

    // Resolve return type. PG synthesizes one when there's no explicit
    // RETURNS but OUT/INOUT params are present.
    let explicit_return_oid = match stmt.return_type.as_ref() {
        Some(tn) => Some(match lookup_type_name(tn, interp) {
            Ok(oid) => oid,
            // compute_return_type (functioncmds.c): a C / internal function
            // may return a not-yet-defined type — PG creates it as a shell
            // (`NOTICE: type "x" is not yet defined`), which is how
            // extension scripts declare a type's I/O functions before the
            // type itself.
            Err(DdlError::TypeNotFound(msg))
                if msg.starts_with("type \"")
                    && tn.array_bounds.is_empty()
                    && !tn.pct_type
                    && matches!(function_language(stmt).as_deref(), Some("c" | "internal")) =>
            {
                let (nsoid, name) = super::util::ensure_qualified_name(interp, &tn.names)?;
                super::types::create_base_type(interp, nsoid, &name)?
            }
            Err(e) => return Err(e),
        }),
        None => None,
    };
    let out_count = proargmodes
        .iter()
        .filter(|m| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
        .count();
    // CreateFunction: a procedure returns void, or record with OUT
    // parameters; a function needs RETURNS unless OUT parameters give it
    // its result type.
    const VOID: PgTypeOid = PgTypeOid::from_raw(2278);
    let prorettype = match explicit_return_oid {
        Some(oid) => oid,
        None if stmt.is_procedure => {
            if out_count == 0 {
                VOID
            } else {
                builtin_oid::RECORD
            }
        }
        None => match out_count {
            0 => {
                return Err(DdlError::Parse(
                    "function result type must be specified".into(),
                ));
            }
            1 => proargmodes
                .iter()
                .zip(proallargtypes.iter())
                .find(|(m, _)| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
                .map(|(_, &oid)| oid)
                .unwrap_or(builtin_oid::UNKNOWN),
            _ => builtin_oid::RECORD,
        },
    };

    let proretset = stmt.return_type.as_ref().is_some_and(|tn| tn.setof);

    // Check options for STRICT (CALLED ON NULL INPUT vs RETURNS NULL ON NULL INPUT).
    let proisstrict = stmt.options.iter().any(|n| {
        if let Some(node::Node::DefElem(de)) = n.node.as_ref()
            && de.defname == "strict"
            && let Some(arg) = de.arg.as_deref()
        {
            if let Some(node::Node::Integer(i)) = arg.node.as_ref() {
                return i.ival == 1;
            }
            if let Some(node::Node::Boolean(b)) = arg.node.as_ref() {
                return b.boolval;
            }
        }
        false
    });

    let prokind = if stmt.is_procedure {
        ProKind::Procedure
    } else {
        ProKind::Function
    };

    // Volatility — `IMMUTABLE` / `STABLE` / `VOLATILE` show up in
    // `stmt.options` as DefElems with defname="volatility". PG's default
    // is `VOLATILE` when the option is absent.
    let provolatile = stmt
        .options
        .iter()
        .find_map(|n| {
            let node::Node::DefElem(de) = n.node.as_ref()? else {
                return None;
            };
            if de.defname != "volatility" {
                return None;
            }
            let arg = de.arg.as_deref()?;
            let node::Node::String(s) = arg.node.as_ref()? else {
                return None;
            };
            match s.sval.as_str() {
                "immutable" => Some(crate::pg_catalog::ProVolatile::Immutable),
                "stable" => Some(crate::pg_catalog::ProVolatile::Stable),
                _ => Some(crate::pg_catalog::ProVolatile::Volatile),
            }
        })
        .unwrap_or(crate::pg_catalog::ProVolatile::Volatile);

    // ProcedureCreate: a polymorphic result (or OUT parameter) needs a
    // polymorphic input to be deduced from (check_valid_polymorphic_signature).
    let input_types: Vec<PgTypeOid> = proargtypes.clone();
    let out_types: Vec<PgTypeOid> = proargmodes
        .iter()
        .zip(&proallargtypes)
        .filter(|(m, _)| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
        .map(|(_, &t)| t)
        .collect();
    for result in std::iter::once(prorettype).chain(out_types.iter().copied()) {
        check_valid_polymorphic_signature(result, &input_types)?;
    }

    // Check for an existing pg_proc row with the same (name, args) — PG
    // shares the function/procedure/aggregate namespace, so a duplicate
    // signature collides regardless of prokind.
    let key = (nsoid, name.clone());
    if let Some(oids) = interp.proc_by_qname.get(&key).cloned() {
        let conflict = oids.iter().find(|&&oid| {
            interp
                .pg_proc
                .get(&oid)
                .is_some_and(|p| p.proargtypes == proargtypes)
        });
        if let Some(&conflict_oid) = conflict {
            let Some(existing) = interp.pg_proc.get(&conflict_oid).cloned() else {
                return Ok(());
            };
            if !stmt.replace {
                let kind = if matches!(prokind, ProKind::Procedure) {
                    "procedure"
                } else {
                    "function"
                };
                return Err(DdlError::DuplicateObject(format!(
                    "{kind} \"{name}\" already exists with same argument types"
                )));
            }
            check_replacement(
                &existing,
                prokind,
                prorettype,
                proretset,
                &proargmodes,
                &proallargtypes,
                &proargnames,
                &proargdefaulttypes,
            )?;
            interp.remove_pg_proc(conflict_oid);
        }
    }

    let oid = PgProcOid::from_nonzero(interp.alloc_oid()?);
    let proc = PgProc {
        oid,
        proname: name,
        pronamespace: nsoid,
        prokind,
        proargtypes,
        prorettype,
        proretset,
        provariadic: variadic_oid,
        proisstrict,
        pronargdefaults,
        proallargtypes,
        proargmodes,
        proargnames,
        provolatile,
        proargdefaulttypes,
    };
    super::function_body::check_pseudo_types(interp, function_language(stmt).as_deref(), &proc)?;
    interp.insert_pg_proc(proc.clone());
    if let Some(body) = super::function_body::inlinable_body(stmt, &proc) {
        interp.inline_sql_bodies.insert(oid, body);
    }
    // fmgr_sql_validator: the body is checked with the function already in
    // the catalog, so a recursive SQL function resolves.
    super::function_body::validate_sql_function(interp, stmt, &proc)?;
    super::function_body::validate_plpgsql_function(interp, stmt)?;

    Ok(())
}

/// The `LANGUAGE` of a CREATE FUNCTION, lowercased.
fn function_language(stmt: &CreateFunctionStmt) -> Option<String> {
    stmt.options.iter().find_map(|n| {
        let node::Node::DefElem(de) = n.node.as_ref()? else {
            return None;
        };
        if de.defname != "language" {
            return None;
        }
        let node::Node::String(s) = de.arg.as_deref()?.node.as_ref()? else {
            return None;
        };
        Some(s.sval.to_ascii_lowercase())
    })
}

/// `ALTER FUNCTION / PROCEDURE name[(args)] action ...`
/// (`AlterFunction`, functioncmds.c): the volatility and strictness
/// actions update the pg_proc row; the rest (cost, security, SET ...) don't
/// affect static analysis.
pub fn alter_function(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterFunctionStmt,
) -> Result<(), DdlError> {
    let Some(func) = stmt.func.as_ref() else {
        return Ok(());
    };
    let object = Some(Box::new(typedpg_pg_query::protobuf::Node {
        node: Some(node::Node::ObjectWithArgs(func.clone())),
    }));
    let Some((schema, name, arg_oids)) = super::alter::extract_func_target(&object, interp) else {
        return Ok(());
    };
    // Without an argument list the name must be unique.
    let any_args = func.args_unspecified;
    let matches = |p: &PgProc| any_args || p.proargtypes == arg_oids;
    let Some((_, oid)) = super::alter::find_proc(interp, schema.as_deref(), &name, &matches) else {
        let args = arg_oids
            .iter()
            .map(|&t| super::util::format_type_for_message(interp, t))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DdlError::TypeNotFound(if any_args {
            format!("could not find a function named \"{name}\"")
        } else {
            format!("function {name}({args}) does not exist")
        }));
    };
    let Some(proc) = interp.pg_proc.get_mut(&oid) else {
        return Ok(());
    };
    for action in &stmt.actions {
        let Some(node::Node::DefElem(de)) = action.node.as_ref() else {
            continue;
        };
        let arg = de.arg.as_deref().and_then(|a| a.node.as_ref());
        match (de.defname.as_str(), arg) {
            ("volatility", Some(node::Node::String(v))) => {
                proc.provolatile = match v.sval.as_str() {
                    "immutable" => crate::pg_catalog::ProVolatile::Immutable,
                    "stable" => crate::pg_catalog::ProVolatile::Stable,
                    _ => crate::pg_catalog::ProVolatile::Volatile,
                };
            }
            ("strict", Some(node::Node::Boolean(b))) => proc.proisstrict = b.boolval,
            ("strict", Some(node::Node::Integer(i))) => proc.proisstrict = i.ival != 0,
            _ => {}
        }
    }
    Ok(())
}

/// `check_valid_polymorphic_signature` (parse_coerce.c): a polymorphic
/// result type is only allowed when an input can determine it.
fn check_valid_polymorphic_signature(
    result: PgTypeOid,
    inputs: &[PgTypeOid],
) -> Result<(), DdlError> {
    use crate::polymorphic::is_polymorphic;
    if !is_polymorphic(result) {
        return Ok(());
    }
    // Type OIDs of the polymorphic pseudo-types (pg_type.dat).
    let is = |t: PgTypeOid, oid: u32| t.get() == oid;
    let range_family1 = |t: PgTypeOid| is(t, 3831) || is(t, 4537); // anyrange, anymultirange
    let range_family2 = |t: PgTypeOid| is(t, 5080) || is(t, 4538); // anycompatible(multi)range
    let family2 = |t: PgTypeOid| is(t, 5077) || is(t, 5078) || is(t, 5079) || range_family2(t);
    let ok = if range_family1(result) {
        // A range / multirange result needs a range or multirange input.
        inputs.iter().any(|t| range_family1(*t))
    } else if range_family2(result) {
        inputs.iter().any(|t| range_family2(*t))
    } else if family2(result) {
        inputs.iter().any(|t| family2(*t))
    } else {
        inputs.iter().any(|t| is_polymorphic(*t) && !family2(*t))
    };
    if ok {
        Ok(())
    } else {
        Err(DdlError::Parse("cannot determine result data type".into()))
    }
}

/// ProcedureCreate's rules for CREATE OR REPLACE of an existing routine:
/// same kind, same result (including SETOF and the OUT-parameter row), the
/// input parameters keep their names, and existing defaults stay (with
/// their types).
#[allow(clippy::too_many_arguments)]
fn check_replacement(
    existing: &PgProc,
    prokind: ProKind,
    prorettype: PgTypeOid,
    proretset: bool,
    proargmodes: &[ArgMode],
    proallargtypes: &[PgTypeOid],
    proargnames: &[String],
    proargdefaulttypes: &[PgTypeOid],
) -> Result<(), DdlError> {
    if std::mem::discriminant(&existing.prokind) != std::mem::discriminant(&prokind) {
        return Err(DdlError::DuplicateObject(
            "cannot change routine kind".into(),
        ));
    }
    if existing.prorettype != prorettype || existing.proretset != proretset {
        return Err(DdlError::DuplicateObject(
            "cannot change return type of existing function".into(),
        ));
    }
    let outs = |modes: &[ArgMode], types: &[PgTypeOid]| -> Vec<PgTypeOid> {
        modes
            .iter()
            .zip(types)
            .filter(|(m, _)| matches!(m, ArgMode::Out | ArgMode::InOut | ArgMode::Table))
            .map(|(_, &t)| t)
            .collect()
    };
    if prorettype == builtin_oid::RECORD
        && outs(&existing.proargmodes, &existing.proallargtypes)
            != outs(proargmodes, proallargtypes)
    {
        return Err(DdlError::DuplicateObject(
            "cannot change return type of existing function".into(),
        ));
    }
    // Input parameter names, in call order.
    let input_names = |modes: &[ArgMode], names: &[String]| -> Vec<String> {
        if modes.is_empty() {
            return names.to_vec();
        }
        modes
            .iter()
            .zip(names)
            .filter(|(m, _)| matches!(m, ArgMode::In | ArgMode::InOut | ArgMode::Variadic))
            .map(|(_, n)| n.clone())
            .collect()
    };
    let old_names = input_names(&existing.proargmodes, &existing.proargnames);
    let new_names = input_names(proargmodes, proargnames);
    for (i, old) in old_names.iter().enumerate() {
        if !old.is_empty() && new_names.get(i) != Some(old) {
            return Err(DdlError::DuplicateObject(format!(
                "cannot change name of input parameter \"{old}\""
            )));
        }
    }
    let old_defaults = existing.pronargdefaults.max(0) as usize;
    if proargdefaulttypes.len() < old_defaults {
        return Err(DdlError::DuplicateObject(
            "cannot remove parameter defaults from existing function".into(),
        ));
    }
    // The overlapping (trailing) defaults must keep their types.
    let new_tail = &proargdefaulttypes[proargdefaulttypes.len() - old_defaults..];
    let old_tail = &existing.proargdefaulttypes[existing
        .proargdefaulttypes
        .len()
        .saturating_sub(old_defaults)..];
    if !old_tail.is_empty() && new_tail != old_tail {
        return Err(DdlError::DuplicateObject(
            "cannot change data type of existing parameter default value".into(),
        ));
    }
    Ok(())
}
