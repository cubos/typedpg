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
    // What each row writes at each target position (`None`: DEFAULT); no
    // row for DEFAULT VALUES, which writes the defaults.
    let mut written: Vec<Vec<Option<ValueInfo>>> = vec![Vec::new()];
    if let Some(select_node) = &ins.select_stmt
        && let Some(node::Node::SelectStmt(val_sel)) = select_node.node.as_ref()
    {
        if !val_sel.values_lists.is_empty() {
            written = analyze_insert_values(ins, val_sel, &tgt, snapshot, params, &cte_scopes)?;
            let rows: Vec<&[protobuf::Node]> = val_sel
                .values_lists
                .iter()
                .filter_map(|l| match l.node.as_ref() {
                    Some(node::Node::List(list)) => Some(list.items.as_slice()),
                    _ => None,
                })
                .collect();
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
            let selected =
                analyze_insert_select(val_sel, &ins.cols, &tgt, snapshot, params, &cte_scopes)?;
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

    // DEFAULT VALUES: every column takes its default.
    if ins.select_stmt.is_none()
        && let Some(v) = omitted_domain_nulls(&tgt, &[], snapshot)
            .into_iter()
            .min_by_key(|v| v.order)
    {
        return Err(v.error);
    }

    let arbiter = match &ins.on_conflict_clause {
        Some(on_conflict) => {
            analyze_insert_on_conflict(on_conflict, relation, &tgt, snapshot, params, &cte_scopes)?
        }
        None => None,
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
    rw.returning = has_returning(&ins.returning_clause);
    rw.on_conflict = ins.on_conflict_clause.as_ref().and_then(|oc| {
        match protobuf::OnConflictAction::try_from(oc.action) {
            Ok(protobuf::OnConflictAction::OnconflictUpdate) => {
                let set = set_list_assigns(&oc.target_list);
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
#[allow(clippy::too_many_arguments)]
fn on_conflict_update_arm(
    oc: &protobuf::OnConflictClause,
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
        &oc.target_list,
        wt,
        &tgt.attrs,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    );
    for a in set_list_assigns(&oc.target_list) {
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
    // The columns the INSERT writes (the leading ones, without a column
    // list) — what a stored query depends on.
    crate::ddl::depend::note(crate::ddl::depend::ObjectAddress::relation(table_oid));
    let written: Vec<&str> = if col_names.is_empty() {
        let arity = match ins.select_stmt.as_deref().and_then(|s| s.node.as_ref()) {
            Some(node::Node::SelectStmt(sel)) => match sel.values_lists.first() {
                Some(row) => match row.node.as_ref() {
                    Some(node::Node::List(l)) => l.items.len(),
                    _ => 0,
                },
                None => sel.target_list.len(),
            },
            _ => 0,
        };
        table_attrs
            .iter()
            .filter(|a| a.attnum > 0)
            .take(arity)
            .map(|a| a.attname.as_str())
            .collect()
    } else {
        col_names.iter().map(String::as_str).collect()
    };
    for name in written {
        crate::ddl::depend::note_column(table_oid, name);
    }
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

/// PG's transformInsertRow arity rule: more values than target columns is
/// always an error, fewer only with an explicit column list — without one
/// the remaining columns take their defaults (`INSERT INTO t VALUES (1)`).
fn arity_mismatch(tgt: &InsertTarget, given: usize, expected: usize) -> bool {
    given > expected || (given < expected && !tgt.col_names.is_empty())
}

/// `INSERT … VALUES (…)`: infer each value with the column's type as goal,
/// enforcing arity, NOT NULL / typmod literal checks, and the
/// generated/identity-column restrictions.
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

    // A literal NULL into a NOT NULL column fails every execution, but at
    // execution: PG reports it only once the whole statement has parsed,
    // for the first failing row, and in that row for the first column in
    // attnum order (ExecConstraints) — omitted columns with no default
    // included — after any NOT NULL domain (checked while the row is
    // built).
    let mut violation: Option<NullViolation> = None;
    let mut first_len: Option<usize> = None;
    // What each row writes at each position (`None`: DEFAULT).
    let mut written: Vec<Vec<Option<ValueInfo>>> = Vec::new();
    for val_list in &val_sel.values_lists {
        let Some(node::Node::List(list)) = val_list.node.as_ref() else {
            continue;
        };
        let mut row_written: Vec<Option<ValueInfo>> = Vec::with_capacity(list.items.len());
        let mut row_violations: Vec<NullViolation> = Vec::new();
        let mut default_nulls: Vec<NullViolation> = Vec::new();
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
                && is_sql_null_literal(val)
                && !tgt.discards(tc)
                && let Some(err) = null_assignment_error(tc, snapshot, &tgt.relname, "insert")
            {
                row_violations.push(NullViolation::of(tc, snapshot, err));
            }
            // An explicit DEFAULT that is NULL competes like an omitted
            // column.
            if let Some(tc) = target_col
                && !indirected
                && is_set_to_default(val)
                && let Some(v) = null_default_violation(tc, tgt, snapshot)
            {
                default_nulls.push(v);
            }
            if let Some(tc) = target_col
                && !indirected
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
        // A column left to a NULL default, of a domain rejecting NULL,
        // fails while the row is built: every execution.
        row_violations.extend(omitted_domain_nulls(tgt, &list.items, snapshot));
        if violation.is_none() && !row_violations.is_empty() {
            row_violations.extend(omitted_not_null_columns(tgt, list.items.len(), snapshot));
            row_violations.extend(default_nulls);
            violation = row_violations.into_iter().min_by_key(|v| v.order);
        }
        written.push(row_written);
    }
    match violation {
        Some(v) => Err(v.error),
        None => Ok(written),
    }
}

/// A NULL a row of an INSERT would store into a NOT NULL column or domain.
struct NullViolation {
    /// When PG reports it: NOT NULL domains while the row is built, then
    /// the columns' NOT NULL in attnum order.
    order: (bool, i16),
    error: AnalyzeError,
}

impl NullViolation {
    fn of(tc: &crate::pg_catalog::PgAttribute, snapshot: &PgCatalog, error: AnalyzeError) -> Self {
        let domain = snapshot.domain_null_violation(tc.atttypid).is_some();
        Self {
            order: (!domain, tc.attnum),
            error,
        }
    }
}

/// Whether column `c`'s default is NULL: no DEFAULT, identity or
/// generation expression, nor one its domain gives it.
fn null_default(c: &crate::pg_catalog::PgAttribute, snapshot: &PgCatalog) -> bool {
    !c.atthasdef
        && c.attidentity.is_none()
        && c.attgenerated.is_none()
        && !snapshot.domain_has_default(c.atttypid)
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
) -> Vec<NullViolation> {
    let is_table = snapshot.pg_class.get(&tgt.oid).is_some_and(|c| {
        matches!(
            c.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        )
    });
    if !is_table {
        return Vec::new();
    }
    let given: Vec<&str> = items
        .iter()
        .enumerate()
        .filter(|(_, v)| !is_set_to_default(v))
        .filter_map(|(i, _)| target_col_at(tgt, i).map(|c| c.attname.as_str()))
        .collect();
    // `DEFAULT NULL` (cast or not).
    fn null_constant(e: &protobuf::Node) -> bool {
        match e.node.as_ref() {
            Some(node::Node::AConst(k)) => k.isnull,
            Some(node::Node::TypeCast(tc)) => tc.arg.as_deref().is_some_and(null_constant),
            _ => false,
        }
    }
    let null_column_default = |c: &crate::pg_catalog::PgAttribute| {
        !c.atthasdef
            || matches!(
                snapshot.attr_default_exprs.get(&(tgt.oid, c.attnum)),
                Some(crate::ddl::tables::check_inherit::StoredExpr::Written(e)) if null_constant(e)
            )
    };
    tgt.attrs
        .iter()
        .filter(|c| c.attnum > 0 && !given.contains(&c.attname.as_str()))
        .filter(|c| c.attidentity.is_none() && c.attgenerated.is_none() && null_column_default(c))
        // A DEFAULT NULL of the column's own overrides the domain's.
        .filter(|c| c.atthasdef || !snapshot.domain_has_default(c.atttypid))
        .filter_map(|c| {
            let msg = snapshot.domain_null_violation(c.atttypid)?;
            Some(NullViolation {
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
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
) -> Option<NullViolation> {
    if !(c.attnum > 0 && c.attnotnull && null_default(c, snapshot)) {
        return None;
    }
    null_assignment_error(c, snapshot, &tgt.relname, "insert")
        .map(|e| NullViolation::of(c, snapshot, e))
}

/// The NOT NULL columns an INSERT row giving `given` values leaves to a
/// NULL default.
fn omitted_not_null_columns(
    tgt: &InsertTarget,
    given: usize,
    snapshot: &PgCatalog,
) -> Vec<NullViolation> {
    let provided: Vec<&str> = (0..given)
        .filter_map(|i| target_col_at(tgt, i).map(|c| c.attname.as_str()))
        .collect();
    tgt.attrs
        .iter()
        .filter(|c| !provided.contains(&c.attname.as_str()))
        .filter_map(|c| null_default_violation(c, tgt, snapshot))
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
) -> Result<Vec<ValueInfo>, AnalyzeError> {
    // Walk the SELECT side of `INSERT … SELECT` so its params are registered
    // and any undefined-column / typo errors inside the SELECT propagate
    // cleanly. PG (transformInsertStmt) analyzes the SELECT first and only
    // then compares its *output* width — after `*` expansion and set
    // operations — with the target list.
    let (sel_cols, _) = analyze_select_with_ctes(val_sel, snapshot, params, cte_scopes)?;
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
            && let Some(node::Node::ParamRef(p)) = rt.val.as_deref().and_then(|v| v.node.as_ref())
            && params.is_untyped_output(p.location)
        {
            if let Err(prev) = params.coerce_untyped(p.number, target_oid) {
                return Err(expr::inconsistent_param_error(
                    p.number, prev, target_oid, p.location, snapshot,
                ));
            }
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

    for (i, target) in direct_targets.iter().enumerate() {
        if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
            && let Some(val) = &rt.val
            && let Some(node::Node::ParamRef(p)) = val.node.as_ref()
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
    Ok(sel_cols
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
        .collect())
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
/// ([`validate_on_conflict_target`]), `None` without an inference clause.
fn analyze_insert_on_conflict(
    on_conflict: &protobuf::OnConflictClause,
    relation: &protobuf::RangeVar,
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<Option<Arbiter>, AnalyzeError> {
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
        return Ok(arbiter);
    }
    let mut conflict_scope = target_scope;
    conflict_scope.sources.extend(excluded_holder.sources);
    let conflict_null_ctx = NullabilityContext::default();
    analyze_set_clause(
        &on_conflict.target_list,
        &tgt.attrs,
        &tgt.relname,
        expr::Ctx::new(&conflict_scope, &conflict_null_ctx, snapshot),
        params,
        false,
    )?;
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
    Ok(arbiter)
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
    if !infer.conname.is_empty()
        && !snapshot
            .pg_constraint_values()
            .any(|c| c.conrelid == tgt.oid && c.conname == infer.conname)
    {
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
    let _level = QueryLevel::enter();
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

    // SET col = expr / (a, b) = (…) / col[i] = expr — assignment context.
    analyze_set_clause(
        &upd.target_list,
        &table_attrs,
        &table_relname,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
        true,
    )?;

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
                &upd.target_list,
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
            for a in set_list_assigns(&upd.target_list) {
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

    let ret_scope = with_written_target(
        &scope,
        alias,
        snapshot,
        table_oid,
        &table_attrs,
        &[DmlEvent::Update],
    );
    let columns = resolve_returning(
        &upd.returning_clause,
        alias,
        rows,
        expr::Ctx::new(&ret_scope, &null_ctx, snapshot),
        params,
    )?;
    let mut rw = Rewrite::single(
        DmlEvent::Update,
        set_list_assigns(&upd.target_list),
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
    let _level = QueryLevel::enter();
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
