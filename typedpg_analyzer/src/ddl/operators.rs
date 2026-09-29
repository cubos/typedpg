//! CREATE OPERATOR / ALTER OPERATOR (`DefineOperator`, `AlterOperator`,
//! operatorcmds.c; `OperatorCreate`, pg_operator.c).

use typedpg_pg_query::protobuf::{AlterOperatorStmt, DefElem, DefineStmt, node};

use crate::oid::{PgNamespaceOid, PgOperatorOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{DepType, PgOperator, oid as builtin};

use super::DdlError;
use super::depend::ObjectAddress;
use super::functions::{func_signature_string, lookup_func_name, typename_type_id};
use super::util::{format_type_for_message, node_string};
use crate::pg_catalog::PgCatalog;

const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);

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

/// `op_signature_string`: `left op right` (`NONE` for a missing left).
fn op_signature(
    interp: &PgCatalog,
    names: &[String],
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> String {
    let left = left.map_or_else(|| "NONE".to_owned(), |l| format_type_for_message(interp, l));
    format!(
        "{left} {} {}",
        names.join("."),
        format_type_for_message(interp, right)
    )
}

/// `validOperatorName` (pg_operator.c).
fn valid_operator_name(name: &str) -> bool {
    let len = name.len();
    if len == 0 || len >= 64 {
        return false;
    }
    if !name.chars().all(|c| "~!@#^&|`?+-*/%<>=".contains(c)) {
        return false;
    }
    if name.contains("/*") || name.contains("--") {
        return false;
    }
    if len > 1 && (name.ends_with('+') || name.ends_with('-')) {
        let head = &name[..len - 1];
        if !head.chars().any(|c| "~!@#^&|`?%".contains(c)) {
            return false;
        }
    }
    name != "!="
}

/// `OperatorLookup`: the operator `names(left, right)`, if any, and
/// whether it is defined (not a shell).
fn operator_lookup(
    interp: &PgCatalog,
    names: &[String],
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> Option<(PgOperatorOid, bool)> {
    let (schema, name) = match names {
        [s, n] => (Some(s.as_str()), n.as_str()),
        [n] => (None, n.as_str()),
        _ => return None,
    };
    let oid = super::drop::find_operator(interp, schema, name, &|o: &PgOperator| {
        o.oprleft == left && o.oprright == right
    })?;
    let defined = interp
        .pg_operator
        .get(&oid)
        .is_some_and(|o| o.oprcode.is_some());
    Some((oid, defined))
}

/// `OperatorValidateParams` (pg_operator.c): which attributes a prefix or
/// non-boolean operator can't have.
#[allow(clippy::too_many_arguments)]
fn operator_validate_params(
    left: Option<PgTypeOid>,
    result: Option<PgTypeOid>,
    has_commutator: bool,
    has_negator: bool,
    has_restrict: bool,
    has_join: bool,
    can_merge: bool,
    can_hash: bool,
) -> Result<(), DdlError> {
    let invalid = |msg: &str| Err(DdlError::Parse(msg.to_owned()));
    if left.is_none() {
        if has_commutator {
            return invalid("only binary operators can have commutators");
        }
        if has_join {
            return invalid("only binary operators can have join selectivity");
        }
        if can_merge {
            return invalid("only binary operators can merge join");
        }
        if can_hash {
            return invalid("only binary operators can hash");
        }
    }
    if result != Some(builtin::BOOL) {
        if has_negator {
            return invalid("only boolean operators can have negators");
        }
        if has_restrict {
            return invalid("only boolean operators can have restriction selectivity");
        }
        if has_join {
            return invalid("only boolean operators can have join selectivity");
        }
        if can_merge {
            return invalid("only boolean operators can merge join");
        }
        if can_hash {
            return invalid("only boolean operators can hash");
        }
    }
    Ok(())
}

/// `ValidateRestrictionEstimator`: `name(internal, oid, internal, int4)`
/// returning float8.
fn validate_restriction_estimator(
    interp: &PgCatalog,
    names: &[String],
) -> Result<PgProcOid, DdlError> {
    let args = [INTERNAL, builtin::OID, INTERNAL, builtin::INT4];
    let oid = lookup_func_name(interp, names, Some(&args))?.ok_or_else(|| {
        DdlError::TypeNotFound(format!(
            "function {} does not exist",
            func_signature_string(interp, names, &args)
        ))
    })?;
    if interp.pg_proc.get(&oid).map(|p| p.prorettype) != Some(builtin::FLOAT8) {
        return Err(DdlError::Parse(format!(
            "restriction estimator function {} must return type double precision",
            names.join(".")
        )));
    }
    Ok(oid)
}

/// `ValidateJoinEstimator`: `name(internal, oid, internal, int2, internal)`
/// (or the older four-argument form) returning float8.
fn validate_join_estimator(interp: &PgCatalog, names: &[String]) -> Result<PgProcOid, DdlError> {
    let args5 = [INTERNAL, builtin::OID, INTERNAL, builtin::INT2, INTERNAL];
    let args4 = [INTERNAL, builtin::OID, INTERNAL, builtin::INT2];
    let five = lookup_func_name(interp, names, Some(&args5))?;
    let four = lookup_func_name(interp, names, Some(&args4))?;
    let oid = match (five, four) {
        (Some(_), Some(_)) => {
            return Err(DdlError::Parse(format!(
                "join estimator function {} has multiple matches",
                names.join(".")
            )));
        }
        (Some(o), None) | (None, Some(o)) => o,
        (None, None) => {
            return Err(DdlError::TypeNotFound(format!(
                "function {} does not exist",
                func_signature_string(interp, names, &args5)
            )));
        }
    };
    if interp.pg_proc.get(&oid).map(|p| p.prorettype) != Some(builtin::FLOAT8) {
        return Err(DdlError::Parse(format!(
            "join estimator function {} must return type double precision",
            names.join(".")
        )));
    }
    Ok(oid)
}

/// `OperatorShellMake`: a placeholder for an operator named as a
/// commutator or negator before it exists.
fn operator_shell_make(
    interp: &mut PgCatalog,
    name: &str,
    nsoid: PgNamespaceOid,
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> Result<PgOperatorOid, DdlError> {
    if !valid_operator_name(name) {
        return Err(DdlError::Parse(format!(
            "\"{name}\" is not a valid operator name"
        )));
    }
    let oid = PgOperatorOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_operator(PgOperator {
        oid,
        oprname: name.to_owned(),
        oprnamespace: nsoid,
        oprleft: left,
        oprright: right,
        oprresult: None,
        oprcode: None,
        oprcom: None,
    });
    Ok(oid)
}

/// `get_other_operator`: the commutator / negator an operator names —
/// existing, the operator itself (`None`), or a new shell.
#[allow(clippy::too_many_arguments)]
fn get_other_operator(
    interp: &mut PgCatalog,
    other: &[String],
    other_left: Option<PgTypeOid>,
    other_right: PgTypeOid,
    name: &str,
    nsoid: PgNamespaceOid,
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> Result<Option<PgOperatorOid>, DdlError> {
    if let Some((oid, _)) = operator_lookup(interp, other, other_left, other_right) {
        return Ok(Some(oid));
    }
    let (other_nsoid, other_name) = super::util::ensure_qualified_name(
        interp,
        &other
            .iter()
            .map(|s| typedpg_pg_query::protobuf::Node {
                node: Some(node::Node::String(typedpg_pg_query::protobuf::String {
                    sval: s.clone(),
                })),
            })
            .collect::<Vec<_>>(),
    )?;
    if other_name == name && other_nsoid == nsoid && other_left == left && other_right == right {
        return Ok(None);
    }
    operator_shell_make(interp, &other_name, other_nsoid, other_left, other_right).map(Some)
}

pub fn define_operator(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    let (nsoid, op_name) = super::util::ensure_qualified_name(interp, &stmt.defnames)?;

    let mut left_name = None;
    let mut right_name = None;
    let mut function: Option<Vec<String>> = None;
    let mut commutator: Option<Vec<String>> = None;
    let mut negator: Option<Vec<String>> = None;
    let mut restrict: Option<Vec<String>> = None;
    let mut join: Option<Vec<String>> = None;
    let mut can_hash = false;
    let mut can_merge = false;
    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let type_name = || match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
            Some(node::Node::TypeName(tn)) => Some(tn.clone()),
            _ => None,
        };
        match de.defname.as_str() {
            "leftarg" | "rightarg" => {
                let tn = type_name();
                if tn.as_ref().is_some_and(|t| t.setof) {
                    return Err(DdlError::Parse(
                        "SETOF type not allowed for operator argument".into(),
                    ));
                }
                if de.defname == "leftarg" {
                    left_name = tn;
                } else {
                    right_name = tn;
                }
            }
            "function" | "procedure" => function = Some(def_qualified_name(de)),
            "commutator" => commutator = Some(def_qualified_name(de)),
            "negator" => negator = Some(def_qualified_name(de)),
            "restrict" => restrict = Some(def_qualified_name(de)),
            "join" => join = Some(def_qualified_name(de)),
            "hashes" => can_hash = def_bool(de),
            "merges" => can_merge = def_bool(de),
            // Obsolete options meaning MERGES.
            "sort1" | "sort2" | "ltcmp" | "gtcmp" => can_merge = true,
            // Anything else is only a WARNING.
            _ => {}
        }
    }
    let Some(function) = function else {
        return Err(DdlError::Parse(
            "operator function must be specified".into(),
        ));
    };
    let left = left_name
        .as_ref()
        .map(|tn| typename_type_id(interp, tn))
        .transpose()?;
    let right = right_name
        .as_ref()
        .map(|tn| typename_type_id(interp, tn))
        .transpose()?;
    let Some(right) = right else {
        return Err(DdlError::Parse(if left.is_some() {
            "operator right argument type must be specified (Postfix operators are not \
             supported.)"
                .into()
        } else {
            "operator argument types must be specified".into()
        }));
    };
    let args: Vec<PgTypeOid> = left.into_iter().chain([right]).collect();
    let Some(proc_oid) = lookup_func_name(interp, &function, Some(&args))? else {
        return Err(DdlError::TypeNotFound(format!(
            "function {} does not exist",
            func_signature_string(interp, &function, &args)
        )));
    };
    let result = interp.pg_proc.get(&proc_oid).map(|p| p.prorettype);
    let mut referenced = vec![ObjectAddress::proc(proc_oid)];
    if let Some(names) = restrict.as_deref() {
        referenced.push(ObjectAddress::proc(validate_restriction_estimator(
            interp, names,
        )?));
    }
    if let Some(names) = join.as_deref() {
        referenced.push(ObjectAddress::proc(validate_join_estimator(interp, names)?));
    }

    // ── OperatorCreate ──
    if !valid_operator_name(&op_name) {
        return Err(DdlError::Parse(format!(
            "\"{op_name}\" is not a valid operator name"
        )));
    }
    operator_validate_params(
        left,
        result,
        commutator.is_some(),
        negator.is_some(),
        restrict.is_some(),
        join.is_some(),
        can_merge,
        can_hash,
    )?;
    // OperatorGet: an operator of this name and operand types in the schema.
    let existing = interp
        .operator_by_qname
        .get(&(nsoid, op_name.clone()))
        .into_iter()
        .flatten()
        .filter_map(|oid| interp.pg_operator.get(oid))
        .find(|o| o.oprleft == left && o.oprright == right)
        .map(|o| (o.oid, o.oprcode.is_some()));
    if let Some((_, true)) = existing {
        return Err(DdlError::DuplicateObject(format!(
            "operator {op_name} already exists"
        )));
    }
    // The commutator has the operand types reversed; `None` inside means
    // the operator is its own.
    let mut commutator_oid: Option<Option<PgOperatorOid>> = None;
    if let Some(names) = commutator.as_deref()
        && let Some(l) = left
    {
        let other =
            get_other_operator(interp, names, Some(right), l, &op_name, nsoid, left, right)?;
        if let Some(other) = other {
            referenced.push(ObjectAddress::operator(other));
        }
        commutator_oid = Some(other);
    }
    if let Some(names) = negator.as_deref() {
        match get_other_operator(interp, names, left, right, &op_name, nsoid, left, right)? {
            None => {
                return Err(DdlError::Parse("operator cannot be its own negator".into()));
            }
            Some(other) => referenced.push(ObjectAddress::operator(other)),
        }
    }
    // A shell of this signature is filled in, keeping its OID — and the
    // commutator link another operator made to it.
    let (oid, shell_oprcom) = match existing {
        Some((shell, false)) => {
            let row = interp.remove_pg_operator(shell);
            (shell, row.and_then(|r| r.oprcom))
        }
        _ => (PgOperatorOid::from_nonzero(interp.alloc_oid()?), None),
    };
    let oprcom = match commutator_oid {
        Some(other) => Some(other.unwrap_or(oid)),
        None => shell_oprcom,
    };
    interp.insert_pg_operator(PgOperator {
        oid,
        oprname: op_name,
        oprnamespace: nsoid,
        oprleft: left,
        oprright: right,
        oprresult: result,
        oprcode: Some(proc_oid),
        oprcom,
    });
    // OperatorUpd: the commutator points back at the new operator.
    if let Some(other) = oprcom
        && other != oid
        && let Some(row) = interp.pg_operator.get_mut(&other)
    {
        row.oprcom = Some(oid);
    }
    // makeOperatorDependencies: the operator depends on its function and
    // estimators (a commutator / negator link is not a dependency).
    referenced.retain(|r| r.classid == crate::pg_catalog::PG_PROC_RELID);
    super::depend::record(
        interp,
        ObjectAddress::operator(oid),
        referenced,
        DepType::Normal,
    );
    Ok(())
}

/// `ALTER OPERATOR name (left, right) SET (...)` (AlterOperator): the
/// operator must exist; only RESTRICT, JOIN, COMMUTATOR, NEGATOR, MERGES and
/// HASHES can change, subject to OperatorValidateParams.
pub fn alter_operator(interp: &mut PgCatalog, stmt: &AlterOperatorStmt) -> Result<(), DdlError> {
    let Some(owa) = stmt.opername.as_ref() else {
        return Ok(());
    };
    let names: Vec<String> = owa
        .objname
        .iter()
        .filter_map(node_string)
        .map(str::to_owned)
        .collect();
    let arg = |n: &typedpg_pg_query::protobuf::Node| -> Result<Option<PgTypeOid>, DdlError> {
        match n.node.as_ref() {
            Some(node::Node::TypeName(tn)) if !tn.names.is_empty() => {
                typename_type_id(interp, tn).map(Some)
            }
            _ => Ok(None),
        }
    };
    let (left, right) = match owa.objargs.as_slice() {
        [l, r] => (arg(l)?, arg(r)?),
        [r] => (None, arg(r)?),
        _ => return Ok(()),
    };
    let Some(right) = right else {
        return Ok(());
    };
    let Some((oid, _)) = operator_lookup(interp, &names, left, right) else {
        return Err(DdlError::TypeNotFound(format!(
            "operator does not exist: {}",
            op_signature(interp, &names, left, right)
        )));
    };
    let Some(op) = interp.pg_operator.get(&oid).cloned() else {
        return Ok(());
    };
    let (mut restrict, mut join, mut commutator, mut negator) = (None, None, None, None);
    let (mut can_merge, mut can_hash) = (false, false);
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        // An option without a value (NONE) resets it.
        let value = de.arg.is_some().then(|| def_qualified_name(de));
        match de.defname.as_str() {
            "restrict" => restrict = value,
            "join" => join = value,
            "commutator" => commutator = value,
            "negator" => negator = value,
            "merges" => can_merge = def_bool(de),
            "hashes" => can_hash = def_bool(de),
            "leftarg" | "rightarg" | "function" | "procedure" => {
                return Err(DdlError::Parse(format!(
                    "operator attribute \"{}\" cannot be changed",
                    de.defname
                )));
            }
            other => {
                return Err(DdlError::Parse(format!(
                    "operator attribute \"{other}\" not recognized"
                )));
            }
        }
    }
    if let Some(names) = restrict.as_deref() {
        validate_restriction_estimator(interp, names)?;
    }
    if let Some(names) = join.as_deref() {
        validate_join_estimator(interp, names)?;
    }
    // ValidateOperatorReference: the linked operators must exist, defined.
    let validate_reference =
        |names: &[String], l: Option<PgTypeOid>, r: PgTypeOid| match operator_lookup(
            interp, names, l, r,
        ) {
            None => Err(DdlError::TypeNotFound(format!(
                "operator does not exist: {}",
                op_signature(interp, names, l, r)
            ))),
            Some((_, false)) => Err(DdlError::TypeNotFound(format!(
                "operator is only a shell: {}",
                op_signature(interp, names, l, r)
            ))),
            Some((oid, true)) => Ok(oid),
        };
    if let Some(names) = commutator.as_deref()
        && let Some(l) = left
    {
        validate_reference(names, Some(right), l)?;
    }
    if let Some(names) = negator.as_deref()
        && validate_reference(names, left, right)? == oid
    {
        return Err(DdlError::Parse("operator cannot be its own negator".into()));
    }
    operator_validate_params(
        op.oprleft,
        op.oprresult,
        commutator.is_some(),
        negator.is_some(),
        restrict.is_some(),
        join.is_some(),
        can_merge,
        can_hash,
    )
}
