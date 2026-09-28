use std::cell::Cell;

use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// MERGE (PG 15+)
// ──────────────────────────────────────────────────────────────────────────────

thread_local! {
    /// Depth of MERGE RETURNING lists currently being analyzed. PG's
    /// `transformMergeSupportFunc` accepts `merge_action()` when the current
    /// parse state — or any parent, so sublinks count — is
    /// `EXPR_KIND_MERGE_RETURNING`; this is that dynamic extent.
    static MERGE_RETURNING_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// `merge_action()` (PG 17): `text NOT NULL` inside a MERGE RETURNING list
/// (directly or in a sublink there), 42601 everywhere else.
pub(crate) fn infer_merge_support_func(
    f: &protobuf::MergeSupportFunc,
) -> Result<expr::ExprType, AnalyzeError> {
    if MERGE_RETURNING_DEPTH.with(Cell::get) == 0 {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError(
                "MERGE_ACTION() can only be used in the RETURNING list of a MERGE command".into(),
            ),
            crate::error::SourceSpan::from_node_qname(f.location),
            None,
        )
        .finalize_implicit());
    }
    Ok(expr::ExprType::scalar(oid::TEXT, false))
}

pub(crate) fn analyze_merge(
    merge: &protobuf::MergeStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_merge_with_outer_ctes(merge, snapshot, params, &HashMap::new())
}

/// MERGE, mirroring `transformMergeStmt` (parse_merge.c): the source is a
/// FROM item joined to the target; the ON condition sees both; each WHEN
/// clause sees only the relations `setNamespaceForMergeWhen` leaves visible
/// (MATCHED: both; NOT MATCHED [BY TARGET]: the source; NOT MATCHED BY
/// SOURCE: the target), and RETURNING (PG 17) sees both — source first, as
/// in `*` expansion.
pub(crate) fn analyze_merge_with_outer_ctes(
    merge: &protobuf::MergeStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let relation = merge
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("MERGE without relation".into()))?;

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
    let target_alias = relation
        .alias
        .as_ref()
        .map(|a| a.aliasname.as_str())
        .unwrap_or(&relation.relname)
        .to_owned();
    let target_qn = crate::qualified_name::QualifiedName::new(&table_nsname, &table_relname);

    // Process the optional `WITH` clause first (CTEs visible to source +
    // ON + every WHEN branch).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &merge.with_clause {
        for cte_node in &with.ctes {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                let cte_columns = analyze_cte(cte, with.recursive, snapshot, params, &cte_scopes)?;
                cte_scopes.insert(cte.ctename.clone(), cte_columns);
            }
        }
    }

    // The target alone, and the source's FROM item alone.
    let mut target_scope = Scope::default();
    target_scope.add_dml_target(snapshot, &target_alias, target_qn.clone(), &table_attrs);
    let mut source_scope = Scope::default();
    let mut null_ctx = NullabilityContext::default();
    if let Some(source_relation) = &merge.source_relation {
        process_from_item(
            source_relation,
            &mut source_scope,
            &mut null_ctx,
            snapshot,
            &cte_scopes,
            params,
        )?;
    }

    // Both relations: the ON condition and WHEN MATCHED arms.
    let mut both = source_scope.clone();
    both.sources.extend(target_scope.sources.iter().cloned());
    // Each one-sided arm keeps the other relation as a shadowed entry, so a
    // qualified reference to it reports PG's `invalid reference to
    // FROM-clause entry for table "x"`.
    let mut target_only = target_scope.clone();
    target_only
        .shadowed_sources
        .extend(source_scope.sources.iter().cloned());
    let mut source_only = source_scope.clone();
    source_only
        .shadowed_sources
        .extend(target_scope.sources.iter().cloned());

    if let Some(join_condition) = &merge.join_condition {
        expr::infer_expr(
            join_condition,
            expr::Ctx::new(&both, &null_ctx, snapshot),
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
    }

    let mut source_may_be_null = false;
    for when_node in &merge.merge_when_clauses {
        if let Some(node::Node::MergeWhenClause(when)) = when_node.node.as_ref() {
            let when_scope = match protobuf::MergeMatchKind::try_from(when.match_kind) {
                Ok(protobuf::MergeMatchKind::MergeWhenNotMatchedBySource) => {
                    // Rows of the target without a source match: the source
                    // side is NULL wherever such an action returns a row.
                    source_may_be_null |= CmdType::try_from(when.command_type)
                        .is_ok_and(|c| c != CmdType::CmdNothing);
                    &target_only
                }
                Ok(protobuf::MergeMatchKind::MergeWhenNotMatchedByTarget) => &source_only,
                _ => &both,
            };
            walk_merge_when_clause(
                when,
                expr::Ctx::new(when_scope, &null_ctx, snapshot),
                params,
                &table_attrs,
                &table_relname,
            )?;
        }
    }

    // RETURNING sees the source and the target. The target columns are the
    // inserted / updated / deleted row, so they keep their base
    // nullability; the source is NULL for NOT MATCHED BY SOURCE actions.
    let mut ret_null_ctx = null_ctx.clone();
    if source_may_be_null {
        ret_null_ctx.mark_all_nullable(&nullability::collect_aliases(&source_scope.sources));
    }
    MERGE_RETURNING_DEPTH.with(|d| d.set(d.get() + 1));
    let columns = resolve_target_list(
        &merge.returning_list,
        expr::Ctx::new(&both, &ret_null_ctx, snapshot),
        params,
    );
    MERGE_RETURNING_DEPTH.with(|d| d.set(d.get() - 1));
    Ok((columns?, None))
}

fn walk_merge_when_clause(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    if let Some(condition) = &when.condition {
        expr::infer_expr(condition, ctx, params, TypeGoal::assignment(oid::BOOL))?;
    }

    let cmd = CmdType::try_from(when.command_type).unwrap_or(CmdType::Undefined);
    match cmd {
        CmdType::CmdUpdate => merge_when_update(when, ctx, params, table_attrs, table_relname),
        CmdType::CmdInsert => merge_when_insert(when, ctx, params, table_attrs, table_relname),
        CmdType::CmdDelete | CmdType::CmdNothing => {
            // No target / value expressions to walk beyond the optional
            // `AND condition` already handled above.
            Ok(())
        }
        _ => Err(AnalyzeError::Unsupported(format!(
            "MERGE WHEN command type {:?} is not supported",
            cmd
        ))),
    }
}

/// `WHEN MATCHED THEN UPDATE SET col = expr [, …]` — each entry is a
/// `ResTarget` with `name = column` and `val = expression`. Validate the
/// column exists, then walk the value with an assignment goal.
fn merge_when_update(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    for set_item in &when.target_list {
        let Some(node::Node::ResTarget(rt)) = set_item.node.as_ref() else {
            continue;
        };
        let Some(val) = &rt.val else { continue };
        let tc = table_attrs
            .iter()
            .find(|c| c.attname == rt.name)
            .ok_or_else(|| {
                crate::scope::undefined_dml_column_error(
                    &rt.name,
                    table_relname,
                    table_attrs,
                    crate::error::SourceSpan::from_node_qname(rt.location),
                )
            })?;
        if is_sql_null_literal(val)
            && let Some(err) = null_assignment_error(tc, snapshot, table_relname, "assign")
        {
            return Err(err);
        }
        if let Some(err) = crate::typmod::check_literal_assignment(
            snapshot,
            tc.atttypid,
            snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
            val,
        ) {
            return Err(err);
        }
        if tc.attgenerated.is_some() && !is_set_to_default(val) {
            return Err(AnalyzeError::Invalid(format!(
                "column \"{}\" can only be updated to DEFAULT \
                 (generated column on `{}`)",
                tc.attname, table_relname,
            )));
        }
        if tc.attidentity == Some(AttIdentity::Always) && !is_set_to_default(val) {
            return Err(AnalyzeError::Invalid(format!(
                "column \"{}\" can only be updated to DEFAULT \
                 (identity column on `{}` defined as GENERATED ALWAYS)",
                tc.attname, table_relname,
            )));
        }
        expr::infer_expr(val, ctx, params, TypeGoal::assignment(tc.atttypid))?;
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
            && !tc.attnotnull
        {
            params.infer_nullable(p.number, true);
        }
    }
    Ok(())
}

/// `WHEN NOT MATCHED THEN INSERT (cols…) VALUES (vals…)` — `target_list` holds
/// the column names (each a `ResTarget` with `name`), `values` holds the
/// parallel value expressions. An empty `target_list` implies the full
/// attribute list.
fn merge_when_insert(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    let res_targets: Vec<&protobuf::ResTarget> = when
        .target_list
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::ResTarget(rt) if !rt.name.is_empty() => Some(rt.as_ref()),
            _ => None,
        })
        .collect();
    let target_attrs: Vec<&crate::pg_catalog::PgAttribute> = if res_targets.is_empty() {
        table_attrs.iter().collect()
    } else {
        res_targets
            .iter()
            .map(|rt| {
                table_attrs
                    .iter()
                    .find(|c| c.attname == rt.name)
                    .ok_or_else(|| {
                        crate::scope::undefined_dml_column_error(
                            &rt.name,
                            table_relname,
                            table_attrs,
                            crate::error::SourceSpan::from_node_qname(rt.location),
                        )
                    })
            })
            .collect::<Result<_, _>>()?
    };
    for (i, val) in when.values.iter().enumerate() {
        let target_col = target_attrs.get(i).copied();
        if let Some(tc) = target_col {
            if is_sql_null_literal(val)
                && let Some(err) = null_assignment_error(tc, snapshot, table_relname, "insert")
            {
                return Err(err);
            }
            if let Some(err) = crate::typmod::check_literal_assignment(
                snapshot,
                tc.atttypid,
                snapshot.effective_typmod(tc.atttypid, tc.atttypmod),
                val,
            ) {
                return Err(err);
            }
            if tc.attgenerated.is_some() && !is_set_to_default(val) {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot insert a non-DEFAULT value into column \"{}\" \
                     (generated column on `{}`)",
                    tc.attname, table_relname,
                )));
            }
            if tc.attidentity == Some(AttIdentity::Always) && !is_set_to_default(val) {
                return Err(AnalyzeError::Invalid(format!(
                    "cannot insert a non-DEFAULT value into column \"{}\" \
                     (identity column on `{}` defined as GENERATED ALWAYS)",
                    tc.attname, table_relname,
                )));
            }
        }
        let goal = target_col
            .map(|tc| TypeGoal::assignment(tc.atttypid))
            .unwrap_or(TypeGoal::NONE);
        expr::infer_expr(val, ctx, params, goal)?;
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
            && let Some(tc) = target_col
            && !tc.attnotnull
        {
            params.infer_nullable(p.number, true);
        }
    }
    Ok(())
}
