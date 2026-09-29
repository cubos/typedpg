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
    if let Some(select_node) = &ins.select_stmt
        && let Some(node::Node::SelectStmt(val_sel)) = select_node.node.as_ref()
    {
        if !val_sel.values_lists.is_empty() {
            analyze_insert_values(ins, val_sel, &tgt, snapshot, params, &cte_scopes)?;
        } else {
            analyze_insert_select(val_sel, &tgt, snapshot, params, &cte_scopes)?;
        }
    }

    if let Some(on_conflict) = &ins.on_conflict_clause {
        analyze_insert_on_conflict(on_conflict, relation, &tgt, snapshot, params, &cte_scopes)?;
    }

    // Resolve RETURNING list.
    let mut ret_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let ret_null_ctx = NullabilityContext::default();
    let target_qn = crate::qualified_name::QualifiedName::new(&tgt.nsname, &tgt.relname);
    ret_scope.add_dml_target(
        snapshot,
        insert_target_alias(relation),
        target_qn.clone(),
        &tgt.attrs,
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
    let columns = resolve_returning(
        &ins.returning_clause,
        insert_target_alias(relation),
        ReturningRows {
            old_may_be_null: true,
            new_may_be_null: false,
        },
        expr::Ctx::new(&ret_scope, &ret_null_ctx, snapshot),
        params,
    )?;

    Ok((columns, None))
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
    /// `OVERRIDING SYSTEM VALUE` or `OVERRIDING USER VALUE` was requested —
    /// either lets a value be written to a GENERATED ALWAYS identity column
    /// (USER VALUE then discards it in favour of the sequence).
    overriding: bool,
}

/// Resolve the INSERT target relation, validate that every column named in the
/// target list exists, and collect the declared column list + overriding flag.
fn resolve_insert_target(
    ins: &protobuf::InsertStmt,
    relation: &protobuf::RangeVar,
    snapshot: &PgCatalog,
) -> Result<InsertTarget, AnalyzeError> {
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
    let col_indirection = ins
        .cols
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => Some(rt.indirection.clone()),
            _ => None,
        })
        .collect();

    // PG enum for `Insert.override`:
    //   1 = OVERRIDING_NOT_SET, 2 = USER_VALUE, 3 = SYSTEM_VALUE.
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
        overriding: ins.r#override == 2 || ins.r#override == 3,
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
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes)?;
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
) -> Result<(), AnalyzeError> {
    // No table in scope for VALUES, but we need scope for possible
    // subqueries/functions inside an individual value expression.
    let scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let null_ctx = NullabilityContext::default();
    let expected_len = insert_arity(tgt);

    for val_list in &val_sel.values_lists {
        let Some(node::Node::List(list)) = val_list.node.as_ref() else {
            continue;
        };
        // Arity check: the VALUES row must match the declared column list
        // (or, when no column list is given, the full table width).
        if arity_mismatch(tgt, list.items.len(), expected_len) {
            // PG (SQLSTATE 42601) emits one of two messages:
            // `INSERT has more expressions than target columns` or
            // `INSERT has more target columns than expressions`. Mirror PG's
            // wording verbatim and tack on our richer detail behind it.
            let pg_msg = if list.items.len() > expected_len {
                "INSERT has more expressions than target columns"
            } else {
                "INSERT has more target columns than expressions"
            };
            return Err(AnalyzeError::Invalid(format!(
                "{pg_msg} (table `{}` expects {expected_len}, got {})",
                tgt.relname,
                list.items.len(),
            )));
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
                && let Some(err) = null_assignment_error(tc, snapshot, &tgt.relname, "insert")
            {
                return Err(err);
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
            if let Some(tc) = target_col
                && tc.attgenerated.is_some()
                && !is_set_to_default(val)
            {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot insert a non-DEFAULT value into column \"{}\" \
                     (generated column on `{}`)",
                    tc.attname, tgt.relname,
                )));
            }
            if let Some(tc) = target_col
                && tc.attidentity == Some(AttIdentity::Always)
                && !is_set_to_default(val)
                && !tgt.overriding
            {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot insert a non-DEFAULT value into column \"{}\" \
                     (identity column on `{}` defined as GENERATED ALWAYS \
                     — hint: use OVERRIDING SYSTEM VALUE to override)",
                    tc.attname, tgt.relname,
                )));
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
            match &target {
                Some(t) => t.infer_value(val, goal, ctx, params)?,
                None if is_set_to_default(val) => expr::ExprType::scalar(oid::UNKNOWN, false),
                None => expr::infer_expr(val, ctx, params, goal)?,
            };

            if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && let Some(tc) = target_col
                && (!tc.attnotnull || indirected)
            {
                params.infer_nullable(p.number, true);
            }
        }
    }
    Ok(())
}

/// `INSERT … SELECT …`: enforce arity, reject GENERATED ALWAYS identity
/// targets (a SELECT can't supply `DEFAULT`), walk the SELECT so its params
/// register and typos propagate, then pin column types onto bare `$N`
/// projections.
fn analyze_insert_select(
    val_sel: &protobuf::SelectStmt,
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(), AnalyzeError> {
    // Walk the SELECT side of `INSERT … SELECT` so its params are registered
    // and any undefined-column / typo errors inside the SELECT propagate
    // cleanly. PG (transformInsertStmt) analyzes the SELECT first and only
    // then compares its *output* width — after `*` expansion and set
    // operations — with the target list.
    let (sel_cols, _) = analyze_select_with_ctes(val_sel, snapshot, params, cte_scopes)?;
    let expected_len = insert_arity(tgt);
    if arity_mismatch(tgt, sel_cols.len(), expected_len) {
        let pg_msg = if sel_cols.len() > expected_len {
            "INSERT has more expressions than target columns"
        } else {
            "INSERT has more target columns than expressions"
        };
        return Err(AnalyzeError::Invalid(format!(
            "{pg_msg} (table `{}` expects {expected_len}, SELECT produces {})",
            tgt.relname,
            sel_cols.len(),
        )));
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
    // INSERT ... SELECT cannot supply `DEFAULT`, so any target column that is
    // `GENERATED ALWAYS AS IDENTITY` is rejected unless the user requested
    // OVERRIDING SYSTEM VALUE, and a generated column (stored or virtual)
    // always is — PG's `rewriteTargetListIU` checks both per column.
    for i in 0..sel_cols.len() {
        let Some(tc) = target_col_at(tgt, i) else {
            continue;
        };
        if tc.attidentity == Some(AttIdentity::Always) && !tgt.overriding {
            return Err(AnalyzeError::Invalid(format!(
                "cannot insert a non-DEFAULT value into column \"{}\" \
                 (identity column on `{}` defined as GENERATED ALWAYS \
                 — hint: use OVERRIDING SYSTEM VALUE to override)",
                tc.attname, tgt.relname,
            )));
        }
        if tc.attgenerated.is_some() {
            return Err(AnalyzeError::Invalid(format!(
                "cannot insert a non-DEFAULT value into column \"{}\" \
                 (generated column on `{}`)",
                tc.attname, tgt.relname,
            )));
        }
    }
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
            if let Err(msg) = crate::literal_input::validate(text, target_oid, snapshot) {
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
            if !tc.attnotnull || indirected {
                params.infer_nullable(p.number, true);
            }
        }
    }
    Ok(())
}

/// `ON CONFLICT (…) DO UPDATE SET …` / `DO NOTHING`.
///
/// DO UPDATE exposes a virtual `EXCLUDED` relation holding the proposed row.
/// We model it in scope as a second alias over the target table: the columns
/// share names and types, and nullability follows the real columns because PG
/// rejects an INSERT that violates NOT NULL before the conflict handler runs.
fn analyze_insert_on_conflict(
    on_conflict: &protobuf::OnConflictClause,
    relation: &protobuf::RangeVar,
    tgt: &InsertTarget,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(), AnalyzeError> {
    // Validate the conflict target (`ON CONFLICT (cols)` / `ON CONFLICT ON
    // CONSTRAINT name`) against pg_constraint. PG rejects targets that don't
    // match a unique/primary-key index; without this check the analyzer
    // accepts any column.
    validate_on_conflict_target(on_conflict, snapshot, tgt.oid, &tgt.relname)?;

    let mut conflict_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let target_qn = crate::qualified_name::QualifiedName::new(&tgt.nsname, &tgt.relname);
    conflict_scope.add_dml_target(
        snapshot,
        insert_target_alias(relation),
        target_qn.clone(),
        &tgt.attrs,
    );
    conflict_scope.add_dml_target(snapshot, "excluded", target_qn, &tgt.attrs);
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
    }
    Ok(())
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

    let table_oid = table.oid;
    let table_relname = table.relname.clone();
    let table_nsname = snapshot
        .namespace_name(table.relnamespace)
        .map(str::to_owned)
        .unwrap_or_default();
    let table_attrs = snapshot.attributes_of(table_oid).to_vec();

    // Walk `UPDATE … WITH (cte) …` so parameters inside the CTE are seen by
    // the collector and the CTE alias is visible to the FROM clause. Same
    // reasoning as the corresponding block in `analyze_insert`.
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &upd.with_clause {
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes)?;
    }

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

    // Process FROM clause (UPDATE ... FROM ... WHERE ...).
    process_from_clause(
        &upd.from_clause,
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
    if let Some(where_clause) = &upd.where_clause {
        crate::clause::coerce_clause_expr(
            where_clause,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::Where,
        )?;
        check_no_srf_in_clause(where_clause, snapshot, "WHERE")?;
    }

    let columns = resolve_returning(
        &upd.returning_clause,
        alias,
        ReturningRows::default(),
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
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
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes)?;
    }

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
    process_from_clause(
        &del.using_clause,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &cte_scopes,
        params,
    )?;

    // WHERE — BOOL goal with assignment coercion.
    if let Some(where_clause) = &del.where_clause {
        crate::clause::coerce_clause_expr(
            where_clause,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::Where,
        )?;
        check_no_srf_in_clause(where_clause, snapshot, "WHERE")?;
    }

    // A deleted row has no new version.
    let columns = resolve_returning(
        &del.returning_clause,
        alias,
        ReturningRows {
            old_may_be_null: false,
            new_may_be_null: true,
        },
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
    Ok((columns, None))
}
