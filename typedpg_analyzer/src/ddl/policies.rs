//! CREATE / ALTER / DROP POLICY. Row-level security doesn't change query
//! types, but PG resolves the table, keeps policy names unique per table,
//! checks which expressions the policy's command takes, type-checks the
//! USING / WITH CHECK expressions over the table's row (`CreatePolicy`,
//! policy.c) and records their dependencies on the columns they read.

use typedpg_pg_query::protobuf::{AlterPolicyStmt, CreatePolicyStmt, RangeVar, node};

use super::DdlError;
use crate::oid::PgClassOid;
use crate::pg_catalog::{PgCatalog, RelKind};

/// A row-security policy (`pg_policy`); what its expressions depend on is
/// in [`super::coldeps`].
#[derive(Clone, Debug)]
pub(crate) struct Policy {
    /// `pg_policy.oid`: the identity `pg_depend` rows name.
    pub(crate) oid: crate::oid::PgGenericOid,
    pub(crate) name: String,
    /// `polcmd`: `*` (ALL), `r` (SELECT), `a` (INSERT), `w` (UPDATE) or `d`
    /// (DELETE).
    cmd: char,
}

/// parse_policy_command.
fn policy_command(cmd_name: &str) -> char {
    match cmd_name {
        "select" => 'r',
        "insert" => 'a',
        "update" => 'w',
        "delete" => 'd',
        _ => '*',
    }
}

/// RangeVarCallbackForPolicy: policies live on plain and partitioned
/// tables, and not on system catalogs.
fn policy_table(interp: &PgCatalog, rv: &RangeVar) -> Result<PgClassOid, DdlError> {
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    if interp.is_system_class(relid) {
        return Err(DdlError::Parse(format!(
            "permission denied: \"{}\" is a system catalog",
            rv.relname
        )));
    }
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
    // CreatePolicy checks the expressions against the command first.
    let cmd = policy_command(&stmt.cmd_name);
    if matches!(cmd, 'r' | 'd') && stmt.with_check.is_some() {
        return Err(DdlError::Parse(
            "WITH CHECK cannot be applied to SELECT or DELETE".into(),
        ));
    }
    if cmd == 'a' && stmt.qual.is_some() {
        return Err(DdlError::Parse(
            "only WITH CHECK expression allowed for INSERT".into(),
        ));
    }
    let relid = policy_table(interp, rv)?;
    check_policy_expression(interp, relid, stmt.qual.as_deref())?;
    check_policy_expression(interp, relid, stmt.with_check.as_deref())?;
    if interp
        .policies
        .get(&relid)
        .is_some_and(|ps| ps.iter().any(|p| p.name == stmt.policy_name))
    {
        return Err(DdlError::DuplicateObject(format!(
            "policy \"{}\" for table \"{}\" already exists",
            stmt.policy_name, rv.relname
        )));
    }
    let oid = crate::oid::PgGenericOid::from_nonzero(interp.alloc_oid()?);
    interp.policies.entry(relid).or_default().push(Policy {
        oid,
        name: stmt.policy_name.clone(),
        cmd,
    });
    Ok(())
}

pub fn alter_policy(interp: &mut PgCatalog, stmt: &AlterPolicyStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.table.as_ref() else {
        return Ok(());
    };
    let relid = policy_table(interp, rv)?;
    // AlterPolicy transforms the new expressions, then finds the policy.
    check_policy_expression(interp, relid, stmt.qual.as_deref())?;
    check_policy_expression(interp, relid, stmt.with_check.as_deref())?;
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    let Some(policy) = interp
        .policies
        .get_mut(&relid)
        .and_then(|ps| ps.iter_mut().find(|p| p.name == stmt.policy_name))
    else {
        return Err(DdlError::TypeNotFound(format!(
            "policy \"{}\" for table \"{relname}\" does not exist",
            stmt.policy_name
        )));
    };
    if matches!(policy.cmd, 'r' | 'd') && stmt.with_check.is_some() {
        return Err(DdlError::Parse(
            "only USING expression allowed for SELECT, DELETE".into(),
        ));
    }
    if policy.cmd == 'a' && stmt.qual.is_some() {
        return Err(DdlError::Parse(
            "only WITH CHECK expression allowed for INSERT".into(),
        ));
    }
    Ok(())
}

/// `DROP POLICY [IF EXISTS] name ON table`.
pub(crate) fn drop_policy(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
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
    let dropped = policies.iter().find(|p| p.name == *name).map(|p| p.oid);
    policies.retain(|p| p.name != *name);
    if let Some(oid) = dropped {
        interp.remove_dependencies_of(super::depend::PG_POLICY_RELID, oid);
    }
    if dropped.is_none() && !missing_ok {
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
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
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
    if policies.iter().any(|p| p.name == stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "policy \"{}\" for table \"{}\" already exists",
            stmt.newname, rv.relname
        )));
    }
    let Some(p) = policies.iter_mut().find(|p| p.name == stmt.subname) else {
        return Err(DdlError::TypeNotFound(format!(
            "policy \"{}\" for table \"{}\" does not exist",
            stmt.subname, rv.relname
        )));
    };
    p.name = stmt.newname.clone();
    Ok(())
}

/// A policy expression, analyzed over the table's row, must be boolean.
fn check_policy_expression(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&typedpg_pg_query::protobuf::Node>,
) -> Result<(), DdlError> {
    let Some(expr) = expr else {
        return Ok(());
    };
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
    super::expr_kind::check_expr_kind(interp, expr, super::expr_kind::ExprKind::Policy)?;
    let bool_oid = crate::pg_catalog::oid::BOOL;
    if result.type_oid != bool_oid && result.type_oid != crate::pg_catalog::oid::UNKNOWN {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of POLICY must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(())
}
