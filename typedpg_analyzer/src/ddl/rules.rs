//! CREATE / DROP / ALTER RULE. Rewrite rules don't change the types of the
//! statements the analyzer checks, but PG validates a rule when it is
//! defined (`transformRuleStmt`, `DefineQueryRewrite`) and keeps rule
//! names unique per relation, so the catalog keeps the names.
//!
//! The rule's WHERE condition is type-checked over NEW / OLD; its actions
//! are checked for the relations they name and analyzed with each NEW /
//! OLD column reference standing in as a typed NULL. The rewriter's checks
//! on an action run when a statement fires the rule, as in PG.

use typedpg_pg_query::protobuf::{CmdType, RuleStmt, node};

use super::DdlError;
use crate::oid::PgClassOid;
use crate::pg_catalog::{PgCatalog, RelKind};

/// A rewrite rule (`pg_rewrite`) as far as the rewriter's treatment of the
/// statements it fires on is concerned (RewriteQuery / matchLocks /
/// fireRules).
#[derive(Clone, Debug)]
pub(crate) struct Rule {
    /// `pg_rewrite.oid`: the identity `pg_depend` rows name.
    pub(crate) oid: crate::oid::PgGenericOid,
    pub(crate) name: String,
    /// `ev_type`.
    pub(crate) event: CmdType,
    /// `is_instead`.
    pub(crate) instead: bool,
    /// Has a WHERE condition (`ev_qual`).
    pub(crate) conditional: bool,
    /// Some action has a RETURNING list (only allowed in an unconditional
    /// INSTEAD rule).
    pub(crate) returning: bool,
    /// Fires in the default (origin) session replication role:
    /// `ev_enabled` is `O` or `A`.
    pub(crate) enabled: bool,
    /// Has actions (not `DO NOTHING`): firing produces product queries.
    pub(crate) has_actions: bool,
    /// The INSERT / UPDATE / DELETE actions, as the rewriter rewrites them
    /// when the rule fires.
    pub(crate) actions: Vec<crate::resolve::rewrite::RuleAction>,
    /// The command of every action, in order (`CmdUtility` for NOTIFY):
    /// the product queries firing the rule adds.
    pub(crate) action_cmds: Vec<CmdType>,
}

pub fn create_rule(interp: &mut PgCatalog, stmt: &RuleStmt) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    let relkind = interp.pg_class.get(&relid).map(|c| c.relkind);
    let event = CmdType::try_from(stmt.event).unwrap_or(CmdType::Undefined);
    match relkind {
        Some(RelKind::MaterializedView) => {
            return Err(DdlError::UnsupportedDdl(
                "rules on materialized views are not supported".into(),
            ));
        }
        Some(RelKind::Table | RelKind::Partitioned | RelKind::View | RelKind::ForeignTable) => {}
        Some(kind) => {
            let kinds = kind.plural();
            return Err(DdlError::Parse(format!(
                "relation \"{}\" cannot have rules (This operation is not supported for \
                 {kinds}.)",
                rv.relname
            )));
        }
        None => return Ok(()),
    }
    if event == CmdType::CmdSelect && relkind != Some(RelKind::View) {
        let kinds = match relkind {
            Some(RelKind::Partitioned) => "partitioned tables",
            Some(RelKind::ForeignTable) => "foreign tables",
            _ => "tables",
        };
        return Err(DdlError::Parse(format!(
            "relation \"{}\" cannot have ON SELECT rules (This operation is not supported \
             for {kinds}.)",
            rv.relname
        )));
    }

    // transformRuleStmt: NEW exists except for DELETE, OLD except for
    // INSERT.
    let (has_old, has_new) = match event {
        CmdType::CmdInsert => (false, true),
        CmdType::CmdDelete => (true, false),
        _ => (true, true),
    };
    // In the WHERE condition the missing one is an invisible range entry
    // (errorMissingRTE); in an action, transformRuleStmt names the event.
    let pseudo_refs =
        |n: &typedpg_pg_query::protobuf::Node, in_qual: bool| -> Result<(), DdlError> {
            let Some(inner) = n.node.as_ref() else {
                return Ok(());
            };
            for (node, ..) in inner.nodes() {
                let typedpg_pg_query::NodeRef::ColumnRef(cr) = node else {
                    continue;
                };
                if cr.fields.len() < 2 {
                    continue;
                }
                let missing = match cr.fields.first().and_then(super::util::node_string) {
                    Some("old") if !has_old => ("old", "ON INSERT rule cannot use OLD"),
                    Some("new") if !has_new => ("new", "ON DELETE rule cannot use NEW"),
                    _ => continue,
                };
                return Err(DdlError::Parse(if in_qual {
                    format!(
                        "invalid reference to FROM-clause entry for table \"{}\" (There is an \
                     entry for table \"{}\", but it cannot be referenced from this part of \
                     the query.)",
                        missing.0, missing.0
                    )
                } else {
                    missing.1.to_owned()
                }));
            }
            Ok(())
        };
    if let Some(qual) = stmt.where_clause.as_deref() {
        pseudo_refs(qual, true)?;
        check_rule_qual(interp, relid, qual, has_old, has_new)?;
    }
    let mut actions = Vec::new();
    // Each action's output columns, when the analysis could tell them.
    let mut outputs = Vec::new();
    for action in &stmt.actions {
        pseudo_refs(action, false)?;
        check_action_relations(interp, action)?;
        let (checked, rewrites) = crate::resolve::rewrite::collect_rule_actions(|| {
            check_action_query(interp, relid, action, has_old, has_new)
        });
        outputs.push(checked?);
        actions.extend(rewrites);
    }
    define_query_rewrite(interp, relid, stmt, event, &outputs)?;
    if event == CmdType::CmdSelect {
        // CREATE OR REPLACE RULE "_RETURN": the view's new query, producing
        // the same columns.
        let view = typedpg_pg_query::protobuf::RangeVar {
            schemaname: interp
                .pg_class
                .get(&relid)
                .and_then(|c| interp.namespace_name(c.relnamespace))
                .unwrap_or_default()
                .to_owned(),
            relname: rv.relname.clone(),
            inh: true,
            relpersistence: "p".into(),
            ..Default::default()
        };
        return super::views::create_view(
            interp,
            &typedpg_pg_query::protobuf::ViewStmt {
                view: Some(view),
                query: stmt.actions.first().cloned().map(Box::new),
                replace: true,
                ..Default::default()
            },
        );
    }

    let relname = rv.relname.clone();
    let returning = stmt.actions.iter().any(|a| match a.node.as_ref() {
        Some(node::Node::InsertStmt(s)) => s
            .returning_clause
            .as_ref()
            .is_some_and(|r| !r.exprs.is_empty()),
        Some(node::Node::UpdateStmt(s)) => s
            .returning_clause
            .as_ref()
            .is_some_and(|r| !r.exprs.is_empty()),
        Some(node::Node::DeleteStmt(s)) => s
            .returning_clause
            .as_ref()
            .is_some_and(|r| !r.exprs.is_empty()),
        _ => false,
    });
    let rule = Rule {
        oid: crate::ddl::depend::PENDING_OID,
        name: stmt.rulename.clone(),
        event,
        instead: stmt.instead,
        conditional: stmt.where_clause.is_some(),
        returning,
        enabled: true,
        has_actions: !stmt.actions.is_empty(),
        actions,
        action_cmds: stmt
            .actions
            .iter()
            .map(|a| match a.node.as_ref() {
                Some(node::Node::SelectStmt(_)) => CmdType::CmdSelect,
                Some(node::Node::InsertStmt(_)) => CmdType::CmdInsert,
                Some(node::Node::UpdateStmt(_)) => CmdType::CmdUpdate,
                Some(node::Node::DeleteStmt(_)) => CmdType::CmdDelete,
                _ => CmdType::CmdUtility,
            })
            .collect(),
    };
    let rules = interp.rules.entry(relid).or_default();
    if let Some(existing) = rules.iter_mut().find(|r| r.name == stmt.rulename) {
        if stmt.replace {
            // DefineQueryRewrite replaces the definition; ev_enabled stays.
            *existing = Rule {
                oid: existing.oid,
                enabled: existing.enabled,
                ..rule
            };
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "rule \"{}\" for relation \"{relname}\" already exists",
            stmt.rulename
        )));
    }
    let oid = crate::oid::PgGenericOid::from_nonzero(interp.alloc_oid()?);
    interp
        .rules
        .entry(relid)
        .or_default()
        .push(Rule { oid, ..rule });
    Ok(())
}

/// ViewSelectRuleName.
const VIEW_SELECT_RULE_NAME: &str = "_RETURN";

/// DefineQueryRewrite's checks on a rule's actions (`outputs`: each
/// action's result columns, `None` when the analysis couldn't tell): an ON
/// SELECT rule is a view's one INSTEAD SELECT, named `_RETURN` and
/// producing exactly the view's columns; any other rule has at most one
/// RETURNING list — only if unconditional and INSTEAD, matching the
/// relation's columns — and must not be named `_RETURN`.
fn define_query_rewrite(
    interp: &PgCatalog,
    relid: PgClassOid,
    stmt: &RuleStmt,
    event: CmdType,
    outputs: &[Option<Vec<crate::resolve::RawColumn>>],
) -> Result<(), DdlError> {
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    if interp.is_system_class(relid) {
        return Err(DdlError::Parse(format!(
            "permission denied: \"{relname}\" is a system catalog"
        )));
    }
    let unsupported = |msg: &str| Err(DdlError::UnsupportedDdl(msg.to_owned()));
    if event == CmdType::CmdSelect {
        let [action] = stmt.actions.as_slice() else {
            return if stmt.actions.is_empty() {
                unsupported(
                    "INSTEAD NOTHING rules on SELECT are not implemented (Use views instead.)",
                )
            } else {
                unsupported("multiple actions for rules on SELECT are not implemented")
            };
        };
        let Some(node::Node::SelectStmt(select)) = action.node.as_ref() else {
            return unsupported("rules on SELECT must have action INSTEAD SELECT");
        };
        if !stmt.instead {
            return unsupported("rules on SELECT must have action INSTEAD SELECT");
        }
        let modifying_cte = select.with_clause.as_ref().is_some_and(|w| {
            w.ctes.iter().any(|cte| {
                matches!(cte.node.as_ref(), Some(node::Node::CommonTableExpr(c))
                    if matches!(c.ctequery.as_deref().and_then(|q| q.node.as_ref()),
                        Some(node::Node::InsertStmt(_) | node::Node::UpdateStmt(_)
                            | node::Node::DeleteStmt(_) | node::Node::MergeStmt(_))))
            })
        });
        if modifying_cte {
            return unsupported(
                "rules on SELECT must not contain data-modifying statements in WITH",
            );
        }
        if stmt.where_clause.is_some() {
            return unsupported("event qualifications are not implemented for rules on SELECT");
        }
        if let Some(Some(columns)) = outputs.first() {
            check_rule_result_list(interp, relid, columns, true)?;
        }
        // A view has its ON SELECT rule already.
        if !stmt.replace {
            return Err(DdlError::Parse(format!("\"{relname}\" is already a view")));
        }
        if stmt.rulename != VIEW_SELECT_RULE_NAME {
            return Err(DdlError::Parse(format!(
                "view rule for \"{relname}\" must be named \"{VIEW_SELECT_RULE_NAME}\""
            )));
        }
        return Ok(());
    }
    let mut have_returning = false;
    for (action, output) in stmt.actions.iter().zip(outputs) {
        if !has_returning(action) {
            continue;
        }
        if have_returning {
            return unsupported("cannot have multiple RETURNING lists in a rule");
        }
        have_returning = true;
        if stmt.where_clause.is_some() {
            return unsupported("RETURNING lists are not supported in conditional rules");
        }
        if !stmt.instead {
            return unsupported("RETURNING lists are not supported in non-INSTEAD rules");
        }
        if let Some(columns) = output {
            check_rule_result_list(interp, relid, columns, false)?;
        }
    }
    if stmt.rulename == VIEW_SELECT_RULE_NAME {
        return Err(DdlError::Parse(format!(
            "non-view rule for \"{relname}\" must not be named \"{VIEW_SELECT_RULE_NAME}\""
        )));
    }
    Ok(())
}

/// The action has a RETURNING list.
fn has_returning(action: &typedpg_pg_query::protobuf::Node) -> bool {
    let returning = match action.node.as_ref() {
        Some(node::Node::InsertStmt(s)) => s.returning_clause.as_ref(),
        Some(node::Node::UpdateStmt(s)) => s.returning_clause.as_ref(),
        Some(node::Node::DeleteStmt(s)) => s.returning_clause.as_ref(),
        Some(node::Node::MergeStmt(s)) => s.returning_clause.as_ref(),
        _ => None,
    };
    returning.is_some_and(|r| !r.exprs.is_empty())
}

/// checkRuleResultList: a SELECT rule's target list or a RETURNING list
/// must produce the relation's columns — as many, of the same types (and
/// typmods, where both have one), and for a view's SELECT the same names.
fn check_rule_result_list(
    interp: &PgCatalog,
    relid: PgClassOid,
    columns: &[crate::resolve::RawColumn],
    is_select: bool,
) -> Result<(), DdlError> {
    let attrs: Vec<crate::pg_catalog::PgAttribute> = interp
        .attributes_of(relid)
        .iter()
        .filter(|a| a.attnum > 0)
        .cloned()
        .collect();
    let (what, entry) = if is_select {
        ("SELECT rule's target", "SELECT target entry")
    } else {
        ("RETURNING list's", "RETURNING list entry")
    };
    let list = if is_select {
        "SELECT rule's target list"
    } else {
        "RETURNING list"
    };
    for (i, column) in columns.iter().enumerate() {
        let Some(attr) = attrs.get(i) else {
            return Err(DdlError::Parse(format!("{list} has too many entries")));
        };
        let n = i + 1;
        if is_select && column.name != attr.attname {
            return Err(DdlError::Parse(format!(
                "SELECT rule's target entry {n} has different column name from column \"{}\" \
                 (SELECT target entry is named \"{}\".)",
                attr.attname, column.name
            )));
        }
        if column.type_oid != attr.atttypid {
            return Err(DdlError::Parse(format!(
                "{what} entry {n} has different type from column \"{}\" ({entry} has type \
                 {}, but column has type {}.)",
                attr.attname,
                super::util::format_type_for_message(interp, column.type_oid),
                super::util::format_type_for_message(interp, attr.atttypid),
            )));
        }
        if let (Some(a), Some(c)) = (attr.atttypmod, column.typmod)
            && a >= 0
            && c >= 0
            && a != c
        {
            return Err(DdlError::Parse(format!(
                "{what} entry {n} has different size from column \"{}\"",
                attr.attname
            )));
        }
    }
    if columns.len() != attrs.len() {
        return Err(DdlError::Parse(format!("{list} has too few entries")));
    }
    Ok(())
}

/// The rule's WHERE: a boolean over NEW / OLD.
fn check_rule_qual(
    interp: &PgCatalog,
    relid: PgClassOid,
    qual: &typedpg_pg_query::protobuf::Node,
    has_old: bool,
    has_new: bool,
) -> Result<(), DdlError> {
    use crate::expr::{TypeGoal, infer_expr};
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let attrs = interp.attributes_of(relid).to_vec();
    let qn = crate::qualified_name::QualifiedName::new(nspname, class.relname.clone());
    let mut scope = crate::scope::Scope::default();
    if has_old {
        scope.add_dml_target(interp, "old", qn.clone(), &attrs);
    }
    if has_new {
        scope.add_dml_target(interp, "new", qn, &attrs);
    }
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let result = infer_expr(
        qual,
        crate::expr::Ctx::new(&scope, &null_ctx, interp),
        &mut params,
        TypeGoal::NONE,
    )
    .map_err(|e| DdlError::UnsupportedDdl(e.to_string()))?;
    let (bool_oid, unknown) = (
        crate::pg_catalog::oid::BOOL,
        crate::pg_catalog::oid::UNKNOWN,
    );
    if result.type_oid != bool_oid && result.type_oid != unknown {
        return Err(DdlError::UnsupportedDdl(format!(
            "argument of WHERE must be type boolean, not type {}",
            super::util::format_type_for_message(interp, result.type_oid)
        )));
    }
    Ok(())
}

/// Every relation an action names must exist (parserOpenTable); CTE names
/// defined by the action don't count.
fn check_action_relations(
    interp: &PgCatalog,
    action: &typedpg_pg_query::protobuf::Node,
) -> Result<(), DdlError> {
    let Some(inner) = action.node.as_ref() else {
        return Ok(());
    };
    let nodes = inner.nodes();
    let ctes: Vec<&str> = nodes
        .iter()
        .filter_map(|(n, ..)| match n {
            typedpg_pg_query::NodeRef::CommonTableExpr(c) => Some(c.ctename.as_str()),
            _ => None,
        })
        .collect();
    for (n, ..) in &nodes {
        let typedpg_pg_query::NodeRef::RangeVar(rv) = n else {
            continue;
        };
        if rv.schemaname.is_empty() && ctes.contains(&rv.relname.as_str()) {
            continue;
        }
        super::util::lookup_relation(interp, rv)?;
    }
    Ok(())
}

/// transformRuleStmt analyzes each action with OLD / NEW in its range table,
/// reachable only through the relation namespace (`old.col`, `new.*`, a
/// whole-row `new`) — see [`crate::scope::with_rule_pseudo_relations`].
/// Returns the action's result columns (its target or RETURNING list),
/// when the analysis could tell them.
fn check_action_query(
    interp: &PgCatalog,
    relid: PgClassOid,
    action: &typedpg_pg_query::protobuf::Node,
    has_old: bool,
    has_new: bool,
) -> Result<Option<Vec<crate::resolve::RawColumn>>, DdlError> {
    let Some(inner) = action.node.as_ref() else {
        return Ok(None);
    };
    if !matches!(
        inner,
        node::Node::SelectStmt(_)
            | node::Node::InsertStmt(_)
            | node::Node::UpdateStmt(_)
            | node::Node::DeleteStmt(_)
    ) {
        return Ok(None);
    }
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(None);
    };
    let nspname = interp
        .namespace_name(class.relnamespace)
        .unwrap_or("public")
        .to_owned();
    let relation = crate::qualified_name::QualifiedName::new(nspname, class.relname.clone());
    let attrs = interp.attributes_of(relid).to_vec();
    let names: Vec<&str> = [has_old.then_some("old"), has_new.then_some("new")]
        .into_iter()
        .flatten()
        .collect();
    crate::scope::with_rule_pseudo_relations(interp, relation, &attrs, &names, || {
        super::dml::analyze_statement(interp, inner)
    })
}

/// `DROP RULE [IF EXISTS] name ON table`.
pub(crate) fn drop_rule(
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
    let rv = typedpg_pg_query::protobuf::RangeVar {
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
        Err(_) if missing_ok => return Ok(()),
        Err(e) => return Err(e),
    };
    // A view's `_RETURN` rule is part of it (DEPENDENCY_INTERNAL).
    let relkind = interp.pg_class.get(&relid).map(|c| c.relkind);
    if name == VIEW_SELECT_RULE_NAME
        && let Some(kind @ (RelKind::View | RelKind::MaterializedView)) = relkind
    {
        let kind = if kind == RelKind::View {
            "view"
        } else {
            "materialized view"
        };
        return Err(DdlError::DependencyError(format!(
            "cannot drop rule {name} on {kind} {rel} because {kind} {rel} requires it (You can \
             drop {kind} {rel} instead.)",
            rel = rv.relname
        )));
    }
    let rules = interp.rules.entry(relid).or_default();
    let before = rules.len();
    rules.retain(|r| &r.name != name);
    if rules.len() == before && !missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "rule \"{name}\" for relation \"{}\" does not exist",
            rv.relname
        )));
    }
    Ok(())
}

/// `ALTER RULE name ON table RENAME TO new` (RenameRewriteRule).
pub(crate) fn rename_rule(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(rv) = stmt.relation.as_ref() else {
        return Ok(());
    };
    let (_, relid) = super::util::lookup_relation(interp, rv)?;
    // RangeVarCallbackForRenameRule.
    let relkind = interp.pg_class.get(&relid).map(|c| c.relkind);
    if !matches!(
        relkind,
        Some(RelKind::Table | RelKind::View | RelKind::Partitioned)
    ) {
        return Err(DdlError::Parse(format!(
            "relation \"{}\" cannot have rules",
            rv.relname
        )));
    }
    if interp.is_system_class(relid) {
        return Err(DdlError::Parse(format!(
            "permission denied: \"{}\" is a system catalog",
            rv.relname
        )));
    }
    let is_view = relkind == Some(RelKind::View);
    let exists = |interp: &PgCatalog, name: &str| {
        (is_view && name == VIEW_SELECT_RULE_NAME)
            || interp
                .rules
                .get(&relid)
                .is_some_and(|rs| rs.iter().any(|r| r.name == name))
    };
    if !exists(interp, &stmt.subname) {
        return Err(DdlError::TypeNotFound(format!(
            "rule \"{}\" for relation \"{}\" does not exist",
            stmt.subname, rv.relname
        )));
    }
    if exists(interp, &stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "rule \"{}\" for relation \"{}\" already exists",
            stmt.newname, rv.relname
        )));
    }
    // A view's ON SELECT rule is its `_RETURN`, and only it.
    if is_view && stmt.subname == VIEW_SELECT_RULE_NAME {
        return Err(DdlError::Parse(
            "renaming an ON SELECT rule is not allowed".into(),
        ));
    }
    if stmt.newname == VIEW_SELECT_RULE_NAME {
        return Err(DdlError::Parse(format!(
            "non-view rule for \"{}\" must not be named \"{VIEW_SELECT_RULE_NAME}\"",
            rv.relname
        )));
    }
    if let Some(rule) = interp
        .rules
        .get_mut(&relid)
        .and_then(|rs| rs.iter_mut().find(|r| r.name == stmt.subname))
    {
        rule.name = stmt.newname.clone();
    }
    Ok(())
}

/// Whether relation `relid` has rule `name` — a rule CREATE RULE added,
/// or the `_RETURN` rule of a view or materialized view.
pub(crate) fn has_rule(interp: &PgCatalog, relid: PgClassOid, name: &str) -> bool {
    let view_like = matches!(
        interp.pg_class.get(&relid).map(|c| c.relkind),
        Some(RelKind::View | RelKind::MaterializedView)
    );
    (view_like && name == VIEW_SELECT_RULE_NAME)
        || interp
            .rules
            .get(&relid)
            .is_some_and(|rs| rs.iter().any(|r| r.name == name))
}

/// `ALTER TABLE ... ENABLE / DISABLE [ALWAYS | REPLICA] RULE name`
/// (EnableDisableRule): `fires` is whether the new `ev_enabled` fires in
/// the origin replication role (`O` / `A`).
pub(crate) fn set_rule_enabled(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    name: &str,
    fires: bool,
) -> Result<(), DdlError> {
    if let Some(rule) = interp
        .rules
        .get_mut(&relid)
        .and_then(|rs| rs.iter_mut().find(|r| r.name == name))
    {
        rule.enabled = fires;
        return Ok(());
    }
    let relname = interp
        .pg_class
        .get(&relid)
        .map(|c| c.relname.clone())
        .unwrap_or_default();
    Err(DdlError::TypeNotFound(format!(
        "rule \"{name}\" for relation \"{relname}\" does not exist"
    )))
}
