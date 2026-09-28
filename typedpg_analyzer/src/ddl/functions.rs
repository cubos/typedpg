//! CREATE FUNCTION handler (signature registration only).

use pg_query::protobuf::{CreateFunctionStmt, FunctionParameterMode, node};

use crate::oid::{PgProcOid, PgTypeOid};
use crate::pg_catalog::{ArgMode, PgProc, ProKind, oid as builtin_oid};

use super::DdlError;
use super::util::{ensure_qualified_name, resolve_type_name};
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
    for param_node in &stmt.parameters {
        let Some(node::Node::FunctionParameter(fp)) = param_node.node.as_ref() else {
            continue;
        };
        let mode =
            FunctionParameterMode::try_from(fp.mode).unwrap_or(FunctionParameterMode::FuncParamIn);
        let Some(resolved_oid) = fp
            .arg_type
            .as_ref()
            .and_then(|tn| resolve_type_name(tn, interp))
        else {
            continue;
        };

        let arg_mode = match mode {
            FunctionParameterMode::FuncParamIn
            | FunctionParameterMode::FuncParamDefault
            | FunctionParameterMode::Undefined => ArgMode::In,
            FunctionParameterMode::FuncParamVariadic => ArgMode::Variadic,
            FunctionParameterMode::FuncParamInout => ArgMode::InOut,
            FunctionParameterMode::FuncParamOut => ArgMode::Out,
            FunctionParameterMode::FuncParamTable => ArgMode::Table,
        };

        if fp.defexpr.is_some() && !matches!(arg_mode, ArgMode::Out | ArgMode::Table) {
            pronargdefaults += 1;
        }
        match arg_mode {
            ArgMode::In => proargtypes.push(resolved_oid),
            ArgMode::Variadic => {
                proargtypes.push(resolved_oid);
                variadic_oid = Some(resolved_oid);
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
    let explicit_return_oid = stmt
        .return_type
        .as_ref()
        .and_then(|tn| resolve_type_name(tn, interp));
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
    interp.insert_pg_proc(PgProc {
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
    });

    Ok(())
}
