//! CREATE / DROP / ALTER RULE. Rewrite rules don't change the types of the
//! statements the analyzer checks, but PG validates a rule when it is
//! defined (`transformRuleStmt`, `DefineQueryRewrite`) and keeps rule
//! names unique per relation, so the catalog keeps the names.
//!
//! The rule's WHERE condition is type-checked over NEW / OLD; its actions
//! are checked for the relations they name and analyzed with each NEW /
//! OLD column reference standing in as a typed NULL.

use typedpg_pg_query::protobuf::{CmdType, RuleStmt, node};

use super::DdlError;
use crate::oid::PgClassOid;
use crate::pg_catalog::{PgCatalog, RelKind};

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
            let kinds = match kind {
                RelKind::Sequence => "sequences",
                RelKind::Index | RelKind::PartitionedIndex => "indexes",
                RelKind::CompositeType => "composite types",
                _ => "this relation",
            };
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
    for action in &stmt.actions {
        pseudo_refs(action, false)?;
        check_action_relations(interp, action)?;
        check_action_query(interp, relid, action, has_old, has_new)?;
    }

    let relname = rv.relname.clone();
    let rules = interp.rules.entry(relid).or_default();
    if rules.contains(&stmt.rulename) {
        if stmt.replace {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "rule \"{}\" for relation \"{relname}\" already exists",
            stmt.rulename
        )));
    }
    rules.push(stmt.rulename.clone());
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
fn check_action_query(
    interp: &PgCatalog,
    relid: PgClassOid,
    action: &typedpg_pg_query::protobuf::Node,
    has_old: bool,
    has_new: bool,
) -> Result<(), DdlError> {
    let Some(inner) = action.node.as_ref() else {
        return Ok(());
    };
    if !matches!(
        inner,
        node::Node::SelectStmt(_)
            | node::Node::InsertStmt(_)
            | node::Node::UpdateStmt(_)
            | node::Node::DeleteStmt(_)
    ) {
        return Ok(());
    }
    let Some(class) = interp.pg_class.get(&relid) else {
        return Ok(());
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
        super::dml::check_statement(interp, inner)
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
    let rules = interp.rules.entry(relid).or_default();
    let before = rules.len();
    rules.retain(|r| r != name);
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
    let rules = interp.rules.entry(relid).or_default();
    let Some(pos) = rules.iter().position(|r| *r == stmt.subname) else {
        return Err(DdlError::TypeNotFound(format!(
            "rule \"{}\" for relation \"{}\" does not exist",
            stmt.subname, rv.relname
        )));
    };
    if rules.contains(&stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "rule \"{}\" for relation \"{}\" already exists",
            stmt.newname, rv.relname
        )));
    }
    rules[pos] = stmt.newname.clone();
    Ok(())
}

/// `ALTER TABLE ... ENABLE / DISABLE [ALWAYS | REPLICA] RULE name`
/// (EnableDisableRule).
pub(crate) fn check_rule_exists(
    interp: &PgCatalog,
    relid: PgClassOid,
    name: &str,
) -> Result<(), DdlError> {
    if interp
        .rules
        .get(&relid)
        .is_some_and(|rs| rs.iter().any(|r| r == name))
    {
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
