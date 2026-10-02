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
    let _level = QueryLevel::enter();
    let relation = merge
        .relation
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("MERGE without relation".into()))?;
    super::from::check_rangevar_catalog(relation)?;

    // Process the optional `WITH` clause first (CTEs visible to source +
    // ON + every WHEN branch) — transformMergeStmt does it before anything
    // else.
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();
    if let Some(with) = &merge.with_clause {
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes, &[])?;
    }

    check_unreachable_when_clauses(&merge.merge_when_clauses)?;

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
    if !matches!(
        table.relkind,
        crate::pg_catalog::RelKind::Table
            | crate::pg_catalog::RelKind::Partitioned
            | crate::pg_catalog::RelKind::View
    ) {
        return Err(
            crate::pgmsg::merge_on_relation_kind(&table.relname, table.relkind).finalize_implicit(),
        );
    }

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

    // The target alone, and the source's FROM item alone.
    let mut target_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    target_scope.add_dml_target(snapshot, &target_alias, target_qn.clone(), &table_attrs);
    let mut source_scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    let mut null_ctx = NullabilityContext::default();
    // The target is in the range table (setTargetTable) but not yet in the
    // namespace while the source is transformed: a reference to it from
    // inside the source is `invalid reference to FROM-clause entry`.
    source_scope.shadowed_sources = target_scope.sources.clone();
    let target_entries = source_scope.shadowed_sources.len();
    if let Some(source_relation) = &merge.source_relation {
        process_from_item(
            source_relation,
            &mut source_scope,
            &mut null_ctx,
            snapshot,
            &cte_scopes,
            params,
        )?;
        // transformMergeStmt: the target and the source RTE may not share a
        // name (a dedicated checkNameSpaceConflicts, 42712).
        if from_item_refname(source_relation).as_deref() == Some(target_alias.as_str()) {
            return Err(crate::pgmsg::merge_name_specified_twice(&target_alias).finalize_implicit());
        }
    }
    source_scope.shadowed_sources.drain(..target_entries);

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

    let on_log = crate::nonnull::StrictLog::default();
    if let Some(join_condition) = &merge.join_condition {
        expr::infer_expr(
            join_condition,
            expr::Ctx::new(&both, &null_ctx, snapshot).logging_strictness(&on_log),
            params,
            TypeGoal::assignment(oid::BOOL),
        )?;
        // transformMergeStmt analyzes it as EXPR_KIND_JOIN_ON.
        crate::clause::check_no_aggregates_or_windows(join_condition, snapshot, "JOIN conditions")?;
        check_no_srf_in_clause(join_condition, snapshot, "JOIN conditions")?;
    }

    let mut source_may_be_null = false;
    let mut rows = ReturningRows::default();
    // What each WHEN clause's condition proves, where its action runs.
    let mut when_facts: Vec<crate::nonnull::Facts> = Vec::new();
    for when_node in &merge.merge_when_clauses {
        if let Some(node::Node::MergeWhenClause(when)) = when_node.node.as_ref() {
            // An INSERT action returns a row with no old version, a DELETE
            // action one with no new version.
            match CmdType::try_from(when.command_type) {
                Ok(CmdType::CmdInsert) => rows.old_may_be_null = true,
                Ok(CmdType::CmdDelete) => rows.new_may_be_null = true,
                _ => {}
            }
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
            let log = crate::nonnull::StrictLog::default();
            walk_merge_when_clause(
                when,
                expr::Ctx::new(when_scope, &null_ctx, snapshot),
                &log,
                params,
                &table_attrs,
                &table_relname,
            )?;
            when_facts.push(match &when.condition {
                Some(c) => crate::nonnull::nonnullable(c, true, when_scope, &log, snapshot)
                    .restricted_to(&crate::nonnull::own_aliases(when_scope)),
                None => crate::nonnull::Facts::default(),
            });
        }
    }

    // RETURNING sees the source and the target (plus the OLD / NEW rows).
    // The target columns are the inserted / updated / deleted row, so they
    // keep their base nullability; the source is NULL for NOT MATCHED BY
    // SOURCE actions.
    let mut ret_null_ctx = null_ctx.clone();
    if source_may_be_null {
        ret_null_ctx.mark_all_nullable(&nullability::collect_aliases(&source_scope.sources));
    }
    if has_returning(&merge.returning_clause)
        && let Some(wt) = WriteTarget::resolve(snapshot, table_oid)
    {
        let on_facts = merge
            .join_condition
            .as_deref()
            .map(|c| {
                crate::nonnull::nonnullable(c, true, &both, &on_log, snapshot)
                    .restricted_to(&crate::nonnull::own_aliases(&both))
            })
            .unwrap_or_default();
        let arms = MergeArms {
            merge,
            wt: &wt,
            target_alias: &target_alias,
            table_attrs: &table_attrs,
            both: &both,
            source_only: &source_only,
            null_ctx: &null_ctx,
            on_facts: &on_facts,
            when_facts: &when_facts,
            snapshot,
        };
        let known = arms.returned(params);
        prove_columns(&mut ret_null_ctx, &target_alias, &known.target);
        rows.old_proven.extend(known.old);
        rows.new_proven.extend(known.new);
        if !source_may_be_null {
            let source_aliases = crate::nonnull::own_aliases(&source_scope);
            let facts = known
                .source
                .iter()
                .filter(|(a, _)| source_aliases.contains(a))
                .fold(crate::nonnull::Facts::default(), |acc, (a, c)| {
                    acc.union(crate::nonnull::Facts::column(a, c))
                });
            if !facts.is_empty() {
                ret_null_ctx.add_where_facts(facts);
            }
        }
    }
    // Rows inserted or updated through a view are not the view's rows.
    let both =
        super::dml::with_written_target(&both, &target_alias, snapshot, table_oid, &table_attrs);
    MERGE_RETURNING_DEPTH.with(|d| d.set(d.get() + 1));
    let columns = resolve_returning(
        &merge.returning_clause,
        &target_alias,
        rows,
        expr::Ctx::new(&both, &ret_null_ctx, snapshot),
        params,
    );
    MERGE_RETURNING_DEPTH.with(|d| d.set(d.get() - 1));
    let columns = columns?;

    let mut actions = Vec::new();
    for when_node in &merge.merge_when_clauses {
        let Some(node::Node::MergeWhenClause(when)) = when_node.node.as_ref() else {
            continue;
        };
        actions.push(match CmdType::try_from(when.command_type) {
            Ok(CmdType::CmdInsert) => {
                // `INSERT (cols) VALUES (…)`, or the leading columns.
                let names: Vec<String> = if when.target_list.is_empty() {
                    table_attrs
                        .iter()
                        .take(when.values.len())
                        .map(|a| a.attname.clone())
                        .collect()
                } else {
                    when.target_list
                        .iter()
                        .filter_map(|t| match t.node.as_ref() {
                            Some(node::Node::ResTarget(rt)) => Some(rt.name.clone()),
                            _ => None,
                        })
                        .collect()
                };
                Action {
                    event: Some(DmlEvent::Insert),
                    assigns: names
                        .into_iter()
                        .zip(&when.values)
                        .map(|(column, v)| Assign {
                            column,
                            default: is_set_to_default(v),
                            null: is_sql_null_literal(v),
                        })
                        .collect(),
                    overriding: when.r#override
                        == protobuf::OverridingKind::OverridingUserValue as i32
                        || when.r#override
                            == protobuf::OverridingKind::OverridingSystemValue as i32,
                }
            }
            Ok(CmdType::CmdUpdate) => Action {
                event: Some(DmlEvent::Update),
                assigns: set_list_assigns(&when.target_list),
                overriding: false,
            },
            Ok(CmdType::CmdDelete) => Action {
                event: Some(DmlEvent::Delete),
                assigns: Vec::new(),
                overriding: false,
            },
            _ => Action {
                event: None,
                assigns: Vec::new(),
                overriding: false,
            },
        });
    }
    let rw = Rewrite {
        merge: true,
        listed: actions
            .iter()
            .flat_map(|a| a.assigns.iter().map(|x| x.column.clone()))
            .collect(),
        actions,
        on_conflict: None,
        returning: has_returning(&merge.returning_clause),
    };
    check_rewrite(snapshot, table_oid, &rw)?;
    Ok((columns, None))
}

fn walk_merge_when_clause(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    log: &crate::nonnull::StrictLog,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    if let Some(condition) = &when.condition {
        // transformMergeStmt: transformWhereClause(…, EXPR_KIND_MERGE_WHEN,
        // "WHEN").
        crate::clause::coerce_clause_expr(
            condition,
            ctx.logging_strictness(log),
            params,
            crate::clause::ClauseKind::MergeWhen,
        )?;
        check_no_srf_in_clause(condition, ctx.snapshot, "MERGE WHEN conditions")?;
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
            "typedpg does not support the MERGE action {} yet",
            cmd.as_str_name()
        ))),
    }
}

/// `WHEN MATCHED THEN UPDATE SET col = expr [, …]` — the shared UPDATE
/// SET analysis against the target table.
fn merge_when_update(
    when: &protobuf::MergeWhenClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    table_attrs: &[crate::pg_catalog::PgAttribute],
    table_relname: &str,
) -> Result<(), AnalyzeError> {
    analyze_set_clause(
        &when.target_list,
        table_attrs,
        table_relname,
        ctx,
        params,
        true,
    )
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
        }
        // transformInsertRow → transformAssignedExpr: a failed coercion
        // names the target column, exactly like a plain INSERT.
        let goal = target_col
            .map(|tc| {
                TypeGoal::assignment(tc.atttypid)
                    .with_source_column(&tc.attname)
                    .with_typmod(snapshot.effective_typmod(tc.atttypid, tc.atttypmod))
            })
            .unwrap_or(TypeGoal::NONE);
        if !is_set_to_default(val) {
            expr::infer_expr(val, ctx, params, goal)?;
            // EXPR_KIND_VALUES_SINGLE.
            crate::clause::check_no_aggregates_or_windows(val, snapshot, "VALUES")?;
        }
        if let Some(node::Node::ParamRef(p)) = val.node.as_ref()
            && let Some(tc) = target_col
            && !tc.attnotnull
        {
            params.infer_nullable(p.number, true);
        }
    }
    // transformInsertRow, after the values are transformed: more values
    // than target columns is always an error, fewer only with an explicit
    // column list (`INSERT DEFAULT VALUES` has no values at all).
    if !when.values.is_empty() {
        if when.values.len() > target_attrs.len() {
            return Err(crate::pgmsg::insert_more_expressions_than_targets(
                target_attrs.len(),
                when.values.len(),
                when.values
                    .get(target_attrs.len())
                    .and_then(crate::error::expr_span),
            )
            .finalize_implicit());
        }
        if !res_targets.is_empty() && when.values.len() < target_attrs.len() {
            return Err(crate::pgmsg::insert_more_targets_than_expressions(
                target_attrs.len(),
                when.values.len(),
                res_targets
                    .get(when.values.len())
                    .and_then(|rt| crate::error::SourceSpan::from_node_qname(rt.location)),
            )
            .finalize_implicit());
        }
    }
    Ok(())
}

/// transformMergeStmt's first pass over the WHEN clauses: once a match kind
/// has an unconditional clause, a later clause of the same kind can never
/// run (42601).
fn check_unreachable_when_clauses(clauses: &[protobuf::Node]) -> Result<(), AnalyzeError> {
    let mut terminal: Vec<i32> = Vec::new();
    for n in clauses {
        let Some(node::Node::MergeWhenClause(when)) = n.node.as_ref() else {
            continue;
        };
        if terminal.contains(&when.match_kind) {
            return Err(crate::pgmsg::merge_unreachable_when_clause().finalize_implicit());
        }
        if when.condition.is_none() {
            terminal.push(when.match_kind);
        }
    }
    Ok(())
}

/// The name a FROM item's range-table entry is known by (its `eref`
/// alias): the alias when given, else the relation / CTE name, the first
/// function's name, `json_table`, or PG's `unnamed_subquery` /
/// `unnamed_join` placeholders.
fn from_item_refname(item: &protobuf::Node) -> Option<String> {
    let alias = |a: &Option<protobuf::Alias>| a.as_ref().map(|a| a.aliasname.clone());
    match item.node.as_ref()? {
        node::Node::RangeVar(rv) => alias(&rv.alias).or_else(|| Some(rv.relname.clone())),
        node::Node::RangeSubselect(rs) => {
            alias(&rs.alias).or_else(|| Some("unnamed_subquery".into()))
        }
        node::Node::JoinExpr(j) => alias(&j.alias).or_else(|| Some("unnamed_join".into())),
        node::Node::RangeFunction(rf) => alias(&rf.alias).or_else(|| {
            let Some(node::Node::List(pair)) = rf.functions.first()?.node.as_ref() else {
                return None;
            };
            match pair.items.first()?.node.as_ref()? {
                node::Node::FuncCall(fc) => expr::extract_string_fields(&fc.funcname).pop(),
                _ => None,
            }
        }),
        node::Node::JsonTable(jt) => alias(&jt.alias).or_else(|| Some("json_table".into())),
        node::Node::RangeTableSample(ts) => from_item_refname(ts.relation.as_deref()?),
        _ => None,
    }
}

/// What MERGE RETURNING knows of the rows its actions return: one row per
/// executed INSERT / UPDATE / DELETE action (ExecMergeMatched /
/// ExecMergeNotMatched), each where its WHEN condition — and, for a
/// MATCHED one, the ON condition — held. What is known of every returned
/// row is what every action that returns one knows.
struct MergeArms<'a> {
    merge: &'a protobuf::MergeStmt,
    wt: &'a WriteTarget,
    target_alias: &'a str,
    table_attrs: &'a [crate::pg_catalog::PgAttribute],
    both: &'a Scope,
    source_only: &'a Scope,
    null_ctx: &'a NullabilityContext,
    on_facts: &'a crate::nonnull::Facts,
    when_facts: &'a [crate::nonnull::Facts],
    snapshot: &'a PgCatalog,
}

/// What every row a MERGE returns is known to hold.
#[derive(Default)]
struct MergeKnown {
    /// The target columns (the new row, a DELETE's old one) proven
    /// non-NULL.
    target: std::collections::HashSet<String>,
    /// OLD's and NEW's, over the actions that have one.
    old: std::collections::HashSet<String>,
    new: std::collections::HashSet<String>,
    /// The source columns proven non-NULL.
    source: std::collections::HashSet<(String, String)>,
}

impl MergeArms<'_> {
    fn returned(&self, params: &mut ParamCollector) -> MergeKnown {
        type Set = std::collections::HashSet<String>;
        let meet = |acc: &mut Option<Set>, s: &Set| match acc {
            Some(a) => a.retain(|c| s.contains(c)),
            None => *acc = Some(s.clone()),
        };
        let (mut target, mut old, mut new): (Option<Set>, Option<Set>, Option<Set>) =
            (None, None, None);
        let mut source: Option<std::collections::HashSet<(String, String)>> = None;
        let whens = self
            .merge
            .merge_when_clauses
            .iter()
            .filter_map(|n| match n.node.as_ref() {
                Some(node::Node::MergeWhenClause(w)) => Some(w.as_ref()),
                _ => None,
            });
        for (when, when_facts) in whens.zip(self.when_facts) {
            let cmd = CmdType::try_from(when.command_type).unwrap_or(CmdType::Undefined);
            if !matches!(
                cmd,
                CmdType::CmdInsert | CmdType::CmdUpdate | CmdType::CmdDelete
            ) {
                continue;
            }
            let kind = protobuf::MergeMatchKind::try_from(when.match_kind)
                .unwrap_or(protobuf::MergeMatchKind::MergeWhenMatched);
            let facts = match kind {
                protobuf::MergeMatchKind::MergeWhenMatched => {
                    self.on_facts.clone().union(when_facts.clone())
                }
                _ => when_facts.clone(),
            };
            let source_cols: std::collections::HashSet<(String, String)> = match kind {
                protobuf::MergeMatchKind::MergeWhenNotMatchedBySource => Default::default(),
                _ => facts
                    .columns
                    .iter()
                    .filter(|(a, _)| a != self.target_alias)
                    .cloned()
                    .collect(),
            };
            match &mut source {
                Some(s) => s.retain(|c| source_cols.contains(c)),
                None => source = Some(source_cols),
            }
            if cmd == CmdType::CmdInsert {
                let k = if self.wt.insert_keeps(self.snapshot) {
                    self.inserted(when, &facts, params)
                } else {
                    RowKnowledge::default()
                };
                let nn = self.wt.row_not_null(self.snapshot, &k);
                meet(&mut target, &nn);
                meet(&mut new, &nn);
                continue;
            }
            let old_k = old_row_knowledge(self.wt, Some(&facts), self.target_alias);
            let old_nn = self.wt.row_not_null(self.snapshot, &old_k);
            meet(&mut old, &old_nn);
            if cmd == CmdType::CmdDelete {
                meet(&mut target, &old_nn);
                continue;
            }
            let new_k = if self.wt.update_keeps(self.snapshot) {
                let mut ctx = self.null_ctx.clone();
                ctx.add_where_facts(facts.clone());
                let set = set_values(
                    &when.target_list,
                    self.wt,
                    self.table_attrs,
                    expr::Ctx::new(self.both, &ctx, self.snapshot),
                    params,
                );
                let mut k = old_k;
                for a in self.snapshot.attributes_of(self.wt.base) {
                    if a.attgenerated.is_some() {
                        k.write(&a.attname, &ValueInfo::default());
                    }
                }
                for a in set_list_assigns(&when.target_list) {
                    if let Some(b) = self.wt.to_base.get(&a.column) {
                        k.write(b, &set.get(&a.column).cloned().unwrap_or_default());
                    }
                }
                k
            } else {
                RowKnowledge::default()
            };
            let new_nn = self.wt.row_not_null(self.snapshot, &new_k);
            meet(&mut target, &new_nn);
            meet(&mut new, &new_nn);
        }
        MergeKnown {
            target: target.unwrap_or_default(),
            old: old.unwrap_or_default(),
            new: new.unwrap_or_default(),
            source: source.unwrap_or_default(),
        }
    }

    /// What an INSERT action stores: its values, evaluated over the source
    /// row where `facts` hold, and the defaults.
    fn inserted(
        &self,
        when: &protobuf::MergeWhenClause,
        facts: &crate::nonnull::Facts,
        params: &mut ParamCollector,
    ) -> RowKnowledge {
        let names: Vec<(String, bool)> = if when.target_list.is_empty() {
            self.table_attrs
                .iter()
                .map(|a| (a.attname.clone(), false))
                .collect()
        } else {
            when.target_list
                .iter()
                .filter_map(|t| match t.node.as_ref() {
                    Some(node::Node::ResTarget(rt)) => {
                        Some((rt.name.clone(), !rt.indirection.is_empty()))
                    }
                    _ => None,
                })
                .collect()
        };
        let mut ctx = self.null_ctx.clone();
        ctx.add_where_facts(facts.clone());
        let mut scratch = params.clone();
        let mut row = Vec::new();
        for ((name, indirected), val) in names.iter().zip(&when.values) {
            let Some(attr) = self.table_attrs.iter().find(|a| &a.attname == name) else {
                continue;
            };
            let info = if is_set_to_default(val) {
                None
            } else if *indirected {
                Some(ValueInfo::default())
            } else {
                let goal = TypeGoal::assignment(attr.atttypid).with_typmod(
                    self.snapshot
                        .effective_typmod(attr.atttypid, attr.atttypmod),
                );
                match expr::infer_expr(
                    val,
                    expr::Ctx::new(self.source_only, &ctx, self.snapshot),
                    &mut scratch,
                    goal,
                ) {
                    Ok(t) => Some(ValueInfo::of(val, t.nullable, attr.atttypid, self.snapshot)),
                    Err(_) => Some(ValueInfo::default()),
                }
            };
            row.push((name.clone(), info));
        }
        params.absorb_non_null_reads(&scratch);
        inserted_rows_knowledge(self.snapshot, self.wt, &[row])
    }
}
