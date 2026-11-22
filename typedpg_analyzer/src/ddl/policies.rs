//! CREATE / ALTER / DROP POLICY. Row-level security doesn't change query
//! types, but PG resolves the table, keeps policy names unique per table,
//! checks which expressions the policy's command takes, type-checks the
//! USING / WITH CHECK expressions over the table's row (`CreatePolicy`,
//! policy.c) and records their dependencies on the columns, relations,
//! functions and operators they read. The policy keeps what the privileges
//! analysis needs: its command, whether it is permissive, the roles it is
//! addressed to, and (in [`super::coldeps::ColumnDeps::policy_quals`]) the
//! clauses themselves.
//!
//! PG stores the clauses analyzed: a column is an attnum of a `Var`, a
//! function its OID. The analyzer keeps the parse tree, but records what
//! each name it contains resolved to ([`PolicyRefs`], by source location),
//! and rewrites the names when the object is renamed or moved
//! ([`rewrite_column_refs`], [`rewrite_relation_refs`],
//! [`rewrite_function_refs`], [`rewrite_schema_refs`]) — the same contract
//! [`super::stored_exprs`] keeps for CHECK constraints and defaults, with
//! the extra scoping a policy's subqueries need.

use std::cell::RefCell;
use std::collections::HashMap;

use typedpg_pg_query::ast::{self, AlterPolicyStmt, CreatePolicyStmt, RangeVar, RoleSpecType};

use super::DdlError;
use crate::oid::{PgClassOid, PgNamespaceOid, PgProcOid};
use crate::pg_catalog::{PgCatalog, RelKind};

/// What one name slot of a stored policy expression resolved to when the
/// expression was analyzed (`CREATE` / `ALTER POLICY`): the parser's name
/// lookup, made explicit, keyed by the slot's source location.
#[derive(Clone, Debug, Default)]
pub(crate) struct PolicyRefs {
    /// A `ColumnRef`'s column (`transformColumnRef`): the relation and the
    /// column's attnum.
    pub(crate) columns: HashMap<i32, (PgClassOid, i16)>,
    /// The relation a `ColumnRef`'s qualifier names (`t.c`, `s.t.c`,
    /// `t.*`), and whether the qualifier is the relation's name (which
    /// renames follow) rather than an alias (which doesn't).
    pub(crate) qualifiers: HashMap<i32, QualifierRef>,
    /// A `RangeVar` in a policy subquery.
    pub(crate) relations: HashMap<i32, PgClassOid>,
    /// A direct function call's routine (`FuncCall.funcname`).
    pub(crate) functions: HashMap<i32, PgProcOid>,
    /// A `TypeName` (`LookupTypeName`): a cast's target, a typed literal's
    /// type, a subquery column's declared type.
    pub(crate) types: HashMap<i32, crate::oid::PgTypeOid>,
    /// A `COLLATE` clause's collation.
    pub(crate) collations: HashMap<i32, crate::oid::PgCollationOid>,
    /// An operator expression's operator (`AExpr`, keyed by its location).
    pub(crate) operators: HashMap<i32, crate::oid::PgOperatorOid>,
    /// A column-notation call `(row).f` (keyed by the argument's location
    /// and the step's name): PG reads it as `f(row)`, a `FuncCall` by OID
    /// that no name in the tree can be rewritten to.
    pub(crate) indirections: HashMap<(i32, String), PgProcOid>,
}

/// How a column reference's qualifier was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QualifierRef {
    pub(crate) relid: PgClassOid,
    /// The qualifier is the relation's name (as opposed to a FROM alias):
    /// `ALTER TABLE ... RENAME` / `SET SCHEMA` rewrite it.
    pub(crate) name: bool,
    /// The qualifier carried the relation's schema (`s.t.c`), which a
    /// schema rename / `SET SCHEMA` rewrites.
    pub(crate) schema: bool,
}

/// A policy clause as stored: the parse tree, and what its names meant.
#[derive(Clone, Debug)]
pub(crate) struct PolicyClause {
    pub(crate) expr: Option<typedpg_pg_query::ast::Node>,
    pub(crate) refs: PolicyRefs,
}

thread_local! {
    /// The bindings of the policy expression being analyzed: the hooks
    /// below record into it while it is set.
    static RECORDING: RefCell<Option<PolicyRefs>> = const { RefCell::new(None) };
}

/// Analyze `f` with the policy binding recorder on, and return what it
/// recorded (empty when `f` reports an error the caller ignores).
pub(crate) fn collect_refs<R>(f: impl FnOnce() -> R) -> (R, PolicyRefs) {
    let saved = RECORDING.with(|r| r.borrow_mut().replace(PolicyRefs::default()));
    let result = f();
    let refs = RECORDING
        .with(|r| r.borrow_mut().take())
        .unwrap_or_default();
    RECORDING.with(|r| *r.borrow_mut() = saved);
    (result, refs)
}

/// Whether a policy expression is being analyzed.
pub(crate) fn recording() -> bool {
    RECORDING.with(|r| r.borrow().is_some())
}

fn with_refs(f: impl FnOnce(&mut PolicyRefs)) {
    RECORDING.with(|r| {
        if let Some(refs) = r.borrow_mut().as_mut() {
            f(refs);
        }
    });
}

/// `ColumnRef` at `location` reads column `attnum` of `relid`.
pub(crate) fn note_column_ref(location: i32, relid: PgClassOid, attnum: i16) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.columns.insert(location, (relid, attnum));
    });
}

/// The qualifier of the `ColumnRef` at `location` names `relid`.
pub(crate) fn note_qualifier_ref(location: i32, relid: PgClassOid, name: bool, schema: bool) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.qualifiers.insert(
            location,
            QualifierRef {
                relid,
                name,
                schema,
            },
        );
    });
}

/// The `RangeVar` at `location` names `relid`.
pub(crate) fn note_relation_ref(location: i32, relid: PgClassOid) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.relations.insert(location, relid);
    });
}

/// The function call at `location` resolved to `oid`.
pub(crate) fn note_function_ref(location: i32, oid: PgProcOid) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.functions.insert(location, oid);
    });
}

/// The `TypeName` at `location` resolved to `oid`.
pub(crate) fn note_type_ref(location: i32, oid: crate::oid::PgTypeOid) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.types.insert(location, oid);
    });
}

/// The `COLLATE` clause at `location` resolved to `oid`.
pub(crate) fn note_collation_ref(location: i32, oid: crate::oid::PgCollationOid) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.collations.insert(location, oid);
    });
}

/// The operator expression at `location` resolved to `oid`.
pub(crate) fn note_operator_ref(location: i32, oid: crate::oid::PgOperatorOid) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.operators.insert(location, oid);
    });
}

/// The column-notation call `(row).name` at `location` resolved to the
/// routine `oid`.
pub(crate) fn note_indirection_ref(location: i32, name: &str, oid: PgProcOid) {
    if location < 0 {
        return;
    }
    with_refs(|refs| {
        refs.indirections.insert((location, name.to_owned()), oid);
    });
}

thread_local! {
    /// The column-notation bindings of the stored policy clauses being
    /// re-analyzed: a call whose routine was renamed or moved still resolves
    /// to it by OID, as PG's stored `FuncExpr` does.
    static STORED_INDIRECTIONS: RefCell<Vec<HashMap<(i32, String), PgProcOid>>> =
        const { RefCell::new(Vec::new()) };
}

/// Run `f` with `bindings` resolving the column-notation calls of the stored
/// policy clauses it analyzes.
pub(crate) fn with_indirection_bindings<R>(
    bindings: HashMap<(i32, String), PgProcOid>,
    f: impl FnOnce() -> R,
) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            STORED_INDIRECTIONS.with(|b| {
                b.borrow_mut().pop();
            });
        }
    }
    if !bindings.is_empty() {
        STORED_INDIRECTIONS.with(|b| b.borrow_mut().push(bindings));
        let _restore = Restore;
        return f();
    }
    f()
}

/// The routine the stored column-notation call `(row).name` at `location`
/// resolved to when the policy was created.
pub(crate) fn bound_indirection_function(location: i32, name: &str) -> Option<PgProcOid> {
    STORED_INDIRECTIONS.with(|b| {
        b.borrow()
            .last()
            .and_then(|m| m.get(&(location, name.to_owned())).copied())
            .or_else(|| {
                b.borrow()
                    .iter()
                    .rev()
                    .find_map(|m| m.get(&(location, name.to_owned())).copied())
            })
    })
}

impl PolicyClause {
    /// Store the clause `expr` as analyzed over relation `relid`, with what
    /// its names resolved to.
    pub(crate) fn of(
        interp: &PgCatalog,
        relid: PgClassOid,
        expr: Option<&typedpg_pg_query::ast::Node>,
    ) -> Self {
        let (_, refs) = collect_refs(|| analyze_over_row(interp, relid, expr));
        Self {
            expr: expr.cloned(),
            refs,
        }
    }
}

/// Whom a policy applies to (`pg_policy.polroles`): PUBLIC, or a role
/// (`check_role_for_policy` resolves memberships with `has_privs_of_role`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyRole {
    Public,
    Role(crate::ddl::roles::RoleId),
}

/// A row-security policy (`pg_policy`); what its expressions depend on is
/// in [`super::coldeps`], and the expressions themselves in
/// [`super::coldeps::ColumnDeps::policy_quals`].
#[derive(Clone, Debug)]
pub(crate) struct Policy {
    /// `pg_policy.oid`: the identity `pg_depend` rows name.
    pub(crate) oid: crate::oid::PgGenericOid,
    pub(crate) name: String,
    /// `polcmd`: `*` (ALL), `r` (SELECT), `a` (INSERT), `w` (UPDATE) or `d`
    /// (DELETE).
    pub(crate) cmd: char,
    /// `polpermissive`: a permissive policy's USING clauses are OR-ed, a
    /// restrictive one's AND-ed (`get_policies_for_relation`).
    pub(crate) permissive: bool,
    /// `polroles`: `TO PUBLIC` (or no `TO` at all) is [`PolicyRole::Public`].
    pub(crate) roles: Vec<PolicyRole>,
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

/// The roles a policy applies to (`transformCreatePolicyStmt`): no `TO`
/// clause is PUBLIC; `CURRENT_USER` / `CURRENT_ROLE` / `SESSION_USER` are
/// resolved like any other role spec.
fn policy_roles(
    interp: &mut PgCatalog,
    roles: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<Vec<PolicyRole>, DdlError> {
    if roles.is_empty() {
        return Ok(vec![PolicyRole::Public]);
    }
    let mut out = Vec::new();
    for node in roles {
        let Some(ast::Node::RoleSpec(spec)) = node.as_ref() else {
            continue;
        };
        if spec.roletype == RoleSpecType::RolespecPublic {
            out.push(PolicyRole::Public);
            continue;
        }
        let id = crate::ddl::roles::resolve_role_spec(interp, spec)?;
        out.push(PolicyRole::Role(interp.roles.canonical(id)));
    }
    Ok(out)
}

/// RangeVarCallbackForPolicy: policies live on plain and partitioned
/// tables, and not on system catalogs.
fn policy_table(interp: &PgCatalog, rv: &RangeVar) -> Result<PgClassOid, DdlError> {
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    crate::ddl::util::check_not_system_class(interp, relid)?;
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
    check_policy_expression(interp, relid, stmt.qual.as_ref())?;
    check_policy_expression(interp, relid, stmt.with_check.as_ref())?;
    let roles = policy_roles(interp, &stmt.roles)?;
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
        permissive: stmt.permissive,
        roles,
    });
    Ok(())
}

pub fn alter_policy(interp: &mut PgCatalog, stmt: &AlterPolicyStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.table.as_ref() else {
        return Ok(());
    };
    let relid = policy_table(interp, rv)?;
    // AlterPolicy parses the new roles, then the expressions, then finds
    // the policy.
    let roles = if stmt.roles.is_empty() {
        None
    } else {
        Some(policy_roles(interp, &stmt.roles)?)
    };
    // AlterPolicy transforms the new expressions, then finds the policy.
    check_policy_expression(interp, relid, stmt.qual.as_ref())?;
    check_policy_expression(interp, relid, stmt.with_check.as_ref())?;
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
    if let Some(roles) = roles {
        policy.roles = roles;
    }
    Ok(())
}

/// `DROP POLICY [IF EXISTS] name ON table`.
pub(crate) fn drop_policy(
    interp: &mut PgCatalog,
    obj_node: Option<&typedpg_pg_query::ast::Node>,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(ast::Node::List(list)) = obj_node else {
        return Ok(());
    };
    let parts: Vec<String> = list
        .items
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
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
    // get_object_address_relobject opens the relation with
    // table_openrv_extended: IF EXISTS skips a missing one, not an index.
    super::util::table_open_check(interp, relid, &rv.relname)?;
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
    stmt: &typedpg_pg_query::ast::RenameStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    // rename_policy looks the table up with RangeVarCallbackForPolicy.
    let relid = policy_table(interp, rv)?;
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
    expr: Option<&typedpg_pg_query::ast::Node>,
) -> Result<(), DdlError> {
    if expr.is_none() {
        return Ok(());
    };
    let result = analyze_over_row(interp, relid, expr)?;
    super::expr_kind::check_expr_kind(interp, expr, super::expr_kind::ExprKind::Policy)?;
    if !crate::coerce::coerces_to_boolean(result.type_oid, interp) {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of POLICY must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(())
}

/// Analyze `expr` over the row of `relid` as `CreatePolicy` /
/// `AlterPolicy` does (`transformExpr` with the table's row in scope, the
/// result coerced to boolean in assignment context).
fn analyze_over_row(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&typedpg_pg_query::ast::Node>,
) -> Result<crate::expr::ExprType, DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    use crate::nullability::NullabilityContext;
    use crate::param_collector::ParamCollector;
    use crate::scope::Scope;

    let Some(class) = interp.pg_class.get(&relid) else {
        return Err(DdlError::UnsupportedDdl(
            "policy table disappeared".to_owned(),
        ));
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
    infer_expr(
        expr,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .and_then(|r| {
        r.check_unknown_value_coercion(
            crate::pg_catalog::oid::BOOL,
            crate::coerce::CoercionContext::Assignment,
            interp,
            expr,
        )?;
        Ok(r)
    })
    .map_err(|e| DdlError::UnsupportedDdl(format!("{e}")))
}

/// The stored clause `expr` of a policy on `relid`, with what its names
/// resolved to.
pub(crate) fn stored_clause(
    interp: &PgCatalog,
    relid: PgClassOid,
    expr: Option<&typedpg_pg_query::ast::Node>,
) -> PolicyClause {
    PolicyClause::of(interp, relid, expr)
}

// ─── Renames ───────────────────────────────────────────────────────────────
//
// PG binds a stored policy to OIDs; the analyzer's parse trees name objects.
// Each rewrite below follows the *recorded* resolution, never a name: a
// policy `USING (EXISTS (SELECT 1 FROM u WHERE u.a = t.a))` keeps its own
// `a` and `u`'s apart when one of them is renamed.

/// Apply `edit` to every node of `clause`.
fn edit_clause(
    clause: &mut PolicyClause,
    edit: &mut dyn FnMut(&mut PolicyClause, typedpg_pg_query::NodeMut<'_>),
) {
    let Some(mut expr) = clause.expr.take() else {
        return;
    };
    expr.for_each_node_mut(|n, _| edit(clause, n));
    clause.expr = Some(expr);
}

/// Replace the (`String`) names of `node` with `names` (`None`: keep a
/// name). Only the string's contents are written, never its node.
fn set_name_parts(names: &mut [Option<typedpg_pg_query::ast::Node>], new: &[Option<String>]) {
    for (i, part) in names.iter_mut().enumerate() {
        if let Some(Some(name)) = new.get(i)
            && let Some(ast::Node::String(s)) = part.as_mut()
        {
            s.sval = name.clone();
        }
    }
}

/// `ALTER TABLE ... RENAME COLUMN`: every stored policy expression that
/// read column `attnum` of `relid` now reads its new name.
pub(crate) fn rewrite_column_refs(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    new_name: &str,
) {
    let new_name = new_name.to_owned();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        if !clause_mentions_column(clause, relid, attnum) {
            continue;
        }
        edit_clause(clause, &mut |clause, n| {
            let typedpg_pg_query::NodeMut::ColumnRef(cr) = n else {
                return;
            };
            if clause.refs.columns.get(&cr.location) == Some(&(relid, attnum))
                && let Some(ast::Node::String(s)) = cr.fields.last_mut().and_then(|f| f.as_mut())
            {
                s.sval = new_name.clone();
            }
        });
    }
}

fn clause_mentions_column(clause: &PolicyClause, relid: PgClassOid, attnum: i16) -> bool {
    clause
        .refs
        .columns
        .values()
        .any(|&(r, a)| r == relid && a == attnum)
}

/// `ALTER TABLE ... RENAME TO` / `SET SCHEMA`: the `RangeVar`s and column
/// qualifiers that named `relid` follow it.
pub(crate) fn rewrite_relation_refs(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    new_schema: Option<&str>,
    new_name: &str,
) {
    let new_schema = new_schema.map(str::to_owned);
    let new_name = new_name.to_owned();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        let touches = clause.refs.relations.values().any(|&r| r == relid)
            || clause.refs.qualifiers.values().any(|q| q.relid == relid);
        if !touches {
            continue;
        }
        edit_clause(clause, &mut |clause, n| match n {
            typedpg_pg_query::NodeMut::RangeVar(rv) => {
                if clause.refs.relations.get(&rv.location) == Some(&relid) {
                    if let Some(schema) = &new_schema {
                        rv.schemaname = schema.clone();
                    }
                    rv.relname = new_name.clone();
                }
            }
            typedpg_pg_query::NodeMut::ColumnRef(cr) => {
                if clause.refs.relations.get(&cr.location) == Some(&relid) {
                    // A whole-row reference (`t`): one name.
                    if let Some(ast::Node::String(s)) =
                        cr.fields.first_mut().and_then(|f| f.as_mut())
                    {
                        s.sval = new_name.clone();
                    }
                    return;
                }
                let Some(q) = clause.refs.qualifiers.get(&cr.location) else {
                    return;
                };
                if q.relid != relid || !q.name {
                    return;
                }
                // The names before the column (or the `*`).
                let col = cr.fields.len().saturating_sub(1);
                let mut names: Vec<Option<String>> =
                    cr.fields.iter().take(col).map(|_| None).collect();
                if q.schema
                    && let Some(schema) = &new_schema
                    && !names.is_empty()
                {
                    names[0] = Some(schema.clone());
                }
                if let Some(last) = names.last_mut() {
                    *last = Some(new_name.clone());
                }
                set_name_parts(&mut cr.fields[..col], &names);
            }
            _ => {}
        });
    }
}

/// `ALTER FUNCTION ... RENAME TO` / `SET SCHEMA`: the calls that resolved to
/// `oid` follow it.
pub(crate) fn rewrite_function_refs(
    interp: &mut PgCatalog,
    oid: PgProcOid,
    new_schema: Option<&str>,
    new_name: &str,
) {
    let new_schema = new_schema.map(str::to_owned);
    let new_name = new_name.to_owned();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        if !clause.refs.functions.values().any(|&f| f == oid) {
            continue;
        }
        edit_clause(clause, &mut |clause, n| {
            let typedpg_pg_query::NodeMut::FuncCall(fc) = n else {
                return;
            };
            if clause.refs.functions.get(&fc.location) != Some(&oid) {
                return;
            }
            if let Some(schema) = &new_schema {
                match fc.funcname.len() {
                    1 => fc.funcname.insert(0, string_node(schema)),
                    _ => {
                        if let Some(ast::Node::String(s)) =
                            fc.funcname.first_mut().and_then(|f| f.as_mut())
                        {
                            s.sval = schema.clone();
                        }
                    }
                }
            }
            if let Some(ast::Node::String(s)) = fc.funcname.last_mut().and_then(|f| f.as_mut()) {
                s.sval = new_name.clone();
            }
        });
    }
}

/// `ALTER SCHEMA ... RENAME TO`: every name slot whose object lives in
/// `nsoid` now names the new schema.
pub(crate) fn rewrite_schema_refs(interp: &mut PgCatalog, nsoid: PgNamespaceOid, new_schema: &str) {
    let new_schema = new_schema.to_owned();
    let relids: HashMap<PgClassOid, _> = interp
        .pg_class
        .iter()
        .filter(|(_, c)| c.relnamespace == nsoid)
        .map(|(&oid, _)| (oid, ()))
        .collect();
    let procids: Vec<PgProcOid> = interp
        .pg_proc
        .iter()
        .filter(|(_, p)| p.pronamespace == nsoid)
        .map(|(&oid, _)| oid)
        .collect();
    let typeids: std::collections::HashSet<crate::oid::PgTypeOid> = interp
        .pg_type
        .iter()
        .filter(|(_, t)| t.typnamespace == nsoid)
        .map(|(&oid, _)| oid)
        .collect();
    let collids: std::collections::HashSet<crate::oid::PgCollationOid> = interp
        .pg_collation
        .iter()
        .filter(|(_, c)| c.collnamespace == nsoid)
        .map(|(&oid, _)| oid)
        .collect();
    let opids: std::collections::HashSet<crate::oid::PgOperatorOid> = interp
        .pg_operator
        .iter()
        .filter(|(_, o)| o.oprnamespace == nsoid)
        .map(|(&oid, _)| oid)
        .collect();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        edit_clause(clause, &mut |clause, n| match n {
            typedpg_pg_query::NodeMut::RangeVar(rv) => {
                if clause
                    .refs
                    .relations
                    .get(&rv.location)
                    .is_some_and(|relid| relids.contains_key(relid))
                {
                    rv.schemaname = new_schema.clone();
                }
            }
            typedpg_pg_query::NodeMut::ColumnRef(cr) => {
                if let Some(q) = clause.refs.qualifiers.get(&cr.location)
                    && q.name
                    && q.schema
                    && relids.contains_key(&q.relid)
                    && let Some(ast::Node::String(s)) =
                        cr.fields.first_mut().and_then(|f| f.as_mut())
                {
                    s.sval = new_schema.clone();
                }
            }
            typedpg_pg_query::NodeMut::FuncCall(fc) => {
                if clause
                    .refs
                    .functions
                    .get(&fc.location)
                    .is_some_and(|oid| procids.contains(oid))
                {
                    match fc.funcname.len() {
                        1 => fc.funcname.insert(0, string_node(&new_schema)),
                        _ => {
                            if let Some(ast::Node::String(s)) =
                                fc.funcname.first_mut().and_then(|f| f.as_mut())
                            {
                                s.sval = new_schema.clone();
                            }
                        }
                    }
                }
            }
            typedpg_pg_query::NodeMut::TypeName(tn) => {
                if !clause
                    .refs
                    .types
                    .get(&tn.location)
                    .is_some_and(|oid| typeids.contains(oid))
                {
                    return;
                }
                match tn.names.len() {
                    0 => tn.names.push(string_node(&new_schema)),
                    1 => tn.names.insert(0, string_node(&new_schema)),
                    _ => {
                        if let Some(ast::Node::String(s)) =
                            tn.names.first_mut().and_then(|f| f.as_mut())
                        {
                            s.sval = new_schema.clone();
                        }
                    }
                }
            }
            typedpg_pg_query::NodeMut::CollateClause(cc) => {
                if !clause
                    .refs
                    .collations
                    .get(&cc.location)
                    .is_some_and(|oid| collids.contains(oid))
                {
                    return;
                }
                match cc.collname.len() {
                    0 => cc.collname.push(string_node(&new_schema)),
                    1 => cc.collname.insert(0, string_node(&new_schema)),
                    _ => {
                        if let Some(ast::Node::String(s)) =
                            cc.collname.first_mut().and_then(|f| f.as_mut())
                        {
                            s.sval = new_schema.clone();
                        }
                    }
                }
            }
            typedpg_pg_query::NodeMut::AExpr(ae) => {
                if !clause
                    .refs
                    .operators
                    .get(&ae.location)
                    .is_some_and(|oid| opids.contains(oid))
                {
                    return;
                }
                match ae.name.len() {
                    0 => {}
                    1 => ae.name.insert(0, string_node(&new_schema)),
                    _ => {
                        if let Some(ast::Node::String(s)) =
                            ae.name.first_mut().and_then(|f| f.as_mut())
                        {
                            s.sval = new_schema.clone();
                        }
                    }
                }
            }
            _ => {}
        });
    }
}

/// The `String` node `sval`.
fn string_node(sval: &str) -> Option<ast::Node> {
    Some(ast::Node::String(typedpg_pg_query::NodeBox::new(
        typedpg_pg_query::ast::String {
            sval: sval.to_owned(),
        },
    )))
}

/// `ALTER TYPE` / `ALTER DOMAIN ... RENAME TO` / `SET SCHEMA`: the stored
/// policy expressions that named `oid` now name it by its new name (and
/// schema). PG keeps the type by OID, so the clause goes on meaning what it
/// meant.
pub(crate) fn rewrite_type_refs(
    interp: &mut PgCatalog,
    oid: crate::oid::PgTypeOid,
    new_schema: Option<&str>,
    new_name: &str,
) {
    let new_schema = new_schema.map(str::to_owned);
    let new_name = new_name.to_owned();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        if !clause.refs.types.values().any(|&t| t == oid) {
            continue;
        }
        edit_clause(clause, &mut |clause, n| {
            let typedpg_pg_query::NodeMut::TypeName(tn) = n else {
                return;
            };
            if clause.refs.types.get(&tn.location) != Some(&oid) {
                return;
            }
            if let Some(schema) = &new_schema {
                match tn.names.len() {
                    0 => tn.names.push(string_node(schema)),
                    1 => tn.names.insert(0, string_node(schema)),
                    _ => {
                        if let Some(ast::Node::String(s)) =
                            tn.names.first_mut().and_then(|f| f.as_mut())
                        {
                            s.sval = schema.clone();
                        }
                    }
                }
            }
            if let Some(ast::Node::String(s)) = tn.names.last_mut().and_then(|f| f.as_mut()) {
                s.sval = new_name.clone();
            }
        });
    }
}

/// `ALTER COLLATION ... RENAME TO` / `SET SCHEMA`: the `COLLATE` clauses
/// that named `oid` follow it.
pub(crate) fn rewrite_collation_refs(
    interp: &mut PgCatalog,
    oid: crate::oid::PgCollationOid,
    new_schema: Option<&str>,
    new_name: &str,
) {
    let new_schema = new_schema.map(str::to_owned);
    let new_name = new_name.to_owned();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        if !clause.refs.collations.values().any(|&c| c == oid) {
            continue;
        }
        edit_clause(clause, &mut |clause, n| {
            let typedpg_pg_query::NodeMut::CollateClause(cc) = n else {
                return;
            };
            if clause.refs.collations.get(&cc.location) != Some(&oid) {
                return;
            }
            if let Some(schema) = &new_schema {
                match cc.collname.len() {
                    0 => cc.collname.push(string_node(schema)),
                    1 => cc.collname.insert(0, string_node(schema)),
                    _ => {
                        if let Some(ast::Node::String(s)) =
                            cc.collname.first_mut().and_then(|f| f.as_mut())
                        {
                            s.sval = schema.clone();
                        }
                    }
                }
            }
            if let Some(ast::Node::String(s)) = cc.collname.last_mut().and_then(|f| f.as_mut()) {
                s.sval = new_name.clone();
            }
        });
    }
}

/// `ALTER OPERATOR ... SET SCHEMA`: the operator expressions that called
/// `oid` now name it in its new schema (operators have no `RENAME`).
pub(crate) fn rewrite_operator_refs(
    interp: &mut PgCatalog,
    oid: crate::oid::PgOperatorOid,
    new_schema: &str,
) {
    let new_schema = new_schema.to_owned();
    for clause in interp
        .column_deps
        .policy_quals
        .values_mut()
        .flat_map(|(using, with_check)| using.iter_mut().chain(with_check.iter_mut()))
    {
        if !clause.refs.operators.values().any(|&o| o == oid) {
            continue;
        }
        edit_clause(clause, &mut |clause, n| {
            let typedpg_pg_query::NodeMut::AExpr(ae) = n else {
                return;
            };
            if clause.refs.operators.get(&ae.location) != Some(&oid) {
                return;
            }
            match ae.name.len() {
                0 => {}
                1 => ae.name.insert(0, string_node(&new_schema)),
                _ => {
                    if let Some(ast::Node::String(s)) = ae.name.first_mut().and_then(|f| f.as_mut())
                    {
                        s.sval = new_schema.clone();
                    }
                }
            }
        });
    }
}
