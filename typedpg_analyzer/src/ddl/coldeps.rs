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

use typedpg_pg_query::protobuf::{self, Node, node};

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
    pub(crate) policy_quals: HashMap<(PgClassOid, String), (Option<Node>, Option<Node>)>,
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
    let referenced = refs
        .into_iter()
        .filter(|&(r, a)| !(r == owner && a == 0))
        .map(|(r, a)| ObjectAddress::column(r, a))
        .chain(functions.into_iter().map(ObjectAddress::proc));
    super::depend::record(interp, addr, referenced, DepType::Normal);
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

fn range_var_node(rv: &protobuf::RangeVar, alias: Option<&str>) -> Node {
    let mut rv = rv.clone();
    if let Some(alias) = alias {
        rv.alias = Some(protobuf::Alias {
            aliasname: alias.to_owned(),
            ..Default::default()
        });
    }
    Node {
        node: Some(node::Node::RangeVar(rv)),
    }
}

fn target(expr: &Node) -> Node {
    Node {
        node: Some(node::Node::ResTarget(Box::new(protobuf::ResTarget {
            val: Some(Box::new(expr.clone())),
            ..Default::default()
        }))),
    }
}

/// `SELECT exprs FROM from`: what the expressions read, resolved by the
/// view machinery's binding walker.
fn select_of(exprs: &[&Node], from: Vec<Node>) -> Node {
    Node {
        node: Some(node::Node::SelectStmt(Box::new(protobuf::SelectStmt {
            target_list: exprs.iter().map(|e| target(e)).collect(),
            from_clause: from,
            limit_option: protobuf::LimitOption::Default as i32,
            op: protobuf::SetOperation::SetopNone as i32,
            ..Default::default()
        }))),
    }
}

/// The expressions of a RETURNING list.
fn returning_vals(clause: &Option<protobuf::ReturningClause>) -> Vec<&Node> {
    crate::resolve::returning_exprs(clause)
        .iter()
        .filter_map(|t| match t.node.as_ref()? {
            node::Node::ResTarget(rt) => rt.val.as_deref(),
            _ => None,
        })
        .collect()
}

fn relation_of(interp: &PgCatalog, rv: &protobuf::RangeVar) -> Option<PgClassOid> {
    super::util::lookup_relation(interp, rv)
        .ok()
        .map(|(_, oid)| oid)
}

fn names_of(interp: &PgCatalog, relid: PgClassOid, names: &[Node]) -> Vec<(PgClassOid, i16)> {
    names
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.as_str()),
            node::Node::ResTarget(rt) => Some(rt.name.as_str()),
            _ => None,
        })
        .filter_map(|name| interp.attribute_by_name(relid, name))
        .map(|a| (relid, a.attnum))
        .collect()
}

/// The relations (attnum 0) and columns a statement reads or writes —
/// SELECT, INSERT, UPDATE, DELETE — with `extra_from` in scope (a rule's
/// NEW and OLD).
fn statement_refs(interp: &PgCatalog, stmt: &Node, extra_from: &[Node]) -> Vec<(PgClassOid, i16)> {
    let mut refs: Vec<(PgClassOid, i16)> = Vec::new();
    let walk = |select: &Node, refs: &mut Vec<(PgClassOid, i16)>| {
        let (relations, columns) = super::views::statement_references(interp, select);
        refs.extend(relations.into_iter().map(|r| (r, 0)));
        refs.extend(columns);
    };
    match stmt.node.as_ref() {
        Some(node::Node::SelectStmt(sel))
            if !extra_from.is_empty() && sel.values_lists.is_empty() =>
        {
            let mut sel = sel.clone();
            sel.from_clause.extend(extra_from.iter().cloned());
            walk(
                &Node {
                    node: Some(node::Node::SelectStmt(sel)),
                },
                &mut refs,
            );
        }
        Some(node::Node::SelectStmt(sel)) if !sel.values_lists.is_empty() => {
            let exprs: Vec<&Node> = sel
                .values_lists
                .iter()
                .flat_map(|row| match row.node.as_ref() {
                    Some(node::Node::List(l)) => l.items.iter().collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect();
            walk(&select_of(&exprs, extra_from.to_vec()), &mut refs);
        }
        Some(node::Node::SelectStmt(_)) => walk(stmt, &mut refs),
        Some(node::Node::InsertStmt(ins)) => {
            if let Some(rv) = ins.relation.as_ref()
                && let Some(relid) = relation_of(interp, rv)
            {
                refs.push((relid, 0));
                refs.extend(names_of(interp, relid, &ins.cols));
                // Without a column list the values go to the leading
                // columns (transformInsertStmt's implicit target list).
                if ins.cols.is_empty() {
                    let given = match ins.select_stmt.as_deref().and_then(|s| s.node.as_ref()) {
                        Some(node::Node::SelectStmt(sel)) => match sel.values_lists.first() {
                            Some(row) => match row.node.as_ref() {
                                Some(node::Node::List(l)) => Some(l.items.len()),
                                _ => None,
                            },
                            None => Some(sel.target_list.len()),
                        },
                        _ => None,
                    };
                    let columns = interp.attributes_of(relid).iter().filter(|a| a.attnum > 0);
                    refs.extend(
                        columns
                            .take(given.unwrap_or(usize::MAX))
                            .map(|a| (relid, a.attnum)),
                    );
                }
            }
            if let Some(source) = ins.select_stmt.as_deref() {
                refs.extend(statement_refs(interp, source, extra_from));
            }
            let returning: Vec<&Node> = returning_vals(&ins.returning_clause);
            if let Some(rv) = ins.relation.as_ref()
                && !returning.is_empty()
            {
                let mut from = vec![range_var_node(rv, None)];
                from.extend(extra_from.iter().cloned());
                walk(&select_of(&returning, from), &mut refs);
            }
        }
        Some(node::Node::UpdateStmt(upd)) => {
            if let Some(rv) = upd.relation.as_ref()
                && let Some(relid) = relation_of(interp, rv)
            {
                refs.push((relid, 0));
                refs.extend(names_of(interp, relid, &upd.target_list));
                let mut exprs: Vec<&Node> = upd
                    .target_list
                    .iter()
                    .filter_map(|t| match t.node.as_ref()? {
                        node::Node::ResTarget(rt) => rt.val.as_deref(),
                        _ => None,
                    })
                    .collect();
                exprs.extend(upd.where_clause.as_deref());
                exprs.extend(returning_vals(&upd.returning_clause));
                let mut from = vec![range_var_node(rv, None)];
                from.extend(upd.from_clause.iter().cloned());
                from.extend(extra_from.iter().cloned());
                walk(&select_of(&exprs, from), &mut refs);
            }
        }
        Some(node::Node::DeleteStmt(del)) => {
            if let Some(rv) = del.relation.as_ref()
                && let Some(relid) = relation_of(interp, rv)
            {
                refs.push((relid, 0));
                let mut exprs: Vec<&Node> = del.where_clause.as_deref().into_iter().collect();
                exprs.extend(returning_vals(&del.returning_clause));
                let mut from = vec![range_var_node(rv, None)];
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
fn statement_functions(interp: &PgCatalog, stmt: &Node, extra_from: &[Node]) -> Vec<PgProcOid> {
    let walk = |select: &Node| super::views::statement_functions(interp, select);
    let with_target = |rv: &protobuf::RangeVar, more: &[Node]| {
        let mut from = vec![range_var_node(rv, None)];
        from.extend(more.iter().cloned());
        from.extend(extra_from.iter().cloned());
        from
    };
    match stmt.node.as_ref() {
        Some(node::Node::SelectStmt(sel)) if !sel.values_lists.is_empty() => {
            let exprs: Vec<&Node> = sel
                .values_lists
                .iter()
                .flat_map(|row| match row.node.as_ref() {
                    Some(node::Node::List(l)) => l.items.iter().collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect();
            walk(&select_of(&exprs, extra_from.to_vec()))
        }
        Some(node::Node::SelectStmt(sel)) => {
            let mut sel = sel.clone();
            sel.from_clause.extend(extra_from.iter().cloned());
            walk(&Node {
                node: Some(node::Node::SelectStmt(sel)),
            })
        }
        Some(node::Node::InsertStmt(ins)) => {
            let mut out = ins
                .select_stmt
                .as_deref()
                .map(|s| statement_functions(interp, s, extra_from))
                .unwrap_or_default();
            let returning = returning_vals(&ins.returning_clause);
            if let Some(rv) = ins.relation.as_ref()
                && !returning.is_empty()
            {
                out.extend(walk(&select_of(&returning, with_target(rv, &[]))));
            }
            out
        }
        Some(node::Node::UpdateStmt(upd)) => {
            let Some(rv) = upd.relation.as_ref() else {
                return Vec::new();
            };
            let mut exprs: Vec<&Node> = upd
                .target_list
                .iter()
                .filter_map(|t| match t.node.as_ref()? {
                    node::Node::ResTarget(rt) => rt.val.as_deref(),
                    _ => None,
                })
                .collect();
            exprs.extend(upd.where_clause.as_deref());
            exprs.extend(returning_vals(&upd.returning_clause));
            walk(&select_of(&exprs, with_target(rv, &upd.from_clause)))
        }
        Some(node::Node::DeleteStmt(del)) => {
            let Some(rv) = del.relation.as_ref() else {
                return Vec::new();
            };
            let mut exprs: Vec<&Node> = del.where_clause.as_deref().into_iter().collect();
            exprs.extend(returning_vals(&del.returning_clause));
            walk(&select_of(&exprs, with_target(rv, &del.using_clause)))
        }
        _ => Vec::new(),
    }
}

// ─── Recording ──────────────────────────────────────────────────────────

/// The functions a policy's USING / WITH CHECK call.
fn policy_functions(
    interp: &PgCatalog,
    table: &protobuf::RangeVar,
    quals: &(Option<Node>, Option<Node>),
) -> Vec<PgProcOid> {
    let exprs: Vec<&Node> = quals.0.iter().chain(quals.1.iter()).collect();
    if exprs.is_empty() {
        return Vec::new();
    }
    super::views::statement_functions(
        interp,
        &select_of(&exprs, vec![range_var_node(table, None)]),
    )
}

fn policy_refs(
    interp: &PgCatalog,
    table: &protobuf::RangeVar,
    quals: &(Option<Node>, Option<Node>),
) -> Vec<(PgClassOid, i16)> {
    let exprs: Vec<&Node> = quals.0.iter().chain(quals.1.iter()).collect();
    if exprs.is_empty() {
        return Vec::new();
    }
    statement_refs(
        interp,
        &select_of(&exprs, vec![range_var_node(table, None)]),
        &[],
    )
}

/// CREATE POLICY: the columns its USING / WITH CHECK read.
pub(crate) fn record_policy(
    interp: &mut PgCatalog,
    stmt: &protobuf::CreatePolicyStmt,
) -> Result<(), DdlError> {
    let Some(table) = stmt.table.as_ref() else {
        return Ok(());
    };
    let Some(relid) = relation_of(interp, table) else {
        return Ok(());
    };
    let quals = (
        stmt.qual.as_deref().cloned(),
        stmt.with_check.as_deref().cloned(),
    );
    let refs = policy_refs(interp, table, &quals);
    let functions = policy_functions(interp, table, &quals);
    let dep = Dependent::Policy {
        relid,
        name: stmt.policy_name.clone(),
    };
    store(interp, &dep, refs, functions)?;
    interp
        .column_deps
        .policy_quals
        .insert((relid, stmt.policy_name.clone()), quals);
    Ok(())
}

/// ALTER POLICY: a new USING or WITH CHECK replaces the old one.
pub(crate) fn record_policy_alter(
    interp: &mut PgCatalog,
    stmt: &protobuf::AlterPolicyStmt,
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
    if let Some(q) = stmt.qual.as_deref() {
        quals.0 = Some(q.clone());
    }
    if let Some(c) = stmt.with_check.as_deref() {
        quals.1 = Some(c.clone());
    }
    let refs = policy_refs(interp, table, &quals);
    let functions = policy_functions(interp, table, &quals);
    let dep = Dependent::Policy {
        relid,
        name: stmt.policy_name.clone(),
    };
    store(interp, &dep, refs, functions)?;
    interp.column_deps.policy_quals.insert(key, quals);
    Ok(())
}

/// CREATE TRIGGER: the columns its WHEN condition reads (as NEW / OLD)
/// and its UPDATE OF columns.
pub(crate) fn record_trigger(
    interp: &mut PgCatalog,
    stmt: &protobuf::CreateTrigStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let Some(relid) = relation_of(interp, rv) else {
        return Ok(());
    };
    let mut refs = names_of(interp, relid, &stmt.columns);
    if let Some(when) = stmt.when_clause.as_deref() {
        let from = vec![
            range_var_node(rv, Some("new")),
            range_var_node(rv, Some("old")),
        ];
        refs.extend(
            statement_refs(interp, &select_of(&[when], from), &[])
                .into_iter()
                .filter(|&(_, attnum)| attnum != 0),
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
    )
}

/// CREATE RULE: what its WHERE condition and actions read, NEW and OLD
/// standing for the rule's relation.
pub(crate) fn record_rule(
    interp: &mut PgCatalog,
    stmt: &protobuf::RuleStmt,
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
        range_var_node(rv, Some("new")),
        range_var_node(rv, Some("old")),
    ];
    let mut refs: Vec<(PgClassOid, i16)> = Vec::new();
    if let Some(qual) = stmt.where_clause.as_deref() {
        refs.extend(statement_refs(
            interp,
            &select_of(&[qual], new_old.clone()),
            &[],
        ));
    }
    for action in &stmt.actions {
        refs.extend(statement_refs(interp, action, &new_old));
    }
    // The rule's own relation, through NEW / OLD, is its owner, not a
    // dependency of its own (its columns are).
    refs.retain(|&(r, attnum)| r != relid || attnum != 0);
    // The functions its qualification and actions call.
    let mut functions: Vec<PgProcOid> = Vec::new();
    if let Some(qual) = stmt.where_clause.as_deref() {
        functions.extend(super::views::statement_functions(
            interp,
            &select_of(&[qual], new_old.clone()),
        ));
    }
    for action in &stmt.actions {
        functions.extend(statement_functions(interp, action, &new_old));
    }
    let dep = Dependent::Rule {
        relid,
        name: stmt.rulename.clone(),
    };
    store(interp, &dep, refs, functions)
}
