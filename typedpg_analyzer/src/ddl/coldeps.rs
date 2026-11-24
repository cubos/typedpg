//! What policies, triggers and rules refer to: a policy's USING / WITH
//! CHECK (CreatePolicy → recordDependencyOnExpr), a trigger's WHEN
//! condition and UPDATE OF columns (CreateTrigger), a rule's qualification
//! and actions (InsertRule). The references are extracted here and stored
//! as `pg_depend` rows by [`super::depend`] — the one dependency store DROP
//! consults: DEPENDENCY_NORMAL on each column (or relation) and function
//! read, and DEPENDENCY_AUTO on the object's own relation, which it goes
//! with. ALTER COLUMN TYPE of a column such an object — or a SQL-standard
//! function body — reads is refused (RememberAllDependentForRebuilding).

use std::collections::HashMap;

use typedpg_pg_query::NodeBox;
use typedpg_pg_query::ast::{self, Node};

use super::DdlError;
use super::depend::{NamedObject, ObjectAddress};
use crate::oid::{PgClassOid, PgProcOid};
use crate::pg_catalog::{DepType, PG_CLASS_RELID, PG_PROC_RELID, PgCatalog};

/// An object depending on columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Dependent {
    Policy { relid: PgClassOid, name: String },
    Trigger { relid: PgClassOid, name: String },
    Rule { relid: PgClassOid, name: String },
    Function(PgProcOid),
}

/// State ALTER POLICY needs beyond `pg_depend`.
#[derive(Clone, Debug, Default)]
pub(crate) struct ColumnDeps {
    /// Each policy's USING and WITH CHECK expressions: ALTER POLICY
    /// replaces them one at a time.
    pub(crate) policy_quals: HashMap<
        (PgClassOid, String),
        (
            Option<super::policies::PolicyClause>,
            Option<super::policies::PolicyClause>,
        ),
    >,
}

impl Dependent {
    fn named(&self) -> Option<NamedObject> {
        Some(match self {
            Dependent::Policy { relid, name } => NamedObject::Policy {
                relid: *relid,
                name: name.clone(),
            },
            Dependent::Trigger { relid, name } => NamedObject::Trigger {
                relid: *relid,
                name: name.clone(),
            },
            Dependent::Rule { relid, name } => NamedObject::Rule {
                relid: *relid,
                name: name.clone(),
            },
            Dependent::Function(_) => return None,
        })
    }

    fn of(interp: &PgCatalog, addr: ObjectAddress) -> Option<Self> {
        if addr.classid == PG_PROC_RELID {
            return PgProcOid::new(addr.objid.get()).map(Dependent::Function);
        }
        match super::depend::named_object_at(interp, addr)? {
            NamedObject::Policy { relid, name } => Some(Dependent::Policy { relid, name }),
            NamedObject::Trigger { relid, name } => Some(Dependent::Trigger { relid, name }),
            NamedObject::Rule { relid, name } => Some(Dependent::Rule { relid, name }),
            _ => None,
        }
    }
}

/// Record `dep`'s references (replacing earlier ones): normal
/// dependencies on the columns and relations in `refs` and the functions
/// in `functions`; its own relation it depends on automatically.
fn store(
    interp: &mut PgCatalog,
    dep: &Dependent,
    refs: Vec<(PgClassOid, i16)>,
    functions: Vec<PgProcOid>,
    analyzed: Vec<Vec<ObjectAddress>>,
) -> Result<(), DdlError> {
    let Some(named) = dep.named() else {
        return Ok(());
    };
    let owner = match dep {
        Dependent::Policy { relid, .. }
        | Dependent::Trigger { relid, .. }
        | Dependent::Rule { relid, .. } => *relid,
        Dependent::Function(_) => return Ok(()),
    };
    let addr = super::depend::named_object(interp, named)?;
    let policy = matches!(dep, Dependent::Policy { .. });
    let mut referenced: Vec<ObjectAddress> = refs
        .into_iter()
        .filter(|&(r, a)| !(r == owner && a == 0))
        .map(|(r, a)| ObjectAddress::column(r, a))
        .chain(functions.into_iter().map(ObjectAddress::proc))
        .collect();
    // A trigger's WHEN and a rule's actions are recorded at once with the
    // rest (one recordDependencyOnExpr): their own relation is their owner.
    if !policy {
        referenced.extend(
            analyzed
                .iter()
                .flatten()
                .copied()
                .filter(|a| *a != ObjectAddress::relation(owner)),
        );
    }
    super::depend::record(interp, addr, referenced, DepType::Normal);
    // A policy's clauses are recorded apart (recordDependencyOnExpr per
    // clause: a relation one reads whole stays, whatever another reads of
    // it); its own relation is among them only when a clause reads it whole
    // in a sub-select (its range table).
    if policy {
        for group in analyzed {
            super::depend::record(interp, addr, group, DepType::Normal);
        }
    }
    super::depend::record(
        interp,
        addr,
        [ObjectAddress::relation(owner)],
        DepType::Auto,
    );
    Ok(())
}

/// The existing policies, triggers, rules and SQL-standard function bodies
/// depending on column `relid.attnum`, in creation order.
pub(crate) fn dependents_on_column(
    interp: &PgCatalog,
    relid: PgClassOid,
    attnum: i16,
) -> Vec<Dependent> {
    let mut out: Vec<Dependent> = Vec::new();
    for d in interp.iter_pg_depend() {
        if d.refclassid != PG_CLASS_RELID
            || d.refobjid.get() != relid.get()
            || d.refobjsubid != attnum
            || d.deptype != DepType::Normal
        {
            continue;
        }
        let addr = ObjectAddress {
            classid: d.classid,
            objid: d.objid,
            objsubid: d.objsubid,
        };
        if !super::depend::object_exists(interp, addr) {
            continue;
        }
        if let Some(dep) = Dependent::of(interp, addr)
            && !out.contains(&dep)
        {
            out.push(dep);
        }
    }
    out
}

/// getObjectDescription.
pub(crate) fn describe(interp: &PgCatalog, dep: &Dependent) -> String {
    let addr = match dep {
        Dependent::Function(oid) => ObjectAddress::proc(*oid),
        other => match other
            .named()
            .and_then(|n| super::depend::named_address_of(interp, &n))
        {
            Some(addr) => addr,
            None => return String::new(),
        },
    };
    super::depend::describe(interp, addr)
}

/// ALTER POLICY / TRIGGER / RULE ... RENAME TO: the object keeps its OID —
/// its `pg_depend` rows — and a policy its analyzed quals.
pub(crate) fn rename(interp: &mut PgCatalog, old: &Dependent, new_name: &str) {
    if let Dependent::Policy { relid, name } = old
        && let Some(quals) = interp
            .column_deps
            .policy_quals
            .remove(&(*relid, name.clone()))
    {
        interp
            .column_deps
            .policy_quals
            .insert((*relid, new_name.to_owned()), quals);
    }
}

// ─── What an expression or statement reads ──────────────────────────────

fn range_var_node(rv: &ast::RangeVar, alias: Option<&str>) -> Node {
    let mut rv = rv.clone();
    if let Some(alias) = alias {
        rv.alias = Some(NodeBox::new(ast::Alias {
            aliasname: alias.to_owned(),
            ..Default::default()
        }));
    }
    ast::Node::RangeVar(NodeBox::new(rv))
}

fn target(expr: &Node) -> Node {
    ast::Node::ResTarget(NodeBox::new(ast::ResTarget {
        val: Some(expr.clone()),
        ..Default::default()
    }))
}

/// `SELECT exprs FROM from`: what the expressions read, resolved by the
/// view machinery's binding walker.
fn select_of(exprs: &[&Node], from: Vec<Option<Node>>) -> Node {
    ast::Node::SelectStmt(NodeBox::new(ast::SelectStmt {
        target_list: exprs.iter().map(|e| Some(target(e))).collect(),
        from_clause: from,
        limit_option: ast::LimitOption::Count,
        op: ast::SetOperation::SetopNone,
        ..Default::default()
    }))
}

/// The expressions of a RETURNING list.
fn returning_vals(clause: Option<&ast::ReturningClause>) -> Vec<&Node> {
    clause
        .map_or(&[][..], |c| c.exprs.as_slice())
        .iter()
        .filter_map(|t| match t.as_ref()? {
            ast::Node::ResTarget(rt) => rt.val.as_ref(),
            _ => None,
        })
        .collect()
}

fn relation_of(interp: &PgCatalog, rv: &ast::RangeVar) -> Option<PgClassOid> {
    super::util::lookup_relation(interp, rv)
        .ok()
        .map(|(_, oid)| oid)
}

fn names_of(
    interp: &PgCatalog,
    relid: PgClassOid,
    names: &[Option<Node>],
) -> Vec<(PgClassOid, i16)> {
    names
        .iter()
        .filter_map(|n| match n.as_ref()? {
            ast::Node::String(s) => Some(s.sval.as_str()),
            ast::Node::ResTarget(rt) => Some(rt.name.as_str()),
            _ => None,
        })
        .filter_map(|name| interp.attribute_by_name(relid, name))
        .map(|a| (relid, a.attnum))
        .collect()
}

/// The relations (attnum 0) and columns a statement reads or writes —
/// SELECT, INSERT, UPDATE, DELETE — with `extra_from` in scope (a rule's
/// NEW and OLD).
fn statement_refs(
    interp: &PgCatalog,
    stmt: &Node,
    extra_from: &[Option<Node>],
) -> Vec<(PgClassOid, i16)> {
    let mut refs: Vec<(PgClassOid, i16)> = Vec::new();
    let walk = |select: &Node, refs: &mut Vec<(PgClassOid, i16)>| {
        let (relations, columns) = super::views::statement_references(interp, select);
        refs.extend(relations.into_iter().map(|r| (r, 0)));
        refs.extend(columns);
    };
    match stmt {
        ast::Node::SelectStmt(sel) if !extra_from.is_empty() && sel.values_lists.is_empty() => {
            // NEW and OLD are in scope, but no FROM item of the statement:
            // an unqualified `*` reads the statement's own FROM items only.
            let is_star = |t: &Option<Node>| {
                matches!(t, Some(ast::Node::ResTarget(rt))
                    if matches!(rt.val.as_ref(), Some(ast::Node::ColumnRef(cr))
                        if cr.fields.len() == 1
                            && matches!(cr.fields[0].as_ref(), Some(ast::Node::AStar(_)))))
            };
            if sel.target_list.iter().any(is_star) {
                let mut star = sel.clone();
                star.target_list.retain(is_star);
                star.where_clause = None;
                star.having_clause = None;
                walk(&ast::Node::SelectStmt(star), &mut refs);
            }
            let mut sel = sel.clone();
            sel.target_list.retain(|t| !is_star(t));
            sel.from_clause.extend(extra_from.iter().cloned());
            walk(&ast::Node::SelectStmt(sel), &mut refs);
        }
        ast::Node::SelectStmt(sel) if !sel.values_lists.is_empty() => {
            let exprs: Vec<&Node> = sel
                .values_lists
                .iter()
                .flat_map(|row| match row.as_ref() {
                    // A NULL cell adds a target without a value: it reads nothing.
                    Some(ast::Node::List(l)) => l.items.iter().flatten().collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect();
            walk(&select_of(&exprs, extra_from.to_vec()), &mut refs);
        }
        ast::Node::SelectStmt(_) => walk(stmt, &mut refs),
        ast::Node::InsertStmt(ins) => {
            if let Some(rv) = ins.relation.as_ref()
                && let Some(relid) = relation_of(interp, rv)
            {
                refs.push((relid, 0));
                // Without a column list the values go to the leading
                // columns, as many as the source has after `*` expansion
                // (transformInsertStmt's implicit target list): the
                // analysis of the statement tells those (record_rule).
                refs.extend(names_of(interp, relid, &ins.cols));
            }
            if let Some(source) = ins.select_stmt.as_ref() {
                refs.extend(statement_refs(interp, source, extra_from));
            }
            let returning: Vec<&Node> = returning_vals(ins.returning_clause.as_deref());
            if let Some(rv) = ins.relation.as_ref()
                && !returning.is_empty()
            {
                let mut from = vec![Some(range_var_node(rv, None))];
                from.extend(extra_from.iter().cloned());
                walk(&select_of(&returning, from), &mut refs);
            }
        }
        ast::Node::UpdateStmt(upd) => {
            if let Some(rv) = upd.relation.as_ref()
                && let Some(relid) = relation_of(interp, rv)
            {
                refs.push((relid, 0));
                refs.extend(names_of(interp, relid, &upd.target_list));
                let mut exprs: Vec<&Node> = upd
                    .target_list
                    .iter()
                    .filter_map(|t| match t.as_ref()? {
                        ast::Node::ResTarget(rt) => rt.val.as_ref(),
                        _ => None,
                    })
                    .collect();
                exprs.extend(upd.where_clause.as_ref());
                exprs.extend(returning_vals(upd.returning_clause.as_deref()));
                let mut from = vec![Some(range_var_node(rv, None))];
                from.extend(upd.from_clause.iter().cloned());
                from.extend(extra_from.iter().cloned());
                walk(&select_of(&exprs, from), &mut refs);
            }
        }
        ast::Node::DeleteStmt(del) => {
            if let Some(rv) = del.relation.as_ref()
                && let Some(relid) = relation_of(interp, rv)
            {
                refs.push((relid, 0));
                let mut exprs: Vec<&Node> = del.where_clause.as_ref().into_iter().collect();
                exprs.extend(returning_vals(del.returning_clause.as_deref()));
                let mut from = vec![Some(range_var_node(rv, None))];
                from.extend(del.using_clause.iter().cloned());
                from.extend(extra_from.iter().cloned());
                walk(&select_of(&exprs, from), &mut refs);
            }
        }
        _ => {}
    }
    refs
}

/// The functions a statement calls — SELECT, INSERT, UPDATE, DELETE —
/// with `extra_from` in scope, as [`statement_refs`] walks it.
fn statement_functions(
    interp: &PgCatalog,
    stmt: &Node,
    extra_from: &[Option<Node>],
) -> Vec<PgProcOid> {
    let walk = |select: &Node| super::views::statement_functions(interp, select);
    let with_target = |rv: &ast::RangeVar, more: &[Option<Node>]| {
        let mut from = vec![Some(range_var_node(rv, None))];
        from.extend(more.iter().cloned());
        from.extend(extra_from.iter().cloned());
        from
    };
    match stmt {
        ast::Node::SelectStmt(sel) if !sel.values_lists.is_empty() => {
            let exprs: Vec<&Node> = sel
                .values_lists
                .iter()
                .flat_map(|row| match row.as_ref() {
                    // A NULL cell adds a target without a value: it reads nothing.
                    Some(ast::Node::List(l)) => l.items.iter().flatten().collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect();
            walk(&select_of(&exprs, extra_from.to_vec()))
        }
        ast::Node::SelectStmt(sel) => {
            let mut sel = sel.clone();
            sel.from_clause.extend(extra_from.iter().cloned());
            walk(&ast::Node::SelectStmt(sel))
        }
        ast::Node::InsertStmt(ins) => {
            let mut out = ins
                .select_stmt
                .as_ref()
                .map(|s| statement_functions(interp, s, extra_from))
                .unwrap_or_default();
            let returning = returning_vals(ins.returning_clause.as_deref());
            if let Some(rv) = ins.relation.as_ref()
                && !returning.is_empty()
            {
                out.extend(walk(&select_of(&returning, with_target(rv, &[]))));
            }
            out
        }
        ast::Node::UpdateStmt(upd) => {
            let Some(rv) = upd.relation.as_ref() else {
                return Vec::new();
            };
            let mut exprs: Vec<&Node> = upd
                .target_list
                .iter()
                .filter_map(|t| match t.as_ref()? {
                    ast::Node::ResTarget(rt) => rt.val.as_ref(),
                    _ => None,
                })
                .collect();
            exprs.extend(upd.where_clause.as_ref());
            exprs.extend(returning_vals(upd.returning_clause.as_deref()));
            walk(&select_of(&exprs, with_target(rv, &upd.from_clause)))
        }
        ast::Node::DeleteStmt(del) => {
            let Some(rv) = del.relation.as_ref() else {
                return Vec::new();
            };
            let mut exprs: Vec<&Node> = del.where_clause.as_ref().into_iter().collect();
            exprs.extend(returning_vals(del.returning_clause.as_deref()));
            walk(&select_of(&exprs, with_target(rv, &del.using_clause)))
        }
        _ => Vec::new(),
    }
}

// ─── Recording ──────────────────────────────────────────────────────────

/// The functions a policy's USING / WITH CHECK call.
fn policy_functions(
    interp: &PgCatalog,
    table: &ast::RangeVar,
    quals: &(Option<Node>, Option<Node>),
) -> Vec<PgProcOid> {
    let exprs: Vec<&Node> = quals.0.iter().chain(quals.1.iter()).collect();
    if exprs.is_empty() {
        return Vec::new();
    }
    super::views::statement_functions(
        interp,
        &select_of(&exprs, vec![Some(range_var_node(table, None))]),
    )
}

fn policy_refs(
    interp: &PgCatalog,
    table: &ast::RangeVar,
    quals: &(Option<Node>, Option<Node>),
) -> Vec<(PgClassOid, i16)> {
    let exprs: Vec<&Node> = quals.0.iter().chain(quals.1.iter()).collect();
    if exprs.is_empty() {
        return Vec::new();
    }
    statement_refs(
        interp,
        &select_of(&exprs, vec![Some(range_var_node(table, None))]),
        &[],
    )
}

/// The operators and cast functions a policy's expressions call:
/// `recordDependencyOnExpr` records an operator (not its function) and the
/// `FuncExpr` of a cast (`pg_cast.castfunc`), so `pg_depend` keeps the ones
/// the policy was written with — DROP OPERATOR / DROP FUNCTION is blocked by
/// them, and the policy's privilege check finds the routines they name.
fn policy_expression_refs(
    interp: &PgCatalog,
    table: &ast::RangeVar,
    quals: &(Option<Node>, Option<Node>),
) -> Vec<Vec<ObjectAddress>> {
    let Some(relid) = relation_of(interp, table) else {
        return Vec::new();
    };
    // recordDependencyOnExpr, once per clause: everything it refers to.
    let mut groups: Vec<Vec<ObjectAddress>> = Vec::new();
    for expr in quals.0.iter().chain(quals.1.iter()) {
        let refs = super::depend::expression_references(interp, Some(expr), Some(relid));
        groups.push(super::depend::reference_addresses(interp, &refs));
    }
    let mut out = Vec::new();
    // The cast functions the expressions run (an explicit `::type`, an
    // implicit argument coercion): `recordDependencyOnExpr` sees them as the
    // `FuncExpr`s the cast became.
    let Some(class) = interp.pg_class.get(&relid) else {
        return groups;
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let mut scope = crate::scope::Scope::default();
    scope.add_dml_target(
        interp,
        &class.relname,
        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
        &attrs,
    );
    let null_ctx = crate::nullability::NullabilityContext::default();
    for expr in quals.0.iter().chain(quals.1.iter()) {
        let used = std::cell::RefCell::new(Vec::new());
        let mut params = crate::param_collector::ParamCollector::default();
        let ctx = crate::expr::Ctx::new(&scope, &null_ctx, interp).recording(&used);
        let _ = crate::expr::infer_expr(Some(expr), ctx, &mut params, crate::expr::TypeGoal::NONE);
        for oid in used.into_inner() {
            if !interp.pg_cast.values().any(|c| c.castfunc == Some(oid)) {
                continue;
            }
            let addr = ObjectAddress::proc(oid);
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    groups.push(out);
    groups
}

/// CREATE POLICY: the columns its USING / WITH CHECK read.
pub(crate) fn record_policy(
    interp: &mut PgCatalog,
    stmt: &ast::CreatePolicyStmt,
) -> Result<(), DdlError> {
    let Some(table) = stmt.table.as_ref() else {
        return Ok(());
    };
    let Some(relid) = relation_of(interp, table) else {
        return Ok(());
    };
    let raw = (
        stmt.qual.as_ref().cloned(),
        stmt.with_check.as_ref().cloned(),
    );
    let refs = policy_refs(interp, table, &raw);
    let functions = policy_functions(interp, table, &raw);
    let operators = policy_expression_refs(interp, table, &raw);
    let dep = Dependent::Policy {
        relid,
        name: stmt.policy_name.clone(),
    };
    store(interp, &dep, refs, functions, operators)?;
    let quals = (
        stmt.qual
            .as_ref()
            .map(|q| super::policies::stored_clause(interp, relid, Some(q))),
        stmt.with_check
            .as_ref()
            .map(|c| super::policies::stored_clause(interp, relid, Some(c))),
    );
    interp
        .column_deps
        .policy_quals
        .insert((relid, stmt.policy_name.clone()), quals);
    Ok(())
}

/// ALTER POLICY: a new USING or WITH CHECK replaces the old one.
pub(crate) fn record_policy_alter(
    interp: &mut PgCatalog,
    stmt: &ast::AlterPolicyStmt,
) -> Result<(), DdlError> {
    let Some(table) = stmt.table.as_ref() else {
        return Ok(());
    };
    let Some(relid) = relation_of(interp, table) else {
        return Ok(());
    };
    let key = (relid, stmt.policy_name.clone());
    let mut quals = interp
        .column_deps
        .policy_quals
        .get(&key)
        .cloned()
        .unwrap_or_default();
    if let Some(q) = stmt.qual.as_ref() {
        quals.0 = Some(super::policies::stored_clause(interp, relid, Some(q)));
    }
    if let Some(c) = stmt.with_check.as_ref() {
        quals.1 = Some(super::policies::stored_clause(interp, relid, Some(c)));
    }
    let raw = (
        quals.0.as_ref().and_then(|c| c.expr.clone()),
        quals.1.as_ref().and_then(|c| c.expr.clone()),
    );
    let refs = policy_refs(interp, table, &raw);
    let functions = policy_functions(interp, table, &raw);
    let operators = policy_expression_refs(interp, table, &raw);
    let dep = Dependent::Policy {
        relid,
        name: stmt.policy_name.clone(),
    };
    store(interp, &dep, refs, functions, operators)?;
    interp.column_deps.policy_quals.insert(key, quals);
    Ok(())
}

/// CREATE TRIGGER: the columns its WHEN condition reads (as NEW / OLD)
/// and its UPDATE OF columns.
pub(crate) fn record_trigger(
    interp: &mut PgCatalog,
    stmt: &ast::CreateTrigStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let Some(relid) = relation_of(interp, rv) else {
        return Ok(());
    };
    let mut refs = names_of(interp, relid, &stmt.columns);
    // recordDependencyOnExpr: everything the WHEN condition refers to.
    let mut analyzed = Vec::new();
    if let Some(when) = stmt.when_clause.as_ref() {
        let from = vec![
            Some(range_var_node(rv, Some("new"))),
            Some(range_var_node(rv, Some("old"))),
        ];
        let select = select_of(&[when], from);
        refs.extend(
            statement_refs(interp, &select, &[])
                .into_iter()
                .filter(|&(_, attnum)| attnum != 0),
        );
        analyzed = super::depend::reference_addresses(
            interp,
            &super::depend::trigger_when_references(interp, Some(when), relid),
        );
    }
    store(
        interp,
        &Dependent::Trigger {
            relid,
            name: stmt.trigname.clone(),
        },
        refs,
        Vec::new(),
        vec![analyzed],
    )
}

/// CREATE RULE: what its WHERE condition and actions read, NEW and OLD
/// standing for the rule's relation.
pub(crate) fn record_rule(
    interp: &mut PgCatalog,
    stmt: &ast::RuleStmt,
    qual_analyzed: &[super::depend::Reference],
    actions_analyzed: &[super::depend::Reference],
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let Some(relid) = relation_of(interp, rv) else {
        return Ok(());
    };
    // A view's `_RETURN` rule is its query, whose dependencies the view
    // records (create_view).
    if !interp
        .rules
        .get(&relid)
        .is_some_and(|rules| rules.iter().any(|r| r.name == stmt.rulename))
    {
        return Ok(());
    }
    let new_old = vec![
        Some(range_var_node(rv, Some("new"))),
        Some(range_var_node(rv, Some("old"))),
    ];
    let mut refs: Vec<(PgClassOid, i16)> = Vec::new();
    if let Some(qual) = stmt.where_clause.as_ref() {
        refs.extend(statement_refs(
            interp,
            &select_of(&[qual], new_old.clone()),
            &[],
        ));
    }
    for action in stmt.actions.iter().flatten() {
        refs.extend(statement_refs(interp, action, &new_old));
    }
    // The rule's own relation, through NEW / OLD, is its owner, not a
    // dependency of its own (its columns are).
    refs.retain(|&(r, attnum)| r != relid || attnum != 0);
    // The functions its qualification and actions call.
    let mut functions: Vec<PgProcOid> = Vec::new();
    if let Some(qual) = stmt.where_clause.as_ref() {
        functions.extend(super::views::statement_functions(
            interp,
            &select_of(&[qual], new_old.clone()),
        ));
    }
    for action in stmt.actions.iter().flatten() {
        functions.extend(statement_functions(interp, action, &new_old));
    }
    let dep = Dependent::Rule {
        relid,
        name: stmt.rulename.clone(),
    };
    let analyzed = super::depend::reference_addresses(
        interp,
        &qual_analyzed
            .iter()
            .chain(actions_analyzed)
            .cloned()
            .collect::<Vec<_>>(),
    );
    store(interp, &dep, refs, functions, vec![analyzed])?;
    // Each action's range table holds OLD and NEW, the rule's relation
    // (DO NOTHING's too; a NOTIFY's has none): the actions depend on it —
    // on the columns they read of it, when they read some
    // (eliminate_duplicate_dependencies).
    // (What the actions read of it — `NEW.*` expanded, as their analysis
    // tells.)
    let action_columns = stmt
        .actions
        .iter()
        .flatten()
        .flat_map(|action| statement_refs(interp, action, &new_old))
        .any(|(r, attnum)| r == relid && attnum != 0)
        || super::depend::reference_addresses(interp, actions_analyzed)
            .iter()
            .any(|a| {
                a.classid == PG_CLASS_RELID && a.objid.get() == relid.get() && a.objsubid != 0
            });
    let has_range_table = stmt.actions.is_empty()
        || stmt
            .actions
            .iter()
            .flatten()
            .any(|a| !matches!(a, ast::Node::NotifyStmt(_)));
    if has_range_table
        && !action_columns
        && let Some(addr) = super::depend::named_address_of(
            interp,
            &NamedObject::Rule {
                relid,
                name: stmt.rulename.clone(),
            },
        )
    {
        super::depend::record(
            interp,
            addr,
            [ObjectAddress::relation(relid)],
            DepType::Normal,
        );
    }
    Ok(())
}
