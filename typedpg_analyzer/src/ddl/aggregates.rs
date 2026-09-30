//! CREATE [OR REPLACE] AGGREGATE (`DefineAggregate`, aggregatecmds.c, and
//! `AggregateCreate`, pg_aggregate.c).
//!
//! An aggregate is a `pg_proc` row with `prokind = Aggregate` plus a
//! `pg_aggregate` row. Its result type is the final function's (resolved
//! against the state and argument types when polymorphic), or the state
//! type when there is none; the support functions are looked up like
//! function calls and must take their arguments without run-time
//! coercion.

use typedpg_pg_query::protobuf::{DefElem, DefineStmt, ObjectType, TypeName, node};

use crate::oid::{PgProcOid, PgTypeOid};
use crate::pg_catalog::{AggKind, DepType, PgAggregate, PgProc, ProKind, ProVolatile, TypType};

use super::DdlError;
use super::depend::ObjectAddress;
use super::functions::{func_signature_string, typename_type_id};
use super::util::{
    ensure_qualified_name, format_type_for_message, node_string, type_name_to_string,
};
use crate::pg_catalog::PgCatalog;

const ANY: PgTypeOid = PgTypeOid::from_raw(2276);
const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);
const BYTEA: PgTypeOid = PgTypeOid::from_raw(17);

/// The options of CREATE AGGREGATE (`DefineAggregate`'s parameter loop).
#[derive(Default)]
struct AggregateOptions<'a> {
    transfn: Option<Vec<String>>,
    finalfn: Option<Vec<String>>,
    combinefn: Option<Vec<String>>,
    serialfn: Option<Vec<String>>,
    deserialfn: Option<Vec<String>>,
    mtransfn: Option<Vec<String>>,
    minvtransfn: Option<Vec<String>>,
    mfinalfn: Option<Vec<String>>,
    finalfn_extra: bool,
    mfinalfn_extra: bool,
    sortop: Option<Vec<String>>,
    base_type: Option<TypeName>,
    trans_type: Option<TypeName>,
    mtrans_type: Option<TypeName>,
    mtrans_space: bool,
    initval: Option<String>,
    minitval: Option<String>,
    parallel: Option<&'a DefElem>,
}

/// `defGetQualifiedName`.
fn def_qualified_name(de: &DefElem) -> Vec<String> {
    match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
        Some(node::Node::TypeName(tn)) => tn
            .names
            .iter()
            .filter_map(node_string)
            .map(str::to_owned)
            .collect(),
        Some(node::Node::List(l)) => l
            .items
            .iter()
            .filter_map(node_string)
            .map(str::to_owned)
            .collect(),
        Some(node::Node::String(s)) => vec![s.sval.clone()],
        _ => Vec::new(),
    }
}

/// `defGetTypeName`.
fn def_type_name(de: &DefElem) -> Option<TypeName> {
    match de.arg.as_deref()?.node.as_ref()? {
        node::Node::TypeName(tn) => Some(tn.clone()),
        node::Node::String(s) => Some(TypeName {
            names: vec![typedpg_pg_query::protobuf::Node {
                node: Some(node::Node::String(s.clone())),
            }],
            typemod: -1,
            ..Default::default()
        }),
        _ => None,
    }
}

/// `defGetString`.
fn def_string(de: &DefElem) -> Option<String> {
    match de.arg.as_deref()?.node.as_ref()? {
        node::Node::String(s) => Some(s.sval.clone()),
        node::Node::Integer(i) => Some(i.ival.to_string()),
        node::Node::Float(f) => Some(f.fval.clone()),
        node::Node::Boolean(b) => Some(if b.boolval { "true" } else { "false" }.into()),
        node::Node::TypeName(tn) => Some(type_name_to_string(tn)),
        node::Node::List(l) => Some(
            l.items
                .iter()
                .filter_map(node_string)
                .collect::<Vec<_>>()
                .join("."),
        ),
        _ => None,
    }
}

/// `defGetBoolean`: no value means true.
fn def_bool(de: &DefElem) -> bool {
    match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
        None => true,
        Some(node::Node::Boolean(b)) => b.boolval,
        Some(node::Node::Integer(i)) => i.ival != 0,
        Some(node::Node::String(s)) => super::reloptions::parse_bool(&s.sval).unwrap_or(false),
        _ => false,
    }
}

/// `extractModify`: FINALFUNC_MODIFY / MFINALFUNC_MODIFY.
fn check_modify(de: &DefElem) -> Result<(), DdlError> {
    match def_string(de).as_deref() {
        Some("read_only" | "shareable" | "read_write") => Ok(()),
        _ => Err(DdlError::Parse(format!(
            "parameter \"{}\" must be READ_ONLY, SHAREABLE, or READ_WRITE",
            de.defname
        ))),
    }
}

pub fn define_aggregate(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, name) = ensure_qualified_name(interp, &stmt.defnames)?;

    // gram.y's `aggr_args` pairs the argument list with the number of
    // direct arguments: -1 for a plain aggregate, >= 0 for an ordered-set
    // one (`agg(direct ORDER BY aggregated)`).
    let mut kind = AggKind::Normal;
    let mut num_direct_args: usize = 0;
    let mut args: &[typedpg_pg_query::protobuf::Node] = &[];
    if !stmt.oldstyle {
        if let Some(node::Node::List(list)) = stmt.args.first().and_then(|n| n.node.as_ref()) {
            args = &list.items;
        }
        if let Some(node::Node::Integer(i)) = stmt.args.get(1).and_then(|n| n.node.as_ref())
            && i.ival >= 0
        {
            kind = AggKind::OrderedSet;
            num_direct_args = i.ival as usize;
        }
    }

    let mut opts = AggregateOptions::default();
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "sfunc" | "sfunc1" => opts.transfn = Some(def_qualified_name(de)),
            "finalfunc" => opts.finalfn = Some(def_qualified_name(de)),
            "combinefunc" => opts.combinefn = Some(def_qualified_name(de)),
            "serialfunc" => opts.serialfn = Some(def_qualified_name(de)),
            "deserialfunc" => opts.deserialfn = Some(def_qualified_name(de)),
            "msfunc" => opts.mtransfn = Some(def_qualified_name(de)),
            "minvfunc" => opts.minvtransfn = Some(def_qualified_name(de)),
            "mfinalfunc" => opts.mfinalfn = Some(def_qualified_name(de)),
            "finalfunc_extra" => opts.finalfn_extra = def_bool(de),
            "mfinalfunc_extra" => opts.mfinalfn_extra = def_bool(de),
            "finalfunc_modify" | "mfinalfunc_modify" => check_modify(de)?,
            "sortop" => opts.sortop = Some(def_qualified_name(de)),
            "basetype" => opts.base_type = def_type_name(de),
            "hypothetical" => {
                if def_bool(de) {
                    if kind == AggKind::Normal {
                        return Err(DdlError::Parse(
                            "only ordered-set aggregates can be hypothetical".into(),
                        ));
                    }
                    kind = AggKind::Hypothetical;
                }
            }
            "stype" | "stype1" => opts.trans_type = def_type_name(de),
            "mstype" => opts.mtrans_type = def_type_name(de),
            "msspace" => opts.mtrans_space = true,
            "initcond" | "initcond1" => opts.initval = def_string(de),
            "minitcond" => opts.minitval = def_string(de),
            "parallel" => opts.parallel = Some(de),
            // "sspace" is accepted; anything else is only a WARNING.
            _ => {}
        }
    }
    let invalid = |msg: &str| Err(DdlError::Parse(msg.to_owned()));
    let Some(trans_type) = opts.trans_type.as_ref() else {
        return invalid("aggregate stype must be specified");
    };
    let Some(transfn_name) = opts.transfn.as_ref() else {
        return invalid("aggregate sfunc must be specified");
    };
    if opts.mtrans_type.is_some() {
        if opts.mtransfn.is_none() {
            return invalid("aggregate msfunc must be specified when mstype is specified");
        }
        if opts.minvtransfn.is_none() {
            return invalid("aggregate minvfunc must be specified when mstype is specified");
        }
    } else {
        if opts.mtransfn.is_some() {
            return invalid("aggregate msfunc must not be specified without mstype");
        }
        if opts.minvtransfn.is_some() {
            return invalid("aggregate minvfunc must not be specified without mstype");
        }
        if opts.mfinalfn.is_some() {
            return invalid("aggregate mfinalfunc must not be specified without mstype");
        }
        if opts.mtrans_space {
            return invalid("aggregate msspace must not be specified without mstype");
        }
        if opts.minitval.is_some() {
            return invalid("aggregate minitcond must not be specified without mstype");
        }
    }

    // The argument types: the old `basetype =` form takes one (or none for
    // "ANY"), the new form a parameter list.
    let (arg_types, arg_names, variadic): (Vec<PgTypeOid>, Vec<String>, Option<PgTypeOid>) =
        if stmt.oldstyle {
            let Some(base) = opts.base_type.as_ref() else {
                return invalid("aggregate input type must be specified");
            };
            if type_name_to_string(base).eq_ignore_ascii_case("any") {
                (Vec::new(), Vec::new(), None)
            } else {
                (vec![typename_type_id(interp, base)?], Vec::new(), None)
            }
        } else {
            if opts.base_type.is_some() {
                return invalid("basetype is redundant with aggregate input type specification");
            }
            let params = super::functions::interpret_function_parameter_list(
                interp,
                args,
                None,
                ObjectType::ObjectAggregate,
            )?;
            (params.in_types, params.names, params.variadic)
        };
    let num_args = arg_types.len();

    let trans_type_id = typename_type_id(interp, trans_type)?;
    let is_pseudo = |t: PgTypeOid| {
        interp
            .pg_type
            .get(&t)
            .is_some_and(|ty| ty.typtype == TypType::Pseudo)
    };
    let polymorphic = crate::polymorphic::is_polymorphic;
    if is_pseudo(trans_type_id) && !polymorphic(trans_type_id) && trans_type_id != INTERNAL {
        return Err(DdlError::Parse(format!(
            "aggregate transition data type cannot be {}",
            format_type_for_message(interp, trans_type_id)
        )));
    }
    match (&opts.serialfn, &opts.deserialfn) {
        (Some(_), Some(_)) if trans_type_id != INTERNAL => {
            return invalid(
                "serialization functions may be specified only when the aggregate transition \
                 data type is internal",
            );
        }
        (Some(_), None) | (None, Some(_)) => {
            return invalid(
                "must specify both or neither of serialization and deserialization functions",
            );
        }
        _ => {}
    }
    let mtrans_type_id = match opts.mtrans_type.as_ref() {
        Some(tn) => {
            let t = typename_type_id(interp, tn)?;
            if is_pseudo(t) && !polymorphic(t) && t != INTERNAL {
                return Err(DdlError::Parse(format!(
                    "aggregate transition data type cannot be {}",
                    format_type_for_message(interp, t)
                )));
            }
            Some(t)
        }
        None => None,
    };
    // The initial conditions go through the state types' input functions.
    if let Some(init) = opts.initval.as_deref()
        && !is_pseudo(trans_type_id)
    {
        crate::literal_input::validate(init, trans_type_id, interp).map_err(DdlError::Parse)?;
    }
    if let (Some(init), Some(mt)) = (opts.minitval.as_deref(), mtrans_type_id)
        && !is_pseudo(mt)
    {
        crate::literal_input::validate(init, mt, interp).map_err(DdlError::Parse)?;
    }
    if let Some(parallel) = opts.parallel
        && !matches!(
            def_string(parallel).as_deref(),
            Some("safe" | "restricted" | "unsafe")
        )
    {
        return invalid("parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE");
    }

    // ── AggregateCreate ──
    if !super::functions::valid_polymorphic_signature(trans_type_id, &arg_types)
        || mtrans_type_id
            .is_some_and(|mt| !super::functions::valid_polymorphic_signature(mt, &arg_types))
    {
        return invalid("cannot determine transition data type");
    }
    if kind != AggKind::Normal && variadic.is_some_and(|v| v != ANY) {
        return Err(DdlError::UnsupportedDdl(
            "a variadic ordered-set aggregate must use VARIADIC type ANY".into(),
        ));
    }
    if kind == AggKind::Hypothetical && num_direct_args < num_args {
        let aggregated = num_args - num_direct_args;
        if variadic.is_some()
            || num_direct_args < aggregated
            || arg_types[num_direct_args - aggregated..num_direct_args]
                != arg_types[num_direct_args..]
        {
            return invalid(
                "a hypothetical-set aggregate must have direct arguments matching its aggregated \
                 arguments",
            );
        }
    }

    // The transition function takes the state and the aggregated arguments.
    let transfn_args: Vec<PgTypeOid> = std::iter::once(trans_type_id)
        .chain(if kind == AggKind::Normal {
            arg_types.iter().copied()
        } else if num_direct_args < num_args {
            arg_types[num_direct_args..].iter().copied()
        } else {
            // Only a variadic direct argument: the transition function
            // takes it once.
            arg_types[num_args.saturating_sub(1)..].iter().copied()
        })
        .collect();
    let mut referenced: Vec<ObjectAddress> = Vec::new();
    let (transfn, rettype) = lookup_agg_function(interp, transfn_name, &transfn_args, variadic)?;
    referenced.push(ObjectAddress::proc(transfn));
    if rettype != trans_type_id {
        return Err(DdlError::UnsupportedDdl(format!(
            "return type of transition function {} is not {}",
            transfn_name.join("."),
            format_type_for_message(interp, trans_type_id)
        )));
    }
    let strict_needs_init = |strict: bool, init: bool, state: PgTypeOid| {
        strict
            && !init
            && (num_args < 1 || !super::functions::is_binary_coercible(interp, arg_types[0], state))
    };
    let must_not_omit = "must not omit initial value when transition function is strict and \
                         transition type is not compatible with input type";
    let proc_strict = |oid: PgProcOid| interp.pg_proc.get(&oid).is_some_and(|p| p.proisstrict);
    if strict_needs_init(proc_strict(transfn), opts.initval.is_some(), trans_type_id) {
        return invalid(must_not_omit);
    }

    if let (Some(mname), Some(mt)) = (opts.mtransfn.as_ref(), mtrans_type_id) {
        let mut margs = transfn_args.clone();
        margs[0] = mt;
        let (mtransfn, mret) = lookup_agg_function(interp, mname, &margs, variadic)?;
        referenced.push(ObjectAddress::proc(mtransfn));
        if mret != mt {
            return Err(DdlError::UnsupportedDdl(format!(
                "return type of transition function {} is not {}",
                mname.join("."),
                format_type_for_message(interp, mt)
            )));
        }
        let mtrans_strict = proc_strict(mtransfn);
        if strict_needs_init(mtrans_strict, opts.minitval.is_some(), mt) {
            return invalid(must_not_omit);
        }
        if let Some(iname) = opts.minvtransfn.as_ref() {
            let (minvtransfn, iret) = lookup_agg_function(interp, iname, &margs, variadic)?;
            referenced.push(ObjectAddress::proc(minvtransfn));
            if iret != mt {
                return Err(DdlError::UnsupportedDdl(format!(
                    "return type of inverse transition function {} is not {}",
                    iname.join("."),
                    format_type_for_message(interp, mt)
                )));
            }
            if proc_strict(minvtransfn) != mtrans_strict {
                return invalid(
                    "strictness of aggregate's forward and inverse transition functions must \
                     match",
                );
            }
        }
    }

    // The final function takes the state, plus the direct arguments (or
    // every argument with FINALFUNC_EXTRA).
    let final_args = |state: PgTypeOid, extra: bool| -> (Vec<PgTypeOid>, Option<PgTypeOid>) {
        let n = if extra { num_args } else { num_direct_args };
        let variadic = if !extra && num_direct_args < num_args {
            None
        } else {
            variadic
        };
        (
            std::iter::once(state)
                .chain(arg_types[..n].iter().copied())
                .collect(),
            variadic,
        )
    };
    let mut finalfn = None;
    let finaltype = match opts.finalfn.as_ref() {
        Some(fname) => {
            let (fargs, fvariadic) = final_args(trans_type_id, opts.finalfn_extra);
            let (oid, ftype) = lookup_agg_function(interp, fname, &fargs, fvariadic)?;
            referenced.push(ObjectAddress::proc(oid));
            if opts.finalfn_extra && proc_strict(oid) {
                return invalid("final function with extra arguments must not be declared STRICT");
            }
            finalfn = Some(oid);
            ftype
        }
        None => trans_type_id,
    };
    if let Some(cname) = opts.combinefn.as_ref() {
        let (oid, ctype) =
            lookup_agg_function(interp, cname, &[trans_type_id, trans_type_id], None)?;
        referenced.push(ObjectAddress::proc(oid));
        if ctype != trans_type_id {
            return Err(DdlError::UnsupportedDdl(format!(
                "return type of combine function {} is not {}",
                cname.join("."),
                format_type_for_message(interp, trans_type_id)
            )));
        }
        if trans_type_id == INTERNAL && proc_strict(oid) {
            return invalid(
                "combine function with transition type internal must not be declared STRICT",
            );
        }
    }
    if let Some(sname) = opts.serialfn.as_ref() {
        let (oid, stype) = lookup_agg_function(interp, sname, &[INTERNAL], None)?;
        referenced.push(ObjectAddress::proc(oid));
        if stype != BYTEA {
            return Err(DdlError::UnsupportedDdl(format!(
                "return type of serialization function {} is not bytea",
                sname.join(".")
            )));
        }
    }
    if let Some(dname) = opts.deserialfn.as_ref() {
        let (oid, dtype) = lookup_agg_function(interp, dname, &[BYTEA, INTERNAL], None)?;
        referenced.push(ObjectAddress::proc(oid));
        if dtype != INTERNAL {
            return Err(DdlError::UnsupportedDdl(format!(
                "return type of deserialization function {} is not internal",
                dname.join(".")
            )));
        }
    }
    if !super::functions::valid_polymorphic_signature(finaltype, &arg_types) {
        return Err(DdlError::UnsupportedDdl(
            "cannot determine result data type".into(),
        ));
    }
    if finaltype == INTERNAL && !arg_types.contains(&INTERNAL) {
        return invalid("unsafe use of pseudo-type \"internal\"");
    }
    if let Some(mt) = mtrans_type_id {
        let mrettype = match opts.mfinalfn.as_ref() {
            Some(mname) => {
                let (fargs, fvariadic) = final_args(mt, opts.mfinalfn_extra);
                let (oid, mtype) = lookup_agg_function(interp, mname, &fargs, fvariadic)?;
                referenced.push(ObjectAddress::proc(oid));
                if opts.mfinalfn_extra && proc_strict(oid) {
                    return invalid(
                        "final function with extra arguments must not be declared STRICT",
                    );
                }
                mtype
            }
            None => mt,
        };
        if mrettype != finaltype {
            return Err(DdlError::Parse(format!(
                "moving-aggregate implementation returns type {}, but plain implementation \
                 returns type {}",
                format_type_for_message(interp, mrettype),
                format_type_for_message(interp, finaltype)
            )));
        }
    }
    if let Some(sortop) = opts.sortop.as_ref() {
        if num_args != 1 {
            return invalid("sort operator can only be specified for single-argument aggregates");
        }
        let (schema, opname) = match sortop.as_slice() {
            [s, n] => (Some(s.as_str()), n.as_str()),
            [n] => (None, n.as_str()),
            _ => (None, ""),
        };
        let arg = arg_types[0];
        let Some(op) = super::drop::find_operator(interp, schema, opname, &|o| {
            o.oprleft == Some(arg) && o.oprright == arg
        }) else {
            let t = format_type_for_message(interp, arg);
            return Err(DdlError::TypeNotFound(format!(
                "operator does not exist: {t} {} {t}",
                sortop.join(".")
            )));
        };
        referenced.push(ObjectAddress::operator(op));
    }

    let existing_kind = interp
        .proc_by_qname
        .get(&(nsoid, name.clone()))
        .into_iter()
        .flatten()
        .filter_map(|oid| interp.pg_proc.get(oid))
        .find(|p| p.proargtypes == arg_types)
        .map(|p| (p.oid, interp.pg_aggregate.get(&p.oid).cloned()));
    let proc = PgProc {
        oid: super::functions::PENDING_OID,
        proname: name.clone(),
        pronamespace: nsoid,
        prokind: ProKind::Aggregate,
        proargtypes: arg_types.clone(),
        prorettype: finaltype,
        proretset: false,
        provariadic: variadic,
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
        provolatile: ProVolatile::Immutable,
        proargdefaulttypes: Vec::new(),
        prolang: crate::pg_catalog::INTERNAL_LANGUAGE,
    };
    let proc = super::functions::procedure_create(interp, proc, stmt.replace)?;
    // AggregateCreate's own replacement rules.
    if let Some((_, Some(old))) = existing_kind {
        if old.aggkind != kind {
            return Err(DdlError::UnsupportedDdl(
                "cannot change routine kind".into(),
            ));
        }
        if old.aggnumdirectargs as usize != num_direct_args {
            return invalid("cannot change number of direct arguments of an aggregate function");
        }
    }
    interp.insert_pg_aggregate(PgAggregate {
        aggfnoid: proc.oid,
        aggfinalfn: finalfn,
        aggkind: kind,
        aggnumdirectargs: num_direct_args as i16,
    });
    super::depend::record(
        interp,
        ObjectAddress::proc(proc.oid),
        referenced,
        DepType::Normal,
    );
    Ok(())
}

/// `lookup_agg_function` (pg_aggregate.c): resolve a support function like
/// a call with `input_types` — it must be a plain function, not return a
/// set, and take the arguments without run-time coercion. Returns it and
/// its result type, resolved against the inputs when polymorphic.
fn lookup_agg_function(
    interp: &PgCatalog,
    names: &[String],
    input_types: &[PgTypeOid],
    variadic: Option<PgTypeOid>,
) -> Result<(PgProcOid, PgTypeOid), DdlError> {
    let signature = || func_signature_string(interp, names, input_types);
    let not_found = || DdlError::TypeNotFound(format!("function {} does not exist", signature()));
    let (schema, name) = match names {
        [s, n] => (Some(s.as_str()), n.as_str()),
        [n] => (None, n.as_str()),
        _ => return Err(not_found()),
    };
    let resolved = crate::functions::resolve_function(
        interp,
        schema,
        name,
        input_types,
        &crate::functions::CallNotation::default(),
        false,
        None,
    )
    .map_err(|_| not_found())?;
    if resolved.is_aggregate || resolved.is_window {
        return Err(not_found());
    }
    if resolved.is_set_returning {
        return Err(DdlError::UnsupportedDdl(format!(
            "function {} returns a set",
            signature()
        )));
    }
    if variadic == Some(ANY) && resolved.provariadic != Some(ANY) {
        return Err(DdlError::UnsupportedDdl(format!(
            "function {} must accept VARIADIC ANY to be used in this aggregate",
            signature()
        )));
    }
    for (&input, &declared) in input_types.iter().zip(&resolved.arg_types) {
        if !super::functions::is_binary_coercible(interp, input, declared) {
            return Err(DdlError::UnsupportedDdl(format!(
                "function {} requires run-time type coercion",
                func_signature_string(interp, names, &resolved.arg_types)
            )));
        }
    }
    Ok((resolved.oid, resolved.return_type_oid))
}
