//! CREATE FUNCTION handler (signature registration only).

use pg_query::protobuf::{CreateFunctionStmt, FunctionParameterMode, node};

use crate::oid::{PgProcOid, PgTypeOid};
use crate::pg_catalog::{ArgMode, PgProc, ProKind, oid as builtin_oid};

use super::DdlError;
use super::util::{ensure_qualified_name, lookup_type_name, type_name_to_string};
use crate::pg_catalog::PgCatalog;

pub fn create_function(interp: &mut PgCatalog, stmt: &CreateFunctionStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.funcname)?;

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
    let prorettype = match explicit_return_oid {
        Some(oid) => oid,
        None => match out_count {
            // Procedures + plain RETURNS-less functions: PG records void here.
            // We don't have a void constant in `oid::*`, so fall back to
            // UNKNOWN — these never appear as expression results anyway.
            0 => builtin_oid::UNKNOWN,
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

    // Check for an existing pg_proc row with the same (name, args) — PG
    // shares the function/procedure/aggregate namespace, so a duplicate
    // signature collides regardless of prokind. CREATE OR REPLACE only
    // overrides when the *kind* matches; otherwise it's still a hard
    // duplicate (SQLSTATE 42723).
    let key = (nsoid, name.clone());
    if let Some(oids) = interp.proc_by_qname.get(&key).cloned() {
        let conflict = oids.iter().find(|&&oid| {
            interp
                .pg_proc
                .get(&oid)
                .is_some_and(|p| p.proargtypes == proargtypes)
        });
        if let Some(&conflict_oid) = conflict {
            let same_kind = interp.pg_proc.get(&conflict_oid).is_some_and(|p| {
                std::mem::discriminant(&p.prokind) == std::mem::discriminant(&prokind)
            });
            if stmt.replace && same_kind {
                // PG: `cannot change return type of existing function`
                // (SQLSTATE 42P13). CREATE OR REPLACE FUNCTION must keep
                // the same return type as the existing function — only the
                // body can change.
                if let Some(existing) = interp.pg_proc.get(&conflict_oid)
                    && existing.prorettype != prorettype
                {
                    return Err(DdlError::DuplicateObject(
                        "cannot change return type of existing function".into(),
                    ));
                }
                interp.remove_pg_proc(conflict_oid);
            } else {
                let kind = if matches!(prokind, ProKind::Procedure) {
                    "procedure"
                } else {
                    "function"
                };
                return Err(DdlError::DuplicateObject(format!(
                    "{kind} \"{name}\" already exists with same argument types"
                )));
            }
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
    interp.insert_pg_proc(proc.clone());
    if let Some(body) = super::function_body::inlinable_body(stmt, &proc) {
        interp.inline_sql_bodies.insert(oid, body);
    }
    // fmgr_sql_validator: the body is checked with the function already in
    // the catalog, so a recursive SQL function resolves.
    super::function_body::validate_sql_function(interp, stmt, &proc)?;

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
    stmt: &pg_query::protobuf::AlterFunctionStmt,
) -> Result<(), DdlError> {
    let Some(func) = stmt.func.as_ref() else {
        return Ok(());
    };
    let object = Some(Box::new(pg_query::protobuf::Node {
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
