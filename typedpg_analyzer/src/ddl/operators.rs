//! CREATE OPERATOR handler.

use typedpg_pg_query::protobuf::{DefineStmt, node};

use crate::oid::{PgOperatorOid, PgTypeOid};
use crate::pg_catalog::PgOperator;

use super::DdlError;
use super::util::resolve_type_name;
use crate::pg_catalog::PgCatalog;

pub fn define_operator(interp: &mut PgCatalog, stmt: &DefineStmt) -> Result<(), DdlError> {
    // Operator name: `defnames` holds either `[name]` or `[schema, name]`.
    let parts: Vec<String> = stmt
        .defnames
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    let (schema, op_name) = match parts.as_slice() {
        [name] => (super::util::creation_schema(interp)?, name.clone()),
        [schema, name] => (schema.clone(), name.clone()),
        _ => return Ok(()),
    };
    let nsoid = super::util::existing_namespace(interp, &schema)?;

    let mut left_type: Option<PgTypeOid> = None;
    let mut right_type: Option<PgTypeOid> = None;
    let mut procedure: Option<(Option<String>, String)> = None;
    let mut commutator: Option<(Option<String>, String)> = None;

    for opt in &stmt.definition {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let Some(arg) = de.arg.as_deref() else {
            continue;
        };
        match de.defname.to_ascii_lowercase().as_str() {
            "leftarg" => {
                if let Some(node::Node::TypeName(tn)) = arg.node.as_ref() {
                    left_type = resolve_type_name(tn, interp);
                }
            }
            "rightarg" => {
                if let Some(node::Node::TypeName(tn)) = arg.node.as_ref() {
                    right_type = resolve_type_name(tn, interp);
                }
            }
            "procedure" | "function" => {
                procedure = parse_func_name(arg);
            }
            "commutator" => {
                commutator = parse_func_name(arg);
            }
            _ => {}
        }
    }

    let Some(right_oid) = right_type else {
        return Ok(());
    };

    // OperatorCreate: the implementing function must exist with the
    // operand types (`function f(integer, integer) does not exist`).
    let Some((schema, fname)) = procedure else {
        return Err(DdlError::Parse(
            "operator function must be specified".into(),
        ));
    };
    let Some((proc_oid, result_oid)) =
        resolve_procedure(interp, schema.as_deref(), &fname, left_type, right_oid)
    else {
        let args = left_type
            .into_iter()
            .chain(std::iter::once(right_oid))
            .map(|t| super::util::format_type_for_message(interp, t))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DdlError::TypeNotFound(format!(
            "function {fname}({args}) does not exist"
        )));
    };

    let oid = PgOperatorOid::from_nonzero(interp.alloc_oid()?);
    interp.insert_pg_operator(PgOperator {
        oid,
        oprname: op_name,
        oprnamespace: nsoid,
        oprleft: left_type,
        oprright: right_oid,
        oprresult: Some(result_oid),
        oprcode: Some(proc_oid),
        oprcom: None,
    });
    if let Some((com_schema, com_name)) = commutator {
        link_commutator(interp, oid, com_schema.as_deref(), &com_name)?;
    }

    Ok(())
}

/// OperatorCreate's COMMUTATOR (get_other_operator / OperatorUpd): the
/// operator with the reversed operand types, whose own `oprcom` is pointed
/// back at the new one — or the new operator itself when it names itself.
/// A commutator not defined yet (PG makes it a shell) links up when it is
/// created naming this one.
fn link_commutator(
    interp: &mut PgCatalog,
    oid: PgOperatorOid,
    schema: Option<&str>,
    name: &str,
) -> Result<(), DdlError> {
    let Some(op) = interp.pg_operator.get(&oid).cloned() else {
        return Ok(());
    };
    let namespaces = match schema {
        Some(s) => vec![super::util::existing_namespace(interp, s)?],
        None => vec![op.oprnamespace],
    };
    let self_link = name == op.oprname
        && namespaces.contains(&op.oprnamespace)
        && op.oprleft == Some(op.oprright);
    let other = if self_link {
        Some(oid)
    } else {
        interp
            .pg_operator
            .values()
            .find(|o| {
                o.oprname == name
                    && namespaces.contains(&o.oprnamespace)
                    && o.oprleft == Some(op.oprright)
                    && op.oprleft == Some(o.oprright)
            })
            .map(|o| o.oid)
    };
    let Some(other) = other else {
        return Ok(());
    };
    if let Some(row) = interp.pg_operator.get_mut(&oid) {
        row.oprcom = Some(other);
    }
    if let Some(row) = interp.pg_operator.get_mut(&other) {
        row.oprcom = Some(oid);
    }
    Ok(())
}

/// Parse a function name from a DefElem argument.
fn parse_func_name(arg: &typedpg_pg_query::protobuf::Node) -> Option<(Option<String>, String)> {
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

/// Look up a procedure in the snapshot and return its result type.
fn resolve_procedure(
    interp: &PgCatalog,
    schema: Option<&str>,
    name: &str,
    left: Option<PgTypeOid>,
    right: PgTypeOid,
) -> Option<(crate::oid::PgProcOid, PgTypeOid)> {
    let candidates = interp.find_functions(schema, name);
    candidates
        .into_iter()
        .find(|f| match (left, f.proargtypes.as_slice()) {
            (Some(l), [a, b]) => *a == l && *b == right,
            (None, [a]) => *a == right,
            _ => false,
        })
        .map(|f| (f.oid, f.prorettype))
}
