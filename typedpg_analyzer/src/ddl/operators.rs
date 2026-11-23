//! CREATE OPERATOR / ALTER OPERATOR (`DefineOperator`, `AlterOperator`,
//! operatorcmds.c; `OperatorCreate`, pg_operator.c).

use typedpg_pg_query::ast::{self, AlterOperatorStmt, DefElem, DefineStmt};

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
    match de.arg.as_ref() {
        Some(ast::Node::TypeName(tn)) => tn
            .names
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .map(str::to_owned)
            .collect(),
        Some(ast::Node::List(l)) => l
            .items
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .map(str::to_owned)
            .collect(),
        Some(ast::Node::String(s)) => vec![s.sval.clone()],
        _ => Vec::new(),
    }
}

/// `defGetBoolean`: no value means true.
pub(crate) fn def_bool(de: &DefElem) -> bool {
    match de.arg.as_ref() {
        None => true,
        Some(ast::Node::Boolean(b)) => b.boolval,
        Some(ast::Node::Integer(i)) => i.ival != 0,
        Some(ast::Node::String(s)) => super::reloptions::parse_bool(&s.sval).unwrap_or(false),
        _ => false,
    }
}

/// `op_signature_string`: `left op right` (`NONE` for a missing left).
/// op_signature_string (parse_oper.c): `left name right`, the name as
/// written (NameListToString), a prefix operator without its left side.
pub(crate) fn op_signature(
    interp: &PgCatalog,
    names: &[String],
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> String {
    let name = names.join(".");
    let right = format_type_for_message(interp, right);
    match left {
        Some(left) => format!("{} {name} {right}", format_type_for_message(interp, left)),
        None => format!("{name} {right}"),
    }
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
    let oid = lookup_func_name(interp, names, Some(&args), false)?.ok_or_else(|| {
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
    let five = lookup_func_name(interp, names, Some(&args5), false)?;
    let four = lookup_func_name(interp, names, Some(&args4), false)?;
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
        oprnegate: None,
        oprcanmerge: false,
        oprcanhash: false,
    });
    Ok(oid)
}

/// `UpdateOperatorLinks` (`OperatorUpd`): make the commutator `com` and the negator `neg` of
/// operator `base` link back to it — an error when one already links to a
/// third operator — or, for a `base` being dropped, clear their links to
/// it.
pub(crate) fn operator_upd(
    interp: &mut PgCatalog,
    base: PgOperatorOid,
    com: Option<PgOperatorOid>,
    neg: Option<PgOperatorOid>,
    result: Option<PgTypeOid>,
    is_delete: bool,
) -> Result<(), DdlError> {
    type Link = fn(&mut PgOperator) -> &mut Option<PgOperatorOid>;
    let links: [(Option<PgOperatorOid>, Link, &str); 2] = [
        (com, |o| &mut o.oprcom, "commutator"),
        (neg, |o| &mut o.oprnegate, "negator"),
    ];
    for (other, link, what) in links {
        let Some(other) = other else {
            continue;
        };
        let third = interp
            .pg_operator
            .get_mut(&other)
            .and_then(|o| *link(o))
            .filter(|&t| t != base);
        if let (Some(third), false) = (third, is_delete) {
            let name = |o: PgOperatorOid| interp.pg_operator.get(&o).map(|o| o.oprname.clone());
            let third = name(third).unwrap_or_else(|| third.get().to_string());
            return Err(DdlError::Parse(format!(
                "{what} operator {} is already the {what} of operator {third}",
                name(other).unwrap_or_default()
            )));
        }
        // PostgreSQL 18.7: an operator and its commutator or negator
        // return the same type.
        if !is_delete
            && let Some(result) = result
            && let Some(row) = interp.pg_operator.get(&other)
            && row.oprresult.is_some_and(|r| r != result)
        {
            let base_name = interp
                .pg_operator
                .get(&base)
                .map(|o| o.oprname.clone())
                .unwrap_or_default();
            return Err(DdlError::Parse(format!(
                "{what} operator {} does not return the same data type as operator {base_name}",
                row.oprname
            )));
        }
        if let Some(row) = interp.pg_operator.get_mut(&other) {
            let l = link(row);
            if is_delete && *l == Some(base) {
                *l = None;
            } else if !is_delete {
                *l = Some(base);
            }
        }
    }
    Ok(())
}

/// `RemoveOperatorById`: drop an operator, clearing its commutator's and
/// negator's links to it.
pub(crate) fn remove_operator(interp: &mut PgCatalog, oid: PgOperatorOid) {
    if let Some(op) = interp.pg_operator.get(&oid) {
        let (com, neg) = (op.oprcom, op.oprnegate);
        // (Clearing links never fails.)
        let _ = operator_upd(interp, oid, com, neg, None, true);
    }
    interp.remove_pg_operator(oid);
    // Its operator families' entries go with it (a loose member
    // automatically; a class's, with the class).
    interp.pg_amop.retain(|o| o.amopopr != oid);
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
            .map(|s| {
                Some(typedpg_pg_query::ast::Node::String(
                    typedpg_pg_query::ast::String { sval: s.clone() }.into(),
                ))
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
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        let type_name = || match de.arg.as_ref() {
            Some(ast::Node::TypeName(tn)) => Some(tn.clone()),
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
    let Some(proc_oid) = lookup_func_name(interp, &function, Some(&args), false)? else {
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
    // A shell's links, set by the operators that named it (PostgreSQL 18.7
    // keeps them).
    let shell = match existing {
        Some((shell, false)) => Some(shell),
        _ => None,
    };
    let (old_commutator, old_negator) = shell
        .and_then(|s| interp.pg_operator.get(&s))
        .map_or((None, None), |o| (o.oprcom, o.oprnegate));
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
    let mut negator_oid = None;
    if let Some(names) = negator.as_deref() {
        // Its own negator: the operator itself, or the shell it fills in.
        match get_other_operator(interp, names, left, right, &op_name, nsoid, left, right)? {
            Some(other) if Some(other) != shell => {
                referenced.push(ObjectAddress::operator(other));
                negator_oid = Some(other);
            }
            _ => {
                return Err(DdlError::Parse("operator cannot be its own negator".into()));
            }
        }
    }
    // PostgreSQL 18.7: a shell's links stay, as the operators they point
    // at link back to it — another one named instead is an error.
    let name_of = |interp: &PgCatalog, o: PgOperatorOid| {
        interp
            .pg_operator
            .get(&o)
            .map_or_else(|| o.get().to_string(), |o| o.oprname.clone())
    };
    if let Some(old) = old_commutator {
        match commutator_oid {
            // Self-linkage resolves to nothing yet (the shell's own OID
            // when it names itself).
            None | Some(None) => commutator_oid = Some(Some(old)),
            Some(Some(new)) if new != old => {
                return Err(DdlError::Parse(format!(
                    "commutator operator {} is already the commutator of operator {op_name}",
                    name_of(interp, old)
                )));
            }
            Some(Some(_)) => {}
        }
    }
    if let Some(old) = old_negator {
        match negator_oid {
            None => negator_oid = Some(old),
            Some(new) if new != old => {
                return Err(DdlError::Parse(format!(
                    "negator operator {} is already the negator of operator {op_name}",
                    name_of(interp, old)
                )));
            }
            Some(_) => {}
        }
    }
    // With the links known for sure (PostgreSQL 18.7: a self-commutator of
    // a new operator isn't one yet).
    operator_validate_params(
        left,
        result,
        matches!(commutator_oid, Some(Some(_))),
        negator_oid.is_some(),
        restrict.is_some(),
        join.is_some(),
        can_merge,
        can_hash,
    )?;
    // A shell of this signature is filled in, keeping its OID.
    let oid = match shell {
        Some(shell) => {
            interp.remove_pg_operator(shell);
            shell
        }
        None => PgOperatorOid::from_nonzero(interp.alloc_oid()?),
    };
    let oprcom = commutator_oid.map(|other| other.unwrap_or(oid));
    let oprnegate = negator_oid;
    interp.insert_pg_operator(PgOperator {
        oid,
        oprname: op_name,
        oprnamespace: nsoid,
        oprleft: left,
        oprright: right,
        oprresult: result,
        oprcode: Some(proc_oid),
        oprcom,
        oprnegate,
        oprcanmerge: can_merge,
        oprcanhash: can_hash,
    });
    // UpdateOperatorLinks: the commutator and the negator point back at
    // the new operator.
    operator_upd(interp, oid, oprcom, oprnegate, result, false)?;
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
        .filter_map(|n| node_string(n.as_ref()))
        .map(str::to_owned)
        .collect();
    let arg = |n: Option<&typedpg_pg_query::ast::Node>| -> Result<Option<PgTypeOid>, DdlError> {
        match n {
            Some(ast::Node::TypeName(tn)) if !tn.names.is_empty() => {
                typename_type_id(interp, tn).map(Some)
            }
            _ => Ok(None),
        }
    };
    let (left, right) = match owa.objargs.as_slice() {
        [l, r] => (arg(l.as_ref())?, arg(r.as_ref())?),
        [r] => (None, arg(r.as_ref())?),
        _ => return Ok(()),
    };
    // LookupOperName: postfix operators are gone.
    let Some(right) = right else {
        return Err(DdlError::Parse(
            "postfix operators are not supported".into(),
        ));
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
    let (mut update_merges, mut update_hashes) = (false, false);
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        // An option without a value (NONE) resets it.
        let value = de.arg.is_some().then(|| def_qualified_name(de));
        match de.defname.as_str() {
            "restrict" => restrict = value,
            "join" => join = value,
            "commutator" => commutator = value,
            "negator" => negator = value,
            "merges" => {
                can_merge = def_bool(de);
                update_merges = true;
            }
            "hashes" => {
                can_hash = def_bool(de);
                update_hashes = true;
            }
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
    let mut commutator_oid = None;
    if let Some(names) = commutator.as_deref()
        && let Some(l) = left
    {
        let other = validate_reference(names, Some(right), l)?;
        // AlterOperator: a link already set can't change.
        if op.oprcom.is_some_and(|c| c != other) {
            return Err(DdlError::Parse(
                "operator attribute \"commutator\" cannot be changed if it has already been set"
                    .into(),
            ));
        }
        commutator_oid = Some(other);
    }
    let mut negator_oid = None;
    if let Some(names) = negator.as_deref() {
        let other = validate_reference(names, left, right)?;
        if other == oid {
            return Err(DdlError::Parse("operator cannot be its own negator".into()));
        }
        // AlterOperator: a link already set can't change.
        if op.oprnegate.is_some_and(|n| n != other) {
            return Err(DdlError::Parse(
                "operator attribute \"negator\" cannot be changed if it has already been set"
                    .into(),
            ));
        }
        negator_oid = Some(other);
    }
    // AlterOperator: MERGES / HASHES, once set, can't be turned off (plans
    // may depend on them).
    for (update, was, now, what) in [
        (update_merges, op.oprcanmerge, can_merge, "merges"),
        (update_hashes, op.oprcanhash, can_hash, "hashes"),
    ] {
        if update && was && !now {
            return Err(DdlError::Parse(format!(
                "operator attribute \"{what}\" cannot be changed if it has already been set"
            )));
        }
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
    )?;
    // The new links, both ways (OperatorUpd).
    if let Some(row) = interp.pg_operator.get_mut(&oid) {
        if commutator_oid.is_some() {
            row.oprcom = commutator_oid;
        }
        if negator_oid.is_some() {
            row.oprnegate = negator_oid;
        }
        if update_merges {
            row.oprcanmerge = can_merge;
        }
        if update_hashes {
            row.oprcanhash = can_hash;
        }
    }
    operator_upd(
        interp,
        oid,
        commutator_oid,
        negator_oid,
        op.oprresult,
        false,
    )
}
