use super::*;
use typedpg_pg_query::NodeBox;

pub(super) mod select_row;

// ──────────────────────────────────────────────────────────────────────────────
// INSERT / UPDATE / DELETE
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_insert(
    ins: &ast::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_insert_with_outer_ctes(ins, snapshot, params, &HashMap::new())
}

/// Whether an INSTEAD rule on `relation` replaces a `cmd` statement on
/// it: the rewriter drops the statement (or, a conditional rule, keeps it
/// under a condition), so what the planner would fold of it may never be
/// planned — its plan-time checks are off.
fn replaced_by_instead_rule(
    relation: Option<&ast::RangeVar>,
    cmd: CmdType,
    snapshot: &PgCatalog,
) -> bool {
    let Some(r) = relation else {
        return false;
    };
    let schema = (!r.schemaname.is_empty()).then_some(r.schemaname.as_str());
    let Some(class) = snapshot.resolve_table(schema, &r.relname) else {
        return false;
    };
    snapshot.rules.get(&class.oid).is_some_and(|rules| {
        rules
            .iter()
            .any(|rule| rule.enabled && rule.event == cmd && rule.instead)
    })
}

/// Like [`analyze_insert`] but accepts CTEs that were defined in an
/// enclosing `WITH` clause (top-level `WITH … INSERT …` mixes them via
/// [`analyze_cte`]). The outer CTEs are merged into the INSERT's local
/// `cte_scopes` so `INSERT … SELECT … FROM <outer_cte>` resolves.
pub(crate) fn analyze_insert_with_outer_ctes(
    ins: &ast::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    if replaced_by_instead_rule(ins.relation.as_deref(), CmdType::CmdInsert, snapshot) {
        crate::expr::replaced_by_rule(|| analyze_insert_here(ins, snapshot, params, outer_ctes))
    } else {
        analyze_insert_here(ins, snapshot, params, outer_ctes)
    }
}

/// For the lint (see [`crate::condition::WriteStatement::base`]): the
/// table a statement doing `events` (a MERGE's actions', when `merge`)
/// into view `relid` stores its rows in, when the view is automatically
/// updatable down to it for those events ([`WriteTarget::resolve_for`]:
/// no DO INSTEAD rule or INSTEAD OF trigger for them on the way); `None`
/// for a table, or a view whose writes are something else.
pub(super) fn write_base(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    events: &[DmlEvent],
    merge: bool,
) -> Option<crate::condition::WriteBase> {
    let is_view = |rel| {
        snapshot
            .pg_class
            .get(&rel)
            .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::View)
    };
    if !is_view(relid) {
        return None;
    }
    let target = WriteTarget::resolve_for(snapshot, relid, events, merge)?;
    let columns = snapshot
        .attributes_of(relid)
        .iter()
        .filter_map(|a| {
            let base = target.to_base.get(&a.attname)?;
            Some((a.attname.clone(), base.clone()))
        })
        .collect();
    // Each view of the chain fills its defaults in (rewriteTargetListIU):
    // a column's own, else its domain's (`build_column_default`), as in
    // `WriteTarget::omitted`.
    let mut defaulted: Vec<String> = Vec::new();
    let mut cur = relid;
    while is_view(cur) {
        let level = WriteTarget::resolve_for(snapshot, cur, events, merge)?;
        for a in snapshot.attributes_of(cur) {
            if a.attnum > 0
                && (a.atthasdef || snapshot.domain_has_default(a.atttypid))
                && let Some(base) = level.to_base.get(&a.attname)
                && !defaulted.contains(base)
            {
                defaulted.push(base.clone());
            }
        }
        cur = snapshot.view_updatability.get(&cur)?.base?;
    }
    Some(crate::condition::WriteBase {
        relid: target.base,
        columns,
        defaulted,
    })
}

fn analyze_insert_here(
    ins: &ast::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    let relation = ins
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("INSERT without relation".into()))?;

    let tgt = resolve_insert_target(ins, relation, snapshot)?;
    let cte_scopes = build_insert_cte_scopes(ins, snapshot, params, outer_ctes)?;
    let _write = crate::pg_catalog::note_write_statement(
        crate::condition::WriteCommand::Insert,
        tgt.oid,
        || write_base(snapshot, tgt.oid, &[DmlEvent::Insert], false),
    );
    crate::pg_catalog::note_relation_use(tgt.oid, 'a');
    crate::pg_catalog::note_target_relation(relation, tgt.oid);

    // Match $N params in VALUES to column types, or analyze INSERT...SELECT.
    // The rewriter later sees one target list entry per supplied column.
    let mut assigns: Vec<Assign> = Vec::new();
    // The first `NULL::d` the VALUES rows build (see `Rewrite::row_error`).
    let mut row_error = None;
    // What each row writes at each target position (`None`: DEFAULT); no
    // row for DEFAULT VALUES, which writes the defaults.
    let mut written: Vec<Vec<Option<ValueInfo>>> = vec![Vec::new()];
    // The output types of a SELECT source (not VALUES).
    let mut select_types: Option<Vec<PgTypeOid>> = None;
    if let Some(ast::Node::SelectStmt(val_sel)) = ins.select_stmt.as_ref() {
        // transformInsertStmt's isGeneralSelect: a VALUES carrying ORDER
        // BY, LIMIT / OFFSET, a locking clause or its own WITH is analyzed
        // as a standalone SELECT whose output columns are then assigned —
        // its column types resolved across the rows (an untyped literal,
        // NULL or `$N` alone becomes text), DEFAULT not allowed. Only a
        // plain VALUES list is the INSERT's own rows.
        if !val_sel.values_lists.is_empty() && !is_general_select(val_sel) {
            let values_lists = expand_values_rows(val_sel, snapshot, params, &cte_scopes)?;
            written =
                analyze_insert_values(ins, &values_lists, &tgt, snapshot, params, &cte_scopes)?;
            let rows: Vec<&[Option<ast::Node>]> = values_lists
                .iter()
                .filter_map(|l| match l.as_ref() {
                    Some(ast::Node::List(list)) => Some(list.items.as_slice()),
                    _ => None,
                })
                .collect();
            row_error = rows
                .iter()
                .flat_map(|r| r.iter())
                .find_map(|v| null_domain_cast_error(v.as_ref(), snapshot));
            let width = rows.first().map_or(0, |r| r.len());
            assigns = (0..width)
                .filter_map(|i| {
                    Some(Assign {
                        column: target_col_at(&tgt, i)?.attname.clone(),
                        default: rows
                            .iter()
                            .all(|r| r.get(i).is_some_and(|n| is_set_to_default(n.as_ref()))),
                        null: tgt.col_indirection.get(i).is_none_or(Vec::is_empty)
                            && rows
                                .iter()
                                .any(|r| r.get(i).is_some_and(|n| is_sql_null_literal(n.as_ref()))),
                        indirected: tgt
                            .col_indirection
                            .get(i)
                            .is_some_and(|ind| !ind.is_empty()),
                    })
                })
                .collect();
        } else {
            let (selected, column_types) =
                analyze_insert_select(val_sel, &ins.cols, &tgt, snapshot, params, &cte_scopes)?;
            // Each output's type: the assignment to its column coerces the
            // value to the column's type unless they already agree.
            for (i, ty) in column_types.iter().enumerate() {
                if let Some(tc) = target_col_at(&tgt, i)
                    && tgt.col_indirection.get(i).is_none_or(Vec::is_empty)
                {
                    crate::privileges::note_column_value(tgt.oid, &tc.attname, Some(*ty));
                }
            }
            if !val_sel.values_lists.is_empty()
                && let Some(e) = general_values_violation(val_sel, &tgt, &column_types, snapshot)
            {
                return Err(e);
            }
            if val_sel.values_lists.is_empty() {
                select_types = Some(column_types);
            }
            let width = selected.len();
            written = vec![selected.into_iter().map(Some).collect()];
            assigns = (0..width)
                .filter_map(|i| {
                    Some(Assign {
                        column: target_col_at(&tgt, i)?.attname.clone(),
                        default: false,
                        null: false,
                        indirected: tgt
                            .col_indirection
                            .get(i)
                            .is_some_and(|ind| !ind.is_empty()),
                    })
                })
                .collect();
        }
    }

    note_written_columns(&tgt, written.first().map_or(0, Vec::len));
    note_insert_privileges(ins, &tgt, written.first().map_or(0, Vec::len));

    // The columns the INSERT leaves to their default — omitted, or
    // `DEFAULT` in some VALUES row — get the default expression, converted
    // to the column's type (rewriteTargetListIU / rewriteValuesRTE): a
    // constant one is folded like a written value.
    if crate::expr::plan_time_checks() {
        let rows: Vec<&[Option<ast::Node>]> = match ins.select_stmt.as_ref() {
            Some(ast::Node::SelectStmt(sel)) => sel
                .values_lists
                .iter()
                .filter_map(|l| match l.as_ref() {
                    Some(ast::Node::List(list)) => Some(list.items.as_slice()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for c in tgt.attrs.iter().filter(|c| c.attnum > 0) {
            let position = (0..assigns.len())
                .find(|&i| target_col_at(&tgt, i).is_some_and(|t| t.attnum == c.attnum));
            let defaulted = match position {
                None => true,
                Some(i) => rows
                    .iter()
                    .any(|r| r.get(i).is_some_and(|n| is_set_to_default(n.as_ref()))),
            };
            if defaulted && let Some(err) = default_fold_error(c, snapshot) {
                return Err(err);
            }
        }
    }

    // DEFAULT VALUES: every column takes its default — a row that may
    // still go to no partition. (Executing it fails after planning folded
    // the defaults above.)
    if ins.select_stmt.is_none()
        && let Some(v) = row_violation(&tgt, &[], &|_| Known::Unknown, snapshot, tgt.oid)
    {
        return Err(v.error);
    }

    // transformInsertStmt: ON CONFLICT DO SELECT returns the conflicting
    // row through RETURNING, which it requires.
    if let Some(oc) = &ins.on_conflict_clause
        && oc.action == ast::OnConflictAction::OnconflictSelect
        && !has_returning(ins.returning_clause.as_deref())
    {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError("ON CONFLICT DO SELECT requires a RETURNING clause".into()),
            crate::error::SourceSpan::from_node_token(oc.location),
            None,
        )
        .finalize_implicit());
    }
    let (arbiter, conflict_set_list) = match &ins.on_conflict_clause {
        Some(on_conflict) => {
            analyze_insert_on_conflict(on_conflict, relation, &tgt, snapshot, params, &cte_scopes)?
        }
        None => (None, Vec::new()),
    };

    // Resolve RETURNING list.
    let mut ret_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let mut ret_null_ctx = NullabilityContext::default();
    let target_qn = crate::qualified_name::QualifiedName::new(&tgt.nsname, &tgt.relname);
    ret_scope.add_dml_target(
        snapshot,
        insert_target_alias(relation),
        target_qn.clone(),
        &tgt.attrs,
    );
    // The rows RETURNING reads: inserted ones, or — ON CONFLICT DO UPDATE
    // — updated ones.
    let mut events = vec![DmlEvent::Insert];
    if ins
        .on_conflict_clause
        .as_ref()
        .is_some_and(|oc| oc.action == ast::OnConflictAction::OnconflictUpdate)
    {
        events.push(DmlEvent::Update);
    }
    let mut ret_scope = with_written_target(
        &ret_scope,
        insert_target_alias(relation),
        snapshot,
        tgt.oid,
        &tgt.attrs,
        &events,
    );
    // ON CONFLICT DO UPDATE / SELECT's EXCLUDED is in the range table but
    // not the namespace RETURNING sees: referencing it is PG's `invalid
    // reference to FROM-clause entry`, and it does not mask a RETURNING
    // OLD / NEW alias.
    if ins.on_conflict_clause.as_ref().is_some_and(|oc| {
        matches!(
            oc.action,
            ast::OnConflictAction::OnconflictUpdate | ast::OnConflictAction::OnconflictSelect
        )
    }) {
        let mut holder = Scope::default();
        holder.add_dml_target(snapshot, "excluded", target_qn, &tgt.attrs);
        ret_scope.shadowed_sources.extend(holder.sources);
    }

    // No returned row of an INSERT has an old version, except the ones
    // ON CONFLICT DO UPDATE updated (or DO SELECT returned: their OLD and
    // NEW are both the existing row).
    let mut rows = ReturningRows {
        old_may_be_null: true,
        new_may_be_null: false,
        ..ReturningRows::default()
    };
    // A returned row is an inserted one, or — ON CONFLICT DO UPDATE — an
    // updated one (DO NOTHING returns only the inserted rows).
    if has_returning(ins.returning_clause.as_deref())
        && let Some(wt) = WriteTarget::resolve(snapshot, tgt.oid)
    {
        let insert_arm = if wt.insert_keeps(snapshot) {
            insert_knowledge(snapshot, &wt, &tgt, &written)
        } else {
            RowKnowledge::default()
        };
        let mut not_null = wt.row_not_null(snapshot, &insert_arm);
        if let Some(oc) = &ins.on_conflict_clause
            && oc.action == ast::OnConflictAction::OnconflictUpdate
        {
            let update_arm = on_conflict_update_arm(
                oc,
                &conflict_set_list,
                relation,
                &tgt,
                &wt,
                &not_null,
                arbiter.as_ref(),
                snapshot,
                params,
                &cte_scopes,
            );
            not_null.retain(|c| update_arm.contains(c));
        }
        if let Some(oc) = &ins.on_conflict_clause
            && oc.action == ast::OnConflictAction::OnconflictSelect
        {
            let select_arm = on_conflict_select_arm(
                oc,
                relation,
                &tgt,
                &wt,
                &not_null,
                arbiter.as_ref(),
                snapshot,
                params,
                &cte_scopes,
            );
            not_null.retain(|c| select_arm.contains(c));
        }
        prove_columns(&mut ret_null_ctx, insert_target_alias(relation), &not_null);
        rows.new_proven = not_null;
    }
    let columns = resolve_returning(
        ins.returning_clause.as_deref(),
        insert_target_alias(relation),
        rows,
        expr::Ctx::new(&ret_scope, &ret_null_ctx, snapshot),
        params,
    )?;

    let mut rw = Rewrite::single(DmlEvent::Insert, assigns, tgt.overriding);
    rw.row_error = row_error;
    rw.returning = has_returning(ins.returning_clause.as_deref());
    rw.on_conflict =
        ins.on_conflict_clause
            .as_ref()
            .and_then(|oc| match oc.action {
                ast::OnConflictAction::OnconflictUpdate => {
                    let set = set_list_assigns(&conflict_set_list);
                    rw.listed.extend(set.iter().map(|a| a.column.clone()));
                    Some(Some(set))
                }
                ast::OnConflictAction::OnconflictNothing
                | ast::OnConflictAction::OnconflictSelect => Some(None),
                _ => None,
            });
    check_rewrite(snapshot, tgt.oid, &rw)?;
    // The planner infers the arbiter index after the rewriter ran.
    if let Some(on_conflict) = &ins.on_conflict_clause {
        let arbiters = match &arbiter {
            Some(arbiter) => {
                validate_on_conflict_target(on_conflict, arbiter, snapshot, tgt.oid, &tgt.relname)?
            }
            None => None,
        };
        check_deferrable_arbiters(ins, on_conflict, arbiters, &tgt, snapshot)?;
    }
    // Every execution evaluates the rows the SELECT yields. (Not followed
    // in a WITH, whose earlier CTEs its names could mean.)
    if let Some(ast::Node::SelectStmt(sel)) = ins.select_stmt.as_ref()
        && outer_ctes.is_empty()
        && ins.with_clause.as_ref().is_none_or(|w| !w.recursive)
    {
        let ctes = ins.with_clause.as_ref().map_or(&[][..], |w| &w.ctes[..]);
        super::failing_rows::check_insert_select(sel, ctes, tgt.oid, snapshot)?;
    }
    // Then storing the row a SELECT yields every time fails, if it does.
    if let (Some(types), Some(ast::Node::SelectStmt(sel))) =
        (&select_types, ins.select_stmt.as_ref())
        && let Some(e) =
            select_row::sure_row_violation(sel, ins.with_clause.as_deref(), &tgt, types, snapshot)
    {
        return Err(e);
    }

    Ok((columns, None))
}

/// `ExecCheckIndexConstraints` (execIndexing.c): the executor refuses a
/// deferrable unique or exclusion index (`!indimmediate`) as an ON
/// CONFLICT arbiter — 55000, as a row's check reaches that index. The
/// arbiters are the ones the target inferred, or every unique and
/// exclusion index without one; they are checked by OID
/// (`RelationGetIndexList`), so an immediate one before may find the
/// conflict and end the check first. Reported only when every execution
/// raises it: the INSERT surely stores a row (VALUES, DEFAULT VALUES, a
/// SELECT that always yields one) that no BEFORE ROW trigger may drop.
fn check_deferrable_arbiters(
    ins: &ast::InsertStmt,
    on_conflict: &ast::OnConflictClause,
    arbiters: Option<super::Arbiters>,
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    if !crate::resolve::executes() || returning_rewritten(snapshot, tgt.oid, &[DmlEvent::Insert]) {
        return Ok(());
    }
    let arbiters = match arbiters {
        Some(a) => a,
        None if on_conflict.infer.is_none() => {
            // Through an updatable view, the base relation's indexes.
            let mut table = tgt.oid;
            while let Some(upd) = snapshot.view_updatability.get(&table) {
                let Some(base) = upd.base else {
                    return Ok(());
                };
                table = base;
            }
            super::Arbiters {
                table,
                indexes: snapshot
                    .pg_index
                    .values()
                    .filter(|i| i.indrelid == table && (i.indisunique || i.indisexclusion))
                    .map(|i| i.indexrelid)
                    .collect(),
            }
        }
        None => return Ok(()),
    };
    let deferrable = |i: &crate::oid::PgClassOid| snapshot.nonimmediate_indexes.contains(i);
    let Some(first) = arbiters.indexes.iter().copied().filter(deferrable).min() else {
        return Ok(());
    };
    if arbiters
        .indexes
        .iter()
        .any(|i| *i < first && !deferrable(i))
    {
        return Ok(());
    }
    let surely_stores = match ins.select_stmt.as_ref() {
        // DEFAULT VALUES.
        None => true,
        Some(ast::Node::SelectStmt(sel)) => {
            !sel.values_lists.is_empty()
                || select_row::surely_yields_row(sel, ins.with_clause.as_deref(), snapshot)
        }
        Some(_) => false,
    };
    if !surely_stores
        || crate::resolve::written_row::before_row_trigger_rewrites(
            snapshot,
            arbiters.table,
            DmlEvent::Insert,
        )
    {
        return Ok(());
    }
    Err(crate::error::RawError::new(
        AnalyzeError::ObjectNotInPrerequisiteState(
            "ON CONFLICT does not support deferrable unique constraints/exclusion constraints \
             as arbiters"
                .into(),
        ),
        None,
        None,
    )
    .finalize_implicit())
}

/// What every row an INSERT stores holds, by base column: the values
/// `written` gives at each target position in each row (`None`: DEFAULT),
/// and the defaults of the columns it gives no value for.
fn insert_knowledge(
    snapshot: &PgCatalog,
    wt: &WriteTarget,
    tgt: &InsertTarget,
    written: &[Vec<Option<ValueInfo>>],
) -> RowKnowledge {
    let rows: Vec<Vec<(String, Option<ValueInfo>)>> = written
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .filter_map(|(i, v)| Some((target_col_at(tgt, i)?.attname.clone(), v.clone())))
                .collect()
        })
        .collect();
    inserted_rows_knowledge(snapshot, wt, &rows, tgt.overriding)
}

/// What every row an INSERT (or MERGE's INSERT action) stores holds, by
/// base column: each row's values for the target columns it names
/// (`None`: DEFAULT), and the defaults of the others. Under OVERRIDING
/// USER VALUE an identity column takes its sequence's value either way.
pub(crate) fn inserted_rows_knowledge(
    snapshot: &PgCatalog,
    wt: &WriteTarget,
    rows: &[Vec<(String, Option<ValueInfo>)>],
    overriding: Overriding,
) -> RowKnowledge {
    let base_attrs: Vec<&crate::pg_catalog::PgAttribute> = snapshot
        .attributes_of(wt.base)
        .iter()
        .filter(|a| a.attnum > 0 && a.attgenerated.is_none())
        .collect();
    let mut defaults: HashMap<String, ValueInfo> = HashMap::new();
    let mut combined: HashMap<String, ValueInfo> = HashMap::new();
    for row in rows {
        for b in &base_attrs {
            // Every target column storing the base column (a view may
            // expose one twice).
            let given: Vec<&Option<ValueInfo>> = row
                .iter()
                .filter(|(c, _)| wt.to_base.get(c) == Some(&b.attname))
                .map(|(_, v)| v)
                .collect();
            let mut omitted = || {
                defaults
                    .entry(b.attname.clone())
                    .or_insert_with(|| wt.omitted(snapshot, b))
                    .clone()
            };
            let info = match given.as_slice() {
                _ if overriding == Overriding::UserValue && b.attidentity.is_some() => {
                    default_value(snapshot, wt.base, b)
                }
                [] | [None] => omitted(),
                [Some(v)] => v.clone(),
                // Several assignments into the column (an error, unless
                // all but one are DEFAULTs no default replaces).
                _ => ValueInfo::default(),
            };
            combined
                .entry(b.attname.clone())
                .and_modify(|v| *v = v.either(&info))
                .or_insert(info);
        }
    }
    let mut k = RowKnowledge::default();
    for (c, v) in &combined {
        k.write(c, v);
    }
    k
}

/// The base columns proven non-NULL in the rows ON CONFLICT DO UPDATE
/// updates (ExecOnConflictUpdate): the conflicting row with the SET values
/// — evaluated where the DO UPDATE WHERE holds, over an EXCLUDED row whose
/// columns `insert_not_null` are non-NULL — and its other columns as
/// stored. Those of the arbiter's key equal the proposed row's (a NULL key
/// never conflicts, and under NULLS NOT DISTINCT only with a NULL one).
/// `set_list` is `oc`'s SET list as [`analyze_set_clause`] expanded it.
#[allow(clippy::too_many_arguments)]
fn on_conflict_update_arm(
    oc: &ast::OnConflictClause,
    set_list: &[Option<ast::Node>],
    relation: &ast::RangeVar,
    tgt: &InsertTarget,
    wt: &WriteTarget,
    insert_not_null: &std::collections::HashSet<String>,
    arbiter: Option<&Arbiter>,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> std::collections::HashSet<String> {
    // Through a view, EXCLUDED and the SET list are the view's: only what
    // holds of any row is used.
    if wt.relid != wt.base || !wt.update_keeps(snapshot) {
        return wt.row_not_null(snapshot, &RowKnowledge::default());
    }
    let alias = insert_target_alias(relation);
    let target_qn = crate::qualified_name::QualifiedName::new(&tgt.nsname, &tgt.relname);
    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    scope.add_dml_target(snapshot, alias, target_qn.clone(), &tgt.attrs);
    let mut holder = Scope::default();
    holder.add_dml_target(snapshot, "excluded", target_qn, &tgt.attrs);
    for s in &mut holder.sources {
        s.system_columns.clear();
        s.dml_target = false;
        // EXCLUDED's RTEPermissionInfo is the target relation's: a column
        // read through it marks that column SELECTed (its `requiredPerms`
        // starts as ACL_SELECT).
        for c in &mut s.columns {
            c.base_not_null |= insert_not_null.contains(&c.name);
        }
    }
    scope.sources.extend(holder.sources);
    let mut null_ctx = NullabilityContext::default();
    let mut k = RowKnowledge::default();
    if let w @ Some(_) = &oc.where_clause {
        let log = crate::nonnull::StrictLog::default();
        let mut scratch = params.clone();
        let typed = expr::infer_expr(
            w.as_ref(),
            expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
            &mut scratch,
            TypeGoal::implicit(oid::BOOL),
        );
        if typed.is_ok() {
            params.absorb_non_null_reads(&scratch);
            let facts = crate::nonnull::nonnullable(w.as_ref(), true, &scope, &log, snapshot)
                .restricted_to(&crate::nonnull::own_aliases(&scope));
            k = RowKnowledge::from_facts(&facts, alias, |c| Some(c.to_owned()));
            null_ctx.add_where_facts(facts);
        }
    }
    let mut key: Vec<i16> = arbiter
        .map(|a| a.cols.iter().copied().filter(|&n| n > 0).collect())
        .unwrap_or_default();
    if let Some(infer) = oc.infer.as_deref()
        && !infer.conname.is_empty()
        && let Some(con) = snapshot.pg_constraint_values().find(|c| {
            c.conrelid == tgt.oid
                && c.conname == infer.conname
                && matches!(c.contype, ConType::Unique | ConType::PrimaryKey)
        })
    {
        key.extend(con.conkey.iter().copied());
    }
    for attnum in key {
        if let Some(a) = tgt.attrs.iter().find(|a| a.attnum == attnum)
            && insert_not_null.contains(&a.attname)
        {
            k.not_null.insert(a.attname.clone());
        }
    }
    let set = set_values(
        set_list,
        wt,
        &tgt.attrs,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    );
    for a in set_list_assigns(set_list) {
        match set.get(&a.column) {
            Some(v) => k.write(&a.column, v),
            None => k.write(&a.column, &ValueInfo::default()),
        }
    }
    for a in tgt.attrs.iter().filter(|a| a.attgenerated.is_some()) {
        k.write(&a.attname, &ValueInfo::default());
    }
    wt.row_not_null(snapshot, &k)
}

/// The columns non-NULL in every row ON CONFLICT DO SELECT returns from
/// the table: an existing row, which the DO SELECT WHERE (over it and
/// EXCLUDED) let through and whose arbiter key equals the proposed row's.
#[allow(clippy::too_many_arguments)]
fn on_conflict_select_arm(
    oc: &ast::OnConflictClause,
    relation: &ast::RangeVar,
    tgt: &InsertTarget,
    wt: &WriteTarget,
    insert_not_null: &std::collections::HashSet<String>,
    arbiter: Option<&Arbiter>,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> std::collections::HashSet<String> {
    // Through a view, only what holds of any row is used.
    if wt.relid != wt.base {
        return wt.row_not_null(snapshot, &RowKnowledge::default());
    }
    let alias = insert_target_alias(relation);
    let target_qn = crate::qualified_name::QualifiedName::new(&tgt.nsname, &tgt.relname);
    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    scope.add_dml_target(snapshot, alias, target_qn.clone(), &tgt.attrs);
    let mut holder = Scope::default();
    holder.add_dml_target(snapshot, "excluded", target_qn, &tgt.attrs);
    for s in &mut holder.sources {
        s.system_columns.clear();
        s.dml_target = false;
        for c in &mut s.columns {
            c.base_not_null |= insert_not_null.contains(&c.name);
        }
    }
    scope.sources.extend(holder.sources);
    let mut k = RowKnowledge::default();
    if let w @ Some(_) = &oc.where_clause {
        let log = crate::nonnull::StrictLog::default();
        let mut scratch = params.clone();
        let null_ctx = NullabilityContext::default();
        let typed = expr::infer_expr(
            w.as_ref(),
            expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
            &mut scratch,
            TypeGoal::implicit(oid::BOOL),
        );
        if typed.is_ok() {
            params.absorb_non_null_reads(&scratch);
            let facts = crate::nonnull::nonnullable(w.as_ref(), true, &scope, &log, snapshot)
                .restricted_to(&crate::nonnull::own_aliases(&scope));
            k = RowKnowledge::from_facts(&facts, alias, |c| Some(c.to_owned()));
        }
    }
    let mut key: Vec<i16> = arbiter
        .map(|a| a.cols.iter().copied().filter(|&n| n > 0).collect())
        .unwrap_or_default();
    if let Some(infer) = oc.infer.as_deref()
        && !infer.conname.is_empty()
        && let Some(con) = snapshot.pg_constraint_values().find(|c| {
            c.conrelid == tgt.oid
                && c.conname == infer.conname
                && matches!(c.contype, ConType::Unique | ConType::PrimaryKey)
        })
    {
        key.extend(con.conkey.iter().copied());
    }
    for attnum in key {
        if let Some(a) = tgt.attrs.iter().find(|a| a.attnum == attnum)
            && insert_not_null.contains(&a.attname)
        {
            k.not_null.insert(a.attname.clone());
        }
    }
    wt.row_not_null(snapshot, &k)
}

/// The name ON CONFLICT and RETURNING use for the INSERT target:
/// `INSERT INTO t AS a` makes it `a` (PG's `transformInsertStmt` adds the
/// target RTE under its alias).
fn insert_target_alias(relation: &ast::RangeVar) -> &str {
    relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname)
}

/// The resolved INSERT target: the catalog table plus the data the per-clause
/// analyzers below all need (the declared column list and the
/// `OVERRIDING SYSTEM VALUE` flag).
struct InsertTarget {
    oid: crate::oid::PgClassOid,
    relname: String,
    nsname: String,
    attrs: Vec<crate::pg_catalog::PgAttribute>,
    /// Columns named in `INSERT INTO t (a, b, …)`; empty means "all columns".
    col_names: Vec<String>,
    /// The indirection (`arr[1]`, `p.x`) of each named column, parallel to
    /// `col_names`.
    col_indirection: Vec<Vec<Option<ast::Node>>>,
    /// `OVERRIDING SYSTEM VALUE` or `OVERRIDING USER VALUE`. Either lets a
    /// value be written to a GENERATED ALWAYS identity column; USER VALUE
    /// then discards it — and any value given for a BY DEFAULT one — in
    /// favour of the sequence.
    overriding: Overriding,
    /// The columns of a view target that are identity columns of the
    /// relation the row is stored in (see [`view_identity_columns`]).
    view_identity: Vec<String>,
    /// The INSERT has an ON CONFLICT clause: a conflicting row is not
    /// inserted, so no view's WITH CHECK OPTION checks it.
    on_conflict: bool,
}

impl InsertTarget {
    /// Whether the value given for column `c` is discarded for its
    /// identity sequence's next value (OVERRIDING USER VALUE) — also
    /// through an automatically updatable view, whose base relation's
    /// rewriteTargetListIU drops it.
    fn discards(&self, c: &crate::pg_catalog::PgAttribute) -> bool {
        self.overriding == Overriding::UserValue
            && (c.attidentity.is_some() || self.view_identity.contains(&c.attname))
    }
}

/// The columns of view `relid` that are identity columns of the relation
/// an INSERT through it stores its row in, down a chain of automatically
/// updatable views. Empty when an INSTEAD OF trigger or a DO INSTEAD rule
/// takes the row instead.
fn view_identity_columns(snapshot: &PgCatalog, relid: crate::oid::PgClassOid) -> Vec<String> {
    let is_view = |r| {
        snapshot
            .pg_class
            .get(&r)
            .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::View)
    };
    if !is_view(relid) || returning_rewritten(snapshot, relid, &[DmlEvent::Insert]) {
        return Vec::new();
    }
    snapshot
        .attributes_of(relid)
        .iter()
        .filter(|c| c.attnum > 0)
        .filter(|c| {
            let (mut rel, mut attnum) = (relid, c.attnum);
            for _ in 0..16 {
                if !is_view(rel) {
                    return snapshot
                        .attributes_of(rel)
                        .iter()
                        .any(|b| b.attnum == attnum && b.attidentity.is_some());
                }
                let Some(upd) = snapshot.view_updatability.get(&rel) else {
                    return false;
                };
                let (Some(base), Some(Ok(a))) = (
                    upd.base,
                    usize::try_from(attnum - 1)
                        .ok()
                        .and_then(|i| upd.columns.get(i)),
                ) else {
                    return false;
                };
                (rel, attnum) = (base, *a);
            }
            false
        })
        .map(|c| c.attname.clone())
        .collect()
}

/// Resolve the INSERT target relation, validate that every column named in the
/// target list exists, and collect the declared column list + overriding flag.
fn resolve_insert_target(
    ins: &ast::InsertStmt,
    relation: &ast::RangeVar,
    snapshot: &PgCatalog,
) -> Result<InsertTarget, AnalyzeError> {
    super::from::check_rangevar_catalog(relation)?;
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    let table = snapshot
        .resolve_table(schema, &relation.relname)
        .ok_or_else(|| {
            crate::scope::undefined_table_error(
                snapshot,
                schema,
                &relation.relname,
                crate::error::SourceSpan::from_node_qname(relation.location),
            )
        })?;
    crate::scope::check_relation_opens(table)?;

    let col_names: Vec<String> = ins
        .cols
        .iter()
        .filter_map(|n| {
            if let Some(ast::Node::ResTarget(rt)) = n.as_ref() {
                Some(rt.name.clone())
            } else {
                None
            }
        })
        .collect();

    let table_oid = table.oid;
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table_oid).to_vec();

    // Validate every column mentioned in the INSERT target list exists on the
    // table. PostgreSQL rejects unknown columns with a clear error; without
    // this check the analyzer would silently treat the corresponding `$N`
    // parameter as text via the UNKNOWN fallback, masking a real bug in the
    // caller's SQL.
    for n in &ins.cols {
        let Some(ast::Node::ResTarget(rt)) = n.as_ref() else {
            continue;
        };
        if !table_attrs.iter().any(|c| c.attname == rt.name) {
            return Err(crate::scope::undefined_dml_column_error(
                &rt.name,
                &table_relname,
                &table_attrs,
                crate::error::SourceSpan::from_node_qname(rt.location),
            ));
        }
    }
    check_insert_target_duplicates(&ins.cols)?;
    // A stored query depends on the relation it writes; on the columns it
    // writes once the source's width is known
    // ([`note_written_columns`]).
    crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::relation(table_oid));
    let col_indirection = ins
        .cols
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::ResTarget(rt)) => Some(rt.indirection.clone()),
            _ => None,
        })
        .collect();

    // `OVERRIDING SYSTEM VALUE` on a table without any identity column is a
    // no-op for PG (silently accepted), so we don't reject the construct
    // here even though it's almost always a caller mistake — keeping
    // `pg_sanity` honest matters more than catching the typo statically.
    Ok(InsertTarget {
        oid: table_oid,
        relname: table_relname,
        nsname: table_nsname,
        attrs: table_attrs,
        col_names,
        col_indirection,
        overriding: Overriding::from_kind(ins.r#override),
        view_identity: view_identity_columns(snapshot, table_oid),
        on_conflict: ins.on_conflict_clause.is_some(),
    })
}

/// Walk the optional `WITH` clause so parameters used only inside the CTE are
/// registered with the collector — without this, `$N` numbers referenced
/// exclusively in the CTE would be missing from `seen` and `into_sorted`
/// would report a spurious "parameter gap". The resolved CTE columns are also
/// threaded into the inner SELECT's scope so `INSERT … SELECT … FROM cte`
/// resolves the CTE alias.
fn build_insert_cte_scopes(
    ins: &ast::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<HashMap<String, Vec<ScopeColumn>>, AnalyzeError> {
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &ins.with_clause {
        let owner = typedpg_pg_query::NodeRef::InsertStmt(ins);
        cte_scopes = super::cte::with_statement_owner(with, owner, snapshot, || {
            analyze_with_clause(with, snapshot, params, &cte_scopes, &[])
        })?;
    }
    Ok(cte_scopes)
}

/// `find_expr_references_walker` for an INSERT: a stored INSERT depends on
/// each column its target list writes — the listed ones, or as many
/// leading columns as the source has values (`width`, after `*`
/// expansion). The relation is noted when the target is resolved.
fn note_written_columns(tgt: &InsertTarget, width: usize) {
    for i in 0..width {
        if let Some(c) = target_col_at(tgt, i) {
            crate::ddl::depend::note_column(tgt.oid, &c.attname);
        }
    }
}

/// What an INSERT requires of its target (`RTEPermissionInfo`): INSERT on
/// the columns of its target list (`insertedCols` — every position the
/// rows give, `DEFAULT` included), and the defaults of the columns it
/// leaves to them, which run with the statement (rewriteTargetListIU).
fn note_insert_privileges(ins: &ast::InsertStmt, tgt: &InsertTarget, width: usize) {
    crate::privileges::note_relation(tgt.oid, crate::ddl::acl::bits::INSERT);
    // INSERT has no ONLY form: it never routes into inheritance children.
    crate::privileges::note_write(tgt.oid, crate::ddl::triggers::TRIGGER_TYPE_INSERT, false);
    for i in 0..width {
        if let Some(c) = target_col_at(tgt, i) {
            crate::privileges::note_column(
                tgt.oid,
                &c.attname,
                crate::privileges::ColumnAccess::Insert,
            );
        }
    }
    let rows: Vec<&[Option<ast::Node>]> = match ins.select_stmt.as_ref() {
        Some(ast::Node::SelectStmt(sel)) => sel
            .values_lists
            .iter()
            .filter_map(|l| match l.as_ref() {
                Some(ast::Node::List(list)) => Some(list.items.as_slice()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for c in tgt.attrs.iter().filter(|c| c.attnum > 0) {
        let position =
            (0..width).find(|&i| target_col_at(tgt, i).is_some_and(|t| t.attnum == c.attnum));
        let defaulted = match position {
            None => true,
            Some(i) => rows
                .iter()
                .any(|r| r.get(i).is_some_and(|n| is_set_to_default(n.as_ref()))),
        };
        if defaulted {
            crate::privileges::note_default(tgt.oid, c.attnum);
        }
    }
}

/// The column targeted by position `i` in a VALUES row / SELECT list, honoring
/// an explicit column list (`INSERT INTO t (a, b)`) or full table order.
fn target_col_at(tgt: &InsertTarget, i: usize) -> Option<&crate::pg_catalog::PgAttribute> {
    if tgt.col_names.is_empty() {
        tgt.attrs.get(i)
    } else {
        tgt.col_names
            .get(i)
            .and_then(|cn| tgt.attrs.iter().find(|c| &c.attname == cn))
    }
}

/// The assignment target at position `i`: the column's type, or the element
/// / field type an indirected column (`arr[1]`) expects.
fn target_at(
    tgt: &InsertTarget,
    i: usize,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Option<AssignTarget>, AnalyzeError> {
    let Some(tc) = target_col_at(tgt, i) else {
        return Ok(None);
    };
    let indirection = tgt.col_indirection.get(i).map(Vec::as_slice).unwrap_or(&[]);
    assignment_target(tc, indirection, ctx, params).map(Some)
}

/// The number of values each row must supply: the explicit column count, or
/// the full table width when no column list is given.
fn insert_arity(tgt: &InsertTarget) -> usize {
    if tgt.col_names.is_empty() {
        tgt.attrs.len()
    } else {
        tgt.col_names.len()
    }
}

/// transformInsertStmt's `isGeneralSelect`, for a VALUES source: a VALUES
/// list with a sort, limit, locking or WITH clause is a full SELECT.
fn is_general_select(sel: &ast::SelectStmt) -> bool {
    !sel.sort_clause.is_empty()
        || sel.limit_offset.is_some()
        || sel.limit_count.is_some()
        || !sel.locking_clause.is_empty()
        || sel.with_clause.is_some()
}

/// PG's transformInsertRow arity rule: more values than target columns is
/// always an error, fewer only with an explicit column list — without one
/// the remaining columns take their defaults (`INSERT INTO t VALUES (1)`).
fn arity_mismatch(tgt: &InsertTarget, given: usize, expected: usize) -> bool {
    given > expected || (given < expected && !tgt.col_names.is_empty())
}

/// `INSERT … VALUES (…)`: infer each value with the column's type as goal,
/// enforcing arity, NOT NULL / typmod literal checks, and the
/// generated/identity-column restrictions.
/// The rows of `INSERT … VALUES` as transformInsertStmt transforms them
/// (transformExpressionList, single row or several): `rel.*` and
/// `(expr).*` expanded into one value per column — a rule action's
/// `new.*`, a SQL function parameter's `v.*` — before rows are counted
/// against the target columns. Borrowed when no row holds a star.
fn expand_values_rows<'a>(
    val_sel: &'a ast::SelectStmt,
    snapshot: &PgCatalog,
    params: &ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<std::borrow::Cow<'a, [Option<ast::Node>]>, AnalyzeError> {
    let scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let null_ctx = NullabilityContext::default();
    let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
    let rows: Vec<Option<Vec<Option<ast::Node>>>> = val_sel
        .values_lists
        .iter()
        .map(|row| match row.as_ref() {
            Some(ast::Node::List(list)) => {
                Ok(match expr::expand_row_args(&list.items, ctx, params)? {
                    std::borrow::Cow::Owned(items) => Some(items),
                    std::borrow::Cow::Borrowed(_) => None,
                })
            }
            _ => Ok(None),
        })
        .collect::<Result<_, AnalyzeError>>()?;
    if rows.iter().all(Option::is_none) {
        return Ok(std::borrow::Cow::Borrowed(&val_sel.values_lists));
    }
    Ok(val_sel
        .values_lists
        .iter()
        .zip(rows)
        .map(|(row, items)| match items {
            Some(items) => Some(ast::Node::List(NodeBox::new(ast::List { items }))),
            None => row.clone(),
        })
        .collect())
}

fn analyze_insert_values(
    ins: &ast::InsertStmt,
    values_lists: &[Option<ast::Node>],
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<Vec<Vec<Option<ValueInfo>>>, AnalyzeError> {
    // No table in scope for VALUES, but we need scope for possible
    // subqueries/functions inside an individual value expression.
    let scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let null_ctx = NullabilityContext::default();
    let expected_len = insert_arity(tgt);

    // A row's NULL into a NOT NULL column, or one no partition takes,
    // fails every execution, but at execution: PG reports it only once the
    // whole statement has parsed, for the first failing row (see
    // [`row_violation`]).
    let mut violation: Option<RowViolation> = None;
    let mut first_len: Option<usize> = None;
    // What each row writes at each position (`None`: DEFAULT).
    let mut written: Vec<Vec<Option<ValueInfo>>> = Vec::new();
    for val_list in values_lists {
        let Some(ast::Node::List(list)) = val_list.as_ref() else {
            continue;
        };
        crate::pg_catalog::note_write_row(tgt.oid);
        let mut row_written: Vec<Option<ValueInfo>> = Vec::with_capacity(list.items.len());
        // transformInsertStmt: every row of a multi-row VALUES must be as
        // long as the first one, which already passed transformInsertRow's
        // arity check.
        match first_len {
            None => first_len = Some(list.items.len()),
            Some(n) if n != list.items.len() => {
                // PG positions it at the row's first value.
                let span = list
                    .items
                    .first()
                    .and_then(|n| crate::error::expr_span(n.as_ref()));
                return Err(crate::pgmsg::values_lists_length(n, list.items.len(), span)
                    .finalize_implicit());
            }
            Some(_) => {}
        }
        // Arity check: the VALUES row must match the declared column list
        // (or, when no column list is given, the full table width).
        if arity_mismatch(tgt, list.items.len(), expected_len) {
            // PG (SQLSTATE 42601), positioned at the first value without a
            // column, or the first column without a value.
            return Err(insert_arity_error(
                &ins.cols,
                expected_len,
                list.items.len(),
                list.items
                    .get(expected_len)
                    .and_then(|n| crate::error::expr_span(n.as_ref())),
            ));
        }
        // The row is transformed whole, then coerced to its columns
        // (transformInsertRow).
        let untyped = expr::replay_transform(
            &list.items.iter().map(Option::as_ref).collect::<Vec<_>>(),
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
        );
        for (i, val) in list.items.iter().enumerate() {
            let target_col = target_col_at(tgt, i);
            let target = target_at(tgt, i, expr::Ctx::new(&scope, &null_ctx, snapshot), params)?;
            // A value stored *inside* the column (`arr[1]`) is not subject to
            // the column-level NOT NULL / typmod checks.
            let indirected = target.as_ref().is_some_and(|t| t.indirected);
            // The matching `ResTarget` in `ins.cols` for column `i` — used to
            // build a `source_span` so a type mismatch surfaces a secondary
            // label at the column reference (not just at the value).
            let target_loc = ins.cols.get(i).and_then(|n| {
                if let Some(ast::Node::ResTarget(rt)) = n.as_ref() {
                    crate::error::SourceSpan::from_node_qname(rt.location)
                } else {
                    None
                }
            });
            if let Some(tc) = target_col
                && !indirected
                && !tgt.discards(tc)
                && let Some(err) = crate::typmod::check_literal_assignment(
                    snapshot,
                    tc.atttypid,
                    snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                    val.as_ref(),
                )
            {
                return Err(err);
            }
            let goal = match (target_col, &target) {
                (Some(tc), Some(t)) => {
                    TypeGoal::assignment(t.type_oid).with_source_column(&tc.attname)
                }
                _ => TypeGoal::NONE,
            };
            let goal = match target_loc {
                Some(s) => goal.with_source(s),
                None => goal,
            };
            let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
            // A multi-row VALUES list is a VALUES RTE, where PG forbids
            // set-returning functions; a single row is the INSERT's own
            // target list.
            if values_lists.len() > 1 {
                check_no_srf_in_clause(val.as_ref(), snapshot, "VALUES")?;
            }
            // EXPR_KIND_VALUES / EXPR_KIND_VALUES_SINGLE.
            crate::clause::check_no_aggregates_or_windows(val.as_ref(), snapshot, "VALUES")?;
            if untyped[i]
                && let Some(p) = expr::bare_param(val.as_ref(), snapshot)
                && let Some(t) = &target
            {
                expr::coerce_untyped_param(p, t.type_oid, snapshot, params)?;
            }
            let inferred = match &target {
                Some(t) => t.infer_value(val.as_ref(), goal, ctx, params)?,
                None if is_set_to_default(val.as_ref()) => {
                    expr::ExprType::scalar(oid::UNKNOWN, false)
                }
                None => expr::infer_expr(val.as_ref(), ctx, params, goal)?,
            };
            // The assignment's value type: `coerce_type` skips its domain
            // coercion when it already is the column's type.
            if !is_set_to_default(val.as_ref())
                && !indirected
                && let Some(tc) = target_col
            {
                crate::privileges::note_column_value(tgt.oid, &tc.attname, Some(inferred.type_oid));
            }
            // A parameter of a domain type is received through the domain's
            // input / receive function (domain_in / domain_recv), which
            // evaluates its checks however it is sent.
            if matches!(val.as_ref(), Some(ast::Node::ParamRef(_)))
                && snapshot
                    .get_type(inferred.type_oid)
                    .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Domain)
            {
                crate::privileges::note_domain_coercion(inferred.type_oid);
            }
            if let Some(tc) = target_col
                && !tgt.discards(tc)
            {
                note_written(
                    tgt.oid,
                    &tc.attname,
                    false,
                    indirected,
                    val.as_ref(),
                    Some(&inferred),
                );
            }
            row_written.push(match target_col {
                _ if is_set_to_default(val.as_ref()) => None,
                // A value stored inside the column says nothing of it.
                Some(_) if indirected => Some(ValueInfo::default()),
                Some(tc) => Some(ValueInfo::assigned(val.as_ref(), &inferred, tc, snapshot)),
                None => Some(ValueInfo::default()),
            });

            if let Some(ast::Node::ParamRef(p)) = val.as_ref()
                && let Some(tc) = target_col
                && (!tc.attnotnull || indirected || tgt.discards(tc))
            {
                params.infer_nullable(p.number, true);
            }
        }
        if violation.is_none() {
            let items: Vec<Option<&ast::Node>> = list.items.iter().map(Option::as_ref).collect();
            let known = |i: usize| match target_col_at(tgt, i) {
                Some(tc) if is_null_value(items[i], tc, None, snapshot) => Known::Null,
                _ => Known::Value(items[i]),
            };
            violation = row_violation(tgt, &items, &known, snapshot, tgt.oid);
        }
        written.push(row_written);
    }
    match violation {
        Some(v) => Err(v.error),
        None => Ok(written),
    }
}

/// An error a row of an INSERT raises every time: a NULL it would store
/// into a NOT NULL column or domain, a constant its column's domain
/// rejects, no partition taking it, a CHECK, partition constraint or
/// check option it fails.
struct RowViolation {
    /// When PG reports it: domains (NOT NULL, CHECKs) while the row is
    /// built, then routing (ExecFindPartition), then the columns' NOT NULL
    /// in attnum order (ExecConstraints, on the partition the row landed
    /// in) — see the constants below for the rest.
    order: (bool, i16),
    error: AnalyzeError,
}

impl RowViolation {
    /// A CHECK constraint's: after every column's NOT NULL
    /// (ExecConstraints).
    const CHECK: (bool, i16) = (true, i16::MAX - 2);
    /// A leaf partition's partition constraint: after ExecConstraints
    /// (ExecInsert's ExecPartitionCheck).
    const PARTITION: (bool, i16) = (true, i16::MAX - 1);
    /// A view's WITH CHECK OPTION: once the row is stored
    /// (ExecWithCheckOptions after ExecInsert's ExecConstraints).
    const VIEW_CHECK: (bool, i16) = (true, i16::MAX);
    /// A partitioned partition's partition constraint: before the row is
    /// routed (ExecFindPartition checks the root's first).
    const PARTITION_ROUTED: (bool, i16) = (true, i16::MIN);

    /// Column `tc`'s violation, in the order of the relation `rel` the row
    /// lands in (a partition's columns may be ordered differently).
    fn of(
        tc: &crate::pg_catalog::PgAttribute,
        snapshot: &PgCatalog,
        error: AnalyzeError,
        rel: crate::oid::PgClassOid,
    ) -> Self {
        let domain = snapshot.domain_null_violation(tc.atttypid).is_some();
        let attnum = snapshot
            .attribute_by_name(rel, &tc.attname)
            .map_or(tc.attnum, |a| a.attnum);
        Self {
            order: (!domain, attnum),
            error,
        }
    }

    /// No partition of `parent` takes the row: reported after the row is
    /// built, before any column's NOT NULL.
    fn no_partition(parent: crate::oid::PgClassOid, snapshot: &PgCatalog) -> Option<Self> {
        let relname = &snapshot.pg_class.get(&parent)?.relname;
        Some(Self {
            order: (true, i16::MIN + 1),
            error: crate::pgmsg::no_partition_for_row(relname),
        })
    }
}

/// An INSERT row's values, by position (see [`row_violation`]).
type RowItems<'a> = Vec<Option<&'a ast::Node>>;

/// What the row checks know of the value an INSERT row gives a column.
#[derive(Clone, Copy)]
enum Known<'a> {
    /// A NULL, stored as is.
    Null,
    /// This expression, stored as the column's type reads it.
    Value(Option<&'a ast::Node>),
    /// Nothing.
    Unknown,
}

/// The error inserting the row `items` (none for DEFAULT VALUES; a
/// position's value as `known` tells, `DEFAULT` aside) raises every time,
/// as PG reports it: a NOT NULL domain while the row is built (a NULL
/// given, or a default left NULL), then the routing failure when no
/// partition takes it (ExecFindPartition), then the first column in attnum
/// order — of the partition the row lands in — storing NULL into a NOT
/// NULL column (ExecConstraints), omitted columns included. None when a
/// DO INSTEAD rule replaces the INSERT.
fn row_violation<'a>(
    tgt: &InsertTarget,
    items: &[Option<&'a ast::Node>],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
    statement_target: crate::oid::PgClassOid,
) -> Option<RowViolation> {
    if returning_rewritten(snapshot, tgt.oid, &[DmlEvent::Insert]) {
        return None;
    }
    // Through an automatically updatable view, the row is stored in its
    // base relation (rewriteTargetView): checked there.
    if let Some((base, base_items, opaque)) = through_view(tgt, items, snapshot) {
        let known = |i: usize| {
            if i < items.len() && !opaque.contains(&i) {
                known(i)
            } else {
                Known::Unknown
            }
        };
        return row_violation(&base, &base_items, &known, snapshot, statement_target);
    }
    let (lands_in, unrouted) = row_relation(tgt, items, known, snapshot, statement_target);
    let mut violations: Vec<RowViolation> = unrouted.into_iter().collect();
    for (i, val) in items.iter().enumerate() {
        let Some(tc) = target_col_at(tgt, i) else {
            continue;
        };
        // A value stored *inside* the column (`arr[1]`) is not subject to
        // the column-level checks.
        if !tgt.col_indirection.get(i).is_none_or(Vec::is_empty) {
            continue;
        }
        if is_set_to_default(*val) {
            // An explicit DEFAULT that is NULL competes like an omitted
            // column.
            violations.extend(null_default_violation(tc, snapshot, &lands_in));
            continue;
        }
        if tgt.discards(tc) {
            continue;
        }
        match known(i) {
            Known::Null => {
                if let Some(err) = insert_null_error(tc, snapshot, &lands_in) {
                    violations.push(RowViolation::of(tc, snapshot, err, lands_in.0));
                }
            }
            // `NULL::d`, for a domain `d` rejecting NULL, fails as the row
            // is built, whatever the column; so does a constant coerced to
            // the column's domain when one of its CHECKs is FALSE for it.
            known @ (Known::Value(_) | Known::Unknown) => {
                let error = null_domain_cast(*val, snapshot)
                    .map(AnalyzeError::Invalid)
                    .or_else(|| match known {
                        Known::Value(v) => domain_check_violation(v, tc, snapshot),
                        _ => None,
                    });
                if let Some(error) = error {
                    let attnum = snapshot
                        .attribute_by_name(lands_in.0, &tc.attname)
                        .map_or(tc.attnum, |a| a.attnum);
                    violations.push(RowViolation {
                        order: (false, attnum),
                        error,
                    });
                }
            }
        }
    }
    // A column left to a NULL default, of a domain rejecting NULL, fails
    // while the row is built.
    violations.extend(omitted_domain_nulls(tgt, items, snapshot));
    violations.extend(omitted_not_null_columns(
        tgt,
        items.len(),
        snapshot,
        &lands_in,
    ));
    violations.extend(check_violation(
        tgt,
        items,
        known,
        snapshot,
        statement_target,
        &lands_in,
    ));
    violations.extend(partition_violation(tgt, items, known, snapshot));
    violations.into_iter().min_by_key(|v| v.order)
}

/// The INSERT `tgt` of row `items` as rewriteTargetView hands it to the
/// base relation of `tgt`, an automatically updatable view (down a chain
/// of them): its columns renamed to the base columns they are, the
/// view's defaults filling the view columns the row omits or sets to
/// DEFAULT (rewriteTargetListIU; values the analyzer doesn't follow), the
/// columns the view hides left to the base's defaults. Plus the positions
/// whose value is no longer the one `items` gives there (a view default).
/// None when `tgt` isn't such a view — or a column the row writes is not
/// updatable, which the rewriter reports first.
fn through_view<'a>(
    tgt: &InsertTarget,
    items: &[Option<&'a ast::Node>],
    snapshot: &PgCatalog,
) -> Option<(InsertTarget, RowItems<'a>, Vec<usize>)> {
    let class = snapshot.pg_class.get(&tgt.oid)?;
    if class.relkind != crate::pg_catalog::RelKind::View {
        return None;
    }
    let upd = snapshot.view_updatability.get(&tgt.oid)?;
    if upd.reason(true).is_some() {
        return None;
    }
    let base_oid = upd.base?;
    let base_class = snapshot.pg_class.get(&base_oid)?;
    let base_attrs = snapshot.attributes_of(base_oid);
    // The base column view column `c` is.
    let base_col = |c: &crate::pg_catalog::PgAttribute| -> Option<String> {
        let attnum = *upd
            .columns
            .get(usize::try_from(c.attnum - 1).ok()?)?
            .as_ref()
            .ok()?;
        Some(
            base_attrs
                .iter()
                .find(|b| b.attnum == attnum)?
                .attname
                .clone(),
        )
    };
    let mut col_names = Vec::new();
    let mut col_indirection = Vec::new();
    let mut base_items = Vec::new();
    let mut opaque = Vec::new();
    let mut given = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let tc = target_col_at(tgt, i)?;
        given.push(tc.attname.as_str());
        col_names.push(base_col(tc)?);
        col_indirection.push(tgt.col_indirection.get(i).cloned().unwrap_or_default());
        if is_set_to_default(*item) && tc.atthasdef {
            opaque.push(i);
            base_items.push(None);
        } else {
            base_items.push(*item);
        }
    }
    // The view's defaults for the columns the row omits.
    for c in tgt.attrs.iter().filter(|c| c.attnum > 0 && c.atthasdef) {
        if !given.contains(&c.attname.as_str()) {
            opaque.push(base_items.len());
            col_names.push(base_col(c)?);
            col_indirection.push(Vec::new());
            base_items.push(None);
        }
    }
    let base = InsertTarget {
        oid: base_oid,
        relname: base_class.relname.clone(),
        nsname: snapshot
            .namespace_name(base_class.relnamespace)
            .map(str::to_owned)
            .unwrap_or_default(),
        attrs: base_attrs.to_vec(),
        col_names,
        col_indirection,
        overriding: tgt.overriding,
        view_identity: view_identity_columns(snapshot, base_oid),
        on_conflict: tgt.on_conflict,
    };
    Some((base, base_items, opaque))
}

/// Whether `val`, a value of type `source` (`None`: its own) stored into
/// `column`, stores a NULL: a bare `NULL`, or a NULL under casts each
/// mapping NULL to NULL (see [`null_under_casts`]) — then coerced to
/// `source` (a VALUES column's common type) and to the column's type.
fn is_null_value(
    val: Option<&ast::Node>,
    column: &crate::pg_catalog::PgAttribute,
    source: Option<PgTypeOid>,
    snapshot: &PgCatalog,
) -> bool {
    use crate::coerce::CoercionContext;
    let Some((NullCast::Null, own)) = null_under_casts(val, snapshot) else {
        return false;
    };
    let read = match source {
        Some(s) if !null_coercion(own, s, None, CoercionContext::Implicit, snapshot) => {
            return false;
        }
        Some(s) => s,
        None => own,
    };
    null_coercion(
        read,
        column.atttypid,
        column.atttypmod,
        CoercionContext::Assignment,
        snapshot,
    )
}

/// The error a NULL cast to a domain rejecting NULL raises when evaluated
/// (`NULL::d`, also under further casts). None when the statement is not
/// run (a view, rule action, SQL function body, prepared statement or
/// plain EXPLAIN).
fn null_domain_cast(val: Option<&ast::Node>, snapshot: &PgCatalog) -> Option<String> {
    if !super::executes() {
        return None;
    }
    null_domain_cast_error(val, snapshot)
}

/// [`null_domain_cast`]'s error, whether or not the statement runs.
fn null_domain_cast_error(val: Option<&ast::Node>, snapshot: &PgCatalog) -> Option<String> {
    match null_under_casts(val, snapshot)? {
        (NullCast::Fails(msg), _) => Some(msg),
        (NullCast::Null, _) => None,
    }
}

/// The relation a VALUES row (`items`; none for DEFAULT VALUES) is checked
/// in, and its name: the partition ExecFindPartition routes it to when the
/// target is partitioned and the analyzer can tell (see
/// `partbound::route_row`), else the target itself. Plus the routing
/// failure when the analyzer can tell no partition takes the row — unless
/// a DO INSTEAD rule replaces the INSERT.
fn row_relation<'a>(
    tgt: &InsertTarget,
    items: &[Option<&'a ast::Node>],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
    statement_target: crate::oid::PgClassOid,
) -> ((crate::oid::PgClassOid, String), Option<RowViolation>) {
    let value_of = |name: &str| row_value(tgt, items, known, snapshot, name);
    use crate::ddl::tables::partbound::Route;
    let route = crate::ddl::tables::partbound::route_row(snapshot, tgt.oid, &value_of);
    // The planner prunes the result relations the rows certainly reach:
    // only their generated columns, checks and triggers are initialized.
    // The route itself is the statement's target's (the view's, when the
    // row is stored in a base relation): `expand_views` hands it to the
    // base, whose target RTE carries it.
    match route {
        Route::Leaf(leaf) => crate::privileges::note_insert_route(statement_target, leaf),
        Route::NoPartition(_) | Route::Unknown => {
            crate::privileges::note_insert_route_unknown(statement_target);
        }
    }
    let lands_in = route
        .leaf()
        .filter(|&leaf| leaf != tgt.oid)
        .and_then(|leaf| Some((leaf, snapshot.pg_class.get(&leaf)?.relname.clone())))
        .unwrap_or_else(|| (tgt.oid, tgt.relname.clone()));
    let unrouted = match route {
        Route::NoPartition(parent)
            if !returning_rewritten(snapshot, tgt.oid, &[DmlEvent::Insert]) =>
        {
            RowViolation::no_partition(parent, snapshot)
        }
        _ => None,
    };
    (lands_in, unrouted)
}

/// Column `name`'s value in the INSERT row `items` of `tgt` (see
/// [`row_violation`]): given, or its default when that is NULL
/// (`Some(None)` for NULL); `None` when unknown.
fn row_value<'a>(
    tgt: &InsertTarget,
    items: &[Option<&'a ast::Node>],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
    name: &str,
) -> Option<Option<&'a ast::Node>> {
    let c = tgt.attrs.iter().find(|c| c.attname == name)?;
    let given =
        (0..items.len()).find(|&i| target_col_at(tgt, i).is_some_and(|t| t.attname == name));
    match given {
        Some(i) if !tgt.col_indirection.get(i).is_none_or(Vec::is_empty) || tgt.discards(c) => None,
        Some(i) if !is_set_to_default(items[i]) => match known(i) {
            Known::Null => Some(None),
            Known::Value(v) => v.map(Some),
            Known::Unknown => None,
        },
        _ => null_default(c, snapshot).then_some(None),
    }
}

/// The CHECK the row `items` of `tgt` (a table, not a view) surely fails
/// once it lands in `lands_in`: the first of the relation's enforced CHECK
/// constraints, in name order, surely FALSE for it (ExecConstraints →
/// ExecRelCheck; a NULL result passes), else the first WITH CHECK OPTION
/// of the views `statement_target` writes it through, inner view first,
/// surely FALSE or NULL for it (ExecWithCheckOptions, once the row is
/// stored).
/// A column's value counts only when it is stored as written (see
/// [`known_literal`]). None when the statement isn't run, or a BEFORE
/// ROW trigger may change the row — on the table or a partition the row
/// may be routed to — or a row-level security policy may reject it first,
/// or the row's partition is unknown, or (for the check options) ON
/// CONFLICT may skip the row. (A DO INSTEAD rule or INSTEAD OF trigger
/// was ruled out by [`row_violation`].)
fn check_violation<'a>(
    tgt: &InsertTarget,
    items: &[Option<&'a ast::Node>],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
    statement_target: crate::oid::PgClassOid,
    lands_in: &(crate::oid::PgClassOid, String),
) -> Option<RowViolation> {
    use crate::nonnull::checks::{Knowledge, RelationChecks};
    use crate::nonnull::{Literal, ValPred};
    if !super::executes()
        || before_row_trigger_rewrites(snapshot, tgt.oid, DmlEvent::Insert)
        || [tgt.oid, lands_in.0]
            .iter()
            .any(|r| snapshot.row_security.contains(r))
        || snapshot
            .pg_class
            .get(&lands_in.0)
            .is_none_or(|c| c.relkind != crate::pg_catalog::RelKind::Table)
    {
        return None;
    }
    // Each column's value by name: `Some(None)` for NULL.
    let value = |name: &str| -> Option<Option<Literal>> {
        let c = tgt.attrs.iter().find(|c| c.attname == name)?;
        match row_value(tgt, items, known, snapshot, name)? {
            None => Some(None),
            Some(v) => known_literal(v, c, snapshot).map(Some),
        }
    };
    let no_preds = |_: &str| Vec::<ValPred>::new();
    // ExecConstraints checked the NOT NULL columns first.
    let lands_attrs = snapshot.attributes_of(lands_in.0);
    let non_null = |c: &str| {
        matches!(value(c), Some(Some(_)))
            || lands_attrs.iter().any(|a| a.attname == c && a.attnotnull)
    };
    let null = |c: &str| matches!(value(c), Some(None));
    let equals = |c: &str| value(c).flatten();
    let k = Knowledge {
        non_null: &non_null,
        null: &null,
        equals: &equals,
        preds: &no_preds,
    };
    let mut constraints: Vec<&crate::pg_catalog::PgConstraint> = snapshot
        .pg_constraint
        .values()
        .filter(|c| {
            c.conrelid == lands_in.0
                && c.contype == crate::pg_catalog::ConType::Check
                && c.conenforced
        })
        .collect();
    // CheckNNConstraintFetch sorts them by name (strcmp).
    constraints.sort_by(|a, b| a.conname.cmp(&b.conname));
    for con in constraints {
        let Some(def) = snapshot.check_defs.get(&con.oid) else {
            continue;
        };
        let crate::ddl::tables::check_inherit::StoredExpr::Written(expr) = &def.expr else {
            continue;
        };
        let Some(trust) = &def.trusted else {
            continue;
        };
        if RelationChecks::surely_false(snapshot, lands_in.0, (**expr).as_ref(), trust, &k) {
            return Some(RowViolation {
                order: RowViolation::CHECK,
                error: crate::pgmsg::check_constraint_violation(&lands_in.1, &con.conname),
            });
        }
    }
    if tgt.on_conflict {
        return None;
    }
    for (view, base) in insert_check_options(snapshot, statement_target) {
        // The view's WHERE reads its base relation's columns: each the
        // table's column it stores into (through the views below), if any.
        let Some(column) = stored_columns(snapshot, base, tgt.oid) else {
            continue;
        };
        let value = |c: &str| column.get(c).and_then(|t| value(t));
        let non_null = |c: &str| matches!(value(c), Some(Some(_)));
        let null = |c: &str| matches!(value(c), Some(None));
        let equals = |c: &str| value(c).flatten();
        let k = Knowledge {
            non_null: &non_null,
            null: &null,
            equals: &equals,
            preds: &no_preds,
        };
        let Some(ast::Node::SelectStmt(query)) =
            crate::ddl::views::stored_view_query(snapshot, view)
        else {
            continue;
        };
        let qual = query.where_clause.as_ref();
        let trust = crate::privileges::suspended(|| {
            crate::ddl::volatile::trusted_over_relation(snapshot, base, qual)
        });
        // ExecWithCheckOptions evaluates the qual with ExecQual: NULL
        // fails it as FALSE does.
        if let Some(trust) = trust
            && RelationChecks::surely_not_true(snapshot, base, qual, &trust, &k)
        {
            let relname = &snapshot.pg_class.get(&view)?.relname;
            return Some(RowViolation {
                order: RowViolation::VIEW_CHECK,
                error: crate::pgmsg::view_check_option_violation(relname),
            });
        }
    }
    None
}

/// The constant column `c` stores for the value `v` given it, as a CHECK
/// compares it: a literal (or one cast to the column's type, see
/// [`crate::nonnull::literal_for`]) stored as written — not into a column
/// with a type modifier (`numeric(2,-1)` rounds, `varchar(2)` drops
/// trailing spaces). A domain's CHECKs don't change the value: one that
/// rejects it fails the row first, as it is built (see
/// [`domain_check_violation`]), an error all the same.
fn known_literal(
    v: &ast::Node,
    c: &crate::pg_catalog::PgAttribute,
    snapshot: &PgCatalog,
) -> Option<crate::nonnull::Literal> {
    if snapshot.effective_typmod(c.atttypid, c.atttypmod).is_some() {
        return None;
    }
    crate::nonnull::literal_for(Some(v), c.atttypid, snapshot)
}

/// The error storing the value `v` into column `tc` raises every time: a
/// bare literal coerced to the column's domain, one of whose CHECKs is
/// FALSE for it (ExecEvalConstraintCheck, while the row is built — before
/// any trigger), or an `ARRAY[…]` of constants coerced to an array of a
/// domain, one of which it rejects (see
/// [`super::failing_rows::array_elements_domain_error`]). None when the
/// statement isn't run.
fn domain_check_violation(
    v: Option<&ast::Node>,
    tc: &crate::pg_catalog::PgAttribute,
    snapshot: &PgCatalog,
) -> Option<AnalyzeError> {
    if !super::executes() {
        return None;
    }
    if let Some(ast::Node::AArrayExpr(arr)) = v {
        return super::failing_rows::array_elements_domain_error(arr, tc.atttypid, snapshot)
            .map(AnalyzeError::Invalid);
    }
    let literal = crate::nonnull::literal(v)?;
    snapshot
        .domain_value_violation_at_execution(tc.atttypid, &literal)
        .map(AnalyzeError::CheckViolation)
}

/// The partition constraint the row `items` of `tgt` surely fails when
/// `tgt` is a partition the row is inserted into directly: the row isn't
/// one its own bound and its ancestors' take (RelationGetPartitionQual) —
/// routed from the root, it reaches a relation that is neither `tgt` nor
/// below it (see `partbound::route_row`). `new row for relation "…"
/// violates partition constraint`: for a leaf, after its CHECKs (ExecInsert's
/// ExecPartitionCheck, which a BEFORE ROW trigger on it runs before, and
/// a row-level security policy's checks too); for a partitioned table,
/// before the row is routed (ExecFindPartition), whatever the triggers.
/// None when the statement isn't run, or the route is unknown.
fn partition_violation<'a>(
    tgt: &InsertTarget,
    items: &[Option<&'a ast::Node>],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
) -> Option<RowViolation> {
    use crate::ddl::tables::partbound::{Route, route_row};
    use crate::nonnull::checks::partition_parent;
    use crate::pg_catalog::RelKind;
    if !super::executes() {
        return None;
    }
    let mut root = partition_parent(snapshot, tgt.oid)?;
    let order = match snapshot.pg_class.get(&tgt.oid)?.relkind {
        RelKind::Partitioned => RowViolation::PARTITION_ROUTED,
        RelKind::Table
            if !before_row_trigger_rewrites(snapshot, tgt.oid, DmlEvent::Insert)
                && !snapshot.row_security.contains(&tgt.oid) =>
        {
            RowViolation::PARTITION
        }
        _ => return None,
    };
    for _ in 0..64 {
        match partition_parent(snapshot, root) {
            Some(p) => root = p,
            None => break,
        }
    }
    let value_of = |name: &str| row_value(tgt, items, known, snapshot, name);
    let mut reached = match route_row(snapshot, root, &value_of) {
        Route::Leaf(rel) | Route::NoPartition(rel) => rel,
        Route::Unknown => return None,
    };
    for _ in 0..64 {
        if reached == tgt.oid {
            return None;
        }
        match partition_parent(snapshot, reached) {
            Some(p) => reached = p,
            None => break,
        }
    }
    Some(RowViolation {
        order,
        error: crate::pgmsg::partition_constraint_violation(&tgt.relname),
    })
}

/// The views whose WITH CHECK OPTION an INSERT into view `top` checks
/// (rewriteTargetView, down the chain of automatically updatable views),
/// each with its base relation, in the order ExecWithCheckOptions checks
/// them: inner views first. A view checks when it has a check option, or
/// a CASCADED one checks above it; one without a WHERE checks nothing,
/// but its own CASCADED option still reaches the views below.
fn insert_check_options(
    snapshot: &PgCatalog,
    top: crate::oid::PgClassOid,
) -> Vec<(crate::oid::PgClassOid, crate::oid::PgClassOid)> {
    let mut out = Vec::new();
    // The `cascaded` flag of the check option added last (the head of
    // withCheckOptions).
    let mut head_cascaded = false;
    let mut cur = top;
    for _ in 0..16 {
        let Some(class) = snapshot.pg_class.get(&cur) else {
            break;
        };
        if class.relkind != crate::pg_catalog::RelKind::View {
            break;
        }
        let Some(base) = snapshot.view_updatability.get(&cur).and_then(|u| u.base) else {
            break;
        };
        let has_wco = snapshot.check_option_views.contains(&cur) || head_cascaded;
        let cascaded = snapshot.cascaded_check_option_views.contains(&cur) || head_cascaded;
        let has_quals = matches!(
            crate::ddl::views::stored_view_query(snapshot, cur),
            Some(ast::Node::SelectStmt(q)) if q.where_clause.is_some()
        );
        if has_wco && (cascaded || has_quals) {
            head_cascaded = cascaded;
            if has_quals {
                out.push((cur, base));
            }
        }
        cur = base;
    }
    out.reverse();
    out
}

/// The column of table `table` each column of relation `rel` (the table
/// itself, or an automatically updatable view down to it) stores into,
/// by name; a view column computed by an expression stores into none.
fn stored_columns(
    snapshot: &PgCatalog,
    rel: crate::oid::PgClassOid,
    table: crate::oid::PgClassOid,
) -> Option<HashMap<String, String>> {
    let attrs = snapshot.attributes_of(rel);
    if rel == table {
        return Some(
            attrs
                .iter()
                .map(|a| (a.attname.clone(), a.attname.clone()))
                .collect(),
        );
    }
    let upd = snapshot.view_updatability.get(&rel)?;
    let base = upd.base?;
    let below = stored_columns(snapshot, base, table)?;
    let base_attrs = snapshot.attributes_of(base);
    Some(
        attrs
            .iter()
            .zip(&upd.columns)
            .filter_map(|(a, col)| {
                let attnum = *col.as_ref().ok()?;
                let base_att = base_attrs.iter().find(|b| b.attnum == attnum)?;
                Some((a.attname.clone(), below.get(&base_att.attname)?.clone()))
            })
            .collect(),
    )
}

/// The error the planner raises folding column `c`'s default (of table
/// `relid`) into a statement writing it: a constant out of its column's
/// range (`int2 DEFAULT 40000`) or failing itself (`DEFAULT 1 / 0`). PG
/// stores such a default — only a statement using it fails.
pub(crate) fn default_fold_error(
    c: &crate::pg_catalog::PgAttribute,
    snapshot: &PgCatalog,
) -> Option<AnalyzeError> {
    if !crate::expr::plan_time_checks() || c.attgenerated.is_some() || c.attidentity.is_some() {
        return None;
    }
    let Some(crate::ddl::tables::check_inherit::StoredExpr::Written(e)) =
        snapshot.attr_default_exprs.get(&(c.attrelid, c.attnum))
    else {
        return None;
    };
    // (The default isn't in the statement's text: no location.)
    if let Some(err) = crate::fold::error((**e).as_ref(), snapshot) {
        return crate::expr::plan_time_error(None, err.raw(None).finalize_implicit());
    }
    crate::typmod::assignment_error(
        snapshot,
        c.atttypid,
        snapshot.effective_typmod(c.atttypid, c.atttypmod),
        (**e).as_ref(),
        None,
        || None,
    )
}

/// Whether column `c`'s default is NULL: no identity or generation
/// expression, and either a stored DEFAULT that evaluates to NULL (see
/// [`stored_default`]) or no DEFAULT at all, nor one its domain gives it.
pub(crate) fn null_default(c: &crate::pg_catalog::PgAttribute, snapshot: &PgCatalog) -> bool {
    c.attidentity.is_none()
        && c.attgenerated.is_none()
        && if c.atthasdef {
            matches!(stored_default(c, snapshot), Some(NullCast::Null))
        } else {
            !snapshot.domain_has_default(c.atttypid)
        }
}

/// What evaluating a NULL under casts gives, when the analyzer can tell.
#[derive(Debug, PartialEq, Eq)]
enum NullCast {
    /// NULL: every cast maps NULL to NULL.
    Null,
    /// The error a cast to a domain rejecting NULL raises (CoerceToDomain
    /// is checked at execution, never folded away).
    Fails(String),
}

/// What column `c`'s stored DEFAULT evaluates to, when it is a NULL under
/// casts (`None` otherwise, or when a cast may turn the NULL into a
/// value). A default cooking to a bare null constant is never stored
/// (AddRelationNewConstraints), but on a domain column it cooks to a
/// CoerceToDomain over the null constant, which PG keeps — overriding the
/// domain's own default — and a typmod's length coercion or a cast to
/// another type cooks to a function call, kept too. The column's own
/// domain is left to the caller (`Null` then means NULL before it).
fn stored_default(c: &crate::pg_catalog::PgAttribute, snapshot: &PgCatalog) -> Option<NullCast> {
    let Some(crate::ddl::tables::check_inherit::StoredExpr::Written(e)) =
        snapshot.attr_default_exprs.get(&(c.attrelid, c.attnum))
    else {
        return None;
    };
    match null_under_casts((**e).as_ref(), snapshot)? {
        // cookDefault's coerce_to_target_type, to the column's type.
        (NullCast::Null, t) => null_coercion(
            t,
            c.atttypid,
            c.atttypmod,
            crate::coerce::CoercionContext::Assignment,
            snapshot,
        )
        .then_some(NullCast::Null),
        (fails, _) => Some(fails),
    }
}

/// What `e` evaluates to when it is `NULL` under casts (`NULL::int8`,
/// `CAST(NULL::int8 AS text)`), with the type of the result: `None` when
/// it is something else, or when a cast may turn the NULL into a value.
fn null_under_casts(e: Option<&ast::Node>, snapshot: &PgCatalog) -> Option<(NullCast, PgTypeOid)> {
    match e? {
        ast::Node::AConst(k) if k.isnull => Some((NullCast::Null, oid::UNKNOWN)),
        ast::Node::TypeCast(tc) => {
            let (inner, from) = null_under_casts(tc.arg.as_ref(), snapshot)?;
            let tn = tc.type_name.as_ref()?;
            let target = crate::ddl::util::resolve_type_name(tn, snapshot)?;
            if inner != NullCast::Null {
                return Some((inner, target));
            }
            let typmod = crate::typmod::encode(snapshot, target, tn).ok()?;
            if !null_coercion(
                from,
                target,
                typmod,
                crate::coerce::CoercionContext::Explicit,
                snapshot,
            ) {
                return None;
            }
            let result = match snapshot.domain_null_violation_at_execution(target) {
                Some(msg) => NullCast::Fails(msg),
                None => NullCast::Null,
            };
            Some((result, target))
        }
        _ => None,
    }
}

/// Whether `coerce_to_target_type` of a NULL of type `from` to `to` /
/// `typmod` yields NULL, `to`'s domain checks aside: an unknown-type
/// literal becomes a null Const; a binary-coercible cast is a RelabelType;
/// an ArrayCoerceExpr and a strict cast function skip a NULL input; an
/// I/O conversion calls the target's input function on NULL only if it is
/// not strict — none built in, but a user's base type may have one — and
/// a typmod's length coercion is a cast function too. A user's cast
/// function needn't be strict, and a non-strict one can return a value.
fn null_coercion(
    from: PgTypeOid,
    to: PgTypeOid,
    typmod: Option<i32>,
    context: crate::coerce::CoercionContext,
    snapshot: &PgCatalog,
) -> bool {
    use crate::coerce::CoercionPath;
    let base = snapshot.unwrap_domain(to);
    let cast_func_strict = |source: PgTypeOid, target: PgTypeOid| {
        snapshot
            .cast_by_pair
            .get(&(source, target))
            .and_then(|oid| snapshot.pg_cast.get(oid))
            .and_then(|c| c.castfunc)
            .is_none_or(|f| snapshot.pg_proc.get(&f).is_some_and(|p| p.proisstrict))
    };
    let converted = from == oid::UNKNOWN
        || match crate::coerce::coercion_pathway(to, from, context, snapshot) {
            Some(CoercionPath::Relabel | CoercionPath::ArrayCoerce) => true,
            Some(CoercionPath::Func) => cast_func_strict(snapshot.unwrap_domain(from), base),
            Some(CoercionPath::CoerceViaIo) => snapshot.get_type(base).is_some_and(|t| {
                !matches!(t.typtype, TypType::Base | TypType::Pseudo)
                    || snapshot.namespace_name(t.typnamespace) == Some("pg_catalog")
            }),
            None => false,
        };
    converted && (typmod.is_none_or(|m| m < 0) || cast_func_strict(base, base))
}

/// The columns a VALUES row (`items`; none for DEFAULT VALUES) leaves to
/// their default — omitted, or `DEFAULT` — when that is NULL and of a
/// domain rejecting NULL (NOT NULL, or a CHECK a NULL fails), or when it
/// casts a NULL to such a domain: the NULL is coerced to the domain as the
/// row is built (`expand_targetlist`), before any trigger. A table's own
/// columns only (a view's defaults are its base table's).
fn omitted_domain_nulls(
    tgt: &InsertTarget,
    items: &[Option<&ast::Node>],
    snapshot: &PgCatalog,
) -> Vec<RowViolation> {
    let is_table = snapshot.pg_class.get(&tgt.oid).is_some_and(|c| {
        matches!(
            c.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        )
    });
    if !is_table || !super::executes() {
        return Vec::new();
    }
    let given: Vec<&str> = items
        .iter()
        .enumerate()
        .filter(|&(_, &v)| !is_set_to_default(v))
        .filter_map(|(i, _)| target_col_at(tgt, i).map(|c| c.attname.as_str()))
        .collect();
    tgt.attrs
        .iter()
        .filter(|c| c.attnum > 0 && !given.contains(&c.attname.as_str()))
        .filter(|c| c.attidentity.is_none() && c.attgenerated.is_none())
        .filter_map(|c| {
            // A DEFAULT casting its NULL to such a domain fails the same way.
            let msg = match c.atthasdef.then(|| stored_default(c, snapshot)).flatten() {
                Some(NullCast::Fails(msg)) => msg,
                _ if null_default(c, snapshot) => {
                    snapshot.domain_null_violation_at_execution(c.atttypid)?
                }
                _ => return None,
            };
            Some(RowViolation {
                order: (false, c.attnum),
                error: AnalyzeError::Invalid(msg),
            })
        })
        .collect()
}

/// NOT NULL column `c` storing its default, when that is NULL (see
/// [`null_default`]).
fn null_default_violation(
    c: &crate::pg_catalog::PgAttribute,
    snapshot: &PgCatalog,
    lands_in: &(crate::oid::PgClassOid, String),
) -> Option<RowViolation> {
    if !(c.attnum > 0 && null_default(c, snapshot)) {
        return None;
    }
    insert_null_error(c, snapshot, lands_in).map(|e| RowViolation::of(c, snapshot, e, lands_in.0))
}

/// The error storing NULL into target column `c` raises once the row
/// lands in `lands_in` (see [`null_assignment_error`]): the NOT NULL may
/// be the partition's own (`PARTITION OF … (b NOT NULL)`), checked there.
pub(super) fn insert_null_error(
    c: &crate::pg_catalog::PgAttribute,
    snapshot: &PgCatalog,
    lands_in: &(crate::oid::PgClassOid, String),
) -> Option<AnalyzeError> {
    null_assignment_error(c, snapshot, &lands_in.1, "insert").or_else(|| {
        let leaf = snapshot.attribute_by_name(lands_in.0, &c.attname)?;
        // A BEFORE ROW trigger on the target may replace the NULL too.
        (lands_in.0 != c.attrelid
            && leaf.attnotnull
            && !before_row_trigger_rewrites(snapshot, c.attrelid, DmlEvent::Insert))
        .then(|| null_assignment_error(leaf, snapshot, &lands_in.1, "insert"))
        .flatten()
    })
}

/// The NOT NULL columns an INSERT row giving `given` values leaves to a
/// NULL default.
fn omitted_not_null_columns(
    tgt: &InsertTarget,
    given: usize,
    snapshot: &PgCatalog,
    lands_in: &(crate::oid::PgClassOid, String),
) -> Vec<RowViolation> {
    let provided: Vec<&str> = (0..given)
        .filter_map(|i| target_col_at(tgt, i).map(|c| c.attname.as_str()))
        .collect();
    tgt.attrs
        .iter()
        .filter(|c| !provided.contains(&c.attname.as_str()))
        .filter_map(|c| null_default_violation(c, snapshot, lands_in))
        .collect()
}

/// transformInsertRow's arity error for `expressions` values against
/// `targets` target columns (`cols`, the explicit column list, maybe
/// empty). `extra_value` is the span of the first value past the targets.
fn insert_arity_error(
    cols: &[Option<ast::Node>],
    targets: usize,
    expressions: usize,
    extra_value: Option<crate::error::SourceSpan>,
) -> AnalyzeError {
    if expressions > targets {
        crate::pgmsg::insert_more_expressions_than_targets(targets, expressions, extra_value)
    } else {
        let column = cols.get(expressions).and_then(|n| match n.as_ref() {
            Some(ast::Node::ResTarget(rt)) => {
                crate::error::SourceSpan::from_node_qname(rt.location)
            }
            _ => None,
        });
        crate::pgmsg::insert_more_targets_than_expressions(targets, expressions, column)
    }
    .finalize_implicit()
}

/// `INSERT … SELECT …`: enforce arity, reject GENERATED ALWAYS identity
/// targets (a SELECT can't supply `DEFAULT`), walk the SELECT so its params
/// register and typos propagate, then pin column types onto bare `$N`
/// projections.
fn analyze_insert_select(
    val_sel: &ast::SelectStmt,
    cols: &[Option<ast::Node>],
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(Vec<ValueInfo>, Vec<PgTypeOid>), AnalyzeError> {
    // Walk the SELECT side of `INSERT … SELECT` so its params are registered
    // and any undefined-column / typo errors inside the SELECT propagate
    // cleanly. PG (transformInsertStmt) analyzes the SELECT first and only
    // then compares its *output* width — after `*` expansion and set
    // operations — with the target list.
    // (Its outputs are coerced to the target columns, not resolved first.)
    let (sel_cols, _) = super::select::keeping_unknown_outputs(|| {
        analyze_select_with_ctes(val_sel, snapshot, params, cte_scopes)
    })?;
    let expected_len = insert_arity(tgt);
    if arity_mismatch(tgt, sel_cols.len(), expected_len) {
        return Err(insert_arity_error(
            cols,
            expected_len,
            sel_cols.len(),
            super::set_ops::branch_column_span(val_sel, expected_len),
        ));
    }
    // The SELECT's target entries line up with its output columns only for
    // a plain SELECT without `*`.
    let direct_targets: &[Option<ast::Node>] = if val_sel.op == SetOperation::SetopNone
        && val_sel.values_lists.is_empty()
        && !val_sel.target_list.iter().any(|t| {
            matches!(t.as_ref(), Some(ast::Node::ResTarget(rt))
                if matches!(rt.val.as_ref(),
                    Some(ast::Node::ColumnRef(cr)) if cr.fields.iter().any(|f|
                        matches!(f.as_ref(), Some(ast::Node::AStar(_))))))
        }) {
        &val_sel.target_list
    } else {
        &[]
    };
    // Each SELECT output column must be assignment-coercible to its target
    // column — PG rejects `INSERT INTO t (int8_col) SELECT jsonb_col …` at
    // parse time with `column "X" is of type Y but expression is of type Z`.
    // Untyped string literals in the projection surface as `text` from the
    // target-list boundary; PG instead coerces them through the target's
    // input function, so for those we validate the literal *content* (and
    // accept) rather than comparing the placeholder text type.
    let empty_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let empty_null = NullabilityContext::default();
    let mut target_types: Vec<Option<PgTypeOid>> = Vec::with_capacity(sel_cols.len());
    for i in 0..sel_cols.len() {
        let t = target_at(
            tgt,
            i,
            expr::Ctx::new(&empty_scope, &empty_null, snapshot),
            params,
        )?;
        target_types.push(t.map(|t| t.type_oid));
    }
    for (i, sel_col) in sel_cols.iter().enumerate() {
        let (Some(tc), Some(Some(target_oid))) = (target_col_at(tgt, i), target_types.get(i))
        else {
            continue;
        };
        let target_oid = *target_oid;
        // A bare `$N` output PG transformed while `$N` was untyped is still
        // `unknown` here and is coerced to the column's type through
        // `variable_coerce_param_hook` (42P08 if WHERE & co. deduced
        // another type — see `expr::note_untyped_output_params`).
        if let Some(ast::Node::ResTarget(rt)) = direct_targets.get(i).and_then(|t| t.as_ref())
            && let Some(p) = expr::bare_param(rt.val.as_ref(), snapshot)
            && params.is_untyped_output(p.location)
        {
            if let Err(prev) = params.coerce_untyped(p.number, target_oid) {
                return Err(expr::inconsistent_param_error(
                    p.number, prev, target_oid, p.location, snapshot,
                ));
            }
            continue;
        }
        // A Var of the SELECT's `unknown` output that is no literal: only
        // the I/O conversion to a string type assigns it.
        if sel_col.unknown_value {
            let node = match direct_targets.get(i).and_then(|t| t.as_ref()) {
                Some(ast::Node::ResTarget(rt)) => rt.val.as_ref(),
                _ => None,
            };
            expr::unknown_value_coercion(
                target_oid,
                crate::coerce::CoercionContext::Assignment,
                snapshot,
                node,
            )?;
            continue;
        }
        if sel_col.type_oid == oid::UNKNOWN || sel_col.type_oid == target_oid {
            continue;
        }
        let literal = direct_targets.get(i).and_then(|t| {
            if let Some(ast::Node::ResTarget(rt)) = t.as_ref()
                && let Some(ast::Node::AConst(ac)) = rt.val.as_ref()
                && !ac.isnull
                && let Some(typedpg_pg_query::ast::AConstValue::String(sv)) = &ac.val
            {
                Some(sv.sval.as_str())
            } else {
                None
            }
        });
        if let Some(text) = literal {
            // Read with the column's typmod, as coerce_type does.
            let typmod = (tc.atttypid == target_oid)
                .then(|| snapshot.effective_typmod(tc.atttypid, tc.atttypmod))
                .flatten();
            let checked = if tc.atttypid == target_oid {
                crate::literal_input::validate_with_typmod(text, target_oid, typmod, snapshot)
            } else {
                crate::literal_input::validate(text, target_oid, snapshot)
            };
            if let Err(msg) = checked {
                return Err(crate::error::RawError::invalid_literal(msg, None).finalize_implicit());
            }
            continue;
        }
        if let Some(ast::Node::ResTarget(rt)) = direct_targets.get(i).and_then(|t| t.as_ref()) {
            crate::type_facts::note_conversion(
                rt.val.as_ref(),
                sel_col.type_oid,
                target_oid,
                false,
                snapshot,
            );
        }
        // The SELECT's outputs reach the INSERT as Vars of its subquery:
        // a `record` one is no row coerce_record_to_complex can take apart
        // (not a ROW(...) nor a whole-row reference), whatever it holds.
        if sel_col.type_oid == oid::RECORD && crate::coerce::is_complex(target_oid, snapshot) {
            return Err(crate::pgmsg::cannot_cast_record_to(
                &crate::ddl::util::format_type_for_message(snapshot, target_oid),
                None,
                None,
            )
            .finalize_implicit());
        }
        if !crate::coerce::can_coerce(
            sel_col.type_oid,
            target_oid,
            crate::coerce::CoercionContext::Assignment,
            snapshot,
        ) {
            let expected = crate::ddl::util::format_type_for_message(snapshot, target_oid);
            let actual = crate::ddl::util::format_type_for_message(snapshot, sel_col.type_oid);
            return Err(crate::error::RawError::invalid(
                format!(
                    "column \"{}\" is of type {expected} but expression is of type {actual}",
                    tc.attname
                ),
                None,
                Some(format!(
                    "cast the SELECT expression, e.g. `expr::{expected}`"
                )),
            )
            .finalize_implicit());
        }
    }

    // A SELECT the planner pulls up into the INSERT (`is_simple_subquery`)
    // has its outputs substituted into the INSERT's target list, where
    // their conversion to the columns' types is folded like a VALUES row's.
    if super::deferred::pulled_up(val_sel) {
        for (i, target) in direct_targets.iter().enumerate() {
            if let Some(ast::Node::ResTarget(rt)) = target.as_ref()
                && let val @ Some(_) = &rt.val
                && let Some(tc) = target_col_at(tgt, i)
                && tgt.col_indirection.get(i).is_none_or(|ind| ind.is_empty())
                && !tgt.discards(tc)
                && let Some(err) = crate::typmod::check_literal_assignment(
                    snapshot,
                    tc.atttypid,
                    snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                    val.as_ref(),
                )
            {
                return Err(err);
            }
        }
    }

    // The expressions each output column takes its value from: the select
    // list's entries, or — a VALUES source with ORDER BY / LIMIT & co. —
    // every row's value at that position.
    let mut outputs: Vec<(usize, Option<&ast::Node>)> = direct_targets
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t.as_ref() {
            Some(ast::Node::ResTarget(rt)) => rt.val.is_some().then_some((i, rt.val.as_ref())),
            _ => None,
        })
        .collect();
    if val_sel.op == SetOperation::SetopNone {
        for row in &val_sel.values_lists {
            if let Some(ast::Node::List(list)) = row.as_ref() {
                outputs.extend(list.items.iter().map(Option::as_ref).enumerate());
            }
        }
    }
    for (i, val) in outputs {
        if let Some(ast::Node::ParamRef(p)) = val
            && let Some(tc) = target_col_at(tgt, i)
            && let Some(Some(target_oid)) = target_types.get(i)
        {
            if params.get(p.number) == oid::UNKNOWN {
                params.record(p.number, *target_oid);
            }
            let indirected = tgt
                .col_indirection
                .get(i)
                .is_some_and(|ind| !ind.is_empty());
            if !tc.attnotnull || indirected || tgt.discards(tc) {
                params.infer_nullable(p.number, true);
            }
        }
    }
    crate::pg_catalog::note_write_row(tgt.oid);
    for (i, c) in sel_cols.iter().enumerate() {
        let Some(tc) = target_col_at(tgt, i) else {
            continue;
        };
        if tgt.discards(tc) {
            continue;
        }
        let indirected = tgt
            .col_indirection
            .get(i)
            .is_some_and(|ind| !ind.is_empty());
        let val = match direct_targets.get(i).and_then(|t| t.as_ref()) {
            Some(ast::Node::ResTarget(rt)) => rt.val.as_ref(),
            _ => None,
        };
        let value = expr::ExprType {
            typmod: c.typmod,
            refine: c.refine.clone(),
            ..expr::ExprType::scalar(c.type_oid, c.nullable)
        };
        // A bare parameter is one only as the SELECT's direct output.
        let val = val.filter(|v| matches!(v, ast::Node::ParamRef(_)));
        note_written(tgt.oid, &tc.attname, false, indirected, val, Some(&value));
    }
    // What each output column writes, once coerced to its column's type
    // (nothing known of a value stored inside its column).
    let written = sel_cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let indirected = tgt
                .col_indirection
                .get(i)
                .is_some_and(|ind| !ind.is_empty());
            let nullable = match target_types.get(i) {
                Some(Some(target)) => expr::assignment_nullable(
                    &expr::ExprType::scalar(c.type_oid, c.nullable),
                    *target,
                    snapshot,
                ),
                _ => c.nullable,
            };
            ValueInfo {
                not_null: !nullable && !indirected,
                ..ValueInfo::default()
            }
        })
        .collect();
    Ok((written, sel_cols.iter().map(|c| c.type_oid).collect()))
}

/// The error every execution of an INSERT whose source is a VALUES with
/// ORDER BY / LIMIT & co. (`column_types`: its output columns' types)
/// raises for a row it stores (see [`row_violation`]) — where the analyzer
/// can tell which rows reach the INSERT, and which fails first. Without
/// ORDER BY the rows come in order, those OFFSET / LIMIT keep; with one
/// their order is unknown, so it takes every row failing alike: any
/// failing when all of them are stored, all of them when the sort decides
/// which are. A value is known only when the VALUES column has the target
/// column's type: another one is converted on the way.
fn general_values_violation(
    val_sel: &ast::SelectStmt,
    tgt: &InsertTarget,
    column_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<AnalyzeError> {
    /// A constant non-negative count (`None`: a NULL / ALL one, which
    /// doesn't count); `Err` when it isn't one.
    fn count(n: Option<&ast::Node>) -> Result<Option<usize>, ()> {
        use typedpg_pg_query::ast::{self, AConstValue};
        match n {
            None => Ok(None),
            Some(ast::Node::AConst(c)) if c.isnull => Ok(None),
            Some(ast::Node::AConst(c)) => match &c.val {
                Some(AConstValue::Integer(i)) => usize::try_from(i.ival).map(Some).map_err(|_| ()),
                _ => Err(()),
            },
            _ => Err(()),
        }
    }
    let rows: Vec<Vec<Option<&ast::Node>>> = val_sel
        .values_lists
        .iter()
        .filter_map(|l| match l.as_ref() {
            Some(ast::Node::List(list)) => Some(list.items.iter().map(Option::as_ref).collect()),
            _ => None,
        })
        .collect();
    let offset = count(val_sel.limit_offset.as_ref()).ok()?.unwrap_or(0);
    let limit = count(val_sel.limit_count.as_ref())
        .ok()?
        .unwrap_or(usize::MAX);
    let violations: Vec<Option<RowViolation>> = rows
        .iter()
        .map(|items| {
            let known = |i: usize| match (target_col_at(tgt, i), column_types.get(i)) {
                (Some(tc), Some(&t)) if t == snapshot.unwrap_domain(tc.atttypid) => {
                    if is_null_value(items[i], tc, Some(t), snapshot) {
                        Known::Null
                    } else {
                        Known::Value(items[i])
                    }
                }
                _ => Known::Unknown,
            };
            row_violation(tgt, items, &known, snapshot, tgt.oid)
        })
        .collect();
    let stored = rows.len().saturating_sub(offset).min(limit);
    if val_sel.sort_clause.is_empty() {
        return violations
            .into_iter()
            .skip(offset)
            .take(stored)
            .flatten()
            .next()
            .map(|v| v.error);
    }
    let all_stored = stored == rows.len();
    let fails = if all_stored {
        violations.iter().any(Option::is_some)
    } else {
        stored > 0 && violations.iter().all(Option::is_some)
    };
    let mut failing = violations.into_iter().flatten();
    let first = failing.next().filter(|_| fails)?;
    let message = first.error.to_string();
    failing
        .all(|v| v.error.to_string() == message)
        .then_some(first.error)
}

/// `ON CONFLICT (…) DO UPDATE SET …` / `DO NOTHING`, following
/// `transformOnConflictClause`: the arbiter specification is transformed
/// against the target alone (EXCLUDED is in the range table but not yet
/// referencable), then DO UPDATE's SET list and WHERE see a virtual
/// `EXCLUDED` relation holding the proposed row. We model it in scope as a
/// second alias over the target table: the columns share names and types,
/// and nullability follows the real columns because PG rejects an INSERT
/// that violates NOT NULL before the conflict handler runs.
///
/// Returns the transformed arbiter for the planner-stage check
/// ([`validate_on_conflict_target`]), `None` without an inference clause,
/// and the DO UPDATE SET list as [`analyze_set_clause`] expanded it.
fn analyze_insert_on_conflict(
    on_conflict: &ast::OnConflictClause,
    relation: &ast::RangeVar,
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(Option<Arbiter>, Vec<Option<ast::Node>>), AnalyzeError> {
    let update = on_conflict.action == ast::OnConflictAction::OnconflictUpdate;
    // DO SELECT (PostgreSQL 19) sees EXCLUDED like DO UPDATE, without a SET
    // list.
    let select = on_conflict.action == ast::OnConflictAction::OnconflictSelect;
    let target_qn = crate::qualified_name::QualifiedName::new(&tgt.nsname, &tgt.relname);
    let mut target_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    target_scope.add_dml_target(
        snapshot,
        insert_target_alias(relation),
        target_qn.clone(),
        &tgt.attrs,
    );
    let mut excluded_holder = Scope::default();
    excluded_holder.add_dml_target(snapshot, "excluded", target_qn, &tgt.attrs);
    // transformOnConflictClause marks the EXCLUDED RTE as
    // RELKIND_COMPOSITE_TYPE, so scanNSItemForColumn offers it no system
    // columns: `excluded.ctid` does not exist. Its perminfo is still the
    // target relation's: reading `excluded.a` needs SELECT on `a`.
    for s in &mut excluded_holder.sources {
        s.system_columns.clear();
        s.dml_target = false;
    }

    // transformOnConflictArbiter.
    if (update || select) && on_conflict.infer.is_none() {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError(format!(
                "ON CONFLICT DO {} requires inference specification or constraint name",
                if update { "UPDATE" } else { "SELECT" }
            )),
            None,
            Some("For example, ON CONFLICT (column_name).".into()),
        )
        .finalize_implicit());
    }
    let arbiter = match on_conflict.infer.as_deref() {
        None => None,
        Some(infer) => {
            let mut arbiter_scope = target_scope.clone();
            if update || select {
                arbiter_scope
                    .shadowed_sources
                    .extend(excluded_holder.sources.iter().cloned());
            }
            Some(transform_on_conflict_arbiter(
                infer,
                insert_target_alias(relation),
                tgt,
                expr::Ctx::new(&arbiter_scope, &NullabilityContext::default(), snapshot),
                params,
            )?)
        }
    };

    if !update && !select {
        return Ok((arbiter, Vec::new()));
    }
    // transformOnConflictClause builds the EXCLUDED pseudo-relation's
    // target list (exclRelTlist) of a Var per column of the target: a
    // stored INSERT ... ON CONFLICT DO UPDATE / SELECT depends on every
    // column.
    for a in tgt.attrs.iter().filter(|a| a.attnum > 0) {
        crate::ddl::depend::note_column(tgt.oid, &a.attname);
    }
    if select {
        // transformInsertStmt: DO SELECT FOR UPDATE / SHARE locks the row
        // it returns, which requires UPDATE on the target. It fires no
        // UPDATE trigger; the target's SELECT policies (and, locking, its
        // UPDATE ones) check the conflicting row.
        let locks = on_conflict.lock_strength != ast::LockClauseStrength::LcsNone;
        if locks {
            crate::privileges::note_relation(tgt.oid, crate::ddl::acl::bits::UPDATE);
        }
        crate::privileges::note_conflict_select(tgt.oid, locks);
        let mut conflict_scope = target_scope;
        conflict_scope.sources.extend(excluded_holder.sources);
        if let where_clause @ Some(_) = &on_conflict.where_clause {
            let conflict_null_ctx = NullabilityContext::default();
            // transformOnConflictClause: transformWhereClause, coerced as
            // WHERE (coerce_to_boolean).
            crate::clause::coerce_clause_expr(
                where_clause.as_ref(),
                expr::Ctx::new(&conflict_scope, &conflict_null_ctx, snapshot),
                params,
                crate::clause::ClauseKind::Where,
            )?;
            crate::clause::check_no_aggregates_or_windows(
                where_clause.as_ref(),
                snapshot,
                "WHERE",
            )?;
            check_no_srf_in_clause(where_clause.as_ref(), snapshot, "WHERE")?;
        }
        return Ok((arbiter, Vec::new()));
    }
    // DO UPDATE requires UPDATE of the columns its SET list writes, and
    // fires the UPDATE triggers. Its auxiliary UPDATE also applies the
    // target's UPDATE policies (`root->onConflict->action`).
    crate::privileges::note_relation(tgt.oid, crate::ddl::acl::bits::UPDATE);
    crate::privileges::note_conflict_update(tgt.oid);
    crate::privileges::note_write(tgt.oid, crate::ddl::triggers::TRIGGER_TYPE_UPDATE, false);
    for t in &on_conflict.target_list {
        if let Some(ast::Node::ResTarget(rt)) = t.as_ref() {
            crate::privileges::note_column(
                tgt.oid,
                &rt.name,
                crate::privileges::ColumnAccess::Update,
            );
        }
    }
    let mut conflict_scope = target_scope;
    conflict_scope.sources.extend(excluded_holder.sources);
    let conflict_null_ctx = NullabilityContext::default();
    let set_list = analyze_set_clause(
        &on_conflict.target_list,
        &tgt.attrs,
        &tgt.relname,
        expr::Ctx::new(&conflict_scope, &conflict_null_ctx, snapshot),
        params,
        false,
    )?
    .into_owned();
    if let where_clause @ Some(_) = &on_conflict.where_clause {
        // transformOnConflictClause: transformWhereClause, coerced as WHERE
        // (coerce_to_boolean).
        crate::clause::coerce_clause_expr(
            where_clause.as_ref(),
            expr::Ctx::new(&conflict_scope, &conflict_null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::Where,
        )?;
        crate::clause::check_no_aggregates_or_windows(where_clause.as_ref(), snapshot, "WHERE")?;
        check_no_srf_in_clause(where_clause.as_ref(), snapshot, "WHERE")?;
    }
    Ok((arbiter, set_list))
}

/// PG's `transformOnConflictArbiter` / `resolve_unique_index_expr`: each
/// inference element is a column or an expression transformed as an index
/// expression (EXPR_KIND_INDEX_EXPRESSION) against the target, without
/// ordering options and with an existing collation / btree operator class;
/// the WHERE is transformed as an index predicate; a named constraint must
/// exist on the table.
fn transform_on_conflict_arbiter(
    infer: &ast::InferClause,
    target_alias: &str,
    tgt: &InsertTarget,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Arbiter, AnalyzeError> {
    let snapshot = ctx.snapshot;
    let mut arbiter = Arbiter {
        cols: Default::default(),
        exprs: Vec::new(),
        whole_row: false,
        where_clause: None,
        qualified: Vec::new(),
    };
    let invalid = |msg: &str| {
        crate::error::RawError::new(AnalyzeError::InvalidColumnReference(msg.into()), None, None)
            .finalize_implicit()
    };
    for elem in &infer.index_elems {
        let Some(ast::Node::IndexElem(ie)) = elem.as_ref() else {
            continue;
        };
        if ie.ordering != ast::SortByDir::SortbyDefault {
            return Err(invalid("ASC/DESC is not allowed in ON CONFLICT clause"));
        }
        if ie.nulls_ordering != ast::SortByNulls::SortbyNullsDefault {
            return Err(invalid(
                "NULLS FIRST/LAST is not allowed in ON CONFLICT clause",
            ));
        }
        let mut elem_attnum = None;
        if !ie.name.is_empty() {
            // A plain column becomes a ColumnRef transformed like any
            // other: a user or system column, else a whole-row reference to
            // the target, else an unknown column.
            if let Some(a) = tgt.attrs.iter().find(|a| a.attname == ie.name) {
                arbiter.cols.insert(a.attnum);
                elem_attnum = Some(a.attnum);
                // The inference element is a Var of the column, read.
                crate::ddl::depend::note_column(tgt.oid, &a.attname);
                crate::privileges::note_target_column_by_name(tgt.oid, &a.attname, false);
            } else if let Some(&(_, _, attnum)) = crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .find(|(n, ..)| *n == ie.name)
            {
                arbiter.cols.insert(attnum);
                elem_attnum = Some(attnum);
            } else if ie.name == target_alias {
                arbiter.whole_row = true;
            } else {
                ctx.scope.resolve_column(None, &ie.name, None)?;
            }
        } else if let e @ Some(_) = &ie.expr {
            check_index_expr_kind(
                e.as_ref(),
                snapshot,
                "index expressions",
                "index expression",
            )?;
            expr::infer_expr(e.as_ref(), ctx, params, TypeGoal::NONE)?;
            check_index_expr_kind(
                e.as_ref(),
                snapshot,
                "index expressions",
                "index expression",
            )?;
            arbiter.exprs.push(e.clone());
        }
        let mut collation = None;
        let mut opclass = None;
        if !ie.collation.is_empty() {
            let parts = expr::extract_string_fields(&ie.collation);
            let (schema, name) = match parts.as_slice() {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => (None, ""),
            };
            match snapshot.resolve_collation(schema, name) {
                Some(c) => collation = Some(c.oid),
                None => return Err(crate::pgmsg::collation_does_not_exist(&parts.join("."))),
            }
        }
        if !ie.opclass.is_empty() {
            let parts = expr::extract_string_fields(&ie.opclass);
            let (schema, name) = match parts.as_slice() {
                [n] => (None, n.as_str()),
                [s, n] => (Some(s.as_str()), n.as_str()),
                _ => (None, ""),
            };
            match crate::ddl::opclass::find_opclass(snapshot, schema, name, "btree") {
                Some(c) => opclass = Some(c.oid),
                None => {
                    return Err(crate::error::RawError::new(
                        AnalyzeError::UndefinedObject(format!(
                            "operator class \"{name}\" does not exist for access method \
                             \"btree\""
                        )),
                        None,
                        None,
                    )
                    .finalize_implicit());
                }
            }
        }
        // infer_arbiter_indexes matches these against the index's
        // indcollation / indclass.
        if collation.is_some() || opclass.is_some() {
            arbiter.qualified.push(crate::resolve::ArbiterElem {
                attnum: elem_attnum,
                expr: ie.expr.as_ref().cloned(),
                collation,
                opclass,
            });
        }
    }
    if let w @ Some(_) = &infer.where_clause {
        check_index_expr_kind(w.as_ref(), snapshot, "index predicates", "index predicate")?;
        expr::infer_expr(w.as_ref(), ctx, params, TypeGoal::NONE)?;
        check_index_expr_kind(w.as_ref(), snapshot, "index predicates", "index predicate")?;
        arbiter.where_clause = w.clone();
    }
    if !infer.conname.is_empty() {
        let constraint = snapshot
            .pg_constraint_values()
            .find(|c| c.conrelid == tgt.oid && c.conname == infer.conname);
        if let Some(c) = constraint {
            // OnConflictExpr.constraint: a stored INSERT depends on it.
            crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::new(
                crate::ddl::depend::PG_CONSTRAINT_RELID,
                c.oid.into_nonzero(),
                0,
            ));
            // transformOnConflictArbiter marks the constrained columns as
            // requiring SELECT, as the inference column list does.
            crate::privileges::note_relation(tgt.oid, crate::ddl::acl::bits::SELECT);
            for attnum in &c.conkey {
                if let Some(a) = tgt.attrs.iter().find(|a| a.attnum == *attnum) {
                    crate::privileges::note_target_column_by_name(tgt.oid, &a.attname, false);
                }
            }
            return Ok(arbiter);
        }
        return Err(crate::error::RawError::new(
            AnalyzeError::UndefinedObject(format!(
                "constraint \"{}\" for table \"{}\" does not exist",
                infer.conname, tgt.relname,
            )),
            None,
            None,
        )
        .finalize_implicit());
    }
    Ok(arbiter)
}

/// The constructs an index expression / predicate forbids
/// (EXPR_KIND_INDEX_EXPRESSION / EXPR_KIND_INDEX_PREDICATE): a sub-select
/// (transformSubLink, 0A000), aggregates and window functions, and
/// set-returning functions.
fn check_index_expr_kind(
    e: Option<&ast::Node>,
    snapshot: &PgCatalog,
    kind: &str,
    subquery_context: &str,
) -> Result<(), AnalyzeError> {
    let mut has_sublink = false;
    visit_same_level(e, &mut |n| {
        has_sublink |= matches!(n, Some(ast::Node::SubLink(_)));
    });
    if has_sublink {
        return Err(crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(format!("cannot use subquery in {subquery_context}")),
            None,
            None,
        )
        .finalize_implicit());
    }
    crate::clause::check_no_aggregates_or_windows(e, snapshot, kind)?;
    check_no_srf_in_clause(e, snapshot, kind)
}

pub(crate) fn analyze_update(
    upd: &ast::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_update_with_outer_ctes(upd, snapshot, params, &HashMap::new())
}

/// [`analyze_update`] for an UPDATE that sees the CTEs of an enclosing
/// `WITH` (a data-modifying CTE body).
pub(crate) fn analyze_update_with_outer_ctes(
    upd: &ast::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    if replaced_by_instead_rule(upd.relation.as_deref(), CmdType::CmdUpdate, snapshot) {
        crate::expr::replaced_by_rule(|| analyze_update_here(upd, snapshot, params, outer_ctes))
    } else {
        analyze_update_here(upd, snapshot, params, outer_ctes)
    }
}

/// An UPDATE's or DELETE's FROM items: its target `relation`, then those
/// of its FROM / USING clause `from`.
fn with_target(
    relation: Option<&ast::RangeVar>,
    from: &[Option<ast::Node>],
) -> Vec<Option<ast::Node>> {
    relation
        .map(|rv| Some(ast::Node::RangeVar(NodeBox::new(rv.clone()))))
        .into_iter()
        .chain(from.iter().cloned())
        .collect()
}

fn analyze_update_here(
    upd: &ast::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    // The target is one of the level's relations.
    let from = with_target(upd.relation.as_deref(), &upd.from_clause);
    let _plan = expr::plan_fold::PlanLevel::enter_dml(
        upd.where_clause.as_ref(),
        &from,
        upd.relation.as_deref(),
        upd.with_clause.as_deref(),
        typedpg_pg_query::NodeRef::UpdateStmt(upd),
        outer_ctes.keys(),
        snapshot,
    );
    let relation = upd
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UPDATE without relation".into()))?;
    super::from::check_rangevar_catalog(relation)?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| {
            crate::scope::undefined_table_error(
                snapshot,
                if relation.schemaname.is_empty() {
                    None
                } else {
                    Some(relation.schemaname.as_str())
                },
                &relation.relname,
                crate::error::SourceSpan::from_node_qname(relation.location),
            )
        })?;
    crate::scope::check_relation_opens(table)?;

    let table_oid = table.oid;
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table_oid).to_vec();
    // The columns the UPDATE writes.
    let _write = crate::pg_catalog::note_write_statement(
        crate::condition::WriteCommand::Update,
        table_oid,
        || write_base(snapshot, table_oid, &[DmlEvent::Update], false),
    );
    crate::pg_catalog::note_relation_use(table_oid, 'w');
    crate::pg_catalog::note_target_relation(relation, table_oid);
    crate::privileges::note_relation(table_oid, crate::ddl::acl::bits::UPDATE);
    crate::privileges::note_write(
        table_oid,
        crate::ddl::triggers::TRIGGER_TYPE_UPDATE,
        !relation.inh,
    );
    for target in &upd.target_list {
        if let Some(ast::Node::ResTarget(rt)) = target.as_ref() {
            crate::ddl::depend::note_column(table_oid, &rt.name);
            crate::privileges::note_column(
                table_oid,
                &rt.name,
                crate::privileges::ColumnAccess::Update,
            );
            // `SET c = DEFAULT` runs the column's default.
            if is_set_to_default(rt.val.as_ref())
                && let Some(a) = table_attrs.iter().find(|a| a.attname == rt.name)
            {
                crate::privileges::note_default(table_oid, a.attnum);
            }
        }
    }

    // Walk `UPDATE … WITH (cte) …` so parameters inside the CTE are seen by
    // the collector and the CTE alias is visible to the FROM clause. Same
    // reasoning as the corresponding block in `analyze_insert`.
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &upd.with_clause {
        let owner = typedpg_pg_query::NodeRef::UpdateStmt(upd);
        cte_scopes = super::cte::with_statement_owner(with, owner, snapshot, || {
            analyze_with_clause(with, snapshot, params, &cte_scopes, &[])
        })?;
    }
    check_current_of_on_view(upd.where_clause.as_ref(), snapshot, table_oid)?;

    // Build scope with target table + FROM clause tables.
    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let mut null_ctx = NullabilityContext::default();
    let alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname);
    scope.add_dml_target(
        snapshot,
        alias,
        crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname),
        &table_attrs,
    );
    if let Some(src) = scope.sources.last_mut() {
        src.only = !relation.inh;
    }

    // Process FROM clause (UPDATE ... FROM ... WHERE ...). transformUpdateStmt
    // marks the target `p_lateral_only` without `p_lateral_ok` meanwhile:
    // a LATERAL item there sees it but may not reference it.
    process_from_clause_beside_target(
        &upd.from_clause,
        alias,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &cte_scopes,
        params,
    )?;

    // transformUpdateStmt transforms WHERE and RETURNING before the SET
    // list: the first of them to type a `$N` fixes its type.
    let set_null_ctx = null_ctx.clone();

    // WHERE — BOOL goal with assignment coercion.
    let mut rows = ReturningRows::default();
    let mut old_facts: Option<crate::nonnull::Facts> = None;
    if let where_clause @ Some(_) = &upd.where_clause {
        let log = crate::nonnull::StrictLog::default();
        // `estimate_expression_value` reduces a pseudoconstant side of a
        // restriction while estimating it.
        let where_null_ctx = with_target_checks(&null_ctx, &scope, alias, table_oid, snapshot);
        expr::plan_fold::with_estimated_qual(where_clause.as_ref(), &scope, snapshot, || {
            crate::clause::coerce_clause_expr(
                where_clause.as_ref(),
                expr::Ctx::new(
                    &scope,
                    where_null_ctx.as_ref().unwrap_or(&null_ctx),
                    snapshot,
                )
                .logging_strictness(&log),
                params,
                crate::clause::ClauseKind::Where,
            )
        })?;
        check_no_srf_in_clause(where_clause.as_ref(), snapshot, "WHERE")?;
        // The FROM entries keep what WHERE saw. The target's new row keeps
        // it only in the columns nothing rewrites: not SET, not generated
        // (recomputed), with no BEFORE ROW trigger, rule or row movement
        // between WHERE and RETURNING. The old row keeps all of it.
        // Only the assigned columns' names are read: the SET list's row
        // expansion ([`analyze_set_clause`], after WHERE) leaves them as is.
        let assigned: std::collections::HashSet<String> = set_list_assigns(&upd.target_list)
            .into_iter()
            .map(|a| a.column)
            .collect();
        let keeps = update_keeps_values(snapshot, table_oid);
        let old_as_is = rows_returned_as_is(snapshot, table_oid);
        let (facts, all) = where_facts(
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            where_clause.as_ref(),
            &log,
            alias,
            &mut rows,
            old_as_is,
            |col| {
                keeps
                    && !assigned.contains(col)
                    && table_attrs
                        .iter()
                        .any(|a| a.attname == col && a.attgenerated.is_none())
            },
        );
        null_ctx.add_where_facts(facts);
        let plan_rels = crate::nonnull::plan_strict::qual_rels(
            where_clause.as_ref(),
            &crate::nonnull::plan_strict::ScopeVars {
                scope: &scope,
                from: upd.from_clause.iter().flatten().collect(),
                null_ctx: &null_ctx,
                log: &log,
                snapshot,
            },
            &crate::nonnull::plan_strict::Strictness {
                log: Some(&log),
                snapshot,
            },
        );
        null_ctx.add_plan_quals(plan_rels);
        old_facts = Some(all);
    }
    // The planner's: a FULL JOIN in FROM / USING no merge or hash join
    // implements — raised once the statement is parsed and rewritten (and
    // its constants folded), so after those errors.
    let full_joins = super::select::check_full_joins(&null_ctx, None, snapshot);

    let ret_scope = with_written_target(
        &scope,
        alias,
        snapshot,
        table_oid,
        &table_attrs,
        &[DmlEvent::Update],
    );
    super::returning::type_returning_params(
        upd.returning_clause.as_deref(),
        alias,
        expr::Ctx::new(&ret_scope, &null_ctx, snapshot),
        params,
    )?;

    // SET col = expr / (a, b) = (…) / col[i] = expr — assignment context.
    let set_list = match analyze_set_clause(
        &upd.target_list,
        &table_attrs,
        &table_relname,
        expr::Ctx::new(&scope, &set_null_ctx, snapshot),
        params,
        true,
    ) {
        Ok(set_list) => set_list,
        Err(e) => {
            // A NULL literal written into a NOT NULL column is the error
            // its execution raises: the planner's come first.
            let mut scratch = params.clone();
            if analyze_set_clause(
                &upd.target_list,
                &table_attrs,
                &table_relname,
                expr::Ctx::new(&scope, &set_null_ctx, snapshot),
                &mut scratch,
                false,
            )
            .is_ok()
            {
                full_joins?;
            }
            return Err(e);
        }
    };

    // The old row is a stored row past WHERE; the new one, with no BEFORE
    // ROW trigger or row movement, that row with the SET values (computed
    // from the old row) and the generated columns recomputed.
    if has_returning(upd.returning_clause.as_deref())
        && let Some(wt) = WriteTarget::resolve(snapshot, table_oid)
    {
        let old_k = old_row_knowledge(&wt, old_facts.as_ref(), alias);
        let old_nn = wt.row_not_null(snapshot, &old_k);
        let new_k = if wt.update_keeps(snapshot) {
            let mut set_ctx = null_ctx.clone();
            if let Some(all) = &old_facts {
                set_ctx.add_where_facts(all.clone());
            }
            let set = set_values(
                &set_list,
                &wt,
                &table_attrs,
                expr::Ctx::new(&scope, &set_ctx, snapshot),
                params,
            );
            let mut k = old_k;
            for a in snapshot.attributes_of(wt.base) {
                if a.attgenerated.is_some() {
                    k.write(&a.attname, &ValueInfo::default());
                }
            }
            for a in set_list_assigns(&set_list) {
                if let Some(b) = wt.to_base.get(&a.column) {
                    k.write(b, &set.get(&a.column).cloned().unwrap_or_default());
                }
            }
            k
        } else {
            RowKnowledge::default()
        };
        let new_nn = wt.row_not_null(snapshot, &new_k);
        prove_columns(&mut null_ctx, alias, &new_nn);
        rows.old_proven.extend(old_nn);
        rows.new_proven.extend(new_nn);
    }

    let columns = resolve_returning(
        upd.returning_clause.as_deref(),
        alias,
        rows,
        expr::Ctx::new(&ret_scope, &null_ctx, snapshot),
        params,
    )?;
    let mut rw = Rewrite::single(
        DmlEvent::Update,
        set_list_assigns(&set_list),
        Overriding::NotSet,
    );
    rw.returning = has_returning(upd.returning_clause.as_deref());
    check_rewrite(snapshot, table_oid, &rw)?;
    full_joins?;
    Ok((columns, None))
}

pub(crate) fn analyze_delete(
    del: &ast::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_delete_with_outer_ctes(del, snapshot, params, &HashMap::new())
}

/// [`analyze_delete`] for a DELETE that sees the CTEs of an enclosing
/// `WITH` (a data-modifying CTE body).
pub(crate) fn analyze_delete_with_outer_ctes(
    del: &ast::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    if replaced_by_instead_rule(del.relation.as_deref(), CmdType::CmdDelete, snapshot) {
        crate::expr::replaced_by_rule(|| analyze_delete_here(del, snapshot, params, outer_ctes))
    } else {
        analyze_delete_here(del, snapshot, params, outer_ctes)
    }
}

fn analyze_delete_here(
    del: &ast::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    // The target is one of the level's relations.
    let from = with_target(del.relation.as_deref(), &del.using_clause);
    let _plan = expr::plan_fold::PlanLevel::enter_dml(
        del.where_clause.as_ref(),
        &from,
        del.relation.as_deref(),
        del.with_clause.as_deref(),
        typedpg_pg_query::NodeRef::DeleteStmt(del),
        outer_ctes.keys(),
        snapshot,
    );
    let relation = del
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("DELETE without relation".into()))?;
    super::from::check_rangevar_catalog(relation)?;

    let table = snapshot
        .resolve_table(
            if relation.schemaname.is_empty() {
                None
            } else {
                Some(&relation.schemaname)
            },
            &relation.relname,
        )
        .ok_or_else(|| {
            crate::scope::undefined_table_error(
                snapshot,
                if relation.schemaname.is_empty() {
                    None
                } else {
                    Some(relation.schemaname.as_str())
                },
                &relation.relname,
                crate::error::SourceSpan::from_node_qname(relation.location),
            )
        })?;
    crate::scope::check_relation_opens(table)?;

    let table_oid = table.oid;
    // transformDeleteStmt: the target requires DELETE.
    crate::pg_catalog::note_relation_use(table_oid, 'd');
    crate::pg_catalog::note_target_relation(relation, table_oid);
    crate::privileges::note_relation(table_oid, crate::ddl::acl::bits::DELETE);
    crate::privileges::note_write(
        table_oid,
        crate::ddl::triggers::TRIGGER_TYPE_DELETE,
        !relation.inh,
    );
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table.oid).to_vec();

    // Walk `DELETE … WITH (cte) …` so parameters inside the CTE register
    // with the collector and the CTE alias is visible to the USING clause.
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &del.with_clause {
        let owner = typedpg_pg_query::NodeRef::DeleteStmt(del);
        cte_scopes = super::cte::with_statement_owner(with, owner, snapshot, || {
            analyze_with_clause(with, snapshot, params, &cte_scopes, &[])
        })?;
    }
    check_current_of_on_view(del.where_clause.as_ref(), snapshot, table_oid)?;

    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let mut null_ctx = NullabilityContext::default();
    let alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname);
    scope.add_dml_target(
        snapshot,
        alias,
        crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname),
        &table_attrs,
    );
    if let Some(src) = scope.sources.last_mut() {
        src.only = !relation.inh;
    }

    // `DELETE … USING t1, t2 …` is UPDATE's FROM: extra joinable sources
    // visible to WHERE and RETURNING.
    process_from_clause_beside_target(
        &del.using_clause,
        alias,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &cte_scopes,
        params,
    )?;

    // A deleted row has no new version.
    let mut rows = ReturningRows {
        old_may_be_null: false,
        new_may_be_null: true,
        ..ReturningRows::default()
    };
    // WHERE — BOOL goal with assignment coercion.
    let mut old_facts: Option<crate::nonnull::Facts> = None;
    if let where_clause @ Some(_) = &del.where_clause {
        let log = crate::nonnull::StrictLog::default();
        let where_null_ctx = with_target_checks(&null_ctx, &scope, alias, table_oid, snapshot);
        expr::plan_fold::with_estimated_qual(where_clause.as_ref(), &scope, snapshot, || {
            crate::clause::coerce_clause_expr(
                where_clause.as_ref(),
                expr::Ctx::new(
                    &scope,
                    where_null_ctx.as_ref().unwrap_or(&null_ctx),
                    snapshot,
                )
                .logging_strictness(&log),
                params,
                crate::clause::ClauseKind::Where,
            )
        })?;
        check_no_srf_in_clause(where_clause.as_ref(), snapshot, "WHERE")?;
        // RETURNING reads the deleted row as WHERE saw it, unless a rule
        // rewrites the statement or the target is a view.
        let keeps = rows_returned_as_is(snapshot, table_oid);
        let (facts, all) = where_facts(
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            where_clause.as_ref(),
            &log,
            alias,
            &mut rows,
            keeps,
            |_| keeps,
        );
        null_ctx.add_where_facts(facts);
        let plan_rels = crate::nonnull::plan_strict::qual_rels(
            where_clause.as_ref(),
            &crate::nonnull::plan_strict::ScopeVars {
                scope: &scope,
                from: del.using_clause.iter().flatten().collect(),
                null_ctx: &null_ctx,
                log: &log,
                snapshot,
            },
            &crate::nonnull::plan_strict::Strictness {
                log: Some(&log),
                snapshot,
            },
        );
        null_ctx.add_plan_quals(plan_rels);
        old_facts = Some(all);
    }
    // The planner's: a FULL JOIN in FROM / USING no merge or hash join
    // implements — raised once the statement is parsed and rewritten (and
    // its constants folded), so after those errors.
    let full_joins = super::select::check_full_joins(&null_ctx, None, snapshot);
    // A deleted row is a stored row past WHERE: its CHECK constraints and
    // generated columns hold too.
    if has_returning(del.returning_clause.as_deref())
        && let Some(wt) = WriteTarget::resolve(snapshot, table_oid)
    {
        let old_nn = wt.row_not_null(snapshot, &old_row_knowledge(&wt, old_facts.as_ref(), alias));
        prove_columns(&mut null_ctx, alias, &old_nn);
        rows.old_proven.extend(old_nn);
    }
    // A DO INSTEAD rule (or an INSTEAD OF trigger) returns rows of its own.
    if has_returning(del.returning_clause.as_deref())
        && returning_rewritten(snapshot, table_oid, &[DmlEvent::Delete])
        && let Some(target) = scope.sources.iter_mut().find(|s| s.alias == alias)
    {
        for c in &mut target.columns {
            c.base_not_null = false;
        }
    }

    let columns = resolve_returning(
        del.returning_clause.as_deref(),
        alias,
        rows,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
    let mut rw = Rewrite::single(DmlEvent::Delete, Vec::new(), Overriding::NotSet);
    rw.returning = has_returning(del.returning_clause.as_deref());
    check_rewrite(snapshot, table_oid, &rw)?;
    full_joins?;
    Ok((columns, None))
}

/// The target's attributes as RETURNING reads them in the rows an INSERT,
/// UPDATE or MERGE (doing `events`) writes. A table's are its own. A row
/// written through a view is not a row of the view: it need not pass the
/// view's WHERE (no CHECK OPTION is assumed), nor that of a view below it,
/// and the foreign keys of the row being written promise nothing yet (its
/// parent may be one the statement's snapshot doesn't see) — so a view
/// column is NOT NULL there only when the view's expressions are over any
/// row written to the relation below it ([`with_written_relation`]). An
/// INSTEAD OF trigger or a DO INSTEAD rule, on the target or down its view
/// chain, returns what it likes ([`returning_rewritten`]): nothing is NOT
/// NULL then.
pub(crate) fn written_row_attrs(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    attrs: &[crate::pg_catalog::PgAttribute],
    events: &[DmlEvent],
) -> Vec<crate::pg_catalog::PgAttribute> {
    let is_view = snapshot
        .pg_class
        .get(&relid)
        .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::View);
    let rewritten = returning_rewritten(snapshot, relid, events)
        || (is_view
            && (snapshot.rules.get(&relid).is_some_and(|r| !r.is_empty())
                || snapshot
                    .triggers
                    .get(&relid)
                    .is_some_and(|ts| ts.iter().any(|t| t.instead_row_events != 0))));
    if !is_view && !rewritten {
        return attrs.to_vec();
    }
    let not_null: Option<Vec<bool>> = if rewritten {
        None
    } else {
        let base = snapshot.view_updatability.get(&relid).and_then(|u| u.base);
        crate::nonnull::without_narrowing(|| {
            with_written_relation(base, events, || {
                crate::ddl::views::reanalyze_view(snapshot, relid)
            })
        })
        .map(|cols| cols.iter().map(|c| !c.nullable).collect())
    };
    attrs
        .iter()
        .enumerate()
        .map(|(i, a)| crate::pg_catalog::PgAttribute {
            attnotnull: a.attnotnull
                && not_null
                    .as_ref()
                    .is_some_and(|nn| nn.get(i).copied().unwrap_or(false)),
            ..a.clone()
        })
        .collect()
}

/// `scope` with the target entry `alias`'s columns as RETURNING reads
/// them in the rows written through relation `relid` by `events`
/// ([`written_row_attrs`]).
pub(crate) fn with_written_target(
    scope: &Scope,
    alias: &str,
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    attrs: &[crate::pg_catalog::PgAttribute],
    events: &[DmlEvent],
) -> Scope {
    let rewritten = returning_rewritten(snapshot, relid, events);
    let written = written_row_attrs(snapshot, relid, attrs, events);
    let mut scope = scope.clone();
    if let Some(target) = scope.sources.iter_mut().find(|s| s.alias == alias) {
        for c in &mut target.columns {
            if let Some(a) = written.iter().find(|a| a.attname == c.name)
                && (rewritten || !snapshot.attr_never_null(a))
            {
                c.base_not_null = false;
            }
        }
    }
    scope
}

/// Whether RETURNING reads something else than the rows `events` write
/// into `relid`: a DO INSTEAD rule for one of them on `relid` or on a
/// relation down its view chain makes it its actions' RETURNING list
/// (over another relation, and NEW / OLD as the statement gave them), and
/// an INSTEAD OF trigger on a view there returns the row it likes
/// (RewriteQuery, rewriteTargetView).
pub(crate) fn returning_rewritten(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    events: &[DmlEvent],
) -> bool {
    let mut cur = relid;
    for _ in 0..16 {
        let Some(class) = snapshot.pg_class.get(&cur) else {
            return false;
        };
        if snapshot.rules.get(&cur).is_some_and(|rs| {
            rs.iter()
                .any(|r| r.instead && events.iter().any(|e| r.event == e.cmd_type()))
        }) {
            return true;
        }
        if class.relkind != crate::pg_catalog::RelKind::View {
            return false;
        }
        if snapshot.triggers.get(&cur).is_some_and(|ts| {
            ts.iter()
                .any(|t| events.iter().any(|e| t.is_instead_row_for(e.trigger_bit())))
        }) {
            return true;
        }
        match snapshot.view_updatability.get(&cur).and_then(|u| u.base) {
            Some(base) => cur = base,
            None => return false,
        }
    }
    true
}

thread_local! {
    /// The relation a view body being re-read for the rows written
    /// through the view stores them in, and the events writing them (see
    /// [`with_written_relation`]).
    static WRITTEN_RELATION: std::cell::RefCell<Option<(crate::oid::PgClassOid, Vec<DmlEvent>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` — the analysis of an automatically updatable view's body for
/// the rows written through it — with its FROM entry over `base` read as
/// the row being written rather than a stored one: a view's columns as
/// [`written_row_attrs`] has them (not narrowed by its WHERE), a table's
/// with no [`crate::scope::Origin`], so no foreign key of the row vouches
/// for its parent.
pub(crate) fn with_written_relation<R>(
    base: Option<crate::oid::PgClassOid>,
    events: &[DmlEvent],
    f: impl FnOnce() -> R,
) -> R {
    let before = WRITTEN_RELATION.with(|w| w.replace(base.map(|b| (b, events.to_vec()))));
    let out = f();
    WRITTEN_RELATION.with(|w| *w.borrow_mut() = before);
    out
}

/// The events writing the rows of `relid`, when the FROM entry being
/// added over it is the written row of [`with_written_relation`] — the
/// first one over it, the view body's single FROM item.
pub(crate) fn take_written_relation(relid: crate::oid::PgClassOid) -> Option<Vec<DmlEvent>> {
    WRITTEN_RELATION.with(|w| {
        let mut w = w.borrow_mut();
        match w.as_ref() {
            Some((r, _)) if *r == relid => w.take().map(|(_, e)| e),
            _ => None,
        }
    })
}

/// What an UPDATE's / DELETE's WHERE proves non-NULL for RETURNING: the
/// facts about the FROM / USING entries, and those about the target's
/// columns `target_keeps` says the returned row still holds (and so NEW,
/// `rows.new_proven`). Every target fact holds for OLD (`rows.old_proven`)
/// when it is read as is (`old_as_is`). Also returns every fact, as they
/// hold of the old row.
fn where_facts(
    ctx: expr::Ctx<'_>,
    where_clause: Option<&ast::Node>,
    log: &crate::nonnull::StrictLog,
    target_alias: &str,
    rows: &mut ReturningRows,
    old_as_is: bool,
    target_keeps: impl Fn(&str) -> bool,
) -> (crate::nonnull::Facts, crate::nonnull::Facts) {
    if let Some(qual) = where_clause {
        crate::nonnull::note_settled_null_tests(qual, ctx.scope, log, ctx.snapshot);
    }
    let mut all = crate::nonnull::nonnullable(where_clause, true, ctx.scope, log, ctx.snapshot)
        .restricted_to(&crate::nonnull::own_aliases(ctx.scope));
    // RETURNING may read the row as rewritten (SET, triggers): no
    // expression facts.
    all.exprs.clear();
    let mut facts = all.clone();
    if old_as_is {
        rows.old_proven = facts
            .columns
            .iter()
            .filter(|(a, _)| a == target_alias)
            .map(|(_, c)| c.clone())
            .collect();
    }
    // Nothing the WHERE says of a column the new row doesn't keep holds
    // for it: not that it is non-NULL, NULL or some constant, nor that it
    // is one of several non-NULL ones.
    let kept = |(a, c): &(String, String)| a != target_alias || target_keeps(c);
    facts.columns.retain(kept);
    facts.nulls.retain(kept);
    facts.equals.retain(|c, _| kept(c));
    facts.preds.retain(|(c, _)| kept(c));
    facts.same.retain(|(a, b)| kept(a) && kept(b));
    facts.disjunctions.retain(|d| d.iter().all(kept));
    rows.new_proven = facts
        .columns
        .iter()
        .filter(|(a, _)| a == target_alias)
        .map(|(_, c)| c.clone())
        .collect();
    (facts, all)
}

/// What an UPDATE's or DELETE's RETURNING knows of the old rows of `wt`
/// past WHERE facts `old` (every fact the WHERE proves, `None` without
/// WHERE): by base column.
pub(crate) fn old_row_knowledge(
    wt: &WriteTarget,
    old: Option<&crate::nonnull::Facts>,
    alias: &str,
) -> RowKnowledge {
    match old {
        Some(f) => RowKnowledge::from_facts(f, alias, |c| wt.to_base.get(c).cloned()),
        None => RowKnowledge::default(),
    }
}

/// Whether nothing between an UPDATE's WHERE and its RETURNING rewrites
/// the values of the columns it doesn't SET: the target is a plain table
/// (no view, no partitions or inheritance children to move rows into),
/// with no BEFORE ROW UPDATE trigger (which may change NEW) and no rule.
pub(crate) fn update_keeps_values(snapshot: &PgCatalog, relid: crate::oid::PgClassOid) -> bool {
    let before_row_update = |t: &crate::ddl::triggers::Trigger| {
        t.row
            && t.timing & crate::ddl::triggers::TRIGGER_TYPE_BEFORE != 0
            && t.events & crate::ddl::triggers::TRIGGER_TYPE_UPDATE != 0
    };
    snapshot
        .pg_class
        .get(&relid)
        .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::Table)
        && !snapshot.pg_inherits.iter().any(|i| i.inhparent == relid)
        && !snapshot.rules.get(&relid).is_some_and(|r| !r.is_empty())
        && !snapshot
            .triggers
            .get(&relid)
            .is_some_and(|ts| ts.iter().any(before_row_update))
}

/// Whether RETURNING reads the old rows (OLD, a DELETE's deleted ones) as
/// the WHERE saw them: the target is a table (partitioned or not; a
/// trigger can't change OLD, a BEFORE DELETE one can only skip a row) and
/// no rule rewrites the statement.
pub(crate) fn rows_returned_as_is(snapshot: &PgCatalog, relid: crate::oid::PgClassOid) -> bool {
    snapshot.pg_class.get(&relid).is_some_and(|c| {
        matches!(
            c.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        )
    }) && !snapshot.rules.get(&relid).is_some_and(|r| !r.is_empty())
}

/// UPDATE's FROM / DELETE's USING list, processed while the target entry
/// `target_alias` is LATERAL-only and not LATERAL-ok (transformUpdateStmt /
/// transformDeleteStmt): a LATERAL subquery or a FROM function referencing
/// it is `invalid reference to FROM-clause entry` (42P10).
fn process_from_clause_beside_target(
    from_clause: &[Option<ast::Node>],
    target_alias: &str,
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let newly_blocked = scope
        .lateral_blocked_aliases
        .insert(target_alias.to_owned());
    let out = process_from_clause(from_clause, scope, null_ctx, snapshot, cte_scopes, params);
    if newly_blocked {
        scope.lateral_blocked_aliases.remove(target_alias);
    }
    out
}

/// transformUpdateStmt / transformDeleteStmt: `WHERE CURRENT OF` can't
/// target a view (0A000).
fn check_current_of_on_view(
    where_clause: Option<&ast::Node>,
    snapshot: &PgCatalog,
    table_oid: crate::oid::PgClassOid,
) -> Result<(), AnalyzeError> {
    if matches!(where_clause, Some(ast::Node::CurrentOfExpr(_)))
        && snapshot
            .pg_class
            .get(&table_oid)
            .is_some_and(|c| c.relkind == crate::pg_catalog::RelKind::View)
    {
        return Err(crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(
                "WHERE CURRENT OF on a view is not implemented".into(),
            ),
            None,
            None,
        )
        .finalize_implicit());
    }
    Ok(())
}

/// For the lint: the error storing the constant `value` into column
/// `column` of relation `relid` by an INSERT (`update` false) or an UPDATE
/// raises for every row written — a domain CHECK of the column's type or a
/// CHECK constraint of the relation (`NOT VALID` ones included: they bind
/// new rows) is FALSE for it whatever the row's other columns are — and
/// the name of that constraint when it is the relation's. `None` when no
/// such error is certain: the relation is no plain or partitioned table, a
/// rule or a BEFORE ROW trigger may change the row, the column has a type
/// modifier that may change the value, or the value isn't a constant
/// stored as written.
pub(crate) fn assignment_violation(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
    column: &str,
    value: &ast::Node,
    update: bool,
) -> Option<(String, Option<String>)> {
    use crate::nonnull::checks::{Knowledge, RelationChecks};
    let class = snapshot.pg_class.get(&relid)?;
    if !matches!(
        class.relkind,
        crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
    ) {
        return None;
    }
    let event = if update {
        DmlEvent::Update
    } else {
        DmlEvent::Insert
    };
    if returning_rewritten(snapshot, relid, &[event])
        || super::written_row::before_row_trigger_rewrites(snapshot, relid, event)
    {
        return None;
    }
    let attr = snapshot
        .attributes_of(relid)
        .iter()
        .find(|a| a.attname == column && a.attnum > 0)?;
    let literal = crate::nonnull::literal_for(Some(value), attr.atttypid, snapshot)?;
    if let Some(typmod) = snapshot.effective_typmod(attr.atttypid, attr.atttypmod) {
        // `varchar(n)` stores a string of at most n characters as is.
        let fits = snapshot.unwrap_domain(attr.atttypid) == oid::VARCHAR
            && literal.kind == crate::nonnull::LitKind::String
            && i64::try_from(literal.text.chars().count())
                .is_ok_and(|n| n <= i64::from(typmod) - 4);
        if !fits {
            return None;
        }
    }
    if let Some(message) = snapshot.domain_value_violation_at_execution(attr.atttypid, &literal) {
        return Some((message, None));
    }
    let has_children = snapshot.pg_inherits.iter().any(|i| i.inhparent == relid);
    let is_column = |c: &str| c == column;
    let not_null = |_: &str| false;
    let equals = |c: &str| is_column(c).then(|| literal.clone());
    let no_preds = |_: &str| Vec::new();
    let k = Knowledge {
        non_null: &is_column,
        null: &not_null,
        equals: &equals,
        preds: &no_preds,
    };
    let mut constraints: Vec<&crate::pg_catalog::PgConstraint> = snapshot
        .pg_constraint
        .values()
        .filter(|c| {
            c.conrelid == relid && c.contype == crate::pg_catalog::ConType::Check && c.conenforced
        })
        .collect();
    constraints.sort_by(|a, b| a.conname.cmp(&b.conname));
    for con in constraints {
        let Some(def) = snapshot.check_defs.get(&con.oid) else {
            continue;
        };
        // An UPDATE of a parent also updates its children's rows, which a
        // NO INHERIT constraint doesn't bind.
        if def.no_inherit && has_children && update {
            continue;
        }
        let (crate::ddl::tables::check_inherit::StoredExpr::Written(expr), Some(trust)) =
            (&def.expr, &def.trusted)
        else {
            continue;
        };
        if RelationChecks::surely_false(snapshot, relid, (**expr).as_ref(), trust, &k) {
            return Some((
                crate::pgmsg::check_constraint_violation(&class.relname, &con.conname).to_string(),
                Some(con.conname.clone()),
            ));
        }
    }
    None
}

/// For the lint (while [`crate::PgCatalog::query_view`] collects its
/// facts): `null_ctx` with the CHECK constraints of the UPDATE / DELETE
/// target `table_oid` (entry `alias`) registered, which every row the
/// WHERE reads satisfies — what the condition facts judge the WHERE with.
/// `None` otherwise, or when the target has none (or isn't a table).
fn with_target_checks(
    null_ctx: &NullabilityContext,
    scope: &Scope,
    alias: &str,
    table_oid: crate::oid::PgClassOid,
    snapshot: &PgCatalog,
) -> Option<NullabilityContext> {
    if !crate::pg_catalog::collecting_column_ref_bindings() {
        return None;
    }
    let class = snapshot.pg_class.get(&table_oid)?;
    if !matches!(
        class.relkind,
        crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
    ) {
        return None;
    }
    let checks = crate::nonnull::checks::RelationChecks::of(snapshot, table_oid)?;
    let src = scope.sources.iter().find(|s| s.alias == alias)?;
    let base: HashMap<String, bool> = src
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.base_not_null))
        .collect();
    let mut narrowed = null_ctx.clone();
    narrowed.register_checks(alias, checks, base);
    Some(narrowed)
}
