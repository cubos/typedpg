//! CREATE AGGREGATE handler.
//!
//! Aggregates produce two rows: a `pg_proc` entry with `prokind = Aggregate`
//! and a `pg_aggregate` entry pointed at by `aggfnoid`. The reported return
//! type is the FINALFUNC return type when one is specified, otherwise the
//! STYPE (state type), mirroring how PostgreSQL resolves aggregate result
//! types.

use pg_query::protobuf::{DefineStmt, FunctionParameterMode, node};

use crate::oid::{PgProcOid, PgTypeOid};
use crate::pg_catalog::{AggKind, PgAggregate, PgProc, ProKind};

use super::DdlError;
use super::util::{ensure_qualified_name, resolve_type_name};
use crate::pg_catalog::PgCatalog;

pub fn define_aggregate(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    // Argument types come from `args`. The shape varies between
    //   CREATE AGGREGATE name (type1, type2)        — bare type list
    //   CREATE AGGREGATE name (a int, b int)        — FunctionParameter list
    //   CREATE AGGREGATE name (* )                  — zero-arg aggregate
    let mut arg_types: Vec<PgTypeOid> = Vec::new();
    // Parallel to `arg_types`; `""` for an unnamed argument.
    let mut arg_names: Vec<String> = Vec::new();
    let mut variadic_oid: Option<PgTypeOid> = None;
    let arg_nodes: Vec<&pg_query::protobuf::Node> = if stmt.args.len() == 2
        && let Some(node::Node::List(list)) = stmt.args[0].node.as_ref()
    {
        list.items.iter().collect()
    } else {
        stmt.args.iter().collect()
    };

    for arg_node in arg_nodes {
        match arg_node.node.as_ref() {
            Some(node::Node::FunctionParameter(fp)) => {
                let mode = FunctionParameterMode::try_from(fp.mode)
                    .unwrap_or(FunctionParameterMode::FuncParamIn);
                let Some(resolved) = fp
                    .arg_type
                    .as_ref()
                    .and_then(|tn| resolve_type_name(tn, interp))
                else {
                    continue;
                };
                if mode == FunctionParameterMode::FuncParamVariadic {
                    variadic_oid = Some(
                        crate::polymorphic::variadic_element_type(resolved, interp).ok_or_else(
                            || {
                                DdlError::UnsupportedDdl(
                                    "VARIADIC parameter must be an array".into(),
                                )
                            },
                        )?,
                    );
                }
                arg_types.push(resolved);
                arg_names.push(fp.name.clone());
            }
            Some(node::Node::TypeName(tn)) => {
                if let Some(oid) = resolve_type_name(tn, interp) {
                    arg_types.push(oid);
                    arg_names.push(String::new());
                }
            }
            _ => {}
        }
    }

    // Walk the option list (`SFUNC`, `STYPE`, `FINALFUNC`, …).
    let mut state_type: Option<PgTypeOid> = None;
    let mut finalfunc: Option<(Option<String>, String)> = None;

    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let Some(arg) = de.arg.as_deref() else {
            continue;
        };
        match de.defname.to_ascii_lowercase().as_str() {
            "stype" => {
                if let Some(node::Node::TypeName(tn)) = arg.node.as_ref() {
                    state_type = resolve_type_name(tn, interp);
                }
            }
            "finalfunc" => {
                finalfunc = parse_func_name(arg);
            }
            _ => {}
        }
    }

    // Resolve the FINALFUNC's pg_proc oid (if declared) — the analyzer
    // walks `aggfinalfn -> pg_proc -> prorettype` at lookup time, so we
    // store the FK rather than the derived type. Also derive the
    // aggregate's own `prorettype` for the pg_proc row: PG sets it to the
    // finalfn's return type when one is declared, otherwise the state
    // type (which is what queries see for things like SUM(int) → bigint).
    let finalfn = finalfunc.as_ref().and_then(|(schema, name)| {
        interp
            .find_functions(schema.as_deref(), name)
            .first()
            .map(|f| f.oid)
    });
    let final_return = finalfn.and_then(|oid| interp.pg_proc.get(&oid).map(|p| p.prorettype));
    let Some(prorettype) = final_return.or(state_type) else {
        return Ok(());
    };

    // PG: aggregates and regular functions share the `pg_proc` namespace, so
    // creating an aggregate `name(args)` collides with any existing function
    // of the same signature (and vice versa). Reject up-front to mirror PG's
    // SQLSTATE 42723 (`function "X" already exists with same argument types`).
    if interp
        .pg_proc
        .values()
        .any(|p| p.pronamespace == nsoid && p.proname == name && p.proargtypes == arg_types)
    {
        return Err(DdlError::DuplicateObject(format!(
            "function \"{name}\" already exists with same argument types"
        )));
    }

    let proc_oid = PgProcOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_proc(PgProc {
        oid: proc_oid,
        proname: name,
        pronamespace: nsoid,
        prokind: ProKind::Aggregate,
        proargtypes: arg_types,
        prorettype,
        proretset: false,
        provariadic: variadic_oid,
        proisstrict: false,
        pronargdefaults: 0,
        proallargtypes: Vec::new(),
        proargmodes: Vec::new(),
        // PG stores no `proargnames` when every argument is unnamed.
        proargnames: if arg_names.iter().all(String::is_empty) {
            Vec::new()
        } else {
            arg_names
        },
        // PG aggregates are conventionally IMMUTABLE w.r.t. their input —
        // the analyzer never traverses an aggregate body in a CHECK /
        // GENERATED / index context anyway.
        provolatile: crate::pg_catalog::ProVolatile::Immutable,
    });
    // gram.y's `aggr_args` pairs the argument list with the number of
    // direct arguments: -1 for a plain aggregate, >= 0 for an ordered-set
    // one (`agg(direct ORDER BY aggregated)`); HYPOTHETICAL marks a
    // hypothetical-set aggregate (PG's DefineAggregate).
    let num_direct_args = match stmt.args.get(1).and_then(|n| n.node.as_ref()) {
        Some(node::Node::Integer(i)) if stmt.args.len() == 2 => i.ival,
        _ => -1,
    };
    let hypothetical = stmt.definition.iter().any(|opt| {
        matches!(opt.node.as_ref(), Some(node::Node::DefElem(de))
            if de.defname.eq_ignore_ascii_case("hypothetical"))
    });
    interp.insert_pg_aggregate(PgAggregate {
        aggfnoid: proc_oid,
        aggfinalfn: finalfn,
        aggkind: match (num_direct_args >= 0, hypothetical) {
            (false, _) => AggKind::Normal,
            (true, false) => AggKind::OrderedSet,
            (true, true) => AggKind::Hypothetical,
        },
        aggnumdirectargs: num_direct_args.max(0) as i16,
    });

    Ok(())
}

/// Parse a function name from a DefElem argument.
fn parse_func_name(arg: &pg_query::protobuf::Node) -> Option<(Option<String>, String)> {
    let parts: Vec<&str> = match arg.node.as_ref()? {
        node::Node::TypeName(tn) => tn
            .names
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect(),
        node::Node::String(s) => vec![s.sval.as_str()],
        node::Node::List(list) => list
            .items
            .iter()
            .filter_map(|n| match n.node.as_ref()? {
                node::Node::String(s) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };

    match parts.as_slice() {
        [schema, name] => Some((Some((*schema).to_owned()), (*name).to_owned())),
        [name] => Some((None, (*name).to_owned())),
        _ => None,
    }
}
