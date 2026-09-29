//! The rewriter's checks on a data-modifying statement's result relation
//! (PG's `RewriteQuery`, rewriteHandler.c): rules firing on it, and — for a
//! view — automatic update through its base relation (`rewriteTargetView`),
//! recursing down nested views. Parse analysis has finished by then, so
//! these errors come after every analysis error of the statement.
//!
//! Each level runs `rewriteTargetListIU` over the target lists first (a
//! generated or `GENERATED ALWAYS` identity column of a base relation
//! reached through a view only accepts DEFAULT; a view's own column
//! defaults replace DEFAULT / omitted values), then `matchLocks` /
//! `fireRules`, then the view's auto-update.

use super::*;
use crate::ddl::triggers::{TRIGGER_TYPE_DELETE, TRIGGER_TYPE_INSERT, TRIGGER_TYPE_UPDATE};
use crate::pg_catalog::RelKind;
use typedpg_pg_query::protobuf::CmdType;

/// A data-modifying event (`CMD_INSERT` / `CMD_UPDATE` / `CMD_DELETE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DmlEvent {
    Insert,
    Update,
    Delete,
}

impl DmlEvent {
    fn cmd_type(self) -> CmdType {
        match self {
            DmlEvent::Insert => CmdType::CmdInsert,
            DmlEvent::Update => CmdType::CmdUpdate,
            DmlEvent::Delete => CmdType::CmdDelete,
        }
    }

    fn trigger_bit(self) -> i32 {
        match self {
            DmlEvent::Insert => TRIGGER_TYPE_INSERT,
            DmlEvent::Update => TRIGGER_TYPE_UPDATE,
            DmlEvent::Delete => TRIGGER_TYPE_DELETE,
        }
    }
}

/// One assignment of a target list: the column (by name, at the current
/// relation) and whether every value it gets is a `DEFAULT` marker.
#[derive(Clone, Debug)]
pub(crate) struct Assign {
    pub(crate) column: String,
    pub(crate) default: bool,
    /// Some value is a bare `NULL` written to the whole column.
    pub(crate) null: bool,
}

/// An INSERT / UPDATE / DELETE, or one MERGE action (`None` = DO NOTHING).
#[derive(Clone, Debug)]
pub(crate) struct Action {
    pub(crate) event: Option<DmlEvent>,
    /// The target list (INSERT / UPDATE).
    pub(crate) assigns: Vec<Assign>,
    /// `OVERRIDING SYSTEM VALUE` / `OVERRIDING USER VALUE`.
    pub(crate) overriding: bool,
}

/// What the rewriter sees of a data-modifying statement.
#[derive(Clone, Debug)]
pub(crate) struct Rewrite {
    /// `true` for MERGE (its `actions` are the WHEN clauses), else the
    /// statement is its single action.
    pub(crate) merge: bool,
    pub(crate) actions: Vec<Action>,
    /// `INSERT … ON CONFLICT`: `Some(None)` for DO NOTHING,
    /// `Some(Some(set))` for DO UPDATE SET.
    pub(crate) on_conflict: Option<Option<Vec<Assign>>>,
    pub(crate) returning: bool,
    /// Every column the statement names as a target (the result RTE's
    /// `insertedCols` / `updatedCols`), DEFAULT ones included.
    pub(crate) listed: Vec<String>,
}

impl Rewrite {
    /// A plain INSERT / UPDATE / DELETE.
    pub(crate) fn single(event: DmlEvent, assigns: Vec<Assign>, overriding: bool) -> Self {
        Rewrite {
            listed: assigns.iter().map(|a| a.column.clone()).collect(),
            merge: false,
            actions: vec![Action {
                event: Some(event),
                assigns,
                overriding,
            }],
            on_conflict: None,
            returning: false,
        }
    }
}

/// Whether a RETURNING clause returns anything.
pub(crate) fn has_returning(clause: &Option<protobuf::ReturningClause>) -> bool {
    clause.as_ref().is_some_and(|c| !c.exprs.is_empty())
}

/// The assignments of an UPDATE-style SET list (`col = v`, `col[i] = v`,
/// `(a, b) = (v, w)`): a whole-column `DEFAULT` keeps the column's default.
pub(crate) fn set_list_assigns(target_list: &[protobuf::Node]) -> Vec<Assign> {
    target_list
        .iter()
        .filter_map(|t| match t.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => Some(rt),
            _ => None,
        })
        .map(|rt| {
            let default = rt.indirection.is_empty()
                && match rt.val.as_deref().and_then(|v| v.node.as_ref()) {
                    Some(node::Node::SetToDefault(_)) => true,
                    // `(a, b) = (DEFAULT, …)`: the row's element.
                    Some(node::Node::MultiAssignRef(m)) => {
                        match m.source.as_deref().and_then(|s| s.node.as_ref()) {
                            Some(node::Node::RowExpr(row)) => row
                                .args
                                .get((m.colno as usize).wrapping_sub(1))
                                .is_some_and(is_set_to_default),
                            _ => false,
                        }
                    }
                    _ => false,
                };
            Assign {
                column: rt.name.clone(),
                default,
                null: rt.indirection.is_empty()
                    && rt.val.as_deref().is_some_and(is_sql_null_literal),
            }
        })
        .collect()
}

/// Run the rewriter's checks for `rw` against result relation `relid`.
pub(crate) fn check_rewrite(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    rw: &Rewrite,
) -> Result<(), AnalyzeError> {
    let mut execution_error = None;
    rewrite_level(snapshot, relid, rw, 0, &mut execution_error)?;
    execution_error.map_or(Ok(()), Err)
}

/// One `RewriteQuery` level. `execution_error` collects the NOT NULL
/// violation a bare `NULL` written through a view to the base relation
/// raises at execution — after every rewriter error.
fn rewrite_level(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    rw: &Rewrite,
    depth: usize,
    execution_error: &mut Option<AnalyzeError>,
) -> Result<(), AnalyzeError> {
    let Some(class) = snapshot.pg_class.get(&relid) else {
        return Ok(());
    };
    let relname = class.relname.as_str();
    let is_view = class.relkind == RelKind::View;
    let attrs = snapshot.attributes_of(relid);

    // rewriteTargetListIU. The result relation named by the statement
    // itself had its generated / identity columns checked during analysis;
    // one reached through a view is checked here.
    let mut actions = Vec::with_capacity(rw.actions.len());
    for action in &rw.actions {
        let assigns = match action.event {
            Some(event @ (DmlEvent::Insert | DmlEvent::Update)) => target_list_iu(
                attrs,
                event,
                &action.assigns,
                action.overriding,
                is_view,
                depth > 0,
            )?,
            _ => Vec::new(),
        };
        actions.push(Action {
            assigns,
            ..action.clone()
        });
    }
    let on_conflict = match &rw.on_conflict {
        Some(Some(set)) => Some(Some(target_list_iu(
            attrs,
            DmlEvent::Update,
            set,
            false,
            is_view,
            depth > 0,
        )?)),
        other => other.clone(),
    };

    // matchLocks: enabled non-SELECT rules; MERGE supports none. hasUpdate
    // counts every UPDATE rule, enabled or not.
    let rules: Vec<&crate::ddl::rules::Rule> = snapshot
        .rules
        .get(&relid)
        .map(|rs| rs.iter().collect())
        .unwrap_or_default();
    let has_update_rule = rules.iter().any(|r| r.event == CmdType::CmdUpdate);
    let active = |r: &&&crate::ddl::rules::Rule| r.enabled && r.event != CmdType::CmdSelect;
    if rw.merge && rules.iter().any(|r| active(&r)) {
        return Err(crate::pgmsg::merge_on_relation_with_rules(relname).finalize_implicit());
    }

    // fireRules for the statement's event.
    let event = if rw.merge {
        None
    } else {
        rw.actions.first().and_then(|a| a.event)
    };
    let matching: Vec<&&crate::ddl::rules::Rule> = rules
        .iter()
        .filter(|r| active(r) && Some(r.event) == event.map(DmlEvent::cmd_type))
        .collect();
    let instead = matching.iter().any(|r| r.instead && !r.conditional);
    let qual_product = matching.iter().any(|r| r.instead && r.conditional);
    let rule_returning = matching
        .iter()
        .any(|r| r.instead && !r.conditional && r.returning);
    let product_queries = !matching.is_empty();

    if depth > 0 && !is_view && execution_error.is_none() {
        let written = actions
            .iter()
            .filter_map(|a| Some((a.event?, &a.assigns)))
            .chain(
                on_conflict
                    .iter()
                    .flatten()
                    .map(|set| (DmlEvent::Update, set)),
            );
        'outer: for (event, assigns) in written {
            for a in assigns.iter().filter(|a| a.null) {
                if let Some(att) = attrs.iter().find(|att| att.attname == a.column)
                    && let Some(err) = null_assignment_error(
                        att,
                        snapshot,
                        relname,
                        if event == DmlEvent::Insert {
                            "insert"
                        } else {
                            "assign"
                        },
                    )
                {
                    *execution_error = Some(err);
                    break 'outer;
                }
            }
        }
    }

    let mut updatable_view = false;
    if !instead && is_view && !view_has_instead_trigger(snapshot, relid, event, &actions) {
        if qual_product && let Some(event) = event {
            return Err(crate::pgmsg::view_not_updatable(
                event,
                relname,
                "Views with conditional DO INSTEAD rules are not automatically updatable.",
                false,
            )
            .finalize_implicit());
        }
        let rewritten = Rewrite {
            actions,
            on_conflict,
            ..rw.clone()
        };
        rewrite_target_view(
            snapshot,
            relid,
            relname,
            rw,
            &rewritten,
            depth,
            execution_error,
        )?;
        updatable_view = true;
    }

    if let Some(event) = event
        && (instead || qual_product)
        && rw.returning
        && !rule_returning
        && !updatable_view
    {
        return Err(
            crate::pgmsg::returning_without_instead_rule_returning(event, relname)
                .finalize_implicit(),
        );
    }
    if rw.on_conflict.is_some() && (product_queries || has_update_rule) && !updatable_view {
        return Err(crate::pgmsg::on_conflict_with_rules().finalize_implicit());
    }
    Ok(())
}

/// rewriteTargetListIU over one target list of relation `attrs`: the
/// GENERATED ALWAYS / generated-column checks (when `check`), and the
/// resulting target list — a view column's default replaces a DEFAULT (or,
/// for INSERT, a missing value); without one, INSERT drops the DEFAULT and
/// UPDATE sets NULL; generated columns never stay in it.
fn target_list_iu(
    attrs: &[crate::pg_catalog::PgAttribute],
    event: DmlEvent,
    assigns: &[Assign],
    overriding: bool,
    is_view: bool,
    check: bool,
) -> Result<Vec<Assign>, AnalyzeError> {
    let mut out = Vec::new();
    for att in attrs {
        let Some(assigned) = assigns.iter().find(|a| a.column == att.attname) else {
            // INSERT fills a missing column with its default; only a view's
            // own default is a value for the relation below it.
            if event == DmlEvent::Insert && is_view && att.atthasdef {
                out.push(Assign {
                    column: att.attname.clone(),
                    default: false,
                    null: false,
                });
            }
            continue;
        };
        if check && !assigned.default {
            let identity_always = att.attidentity == Some(AttIdentity::Always);
            match event {
                DmlEvent::Insert if identity_always && !overriding => {
                    return Err(crate::pgmsg::insert_non_default_into_generated(
                        &att.attname,
                        true,
                    )
                    .finalize_implicit());
                }
                DmlEvent::Update if identity_always => {
                    return Err(
                        crate::pgmsg::update_generated_to_non_default(&att.attname, true)
                            .finalize_implicit(),
                    );
                }
                _ => {}
            }
            if att.attgenerated.is_some() {
                return Err(match event {
                    DmlEvent::Insert => {
                        crate::pgmsg::insert_non_default_into_generated(&att.attname, false)
                    }
                    _ => crate::pgmsg::update_generated_to_non_default(&att.attname, false),
                }
                .finalize_implicit());
            }
        }
        if att.attgenerated.is_some() {
            continue;
        }
        if !assigned.default {
            out.push(assigned.clone());
        } else if (is_view && att.atthasdef) || event == DmlEvent::Update {
            // build_column_default: the view's default, or (UPDATE) NULL.
            out.push(Assign {
                column: att.attname.clone(),
                default: false,
                null: !(is_view && att.atthasdef),
            });
        }
    }
    Ok(out)
}

/// view_has_instead_trigger: an INSTEAD OF row trigger for the event — for
/// MERGE, for every action that isn't DO NOTHING.
fn view_has_instead_trigger(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    event: Option<DmlEvent>,
    actions: &[Action],
) -> bool {
    let triggers = snapshot.triggers.get(&relid);
    let has = |e: DmlEvent| {
        triggers.is_some_and(|ts| ts.iter().any(|t| t.is_instead_row_for(e.trigger_bit())))
    };
    match event {
        Some(e) => has(e),
        None => actions.iter().filter_map(|a| a.event).all(has),
    }
}

/// rewriteTargetView: the view must be automatically updatable, every
/// column the statement modifies must be one of its base relation's, and
/// the statement continues against the base relation.
fn rewrite_target_view(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    relname: &str,
    rw: &Rewrite,
    rewritten: &Rewrite,
    depth: usize,
    execution_error: &mut Option<AnalyzeError>,
) -> Result<(), AnalyzeError> {
    let (actions, on_conflict) = (&rewritten.actions, &rewritten.on_conflict);
    // Views not defined through DDL here (seeded system views) carry no
    // stored query analysis; leave them alone.
    let Some(upd) = snapshot.view_updatability.get(&relid) else {
        return Ok(());
    };
    let insert_or_update = rw
        .actions
        .iter()
        .any(|a| matches!(a.event, Some(DmlEvent::Insert | DmlEvent::Update)));
    if let Some(reason) = upd.reason(insert_or_update) {
        // error_view_not_updatable: for MERGE, the first action without
        // an INSTEAD OF trigger names the event.
        let event = rw.actions.iter().filter_map(|a| a.event).find(|&e| {
            !rw.merge
                || !snapshot
                    .triggers
                    .get(&relid)
                    .is_some_and(|ts| ts.iter().any(|t| t.is_instead_row_for(e.trigger_bit())))
        });
        return match event {
            Some(event) => Err(
                crate::pgmsg::view_not_updatable(event, relname, reason, rw.merge)
                    .finalize_implicit(),
            ),
            None => Ok(()),
        };
    }
    let view_attrs = snapshot.attributes_of(relid);
    if insert_or_update {
        // The modified columns: those the statement names (insertedCols /
        // updatedCols) plus the rewritten target lists (view defaults).
        let modified = |name: &str| {
            rw.listed.iter().any(|x| x == name)
                || actions
                    .iter()
                    .any(|a| a.assigns.iter().any(|x| x.column == name))
                || matches!(on_conflict, Some(Some(set)) if set.iter().any(|x| x.column == name))
        };
        for (att, col) in view_attrs.iter().zip(&upd.columns) {
            if let Err(reason) = col
                && modified(&att.attname)
            {
                let verb = if rw.merge {
                    "merge into"
                } else if rw.actions.first().and_then(|a| a.event) == Some(DmlEvent::Update) {
                    "update"
                } else {
                    "insert into"
                };
                return Err(crate::pgmsg::view_column_not_updatable(
                    verb,
                    &att.attname,
                    relname,
                    reason,
                )
                .finalize_implicit());
            }
        }
    }
    if rw.merge
        && rw
            .actions
            .iter()
            .filter_map(|a| a.event)
            .any(|e| view_has_instead_trigger(snapshot, relid, Some(e), &[]))
    {
        return Err(crate::pgmsg::merge_view_partial_instead_triggers(relname).finalize_implicit());
    }

    // Continue against the base relation, with the view's columns renamed
    // to the base columns they are.
    let Some(base) = upd.base else {
        return Ok(());
    };
    let base_attrs = snapshot.attributes_of(base);
    let base_name = |column: &str| -> Option<String> {
        let pos = view_attrs.iter().position(|v| v.attname == column)?;
        let attnum = *upd.columns.get(pos)?.as_ref().ok()?;
        let base_att = base_attrs.iter().find(|b| b.attnum == attnum)?;
        Some(base_att.attname.clone())
    };
    let to_base = |assigns: &[Assign]| -> Vec<Assign> {
        assigns
            .iter()
            .filter_map(|a| {
                Some(Assign {
                    column: base_name(&a.column)?,
                    ..a.clone()
                })
            })
            .collect()
    };
    let base_rw = Rewrite {
        merge: rw.merge,
        actions: actions
            .iter()
            .map(|a| Action {
                assigns: to_base(&a.assigns),
                ..a.clone()
            })
            .collect(),
        on_conflict: on_conflict
            .as_ref()
            .map(|oc| oc.as_ref().map(|set| to_base(set))),
        returning: rw.returning,
        listed: rw.listed.iter().filter_map(|c| base_name(c)).collect(),
    };
    rewrite_level(snapshot, base, &base_rw, depth + 1, execution_error)
}
