//! CREATE / ALTER / DROP POLICY. Row-level security doesn't change query
//! types, but PG resolves the table, keeps policy names unique per table
//! and type-checks the USING / WITH CHECK expressions over the table's row
//! (`CreatePolicy`, policy.c), so the catalog keeps the names.

use pg_query::protobuf::{AlterPolicyStmt, CreatePolicyStmt, RangeVar, node};

use super::DdlError;
use crate::oid::PgClassOid;
use crate::pg_catalog::{PgCatalog, RelKind};

/// RangeVarCallbackForPolicy: policies live on plain and partitioned tables.
fn policy_table(interp: &PgCatalog, rv: &RangeVar) -> Result<PgClassOid, DdlError> {
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let relkind = interp.pg_class.get(&relid).map(|c| c.relkind);
    if !matches!(relkind, Some(RelKind::Table | RelKind::Partitioned)) {
        return Err(DdlError::UnsupportedDdl(format!(
            "\"{}\" is not a table",
            rv.relname
        )));
    }
    Ok(relid)
}

pub fn create_policy(interp: &mut PgCatalog, stmt: &CreatePolicyStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.table.as_ref() else {
        return Ok(());
    };
    let relid = policy_table(interp, rv)?;
    if interp
        .policies
        .get(&relid)
        .is_some_and(|ps| ps.contains(&stmt.policy_name))
    {
        return Err(DdlError::DuplicateObject(format!(
            "policy \"{}\" for table \"{}\" already exists",
            stmt.policy_name, rv.relname
        )));
    }
    for expr in [stmt.qual.as_deref(), stmt.with_check.as_deref()]
        .into_iter()
        .flatten()
    {
        check_policy_expression(interp, relid, expr)?;
    }
    interp
        .policies
        .entry(relid)
        .or_default()
        .push(stmt.policy_name.clone());
    Ok(())
}

pub fn alter_policy(interp: &PgCatalog, stmt: &AlterPolicyStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.table.as_ref() else {
        return Ok(());
    };
    let relid = policy_table(interp, rv)?;
    if !interp
        .policies
        .get(&relid)
        .is_some_and(|ps| ps.contains(&stmt.policy_name))
    {
        return Err(DdlError::TypeNotFound(format!(
            "policy \"{}\" for table \"{}\" does not exist",
            stmt.policy_name, rv.relname
        )));
    }
    for expr in [stmt.qual.as_deref(), stmt.with_check.as_deref()]
        .into_iter()
        .flatten()
    {
        check_policy_expression(interp, relid, expr)?;
    }
    Ok(())
}

/// `DROP POLICY [IF EXISTS] name ON table`.
pub(crate) fn drop_policy(
    interp: &mut PgCatalog,
    obj_node: &pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(node::Node::List(list)) = obj_node.node.as_ref() else {
        return Ok(());
    };
    let parts: Vec<String> = list
        .items
        .iter()
        .filter_map(super::util::node_string)
        .map(str::to_owned)
        .collect();
    let Some((name, rel)) = parts.split_last() else {
        return Ok(());
    };
    let rv = RangeVar {
        schemaname: if rel.len() == 2 {
            rel[0].clone()
        } else {
            String::new()
        },
        relname: rel.last().cloned().unwrap_or_default(),
        inh: true,
        ..Default::default()
    };
    let relid = match super::util::lookup_relation(interp, &rv) {
        Ok((_, oid)) => oid,
        // does_not_exist_skipping: `relation "x" does not exist, skipping`.
        Err(_) if missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    let policies = interp.policies.entry(relid).or_default();
    let before = policies.len();
    policies.retain(|p| p != name);
    if policies.len() == before && !missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "policy \"{name}\" for table \"{}\" does not exist",
            rv.relname
        )));
    }
    Ok(())
}

/// `ALTER POLICY name ON table RENAME TO new` (rename_policy).
pub(crate) fn rename_policy(
    interp: &mut PgCatalog,
    stmt: &pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let relid = match super::util::lookup_relation(interp, rv) {
        Ok((_, oid)) => oid,
        Err(_) if stmt.missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    let policies = interp.policies.entry(relid).or_default();
    if policies.contains(&stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "policy \"{}\" for table \"{}\" already exists",
            stmt.newname, rv.relname
        )));
    }
    let Some(p) = policies.iter_mut().find(|p| **p == stmt.subname) else {
        return Err(DdlError::TypeNotFound(format!(
            "policy \"{}\" for table \"{}\" does not exist",
            stmt.subname, rv.relname
        )));
    };
    *p = stmt.newname.clone();
    Ok(())
}

/// A policy expression, analyzed over the table's row, must be boolean.
fn check_policy_expression(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: &pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::Scope;

    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
        &attrs,
    );
    let null_ctx = NullabilityContext::default();
    let mut params = ParamCollector::default();
    let result = infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(|e| DdlError::UnsupportedDdl(format!("{e}")))?;
    // EXPR_KIND_POLICY forbids aggregates and window functions.
    crate::clause::check_no_aggregates_or_windows(expr, interp, "policy expressions")
        .map_err(|e| DdlError::UnsupportedDdl(format!("{e}")))?;
    let bool_oid = crate::pg_catalog::oid::BOOL;
    if result.type_oid != bool_oid && result.type_oid != crate::pg_catalog::oid::UNKNOWN {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of POLICY must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(())
}
