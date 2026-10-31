use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// INSERT / UPDATE / DELETE
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_insert(
    ins: &protobuf::InsertStmt,
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
    relation: Option<&protobuf::RangeVar>,
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
    ins: &protobuf::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    if replaced_by_instead_rule(ins.relation.as_ref(), CmdType::CmdInsert, snapshot) {
        crate::expr::replaced_by_rule(|| analyze_insert_here(ins, snapshot, params, outer_ctes))
    } else {
        analyze_insert_here(ins, snapshot, params, outer_ctes)
    }
}

fn analyze_insert_here(
    ins: &protobuf::InsertStmt,
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

    // Match $N params in VALUES to column types, or analyze INSERT...SELECT.
    // The rewriter later sees one target list entry per supplied column.
    let mut assigns: Vec<Assign> = Vec::new();
    // The first `NULL::d` the VALUES rows build (see `Rewrite::row_error`).
    let mut row_error = None;
    // What each row writes at each target position (`None`: DEFAULT); no
    // row for DEFAULT VALUES, which writes the defaults.
    let mut written: Vec<Vec<Option<ValueInfo>>> = vec![Vec::new()];
    if let Some(select_node) = &ins.select_stmt
        && let Some(node::Node::SelectStmt(val_sel)) = select_node.node.as_ref()
    {
        // transformInsertStmt's isGeneralSelect: a VALUES carrying ORDER
        // BY, LIMIT / OFFSET, a locking clause or its own WITH is analyzed
        // as a standalone SELECT whose output columns are then assigned —
        // its column types resolved across the rows (an untyped literal,
        // NULL or `$N` alone becomes text), DEFAULT not allowed. Only a
        // plain VALUES list is the INSERT's own rows.
        if !val_sel.values_lists.is_empty() && !is_general_select(val_sel) {
            let expanded = expand_values_rows(val_sel, snapshot, params, &cte_scopes)?;
            let val_sel: &protobuf::SelectStmt = &expanded;
            written = analyze_insert_values(ins, val_sel, &tgt, snapshot, params, &cte_scopes)?;
            let rows: Vec<&[protobuf::Node]> = val_sel
                .values_lists
                .iter()
                .filter_map(|l| match l.node.as_ref() {
                    Some(node::Node::List(list)) => Some(list.items.as_slice()),
                    _ => None,
                })
                .collect();
            row_error = rows
                .iter()
                .flat_map(|r| r.iter())
                .find_map(|v| null_domain_cast_error(v, snapshot));
            let width = rows.first().map_or(0, |r| r.len());
            assigns = (0..width)
                .filter_map(|i| {
                    Some(Assign {
                        column: target_col_at(&tgt, i)?.attname.clone(),
                        default: rows.iter().all(|r| r.get(i).is_some_and(is_set_to_default)),
                        null: tgt.col_indirection.get(i).is_none_or(Vec::is_empty)
                            && rows
                                .iter()
                                .any(|r| r.get(i).is_some_and(is_sql_null_literal)),
                    })
                })
                .collect();
        } else {
            let (selected, column_types) =
                analyze_insert_select(val_sel, &ins.cols, &tgt, snapshot, params, &cte_scopes)?;
            if !val_sel.values_lists.is_empty()
                && let Some(e) = general_values_violation(val_sel, &tgt, &column_types, snapshot)
            {
                return Err(e);
            }
            let width = selected.len();
            written = vec![selected.into_iter().map(Some).collect()];
            assigns = (0..width)
                .filter_map(|i| {
                    Some(Assign {
                        column: target_col_at(&tgt, i)?.attname.clone(),
                        default: false,
                        null: false,
                    })
                })
                .collect();
        }
    }

    note_written_columns(&tgt, written.first().map_or(0, Vec::len));

    // The columns the INSERT leaves to their default — omitted, or
    // `DEFAULT` in some VALUES row — get the default expression, converted
    // to the column's type (rewriteTargetListIU / rewriteValuesRTE): a
    // constant one is folded like a written value.
    if crate::expr::plan_time_checks() {
        let rows: Vec<&[protobuf::Node]> =
            match ins.select_stmt.as_deref().and_then(|n| n.node.as_ref()) {
                Some(node::Node::SelectStmt(sel)) => sel
                    .values_lists
                    .iter()
                    .filter_map(|l| match l.node.as_ref() {
                        Some(node::Node::List(list)) => Some(list.items.as_slice()),
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
                Some(i) => rows.iter().any(|r| r.get(i).is_some_and(is_set_to_default)),
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
        && let Some(v) = row_violation(&tgt, &[], &|_| Known::Unknown, snapshot)
    {
        return Err(v.error);
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
        .is_some_and(|oc| oc.action == protobuf::OnConflictAction::OnconflictUpdate as i32)
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
    // ON CONFLICT DO UPDATE's EXCLUDED is in the range table but not the
    // namespace RETURNING sees: referencing it is PG's `invalid reference
    // to FROM-clause entry`, and it does not mask a RETURNING OLD / NEW
    // alias.
    if ins
        .on_conflict_clause
        .as_ref()
        .is_some_and(|oc| oc.action == protobuf::OnConflictAction::OnconflictUpdate as i32)
    {
        let mut holder = Scope::default();
        holder.add_dml_target(snapshot, "excluded", target_qn, &tgt.attrs);
        ret_scope.shadowed_sources.extend(holder.sources);
    }

    // No returned row of an INSERT has an old version, except the ones
    // ON CONFLICT DO UPDATE updated.
    let mut rows = ReturningRows {
        old_may_be_null: true,
        new_may_be_null: false,
        ..ReturningRows::default()
    };
    // A returned row is an inserted one, or — ON CONFLICT DO UPDATE — an
    // updated one (DO NOTHING returns only the inserted rows).
    if has_returning(&ins.returning_clause)
        && let Some(wt) = WriteTarget::resolve(snapshot, tgt.oid)
    {
        let insert_arm = if wt.insert_keeps(snapshot) {
            insert_knowledge(snapshot, &wt, &tgt, &written)
        } else {
            RowKnowledge::default()
        };
        let mut not_null = wt.row_not_null(snapshot, &insert_arm);
        if let Some(oc) = &ins.on_conflict_clause
            && oc.action == protobuf::OnConflictAction::OnconflictUpdate as i32
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
        prove_columns(&mut ret_null_ctx, insert_target_alias(relation), &not_null);
        rows.new_proven = not_null;
    }
    let columns = resolve_returning(
        &ins.returning_clause,
        insert_target_alias(relation),
        rows,
        expr::Ctx::new(&ret_scope, &ret_null_ctx, snapshot),
        params,
    )?;

    let mut rw = Rewrite::single(DmlEvent::Insert, assigns, tgt.overriding);
    rw.row_error = row_error;
    rw.returning = has_returning(&ins.returning_clause);
    rw.on_conflict = ins.on_conflict_clause.as_ref().and_then(|oc| {
        match protobuf::OnConflictAction::try_from(oc.action) {
            Ok(protobuf::OnConflictAction::OnconflictUpdate) => {
                let set = set_list_assigns(&conflict_set_list);
                rw.listed.extend(set.iter().map(|a| a.column.clone()));
                Some(Some(set))
            }
            Ok(protobuf::OnConflictAction::OnconflictNothing) => Some(None),
            _ => None,
        }
    });
    check_rewrite(snapshot, tgt.oid, &rw)?;
    // The planner infers the arbiter index after the rewriter ran.
    if let (Some(on_conflict), Some(arbiter)) = (&ins.on_conflict_clause, &arbiter) {
        validate_on_conflict_target(on_conflict, arbiter, snapshot, tgt.oid, &tgt.relname)?;
    }
    // Every execution evaluates the rows the SELECT yields. (Not followed
    // in a WITH, whose earlier CTEs its names could mean.)
    if let Some(node::Node::SelectStmt(sel)) =
        ins.select_stmt.as_deref().and_then(|s| s.node.as_ref())
        && outer_ctes.is_empty()
        && ins.with_clause.as_ref().is_none_or(|w| !w.recursive)
    {
        let ctes = ins.with_clause.as_ref().map_or(&[][..], |w| &w.ctes[..]);
        super::failing_rows::check_insert_select(sel, ctes, tgt.oid, snapshot)?;
    }

    Ok((columns, None))
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
    oc: &protobuf::OnConflictClause,
    set_list: &[protobuf::Node],
    relation: &protobuf::RangeVar,
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
        for c in &mut s.columns {
            c.base_not_null |= insert_not_null.contains(&c.name);
        }
    }
    scope.sources.extend(holder.sources);
    let mut null_ctx = NullabilityContext::default();
    let mut k = RowKnowledge::default();
    if let Some(w) = &oc.where_clause {
        let log = crate::nonnull::StrictLog::default();
        let mut scratch = params.clone();
        let typed = expr::infer_expr(
            w,
            expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
            &mut scratch,
            TypeGoal::implicit(oid::BOOL),
        );
        if typed.is_ok() {
            params.absorb_non_null_reads(&scratch);
            let facts = crate::nonnull::nonnullable(w, true, &scope, &log, snapshot)
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

/// The name ON CONFLICT and RETURNING use for the INSERT target:
/// `INSERT INTO t AS a` makes it `a` (PG's `transformInsertStmt` adds the
/// target RTE under its alias).
fn insert_target_alias(relation: &protobuf::RangeVar) -> &str {
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
    col_indirection: Vec<Vec<protobuf::Node>>,
    /// `OVERRIDING SYSTEM VALUE` or `OVERRIDING USER VALUE`. Either lets a
    /// value be written to a GENERATED ALWAYS identity column; USER VALUE
    /// then discards it — and any value given for a BY DEFAULT one — in
    /// favour of the sequence.
    overriding: Overriding,
}

impl InsertTarget {
    /// Whether the value given for column `c` is discarded for its
    /// identity sequence's next value (OVERRIDING USER VALUE).
    fn discards(&self, c: &crate::pg_catalog::PgAttribute) -> bool {
        self.overriding == Overriding::UserValue && c.attidentity.is_some()
    }
}

/// Resolve the INSERT target relation, validate that every column named in the
/// target list exists, and collect the declared column list + overriding flag.
fn resolve_insert_target(
    ins: &protobuf::InsertStmt,
    relation: &protobuf::RangeVar,
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
            if let Some(node::Node::ResTarget(rt)) = n.node.as_ref() {
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
        let Some(node::Node::ResTarget(rt)) = n.node.as_ref() else {
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
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => Some(rt.indirection.clone()),
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
    })
}

/// Walk the optional `WITH` clause so parameters used only inside the CTE are
/// registered with the collector — without this, `$N` numbers referenced
/// exclusively in the CTE would be missing from `seen` and `into_sorted`
/// would report a spurious "parameter gap". The resolved CTE columns are also
/// threaded into the inner SELECT's scope so `INSERT … SELECT … FROM cte`
/// resolves the CTE alias.
fn build_insert_cte_scopes(
    ins: &protobuf::InsertStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<HashMap<String, Vec<ScopeColumn>>, AnalyzeError> {
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &ins.with_clause {
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes, &[])?;
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
fn is_general_select(sel: &protobuf::SelectStmt) -> bool {
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
/// `INSERT … VALUES` with each row as transformInsertStmt transforms it
/// (transformExpressionList, single row or several): `rel.*` and
/// `(expr).*` expanded into one value per column — a rule action's
/// `new.*`, a SQL function parameter's `v.*` — before rows are counted
/// against the target columns. Borrowed when no row holds a star.
fn expand_values_rows<'a>(
    val_sel: &'a protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<std::borrow::Cow<'a, protobuf::SelectStmt>, AnalyzeError> {
    let scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let null_ctx = NullabilityContext::default();
    let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
    let rows: Vec<Option<Vec<protobuf::Node>>> = val_sel
        .values_lists
        .iter()
        .map(|row| match row.node.as_ref() {
            Some(node::Node::List(list)) => {
                Ok(match expr::expand_row_args(&list.items, ctx, params)? {
                    std::borrow::Cow::Owned(items) => Some(items),
                    std::borrow::Cow::Borrowed(_) => None,
                })
            }
            _ => Ok(None),
        })
        .collect::<Result<_, AnalyzeError>>()?;
    if rows.iter().all(Option::is_none) {
        return Ok(std::borrow::Cow::Borrowed(val_sel));
    }
    let values_lists = val_sel
        .values_lists
        .iter()
        .zip(rows)
        .map(|(row, items)| match items {
            Some(items) => protobuf::Node {
                node: Some(node::Node::List(protobuf::List { items })),
            },
            None => row.clone(),
        })
        .collect();
    Ok(std::borrow::Cow::Owned(protobuf::SelectStmt {
        values_lists,
        ..val_sel.clone()
    }))
}

fn analyze_insert_values(
    ins: &protobuf::InsertStmt,
    val_sel: &protobuf::SelectStmt,
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
    for val_list in &val_sel.values_lists {
        let Some(node::Node::List(list)) = val_list.node.as_ref() else {
            continue;
        };
        let mut row_written: Vec<Option<ValueInfo>> = Vec::with_capacity(list.items.len());
        // transformInsertStmt: every row of a multi-row VALUES must be as
        // long as the first one, which already passed transformInsertRow's
        // arity check.
        match first_len {
            None => first_len = Some(list.items.len()),
            Some(n) if n != list.items.len() => {
                // PG positions it at the row's first value.
                let span = list.items.first().and_then(crate::error::expr_span);
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
                    .and_then(crate::error::expr_span),
            ));
        }
        // The row is transformed whole, then coerced to its columns
        // (transformInsertRow).
        let untyped = expr::replay_transform(
            &list.items.iter().collect::<Vec<_>>(),
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
                if let Some(node::Node::ResTarget(rt)) = n.node.as_ref() {
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
                    val,
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
            if val_sel.values_lists.len() > 1 {
                check_no_srf_in_clause(val, snapshot, "VALUES")?;
            }
            // EXPR_KIND_VALUES / EXPR_KIND_VALUES_SINGLE.
            crate::clause::check_no_aggregates_or_windows(val, snapshot, "VALUES")?;
            if untyped[i]
                && let Some(p) = expr::bare_param(val, snapshot)
                && let Some(t) = &target
            {
                expr::coerce_untyped_param(p, t.type_oid, snapshot, params)?;
            }
            let inferred = match &target {
                Some(t) => t.infer_value(val, goal, ctx, params)?,
                None if is_set_to_default(val) => expr::ExprType::scalar(oid::UNKNOWN, false),
                None => expr::infer_expr(val, ctx, params, goal)?,
            };
            row_written.push(match target_col {
                _ if is_set_to_default(val) => None,
                // A value stored inside the column says nothing of it.
                Some(_) if indirected => Some(ValueInfo::default()),
                Some(tc) => Some(ValueInfo::assigned(val, &inferred, tc, snapshot)),
                None => Some(ValueInfo::default()),
            });

            if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && let Some(tc) = target_col
                && (!tc.attnotnull || indirected || tgt.discards(tc))
            {
                params.infer_nullable(p.number, true);
            }
        }
        if violation.is_none() {
            let known = |i: usize| match target_col_at(tgt, i) {
                Some(tc) if is_null_value(&list.items[i], tc.atttypid, None, snapshot) => {
                    Known::Null
                }
                _ => Known::Value(&list.items[i]),
            };
            violation = row_violation(tgt, &list.items, &known, snapshot);
        }
        written.push(row_written);
    }
    match violation {
        Some(v) => Err(v.error),
        None => Ok(written),
    }
}

/// An error a row of an INSERT raises every time: a NULL it would store
/// into a NOT NULL column or domain, or no partition taking it.
struct RowViolation {
    /// When PG reports it: NOT NULL domains while the row is built, then
    /// routing (ExecFindPartition), then the columns' NOT NULL in attnum
    /// order (ExecConstraints, on the partition the row landed in).
    order: (bool, i16),
    error: AnalyzeError,
}

impl RowViolation {
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
            order: (true, i16::MIN),
            error: crate::pgmsg::no_partition_for_row(relname),
        })
    }
}

/// What the row checks know of the value an INSERT row gives a column.
#[derive(Clone, Copy)]
enum Known<'a> {
    /// A NULL, stored as is.
    Null,
    /// This expression, stored as the column's type reads it.
    Value(&'a protobuf::Node),
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
    items: &'a [protobuf::Node],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
) -> Option<RowViolation> {
    if returning_rewritten(snapshot, tgt.oid, &[DmlEvent::Insert]) {
        return None;
    }
    let (lands_in, unrouted) = row_relation(tgt, items, known, snapshot);
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
        if is_set_to_default(val) {
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
            // is built, whatever the column.
            Known::Value(_) | Known::Unknown => {
                if let Some(msg) = null_domain_cast(val, snapshot) {
                    let attnum = snapshot
                        .attribute_by_name(lands_in.0, &tc.attname)
                        .map_or(tc.attnum, |a| a.attnum);
                    violations.push(RowViolation {
                        order: (false, attnum),
                        error: AnalyzeError::Invalid(msg),
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
    violations.into_iter().min_by_key(|v| v.order)
}

/// Whether `val`, a value of type `source` (`None`: its own) stored into
/// a column of type `column`, stores a NULL: a bare `NULL`, or `NULL::T`
/// for a type `T` that isn't a domain (whose cast would check it) — when
/// nothing but a domain coercion separates it from the column's type, as
/// a cast function needn't keep a NULL NULL.
fn is_null_value(
    val: &protobuf::Node,
    column: PgTypeOid,
    source: Option<PgTypeOid>,
    snapshot: &PgCatalog,
) -> bool {
    let own = match val.node.as_ref() {
        Some(node::Node::AConst(c)) if c.isnull => None,
        Some(node::Node::TypeCast(tc)) if tc.arg.as_deref().is_some_and(is_sql_null_literal) => {
            match tc
                .type_name
                .as_ref()
                .and_then(|tn| crate::ddl::util::resolve_type_name(tn, snapshot))
            {
                Some(t) if snapshot.unwrap_domain(t) == t => Some(t),
                _ => return false,
            }
        }
        _ => return false,
    };
    let base = snapshot.unwrap_domain(column);
    match (own, source) {
        (None, None) => true,
        (Some(t), None) | (None, Some(t)) => t == base,
        (Some(t), Some(s)) => t == s && s == base,
    }
}

/// The error `NULL::d` raises when evaluated, for a domain `d` rejecting
/// NULL (CoerceToDomain is checked at execution, never folded away). None
/// when the statement is not run (a view, rule action, SQL function body,
/// prepared statement or plain EXPLAIN).
fn null_domain_cast(val: &protobuf::Node, snapshot: &PgCatalog) -> Option<String> {
    if !super::executes() {
        return None;
    }
    null_domain_cast_error(val, snapshot)
}

/// [`null_domain_cast`]'s error, whether or not the statement runs.
fn null_domain_cast_error(val: &protobuf::Node, snapshot: &PgCatalog) -> Option<String> {
    let Some(node::Node::TypeCast(tc)) = val.node.as_ref() else {
        return None;
    };
    if !tc.arg.as_deref().is_some_and(is_sql_null_literal) {
        return None;
    }
    let t = crate::ddl::util::resolve_type_name(tc.type_name.as_ref()?, snapshot)?;
    snapshot.domain_null_violation_at_execution(t)
}

/// The relation a VALUES row (`items`; none for DEFAULT VALUES) is checked
/// in, and its name: the partition ExecFindPartition routes it to when the
/// target is partitioned and the analyzer can tell (see
/// `partbound::route_row`), else the target itself. Plus the routing
/// failure when the analyzer can tell no partition takes the row — unless
/// a DO INSTEAD rule replaces the INSERT.
fn row_relation<'a>(
    tgt: &InsertTarget,
    items: &'a [protobuf::Node],
    known: &dyn Fn(usize) -> Known<'a>,
    snapshot: &PgCatalog,
) -> ((crate::oid::PgClassOid, String), Option<RowViolation>) {
    // A column's value in the row: given, or its default when that is NULL
    // (`Some(None)` for NULL); `None` when unknown.
    let value_of = |name: &str| -> Option<Option<&protobuf::Node>> {
        let c = tgt.attrs.iter().find(|c| c.attname == name)?;
        let given =
            (0..items.len()).find(|&i| target_col_at(tgt, i).is_some_and(|t| t.attname == name));
        match given {
            Some(i) if !tgt.col_indirection.get(i).is_none_or(Vec::is_empty) || tgt.discards(c) => {
                None
            }
            Some(i) if !is_set_to_default(&items[i]) => match known(i) {
                Known::Null => Some(None),
                Known::Value(v) => Some(Some(v)),
                Known::Unknown => None,
            },
            _ => null_default(c, snapshot).then_some(None),
        }
    };
    use crate::ddl::tables::partbound::Route;
    let route = crate::ddl::tables::partbound::route_row(snapshot, tgt.oid, &value_of);
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
    if let Some(err) = crate::fold::error(e, snapshot) {
        return crate::expr::plan_time_error(None, err.raw(None).finalize_implicit());
    }
    crate::typmod::assignment_error(
        snapshot,
        c.atttypid,
        snapshot.effective_typmod(c.atttypid, c.atttypmod),
        e,
        None,
        || None,
    )
}

/// Whether column `c`'s default is NULL: no identity or generation
/// expression, and either a stored DEFAULT that is NULL (see
/// [`stored_default_is_null`]) or no DEFAULT at all, nor one its domain
/// gives it.
pub(crate) fn null_default(c: &crate::pg_catalog::PgAttribute, snapshot: &PgCatalog) -> bool {
    c.attidentity.is_none()
        && c.attgenerated.is_none()
        && if c.atthasdef {
            stored_default_is_null(c, snapshot)
        } else {
            !snapshot.domain_has_default(c.atttypid)
        }
}

/// Whether column `c`'s stored DEFAULT evaluates to NULL. A default
/// cooking to a bare null constant is never stored (AddRelationNewConstraints),
/// but on a domain column it cooks to a CoerceToDomain over the null
/// constant, which PG keeps — overriding the domain's own default — and
/// which still yields NULL. So does `DEFAULT NULL` on a column whose typmod
/// adds a (strict) length coercion. A NULL cast to another type is left
/// out: a cast function needn't keep a NULL NULL.
fn stored_default_is_null(c: &crate::pg_catalog::PgAttribute, snapshot: &PgCatalog) -> bool {
    matches!(
        snapshot.attr_default_exprs.get(&(c.attrelid, c.attnum)),
        Some(crate::ddl::tables::check_inherit::StoredExpr::Written(e))
            if is_null_value(e, c.atttypid, None, snapshot)
    )
}

/// The columns a VALUES row (`items`; none for DEFAULT VALUES) leaves to
/// their default — omitted, or `DEFAULT` — when that is NULL and of a
/// domain rejecting NULL (NOT NULL, or a CHECK a NULL fails): the NULL is
/// coerced to the domain as the row is built (`expand_targetlist`), before
/// any trigger. A table's own columns only (a view's defaults are its
/// base table's).
fn omitted_domain_nulls(
    tgt: &InsertTarget,
    items: &[protobuf::Node],
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
        .filter(|(_, v)| !is_set_to_default(v))
        .filter_map(|(i, _)| target_col_at(tgt, i).map(|c| c.attname.as_str()))
        .collect();
    tgt.attrs
        .iter()
        .filter(|c| c.attnum > 0 && !given.contains(&c.attname.as_str()))
        .filter(|c| null_default(c, snapshot))
        .filter_map(|c| {
            let msg = snapshot.domain_null_violation_at_execution(c.atttypid)?;
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
    cols: &[protobuf::Node],
    targets: usize,
    expressions: usize,
    extra_value: Option<crate::error::SourceSpan>,
) -> AnalyzeError {
    if expressions > targets {
        crate::pgmsg::insert_more_expressions_than_targets(targets, expressions, extra_value)
    } else {
        let column = cols.get(expressions).and_then(|n| match n.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => {
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
    val_sel: &protobuf::SelectStmt,
    cols: &[protobuf::Node],
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
    let direct_targets: &[protobuf::Node] = if val_sel.op == SetOperation::SetopNone as i32
        && val_sel.values_lists.is_empty()
        && !val_sel.target_list.iter().any(|t| {
            matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
                if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
                    Some(node::Node::ColumnRef(cr)) if cr.fields.iter().any(|f|
                        matches!(f.node.as_ref(), Some(node::Node::AStar(_))))))
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
        if let Some(node::Node::ResTarget(rt)) = direct_targets.get(i).and_then(|t| t.node.as_ref())
            && let Some(p) = rt
                .val
                .as_deref()
                .and_then(|v| expr::bare_param(v, snapshot))
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
            let fallback = protobuf::Node { node: None };
            let node = match direct_targets.get(i).and_then(|t| t.node.as_ref()) {
                Some(node::Node::ResTarget(rt)) => rt.val.as_deref(),
                _ => None,
            };
            expr::unknown_value_coercion(
                target_oid,
                crate::coerce::CoercionContext::Assignment,
                snapshot,
                node.unwrap_or(&fallback),
            )?;
            continue;
        }
        if sel_col.type_oid == oid::UNKNOWN || sel_col.type_oid == target_oid {
            continue;
        }
        let literal = direct_targets.get(i).and_then(|t| {
            if let Some(node::Node::ResTarget(rt)) = t.node.as_ref()
                && let Some(val) = &rt.val
                && let Some(node::Node::AConst(ac)) = val.node.as_ref()
                && !ac.isnull
                && let Some(typedpg_pg_query::protobuf::a_const::Val::Sval(sv)) = &ac.val
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
            if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
                && let Some(val) = &rt.val
                && let Some(tc) = target_col_at(tgt, i)
                && tgt.col_indirection.get(i).is_none_or(|ind| ind.is_empty())
                && !tgt.discards(tc)
                && let Some(err) = crate::typmod::check_literal_assignment(
                    snapshot,
                    tc.atttypid,
                    snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                    val,
                )
            {
                return Err(err);
            }
        }
    }

    // The expressions each output column takes its value from: the select
    // list's entries, or — a VALUES source with ORDER BY / LIMIT & co. —
    // every row's value at that position.
    let mut outputs: Vec<(usize, &protobuf::Node)> = direct_targets
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => Some((i, rt.val.as_deref()?)),
            _ => None,
        })
        .collect();
    if val_sel.op == SetOperation::SetopNone as i32 {
        for row in &val_sel.values_lists {
            if let Some(node::Node::List(list)) = row.node.as_ref() {
                outputs.extend(list.items.iter().enumerate());
            }
        }
    }
    for (i, val) in outputs {
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
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
    val_sel: &protobuf::SelectStmt,
    tgt: &InsertTarget,
    column_types: &[PgTypeOid],
    snapshot: &PgCatalog,
) -> Option<AnalyzeError> {
    /// A constant non-negative count (`None`: a NULL / ALL one, which
    /// doesn't count); `Err` when it isn't one.
    fn count(n: Option<&protobuf::Node>) -> Result<Option<usize>, ()> {
        use typedpg_pg_query::protobuf::a_const::Val;
        match n.and_then(|n| n.node.as_ref()) {
            None => Ok(None),
            Some(node::Node::AConst(c)) if c.isnull => Ok(None),
            Some(node::Node::AConst(c)) => match &c.val {
                Some(Val::Ival(i)) => usize::try_from(i.ival).map(Some).map_err(|_| ()),
                _ => Err(()),
            },
            _ => Err(()),
        }
    }
    let rows: Vec<&[protobuf::Node]> = val_sel
        .values_lists
        .iter()
        .filter_map(|l| match l.node.as_ref() {
            Some(node::Node::List(list)) => Some(list.items.as_slice()),
            _ => None,
        })
        .collect();
    let offset = count(val_sel.limit_offset.as_deref()).ok()?.unwrap_or(0);
    let limit = count(val_sel.limit_count.as_deref())
        .ok()?
        .unwrap_or(usize::MAX);
    let violations: Vec<Option<RowViolation>> = rows
        .iter()
        .map(|items| {
            let known = |i: usize| match (target_col_at(tgt, i), column_types.get(i)) {
                (Some(tc), Some(&t)) if t == snapshot.unwrap_domain(tc.atttypid) => {
                    if is_null_value(&items[i], tc.atttypid, Some(t), snapshot) {
                        Known::Null
                    } else {
                        Known::Value(&items[i])
                    }
                }
                _ => Known::Unknown,
            };
            row_violation(tgt, items, &known, snapshot)
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
    on_conflict: &protobuf::OnConflictClause,
    relation: &protobuf::RangeVar,
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(Option<Arbiter>, Vec<protobuf::Node>), AnalyzeError> {
    let update = on_conflict.action() == protobuf::OnConflictAction::OnconflictUpdate;
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
    // columns: `excluded.ctid` does not exist.
    for s in &mut excluded_holder.sources {
        s.system_columns.clear();
        s.dml_target = false;
    }

    // transformOnConflictArbiter.
    if update && on_conflict.infer.is_none() {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError(
                "ON CONFLICT DO UPDATE requires inference specification or constraint name".into(),
            ),
            None,
            Some("For example, ON CONFLICT (column_name).".into()),
        )
        .finalize_implicit());
    }
    let arbiter = match on_conflict.infer.as_deref() {
        None => None,
        Some(infer) => {
            let mut arbiter_scope = target_scope.clone();
            if update {
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

    if !update {
        return Ok((arbiter, Vec::new()));
    }
    // transformOnConflictClause builds the EXCLUDED pseudo-relation's
    // target list (exclRelTlist) of a Var per column of the target: a
    // stored INSERT ... ON CONFLICT DO UPDATE depends on every column.
    for a in tgt.attrs.iter().filter(|a| a.attnum > 0) {
        crate::ddl::depend::note_column(tgt.oid, &a.attname);
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
    if let Some(where_clause) = &on_conflict.where_clause {
        expr::infer_expr(
            where_clause,
            expr::Ctx::new(&conflict_scope, &conflict_null_ctx, snapshot),
            params,
            TypeGoal::implicit(oid::BOOL),
        )?;
        crate::clause::check_no_aggregates_or_windows(where_clause, snapshot, "WHERE")?;
        check_no_srf_in_clause(where_clause, snapshot, "WHERE")?;
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
    infer: &protobuf::InferClause,
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
        let Some(node::Node::IndexElem(ie)) = elem.node.as_ref() else {
            continue;
        };
        if !matches!(
            ie.ordering(),
            protobuf::SortByDir::SortbyDefault | protobuf::SortByDir::Undefined
        ) {
            return Err(invalid("ASC/DESC is not allowed in ON CONFLICT clause"));
        }
        if !matches!(
            ie.nulls_ordering(),
            protobuf::SortByNulls::SortbyNullsDefault | protobuf::SortByNulls::Undefined
        ) {
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
                // The inference element is a Var of the column.
                crate::ddl::depend::note_column(tgt.oid, &a.attname);
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
        } else if let Some(e) = ie.expr.as_deref() {
            check_index_expr_kind(e, snapshot, "index expressions", "index expression")?;
            expr::infer_expr(e, ctx, params, TypeGoal::NONE)?;
            check_index_expr_kind(e, snapshot, "index expressions", "index expression")?;
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
                expr: ie.expr.as_deref().cloned(),
                collation,
                opclass,
            });
        }
    }
    if let Some(w) = infer.where_clause.as_deref() {
        check_index_expr_kind(w, snapshot, "index predicates", "index predicate")?;
        expr::infer_expr(w, ctx, params, TypeGoal::NONE)?;
        check_index_expr_kind(w, snapshot, "index predicates", "index predicate")?;
        arbiter.where_clause = Some(w.clone());
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
    e: &protobuf::Node,
    snapshot: &PgCatalog,
    kind: &str,
    subquery_context: &str,
) -> Result<(), AnalyzeError> {
    let mut has_sublink = false;
    visit_same_level(e, &mut |n| {
        has_sublink |= matches!(n.node.as_ref(), Some(node::Node::SubLink(_)));
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
    upd: &protobuf::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_update_with_outer_ctes(upd, snapshot, params, &HashMap::new())
}

/// [`analyze_update`] for an UPDATE that sees the CTEs of an enclosing
/// `WITH` (a data-modifying CTE body).
pub(crate) fn analyze_update_with_outer_ctes(
    upd: &protobuf::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    if replaced_by_instead_rule(upd.relation.as_ref(), CmdType::CmdUpdate, snapshot) {
        crate::expr::replaced_by_rule(|| analyze_update_here(upd, snapshot, params, outer_ctes))
    } else {
        analyze_update_here(upd, snapshot, params, outer_ctes)
    }
}

fn analyze_update_here(
    upd: &protobuf::UpdateStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    let _plan =
        expr::plan_fold::PlanLevel::enter(upd.where_clause.as_deref(), &upd.from_clause, snapshot);
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
    for target in &upd.target_list {
        if let Some(node::Node::ResTarget(rt)) = target.node.as_ref() {
            crate::ddl::depend::note_column(table_oid, &rt.name);
        }
    }

    // Walk `UPDATE … WITH (cte) …` so parameters inside the CTE are seen by
    // the collector and the CTE alias is visible to the FROM clause. Same
    // reasoning as the corresponding block in `analyze_insert`.
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &upd.with_clause {
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes, &[])?;
    }
    check_current_of_on_view(upd.where_clause.as_deref(), snapshot, table_oid)?;

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
    if let Some(where_clause) = &upd.where_clause {
        let log = crate::nonnull::StrictLog::default();
        crate::clause::coerce_clause_expr(
            where_clause,
            expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
            params,
            crate::clause::ClauseKind::Where,
        )?;
        check_no_srf_in_clause(where_clause, snapshot, "WHERE")?;
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
            where_clause,
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
        old_facts = Some(all);
    }

    let ret_scope = with_written_target(
        &scope,
        alias,
        snapshot,
        table_oid,
        &table_attrs,
        &[DmlEvent::Update],
    );
    super::returning::type_returning_params(
        &upd.returning_clause,
        alias,
        expr::Ctx::new(&ret_scope, &null_ctx, snapshot),
        params,
    )?;

    // SET col = expr / (a, b) = (…) / col[i] = expr — assignment context.
    let set_list = analyze_set_clause(
        &upd.target_list,
        &table_attrs,
        &table_relname,
        expr::Ctx::new(&scope, &set_null_ctx, snapshot),
        params,
        true,
    )?;

    // The old row is a stored row past WHERE; the new one, with no BEFORE
    // ROW trigger or row movement, that row with the SET values (computed
    // from the old row) and the generated columns recomputed.
    if has_returning(&upd.returning_clause)
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
        &upd.returning_clause,
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
    rw.returning = has_returning(&upd.returning_clause);
    check_rewrite(snapshot, table_oid, &rw)?;
    Ok((columns, None))
}

pub(crate) fn analyze_delete(
    del: &protobuf::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_delete_with_outer_ctes(del, snapshot, params, &HashMap::new())
}

/// [`analyze_delete`] for a DELETE that sees the CTEs of an enclosing
/// `WITH` (a data-modifying CTE body).
pub(crate) fn analyze_delete_with_outer_ctes(
    del: &protobuf::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    if replaced_by_instead_rule(del.relation.as_ref(), CmdType::CmdDelete, snapshot) {
        crate::expr::replaced_by_rule(|| analyze_delete_here(del, snapshot, params, outer_ctes))
    } else {
        analyze_delete_here(del, snapshot, params, outer_ctes)
    }
}

fn analyze_delete_here(
    del: &protobuf::DeleteStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    let _plan =
        expr::plan_fold::PlanLevel::enter(del.where_clause.as_deref(), &del.using_clause, snapshot);
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
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes, &[])?;
    }
    check_current_of_on_view(del.where_clause.as_deref(), snapshot, table_oid)?;

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
    if let Some(where_clause) = &del.where_clause {
        let log = crate::nonnull::StrictLog::default();
        crate::clause::coerce_clause_expr(
            where_clause,
            expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
            params,
            crate::clause::ClauseKind::Where,
        )?;
        check_no_srf_in_clause(where_clause, snapshot, "WHERE")?;
        // RETURNING reads the deleted row as WHERE saw it, unless a rule
        // rewrites the statement or the target is a view.
        let keeps = rows_returned_as_is(snapshot, table_oid);
        let (facts, all) = where_facts(
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            where_clause,
            &log,
            alias,
            &mut rows,
            keeps,
            |_| keeps,
        );
        null_ctx.add_where_facts(facts);
        old_facts = Some(all);
    }
    // A deleted row is a stored row past WHERE: its CHECK constraints and
    // generated columns hold too.
    if has_returning(&del.returning_clause)
        && let Some(wt) = WriteTarget::resolve(snapshot, table_oid)
    {
        let old_nn = wt.row_not_null(snapshot, &old_row_knowledge(&wt, old_facts.as_ref(), alias));
        prove_columns(&mut null_ctx, alias, &old_nn);
        rows.old_proven.extend(old_nn);
    }
    // A DO INSTEAD rule (or an INSTEAD OF trigger) returns rows of its own.
    if has_returning(&del.returning_clause)
        && returning_rewritten(snapshot, table_oid, &[DmlEvent::Delete])
        && let Some(target) = scope.sources.iter_mut().find(|s| s.alias == alias)
    {
        for c in &mut target.columns {
            c.base_not_null = false;
        }
    }

    let columns = resolve_returning(
        &del.returning_clause,
        alias,
        rows,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
    let mut rw = Rewrite::single(DmlEvent::Delete, Vec::new(), Overriding::NotSet);
    rw.returning = has_returning(&del.returning_clause);
    check_rewrite(snapshot, table_oid, &rw)?;
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
    where_clause: &protobuf::Node,
    log: &crate::nonnull::StrictLog,
    target_alias: &str,
    rows: &mut ReturningRows,
    old_as_is: bool,
    target_keeps: impl Fn(&str) -> bool,
) -> (crate::nonnull::Facts, crate::nonnull::Facts) {
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
    from_clause: &[protobuf::Node],
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
    where_clause: Option<&protobuf::Node>,
    snapshot: &PgCatalog,
    table_oid: crate::oid::PgClassOid,
) -> Result<(), AnalyzeError> {
    if matches!(
        where_clause.and_then(|w| w.node.as_ref()),
        Some(node::Node::CurrentOfExpr(_))
    ) && snapshot
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
