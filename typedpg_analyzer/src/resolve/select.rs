use super::*;
use typedpg_pg_query::NodeBox;
use typedpg_pg_query::ast;

// ──────────────────────────────────────────────────────────────────────────────
// SELECT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_select(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, &HashMap::new(), &[], &[])
}

/// Like [`analyze_select`] but seeds the initial scope with the enclosing
/// levels as **correlated** references — a subquery's column lookup tries
/// its local FROM first and only falls back to these outer levels when
/// nothing matched. Used for `EXISTS (...)`, scalar sublinks, and
/// `IN (SELECT ...)`, where PG's lexical rule says inner aliases shadow
/// outer ones.
pub(crate) fn analyze_correlated_select(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_scope: &crate::scope::Scope,
    outer_null_ctx: &NullabilityContext,
) -> AnalyzeResult {
    // Everything the enclosing level can see — its own FROM items and the
    // levels around it — is reachable from the sublink as a correlated
    // (fallback-only) reference, and so is what *it* reaches that way (PG
    // searches the whole ParseState chain). Each enclosing level stays a
    // tier of its own, nearest first: an inner `a` hides an outer one
    // instead of making the reference ambiguous. The enclosing level's own
    // entries carry its nullability (outer joins, quals proven so far)
    // down; the tiers it received already carry their levels'.
    // Planned only when the expression around the sublink is.
    let subplan = expr::plan_fold::next_level_is_subplan();
    let outer_ctx = expr::Ctx::new(outer_scope, outer_null_ctx, snapshot);
    // Or turned into a semi-join before the expression is folded.
    let semijoin = expr::semijoin_level(sel, outer_ctx);
    let _skipped = expr::skipped_if(semijoin.is_none() && !expr::sublink_planned(outer_ctx));
    // The sublink sees the enclosing level's entries its expression does.
    // Its outer levels keep PG's tiering: the enclosing level's own FROM
    // items are the nearest tier, then each level that level received in
    // turn. A name resolves in the nearest tier holding it, so an inner `a`
    // hides an outer `a` instead of making the reference ambiguous.
    let mut outer_tiers: Vec<Vec<crate::scope::TableSource>> =
        Vec::with_capacity(outer_scope.outer_tiers.len() + 1);
    outer_tiers.push(outer_null_ctx.bake_sources(&outer_scope.sources));
    outer_tiers.extend(outer_scope.outer_tiers.iter().cloned());
    let mut analyze = || {
        grouping::with_visible_entries(&outer_scope.sources, || {
            expr::with_outer_grouping(outer_scope, outer_null_ctx, || {
                analyze_select_with_ctes_and_outer(
                    sel,
                    snapshot,
                    params,
                    &outer_scope.ctes,
                    &outer_tiers,
                    // Unreferencable entries stay so inside the sublink
                    // (errorMissingRTE searches every enclosing range
                    // table).
                    &outer_scope.shadowed_sources,
                )
            })
        })
    };
    let (mut cols, p) = match semijoin {
        Some(next) => {
            expr::plan_fold::with_next_level(next, || expr::plan_fold::with_next_semijoin(analyze))
        }
        None => analyze(),
    }?;
    // A planned sublink's plan is started — a SubPlan's always, a
    // semi-join's unless the plan around it is a dummy one.
    super::check_values_sort(sel, snapshot, subplan)?;
    resolve_unknown_outputs(sel, &mut cols, params, snapshot)?;
    Ok((cols, p))
}

/// PG's `resolveTargetListUnknowns` for a subquery, sublink or CTE body:
/// an output column still of type `unknown` (an untyped literal or a bare
/// parameter nothing pinned) becomes `text`, and so does that parameter —
/// `WHERE id = (SELECT $1)` is then `integer = text`, exactly like PG.
/// Set-operation arms and INSERT … SELECT keep their unknowns (PG resolves
/// those against the other arm / the target column) and don't come here.
pub(crate) fn resolve_unknown_outputs(
    sel: &ast::SelectStmt,
    cols: &mut [RawColumn],
    params: &mut ParamCollector,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    expr::resolve_untyped_output_params(sel, snapshot, params)?;
    let direct = sel.op == SetOperation::SetopNone
        && sel.values_lists.is_empty()
        && sel.target_list.len() == cols.len();
    for (i, col) in cols.iter_mut().enumerate() {
        if col.type_oid != oid::UNKNOWN {
            continue;
        }
        col.type_oid = oid::TEXT;
        // (Only a recursive CTE's non-recursive term, which keeps its
        // unknowns, still has an `unknown` value here: analyzeCTETargetList
        // labels its column text, no coercion runs.)
        col.unknown_value = false;
        if direct
            && let Some(ast::Node::ResTarget(rt)) = sel.target_list[i].as_ref()
            && let Some(ast::Node::ParamRef(p)) = rt.val.as_ref()
            && params.get(p.number) == oid::UNKNOWN
        {
            params.record(p.number, oid::TEXT);
        }
    }
    Ok(())
}

pub(crate) fn analyze_select_with_ctes(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, outer_ctes, &[], &[])
}

/// Core SELECT analyzer.
///
/// `outer_tiers` are the enclosing levels this query is nested in, nearest
/// first — PG's parse-state chain: for a `LATERAL` subquery the items to its
/// left, for a plain sublink or a non-`LATERAL` `FROM` subquery the levels
/// around it. A name resolves in the nearest tier holding it and is
/// ambiguous only within one tier.
/// `shadowed_sources` are aliases visible in the enclosing FROM but
/// *unreachable* from inside this query (non-LATERAL FROM subquery). Not
/// used for resolution — only to upgrade the diagnostic from a generic
/// missing-column to PG's `invalid reference to FROM-clause entry for
/// table "x"` when the SQL tries to reach across the boundary.
pub(crate) fn analyze_select_with_ctes_and_outer(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_tiers: &[Vec<crate::scope::TableSource>],
    shadowed_sources: &[crate::scope::TableSource],
) -> AnalyzeResult {
    // What each output column of this level runs is kept apart while its
    // expressions are resolved: the planner drops an output nothing reads
    // (see `privileges::collect_output`).
    let _outputs = crate::privileges::LevelOutputsGuard::enter(sel, snapshot);
    crate::stack::step(|| {
        analyze_select_here(
            sel,
            snapshot,
            params,
            outer_ctes,
            outer_tiers,
            shadowed_sources,
        )
    })
}

thread_local! {
    /// Whether the next query level analyzed keeps its `unknown` outputs
    /// (see [`keeping_unknown_outputs`]).
    static KEEP_UNKNOWN_OUTPUTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f`, which analyzes one query level, with PG's `p_resolve_unknowns`
/// off for that level: a set-operation arm (transformSetOperationTree) and
/// INSERT … SELECT's SELECT (transformInsertStmt) leave an `unknown` output
/// to the coercion the set operation or the target column applies, instead
/// of resolving it as `text`. Every level below resolves its own.
pub(crate) fn keeping_unknown_outputs<T>(f: impl FnOnce() -> T) -> T {
    let prev = KEEP_UNKNOWN_OUTPUTS.with(|k| k.replace(true));
    let r = f();
    KEEP_UNKNOWN_OUTPUTS.with(|k| k.set(prev));
    r
}

/// PG's `resolveTargetListUnknowns` for an `unknown` output that is no
/// literal nor parameter ([`RawColumn::unknown_value`]): its coercion to
/// `text` fails (`SELECT 'a'::text::unknown`).
pub(crate) fn resolve_unknown_values(
    target_list: &[Option<ast::Node>],
    columns: &[RawColumn],
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let Some(i) = columns.iter().position(|c| c.unknown_value) else {
        return Ok(());
    };
    // The entry, when the select list has no `*` before it.
    let node = (target_list.len() == columns.len())
        .then(|| match target_list[i].as_ref() {
            Some(ast::Node::ResTarget(rt)) => rt.val.as_ref(),
            _ => None,
        })
        .flatten();
    expr::unknown_value_coercion(
        oid::TEXT,
        crate::coerce::CoercionContext::Implicit,
        snapshot,
        node,
    )
}

fn analyze_select_here(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_tiers: &[Vec<crate::scope::TableSource>],
    shadowed_sources: &[crate::scope::TableSource],
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    let _plan = expr::plan_fold::PlanLevel::enter_select(sel, outer_ctes.keys(), snapshot);
    super::deferred::record_target_list(sel, snapshot);
    let resolve_unknowns = !KEEP_UNKNOWN_OUTPUTS.with(|k| k.replace(false));
    // transformSetOperationStmt: an INTO on the leftmost member is the
    // statement's only where the statement is the top level, which
    // `analyze_raw_node` strips; here it is out of place.
    if sel.op != SetOperation::SetopNone {
        let mut leftmost = sel;
        while leftmost.op != SetOperation::SetopNone
            && let Some(l) = leftmost.larg.as_deref()
        {
            leftmost = l;
        }
        if let Some(into) = &leftmost.into_clause {
            return Err(crate::pgmsg::select_into_not_allowed(into_span(into)).finalize_implicit());
        }
    }
    let _expressions = expr::Enclosing::boundary();
    // Start with outer CTEs (from parent WITH clause).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();

    // Process this SELECT's own CTEs (before UNION check, since WITH wraps UNION).
    if let Some(with) = &sel.with_clause {
        // A CTE body is analyzed before this level's FROM: it sees the
        // enclosing levels, never this level's own items.
        cte_scopes = super::cte::with_clause_owner(sel, snapshot, || {
            analyze_with_clause(with, snapshot, params, &cte_scopes, outer_tiers)
        })?;
    }

    // Handle UNION/INTERSECT/EXCEPT. The set operation is a query level of
    // its own, without a FROM clause; its arms are the levels below.
    // Outer references reach the arms and the VALUES rows below like any
    // subquery's.
    if sel.op != SetOperation::SetopNone {
        grouping::register_level(sel, &[], &[]);
        let result = analyze_set_operation(
            sel,
            snapshot,
            params,
            &cte_scopes,
            outer_tiers,
            shadowed_sources,
        );
        // The arms' summaries say nothing of the whole.
        set_level_summary(LevelSummary::default());
        return result;
    }

    // Handle `VALUES (…), (…), …` — a `SelectStmt` without a FROM/target
    // list, carrying rows in `values_lists`. Column types are derived from
    // the first row; names default to `column1`/`column2`/… (PG convention)
    // and are typically overridden by a `AS alias(col1, col2)` column list
    // at the RangeSubselect that wraps the VALUES.
    // The rows see the LATERAL and correlated outer levels like any
    // expression of this level (`LATERAL (VALUES (v.a))`).
    if !sel.values_lists.is_empty() {
        // transformValuesClause: a bare VALUES has no relation to lock.
        if let Some(ast::Node::LockingClause(lc)) =
            sel.locking_clause.first().and_then(|n| n.as_ref())
        {
            return Err(crate::pgmsg::locking_values(lock_strength_name(lc)).finalize_implicit());
        }
        grouping::register_level(sel, &[], &[]);
        let mut values_scope = Scope {
            ctes: cte_scopes.clone(),
            ..Scope::default()
        };
        values_scope.outer_tiers.extend(outer_tiers.iter().cloned());
        values_scope
            .shadowed_sources
            .extend(shadowed_sources.iter().cloned());
        let columns = analyze_values_lists(&sel.values_lists, snapshot, params, &values_scope)?;
        let sort_sources =
            values_sort_and_limit(sel, &columns, snapshot, params, &cte_scopes, outer_tiers)?;
        grouping::finish_values_level(sel, &values_scope, &sort_sources, snapshot)?;
        set_level_summary(LevelSummary {
            min_one_row: limit_keeps_a_row(sel),
            ..LevelSummary::default()
        });
        return Ok((columns, None));
    }

    // transformSelectStmt: INTO belongs to the top-level SELECT only (see
    // `analyze_raw_node`).
    if let Some(into) = &sel.into_clause {
        return Err(crate::pgmsg::select_into_not_allowed(into_span(into)).finalize_implicit());
    }

    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    // LATERAL and correlated outer aliases resolve like outer references:
    // the subquery's own FROM wins first, they're excluded from `*`
    // expansion, and two sources sharing a column name are ambiguous only
    // within their own level. Each enclosing level is a tier, nearest first.
    // Shadowed: aliases live only as a hint for the diagnostic when the SQL
    // reaches across the boundary.
    scope.outer_tiers.extend(outer_tiers.iter().cloned());
    scope
        .shadowed_sources
        .extend(shadowed_sources.iter().cloned());
    let mut null_ctx = NullabilityContext::default();
    null_ctx.has_group_by = !sel.group_clause.is_empty();
    null_ctx.srfs_in_lockstep = level_srf_calls(sel, snapshot).len() > 1;
    null_ctx.window_frames = sel
        .window_clause
        .iter()
        .filter_map(|w| match w.as_ref()? {
            ast::Node::WindowDef(wd) => Some((wd.name.clone(), expr::effective_frame_options(wd))),
            _ => None,
        })
        .collect();

    // Process FROM clause. An aggregate of this level inside it (in a
    // LATERAL subquery) is PG's EXPR_KIND_FROM_SUBSELECT error; JOIN
    // conditions and function arguments name their own clause.
    //
    // A level whose WHERE is a constant FALSE (or NULL) is a dummy plan, and
    // so is an inner join whose ON is: the planner replaces those scans with
    // dummy relations, whose security quals are never built. Record the FROM
    // items certainly proven empty so their policy functions aren't checked
    // (`check_row_security`).
    let empty_from =
        certainly_empty_from_aliases(&sel.from_clause, sel.where_clause.as_ref(), snapshot);
    grouping::with_clause(Some("FROM clause of their own query level"), || {
        crate::privileges::with_empty_from_aliases(&empty_from, || {
            process_from_clause(
                &sel.from_clause,
                &mut scope,
                &mut null_ctx,
                snapshot,
                &cte_scopes,
                params,
            )
        })
    })?;
    grouping::register_level(sel, &scope.sources, &scope.shadowed_sources);
    {
        let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
        // Select-list SRFs that all yield as many rows are never padded.
        let lockstep = null_ctx.srfs_in_lockstep
            && !crate::resolve::srfs_have_equal_static_rows(
                crate::resolve::level_srf_calls(sel, snapshot),
                ctx,
                params,
            );
        // An input that always has rows (VALUES, a constant series, …).
        let never_empty = crate::resolve::select_input_never_empty(sel, ctx, params);
        null_ctx.srfs_in_lockstep = lockstep;
        null_ctx.input_not_empty = never_empty;
    }

    // PG transforms the target list right after FROM; note which bare `$N`
    // outputs it sees untyped before WHERE & co. can type them.
    expr::note_untyped_output_params(
        &sel.target_list,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;

    // Expand `GROUPING SETS` / `ROLLUP` / `CUBE`: promote columns that
    // some grouping set omits to nullable, and remember whether any
    // grouping set is empty (drives aggregate-result nullability).
    // Ordinals index the star-expanded target list (findTargetlistEntrySQL92).
    let targets = expanded_target_entries(
        &sel.target_list,
        &scope,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    );
    let expansion = grouping::expand_grouping_sets(&sel.group_clause, &scope, &targets, snapshot);
    null_ctx.set_grouping_omitted(expansion.omitted);
    null_ctx.has_empty_grouping_set = expansion.has_empty_set;
    let omitted_targets = expansion.omitted_targets;

    // Process WHERE clause — PG uses COERCION_ASSIGNMENT + BOOL goal, and
    // emits its own wording on mismatch: `argument of WHERE must be type
    // boolean, not type X`. Catch the generic coerce error and rewrite to
    // PG's exact message so `pglite_sanity` matches.
    let mut correlated: Option<crate::nonnull::Facts> = None;
    let mut own_facts: Option<crate::nonnull::Facts> = None;
    let log = crate::nonnull::StrictLog::default();
    if let Some(where_clause) = &sel.where_clause {
        // The view being defined keeps what its WHERE runs for a write
        // through it (see `privileges::collect_view_qual`).
        crate::privileges::collect_view_qual(|| -> Result<(), crate::AnalyzeError> {
            // PG rejects aggregate / window function calls inside WHERE (they
            // reference the post-aggregation row, not the pre-aggregation one) —
            // but only after the expression itself resolves; the ordering lives
            // in `coerce_bool_clause`.
            expr::plan_fold::with_estimated_qual(
                Some(where_clause),
                &scope,
                snapshot,
                || -> Result<(), crate::AnalyzeError> {
                    crate::clause::coerce_clause_expr(
                        Some(where_clause),
                        expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
                        params,
                        crate::clause::ClauseKind::Where,
                    )?;
                    check_no_srf_in_clause(Some(where_clause), snapshot, "WHERE")?;
                    Ok(())
                },
            )?;
            // What the rows past WHERE have non-NULL — read by everything
            // evaluated after it, and reducing outer joins PG would reduce.
            let facts =
                crate::nonnull::nonnullable(Some(where_clause), true, &scope, &log, snapshot);
            crate::nonnull::note_settled_null_tests(where_clause, &scope, &log, snapshot);
            let own = crate::nonnull::own_aliases(&scope);
            // An EXISTS / LATERAL subquery hands what it proves of the levels
            // around it up (see `nonnull::capture_correlation`).
            if crate::nonnull::correlation_wanted(sel) {
                let outer: std::collections::HashSet<String> = scope
                    .outer_tiers
                    .iter()
                    .flatten()
                    .map(|s| s.alias.clone())
                    .filter(|a| !own.contains(a))
                    .collect();
                correlated = Some(facts.clone().restricted_to(&outer));
            }
            // An `IN` subquery hands what it proves of its own rows up (see
            // `nonnull::capture_own`).
            if crate::nonnull::own_wanted(sel) {
                own_facts = Some(facts.clone().restricted_to(&own));
            }
            if crate::pg_catalog::collecting_column_ref_bindings() {
                null_ctx.note_nullified_joins(where_clause, &scope, &log, snapshot);
            }
            null_ctx.add_where_facts(facts.restricted_to(&own));
            Ok(())
        })?;
    }
    // An empty grouping set yields its row from no input, aggregates or
    // not (`GROUP BY ()`, `GROUPING SETS ((), ())`); so does an aggregate
    // query without GROUP BY.
    null_ctx.where_exprs_off = null_ctx.has_empty_grouping_set
        || (sel.group_clause.is_empty() && may_aggregate(sel, &scope, snapshot));
    // FROM and WHERE always leaving a row: an aggregate without GROUP BY
    // sees rows, and the level yields one.
    let input_nonempty = input_nonempty(sel, &scope, &log, snapshot);
    if input_nonempty {
        null_ctx.input_not_empty = true;
    }

    // Collect the select list's output-column names so GROUP BY can fall
    // back to them when a bare identifier doesn't resolve against the FROM
    // scope. PG accepts `SELECT name AS n FROM t GROUP BY n`; without this
    // fallback, propagating errors from that walk would regress legitimate
    // queries. PG matches the target entry's
    // `resname`, which for an unaliased expression is the name it derives
    // (`count` for `count(*)`); a `*` column is one the FROM scope
    // resolves.
    let select_aliases: std::collections::HashSet<String> = targets
        .iter()
        .filter_map(|t| match t {
            ExpandedTarget::Expr(rt) => Some(target_resname(rt)),
            ExpandedTarget::StarColumn { .. } => None,
        })
        .collect();
    // Every output column's name, as ORDER BY and DISTINCT ON match a bare
    // name against them (`*` columns included).
    let output_names = output_column_names(
        &targets,
        &sel.target_list,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    );

    // Process HAVING clause — same boolean goal as WHERE, but aggregates
    // are of course allowed there.
    if let having @ Some(_) = &sel.having_clause {
        let log = crate::nonnull::StrictLog::default();
        crate::clause::coerce_clause_expr(
            having.as_ref(),
            expr::Ctx::new(&scope, &null_ctx, snapshot).logging_strictness(&log),
            params,
            crate::clause::ClauseKind::Having,
        )?;
        check_no_srf_in_clause(having.as_ref(), snapshot, "HAVING")?;
        // A grouped column HAVING proves non-NULL is so in every group
        // left — the groups' rows share it — even one a grouping set
        // omits (those groups are the ones HAVING drops).
        let facts = crate::nonnull::nonnullable(having.as_ref(), true, &scope, &log, snapshot)
            .restricted_to(&crate::nonnull::own_aliases(&scope));
        if let Some(qual) = having.as_ref() {
            crate::nonnull::note_settled_null_tests(qual, &scope, &log, snapshot);
        }
        null_ctx.add_having_facts(facts);
        // What the groups left must hold: rows, non-NULL aggregated values.
        let facts = crate::having::having_facts(having.as_ref(), &scope, snapshot);
        null_ctx.input_not_empty |= facts.input_not_empty;
        null_ctx.nonnull_agg_inputs = facts.nonnull_inputs;
    }

    // Process ORDER BY expressions. Sort items are wrapped in `SortBy` nodes
    // — we walk the inner expression so parameters referenced there (e.g.
    // `ORDER BY embedding <=> $embedding`) get their types inferred from
    // operator context and any column refs are validated. A bare
    // identifier naming an output column is that column, before any input
    // column (findTargetlistEntrySQL92). Integer literals are *ordinals*:
    // they reference a projection position and must be in range (42P10).
    let n_targets = targets.len();
    // `SELECT DISTINCT` (the plain form parses as one empty node) restricts
    // ORDER BY to expressions that appear in the select list.
    let plain_distinct =
        !sel.distinct_clause.is_empty() && sel.distinct_clause.iter().all(|n| n.is_none());
    for sort_node in &sel.sort_clause {
        let Some(ast::Node::SortBy(sb)) = sort_node.as_ref() else {
            continue;
        };
        let inner = &sb.node;
        if inner.is_none() {
            continue;
        }
        if let Some(ord) = sql92_position_of(inner.as_ref(), "ORDER BY")? {
            if ord < 1 || ord as usize > n_targets {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "ORDER BY",
                    ord,
                    crate::error::node_location(inner.as_ref())
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            if let ExpandedTarget::Expr(rt) = targets[ord as usize - 1] {
                resolve_unknown_target(rt, snapshot, params)?;
            }
            continue;
        }
        // A bare name of an output column is that column, whatever the
        // FROM scope holds; addTargetToSortList resolves its entry still
        // `unknown` as text, before the next key is transformed.
        if let Some(i) = named_output_column(
            inner.as_ref(),
            "ORDER BY",
            &targets,
            &output_names,
            &scope,
            snapshot,
        )? {
            if let ExpandedTarget::Expr(rt) = targets[i] {
                resolve_unknown_target(rt, snapshot, params)?;
            }
            continue;
        }
        expr::infer_expr(
            inner.as_ref(),
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            TypeGoal::NONE,
        )?;
        expr::resolve_unknown_key(inner.as_ref(), snapshot, params)?;
        if plain_distinct && !sort_expr_in_select_list(inner.as_ref(), &targets, &scope, snapshot) {
            return Err(crate::pgmsg::distinct_order_by_not_in_select_list(
                crate::error::node_location(inner.as_ref())
                    .and_then(crate::error::SourceSpan::from_node_qname),
            )
            .finalize_implicit());
        }
    }

    // Process GROUP BY expressions — after ORDER BY, as transformSelectStmt
    // does (the first clause to type a `$N` fixes its type) — no type
    // expectation, but we still need
    // to walk them so any parameters referenced are collected and typed
    // and column refs validated. `GroupingSet` nodes (`GROUPING SETS` /
    // `ROLLUP` / `CUBE`) are not real expressions; recurse into their
    // `content` to reach the underlying column references and
    // aggregate-rejection checks.
    grouping::with_clause(Some("GROUP BY"), || {
        for group_node in &sel.group_clause {
            walk_group_clause_node(
                group_node.as_ref(),
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                params,
                &select_aliases,
                &targets,
            )?;
        }
        Ok::<(), AnalyzeError>(())
    })?;

    // Process `DISTINCT ON (…)` expressions the same way — they resolve
    // like ORDER BY items (select-list aliases allowed). A plain `DISTINCT`
    // parses as a single empty node; skip it. Without this walk, a `$N`
    // referenced only inside DISTINCT ON was never registered with the
    // collector and analysis died on the param-count invariant.
    // transformDistinctOnClause transforms every key, then resolves those
    // still `unknown` as text (addTargetToGroupList): `unknown_keys` are
    // the select-list entries keys name, and the bare `$N` keys transformed
    // untyped.
    let mut unknown_keys: Vec<Result<&ast::ResTarget, &ast::ParamRef>> = Vec::new();
    for distinct_node in &sel.distinct_clause {
        if let Some(ord) = sql92_position_of(distinct_node.as_ref(), "DISTINCT ON")? {
            if ord < 1 || ord as usize > n_targets {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "DISTINCT ON",
                    ord,
                    crate::error::node_location(distinct_node.as_ref())
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            if let ExpandedTarget::Expr(rt) = targets[ord as usize - 1] {
                unknown_keys.push(Ok(rt));
            }
            continue;
        }
        if let Some(i) = named_output_column(
            distinct_node.as_ref(),
            "DISTINCT ON",
            &targets,
            &output_names,
            &scope,
            snapshot,
        )? {
            if let ExpandedTarget::Expr(rt) = targets[i] {
                unknown_keys.push(Ok(rt));
            }
            continue;
        }
        if let Some(p) = expr::bare_param(distinct_node.as_ref(), snapshot)
            && params.bound_arg(p.number).is_none()
            && params.get(p.number) == oid::UNKNOWN
        {
            unknown_keys.push(Err(p));
        }
        if distinct_node.is_some() {
            expr::infer_expr(
                distinct_node.as_ref(),
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                params,
                TypeGoal::NONE,
            )?;
        }
    }
    for key in unknown_keys {
        match key {
            Ok(rt) => resolve_unknown_target(rt, snapshot, params)?,
            Err(p) => expr::coerce_untyped_param(p, oid::TEXT, snapshot, params)?,
        }
    }

    // Named-window references: `OVER w` (and `OVER (w …)` inheritance) must
    // name a window defined in this SELECT's WINDOW clause — PG (42704):
    // `window "w" does not exist`. Window calls only appear in the target
    // list, ORDER BY and DISTINCT ON.
    let defined_windows: std::collections::HashSet<&str> = sel
        .window_clause
        .iter()
        .filter_map(|n| match n.as_ref()? {
            ast::Node::WindowDef(w) if !w.name.is_empty() => Some(w.name.as_str()),
            _ => None,
        })
        .collect();
    for t in &sel.target_list {
        if let Some(ast::Node::ResTarget(rt)) = t.as_ref()
            && rt.val.is_some()
        {
            check_window_refs(rt.val.as_ref(), &defined_windows)?;
        }
    }
    for sort_node in &sel.sort_clause {
        if let Some(ast::Node::SortBy(sb)) = sort_node.as_ref()
            && sb.node.is_some()
        {
            check_window_refs(sb.node.as_ref(), &defined_windows)?;
        }
    }
    for distinct_node in &sel.distinct_clause {
        check_window_refs(distinct_node.as_ref(), &defined_windows)?;
    }
    // The WINDOW clause's own definitions, and the inline windows that
    // inherit from them (PG's transformWindowDefinitions).
    // PG transforms OFFSET / LIMIT before the WINDOW clause.
    analyze_limit_offset(sel, expr::Ctx::new(&scope, &null_ctx, snapshot), params)?;
    expr::check_window_clause(sel, expr::Ctx::new(&scope, &null_ctx, snapshot), params)?;

    // Resolve target list (SELECT expressions) — no type expectation. The
    // replay after FROM analyzed its subqueries in full, except under a
    // `(…).*`, which it leaves to this pass.
    let mut resolve_targets = || {
        resolve_target_list_explicit(
            &sel.target_list,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
        )
    };
    let (mut columns, explicit_collations) = if star_over_expression(&sel.target_list) {
        resolve_targets()
    } else {
        expr::with_targets_replayed(resolve_targets)
    }?;
    // An entry that is a grouping item of its own some grouping set omits
    // is NULL in that set's rows, whatever the item its key reads (unless
    // an alias' annotation settles it).
    for &i in &omitted_targets {
        let annotated = matches!(
            targets.get(i),
            Some(ExpandedTarget::Expr(rt))
                if super::type_resolution::annotation_nullability(&rt.name).is_some()
        );
        if let Some(c) = columns.get_mut(i)
            && !annotated
        {
            c.nullable = true;
        }
    }
    grouping::set_output_columns(
        sel,
        columns.iter().map(|c| c.name.clone()).collect(),
        explicit_collations,
    );

    // CASE / COALESCE / aggregate and window arguments may not contain
    // set-returning functions anywhere in this level.
    for n in sel
        .target_list
        .iter()
        .chain(std::iter::once(&sel.where_clause))
        .chain(std::iter::once(&sel.having_clause))
        .chain(sel.sort_clause.iter())
        .chain(sel.group_clause.iter())
    {
        check_srf_nesting(n.as_ref(), snapshot)?;
    }

    check_order_by_using(
        &sel.sort_clause,
        &sel.target_list,
        &columns,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
    check_distinct_on_matches_order_by(sel, &columns, &scope, snapshot)?;
    // In a subquery the planner pulls up, the only keys are its WINDOW
    // clause's, which it never plans (see `check_window_clause`).
    expr::plan_fold::unless_dead(expr::plan_fold::level_pulled_up(), || {
        check_sort_group_keys(
            sel,
            &targets,
            &columns,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
        )
    })?;
    if resolve_unknowns {
        resolve_unknown_values(&sel.target_list, &columns, snapshot)?;
    }

    // `FOR UPDATE [OF …]` (and FOR SHARE / NO KEY UPDATE / KEY SHARE) —
    // PG checks it last in transformSelectStmt.
    check_locking_clause(sel, &scope, &null_ctx, snapshot)?;

    // Aggregate ownership and parseCheckAggregates — which PG runs after
    // everything above.
    grouping::finish_select_level(sel, &scope, &targets, snapshot)?;

    // Every row comes from one WHERE passed — unless an aggregate query
    // without GROUP BY (or with an empty grouping set) makes a row of an
    // empty input.
    if let Some(facts) = correlated {
        let rows_from_where = if sel.group_clause.is_empty() {
            sel.having_clause.is_none() && !grouping::level_info(sel).is_none_or(|l| l.has_aggs)
        } else {
            !null_ctx.has_empty_grouping_set
        };
        if rows_from_where {
            crate::nonnull::deposit_correlation(sel, facts);
        }
    }
    if let Some(facts) = own_facts {
        let rows_from_where = sel.group_clause.is_empty()
            && sel.having_clause.is_none()
            && !grouping::level_info(sel).is_some_and(|l| l.has_aggs);
        let target = match sel.target_list.as_slice() {
            [t] => match t.as_ref() {
                Some(ast::Node::ResTarget(rt)) => rt
                    .val
                    .as_ref()
                    .and_then(|v| crate::nonnull::plain_column(v, &scope))
                    .map(|(col, _)| col),
                _ => None,
            },
            _ => None,
        };
        if rows_from_where {
            crate::nonnull::deposit_own(
                sel,
                crate::nonnull::OwnRows {
                    facts,
                    sources: scope
                        .sources
                        .iter()
                        .map(|s| (s.alias.clone(), s.relid))
                        .collect(),
                    target,
                },
            );
        }
    }

    let has_aggs = grouping::level_info(sel).is_some_and(|l| l.has_aggs);
    let mut summary = finish_level(
        sel,
        &scope,
        has_aggs,
        input_nonempty,
        &mut columns,
        snapshot,
    );
    // The level model knows when the plan-time checks are on.
    let not_dummy = expr::plan_time_checks()
        && !expr::plan_fold::level_may_be_empty()
        && !expr::plan_fold::level_may_be_dummy();
    summary.removable_scan = removable_scan(
        sel,
        &columns,
        &|name| outer_ctes.contains_key(name),
        not_dummy,
        snapshot,
    );
    summary.scan_facts = scan_facts(sel, &scope, &columns, snapshot);
    set_planned_top(sel, &scope, &mut columns, snapshot);
    set_level_summary(summary);
    Ok((columns, None))
}

/// See [`super::LevelSummary::removable_scan`]: whether a SubqueryScan
/// over `sel` (with outputs `columns`, reading the CTEs `cte` names),
/// should it be a trivial one, is certainly removed. Not when the planner
/// may prove the level empty: unless `not_dummy` (the level model of
/// [`expr::plan_fold`] says it can't), only a level without quals reading
/// nothing but plain tables without row security is sure not to be; and
/// a partitioned table read anywhere below may have no partition left
/// once pruned. Not when its target list may carry a resjunk entry: a
/// row mark (`FOR UPDATE`), a sort, grouping or DISTINCT ON key that is no
/// output (an ordinal, or for ORDER BY and DISTINCT ON an output's name,
/// is one), a window's key.
pub(crate) fn removable_scan(
    sel: &ast::SelectStmt,
    columns: &[RawColumn],
    cte: &dyn Fn(&str) -> bool,
    not_dummy: bool,
    snapshot: &PgCatalog,
) -> bool {
    if sel.op != SetOperation::SetopNone
        || !sel.values_lists.is_empty()
        || !sel.locking_clause.is_empty()
        || !sel.window_clause.is_empty()
    {
        return false;
    }
    let own_cte = |name: &str| {
        cte(name)
            || sel.with_clause.as_ref().is_some_and(|w| {
                w.ctes.iter().any(|c| {
                    matches!(c.as_ref(), Some(ast::Node::CommonTableExpr(c))
                        if c.ctename == name)
                })
            })
    };
    let plain_table = |n: Option<&ast::Node>| match n {
        Some(ast::Node::RangeVar(rv)) => {
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            (schema.is_some() || !own_cte(&rv.relname))
                && snapshot
                    .resolve_table(schema, &rv.relname)
                    .is_some_and(|c| {
                        c.relkind == crate::pg_catalog::RelKind::Table
                            && !snapshot.row_security.contains(&c.oid)
                    })
        }
        _ => false,
    };
    if !not_dummy
        && (sel.where_clause.is_some()
            || sel.having_clause.is_some()
            || !sel.from_clause.iter().all(|n| plain_table(n.as_ref())))
    {
        return false;
    }
    let ordinal = |n: Option<&ast::Node>| {
        matches!(
            n,
            Some(ast::Node::AConst(c)) if matches!(c.val, Some(ast::AConstValue::Integer(_)))
        )
    };
    let output_name = |n: Option<&ast::Node>| match n {
        Some(ast::Node::ColumnRef(cr)) => match cr.fields.as_slice() {
            [f] => matches!(f.as_ref(), Some(ast::Node::String(s))
                if columns.iter().filter(|c| c.name == s.sval).count() == 1),
            _ => false,
        },
        _ => false,
    };
    let sort_keys_are_outputs = sel.sort_clause.iter().all(|s| match s.as_ref() {
        Some(ast::Node::SortBy(sb)) => {
            sb.node.is_some() && (ordinal(sb.node.as_ref()) || output_name(sb.node.as_ref()))
        }
        _ => false,
    });
    // A plain DISTINCT is one empty node.
    let distinct_keys_are_outputs = sel
        .distinct_clause
        .iter()
        .all(|d| d.is_none() || ordinal(d.as_ref()) || output_name(d.as_ref()));
    if !sort_keys_are_outputs
        || !distinct_keys_are_outputs
        || !sel.group_clause.iter().all(|n| ordinal(n.as_ref()))
    {
        return false;
    }
    // A window call's PARTITION BY / ORDER BY keys are resjunk entries
    // unless outputs; a partitioned table read below (or an outer CTE,
    // whose query this level doesn't see) may be empty.
    let mut todo: Vec<typedpg_pg_query::NodeRef<'_>> = sel
        .target_list
        .iter()
        .chain(&sel.from_clause)
        .filter_map(|n| n.as_ref())
        .map(|n| n.to_ref())
        .collect();
    if let Some(with) = &sel.with_clause {
        todo.extend(
            with.ctes
                .iter()
                .filter_map(|n| n.as_ref())
                .map(|n| n.to_ref()),
        );
    }
    while let Some(n) = todo.pop() {
        match n {
            // A window's PARTITION BY / ORDER BY keys (an `OVER ()` has none).
            typedpg_pg_query::NodeRef::FuncCall(fc)
                if fc.over.as_ref().is_some_and(|w| {
                    !w.partition_clause.is_empty()
                        || !w.order_clause.is_empty()
                        || !w.refname.is_empty()
                        || !w.name.is_empty()
                }) =>
            {
                return false;
            }
            typedpg_pg_query::NodeRef::RangeVar(rv) => {
                if rv.schemaname.is_empty() && cte(&rv.relname) {
                    return false;
                }
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                if let Some(class) = snapshot.resolve_table(schema, &rv.relname)
                    && reads_partitioned(snapshot, class.oid)
                {
                    return false;
                }
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    true
}

/// Whether relation `relid` is a partitioned table, or a view reading one
/// (as it bound its relations), at any depth.
fn reads_partitioned(snapshot: &PgCatalog, relid: crate::oid::PgClassOid) -> bool {
    use crate::pg_catalog::RelKind;
    let mut todo = vec![relid];
    let mut seen = std::collections::HashSet::new();
    while let Some(rel) = todo.pop() {
        if !seen.insert(rel) {
            continue;
        }
        match snapshot.pg_class.get(&rel).map(|c| c.relkind) {
            Some(RelKind::Partitioned) => return true,
            Some(RelKind::View) => {
                for r in snapshot.view_references.get(&rel).into_iter().flatten() {
                    if let crate::ddl::depend::Reference::Object(a) = r
                        && a.classid == crate::pg_catalog::PG_CLASS_RELID
                    {
                        todo.push(crate::oid::PgClassOid::from_nonzero(a.objid.into_nonzero()));
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// What an output of a query level is to `trivial_subqueryscan`.
enum TopOutput<'a> {
    /// A Var of a FROM entry of the level (or a same-type cast of one,
    /// which parse analysis drops).
    Var(&'a ScopeColumn),
    /// An expression planning certainly folds to a Var of a FROM entry of
    /// the level (a CASE whose branch is decided by constants): a Var in
    /// the plan, though not as parsed.
    FoldedVar(&'a ScopeColumn),
    /// An expression planning may reduce to a Var (a CASE or COALESCE it
    /// folds) of one of these FROM entries' columns.
    MaybeVar(Vec<&'a ScopeColumn>),
    /// Never a Var of the level.
    Other,
}

/// Set [`RawColumn::planned_top`] for the outputs `columns` of SELECT
/// `sel`, whose FROM entries `scope` holds: where the level reads a single
/// subquery, and its target list is that subquery's columns in order and
/// nothing else, with no clause that adds a plan node or a qual above the
/// SubqueryScan, setrefs removes the scan (`trivial_subqueryscan`) and the
/// subquery's own target list, with the collations planning gave it, is
/// the plan's. Where the scan certainly stays, or a plan node certainly
/// sits above it, a Var of it keeps its parsed collation; where the
/// analyzer can't tell, the collation is unknown.
fn set_planned_top(
    sel: &ast::SelectStmt,
    scope: &Scope,
    columns: &mut [RawColumn],
    snapshot: &PgCatalog,
) {
    use crate::expr::Planned;
    use crate::expr::plan_fold::Folded;
    use crate::scope::ScanVar;
    let local = |c: &ScopeColumn| {
        scope
            .sources
            .iter()
            .any(|s| s.columns.iter().any(|x| std::ptr::eq(x, c)))
    };
    // The SubqueryScan column a column of the level's entries is a Var of:
    // a subquery's planned on its own, or (pulled up) one's Var of such a
    // scan.
    let scan_of = |c: &ScopeColumn| -> Option<ScanVar> {
        if let Some(sv) = c.deferred.scan_var {
            return Some(sv);
        }
        if !c.deferred.subquery_scan {
            return None;
        }
        scope.sources.iter().find_map(|s| {
            let index = s.columns.iter().position(|x| std::ptr::eq(x, c))?;
            Some(ScanVar {
                index,
                len: s.columns.len(),
                planned_top: c.deferred.planned_top,
                facts: c.deferred.scan_facts,
            })
        })
    };
    // Nothing to do unless a column of the level's entries is a subquery's.
    if !scope.sources.iter().flat_map(|s| &s.columns).any(|c| {
        c.deferred.planned_top.is_some()
            || c.deferred.subquery_scan
            || c.deferred.scan_var.is_some()
    }) {
        return;
    }
    let differs = |c: &ScopeColumn| c.deferred.planned_top.is_some_and(|p| p != c.planned());
    let mut outputs: Vec<TopOutput<'_>> = Vec::new();
    let mut opaque = false;
    for t in &sel.target_list {
        let Some(ast::Node::ResTarget(rt)) = t.as_ref() else {
            continue;
        };
        let val = &rt.val;
        if val.is_none() {
            continue;
        }
        match val.as_ref() {
            Some(ast::Node::ColumnRef(cr))
                if cr
                    .fields
                    .iter()
                    .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_)))) =>
            {
                let table = cr.fields.iter().rev().find_map(|f| match f.as_ref()? {
                    ast::Node::String(s) => Some(s.sval.as_str()),
                    _ => None,
                });
                let cols: Vec<&ScopeColumn> = match table {
                    Some(t) => match scope.find_source(t) {
                        Some(src) => src.columns.iter().collect(),
                        None => {
                            opaque = true;
                            Vec::new()
                        }
                    },
                    None => scope
                        .sources
                        .iter()
                        .flat_map(|s| s.visible_columns())
                        .collect(),
                };
                outputs.extend(cols.into_iter().map(|c| {
                    if local(c) {
                        TopOutput::Var(c)
                    } else {
                        TopOutput::Other
                    }
                }));
            }
            Some(ast::Node::ColumnRef(cr)) => match column_ref_target(cr, scope) {
                Some(c) if local(c) => outputs.push(TopOutput::Var(c)),
                Some(_) => outputs.push(TopOutput::Other),
                None => opaque = true,
            },
            _ => {
                let i = outputs.len();
                if let Some((folded, c)) = folds_to_column(val.as_ref(), scope, snapshot)
                    && local(c)
                    && columns.get(i).is_some_and(|o| o.type_oid == c.type_oid)
                {
                    outputs.push(if folded {
                        TopOutput::FoldedVar(c)
                    } else {
                        TopOutput::Var(c)
                    });
                    continue;
                }
                let mut cols = Vec::new();
                let mut todo: Vec<_> = val.as_ref().map(|n| n.to_ref()).into_iter().collect();
                while let Some(n) = todo.pop() {
                    match n {
                        // A sublink's columns are its own query's.
                        typedpg_pg_query::NodeRef::SubLink(_) => continue,
                        typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                            match column_ref_target(cr, scope) {
                                Some(c) if local(c) => cols.push(c),
                                Some(_) => {}
                                None => opaque = true,
                            }
                            continue;
                        }
                        _ => {}
                    }
                    todo.extend(n.children());
                }
                let mut cast = val;
                while let Some(ast::Node::TypeCast(tc)) = cast.as_ref()
                    && tc.arg.is_some()
                {
                    cast = &tc.arg;
                }
                let may_be_var = matches!(cast.as_ref(), Some(ast::Node::ColumnRef(_)))
                    || columns
                        .get(i)
                        .is_some_and(|c| c.planned != Planned::AsParsed);
                outputs.push(if cols.is_empty() || !may_be_var {
                    TopOutput::Other
                } else {
                    TopOutput::MaybeVar(cols)
                });
            }
        }
    }
    if opaque || outputs.len() != columns.len() {
        // Which column an output reads is unknown.
        if scope.sources.iter().flat_map(|s| &s.columns).any(differs) {
            for c in columns.iter_mut() {
                c.planned_top = Some(Planned::Unknown);
            }
        }
        return;
    }
    let single = match (sel.from_clause.as_slice(), scope.sources.as_slice()) {
        ([item], [src]) if !matches!(item.as_ref(), Some(ast::Node::JoinExpr(_))) => Some(src),
        _ => None,
    };
    // limit_needed: a NULL count, a NULL or zero offset, makes no Limit
    // node; another constant does.
    let constant = |n: Option<&ast::Node>| match n {
        None => Some(None),
        Some(ast::Node::AConst(c)) if c.isnull => Some(None),
        Some(ast::Node::AConst(c)) => match &c.val {
            Some(ast::AConstValue::Integer(i)) => Some(Some(i.ival)),
            _ => None,
        },
        _ => None,
    };
    let (count, offset) = (
        constant(sel.limit_count.as_ref()),
        constant(sel.limit_offset.as_ref()),
    );
    let no_limit = count == Some(None) && matches!(offset, Some(None | Some(0)));
    let limit_node = matches!(count, Some(Some(_))) || matches!(offset, Some(Some(o)) if o != 0);
    // What preprocessing makes of the WHERE clause: a TRUE constant is
    // dropped; a FALSE (or NULL) one makes the relation a dummy one, a
    // Result over the level's Vars; a qual that is no constant (one no
    // custom plan's parameter values or sublink may turn into one) stays
    // above the scan, unless pushed down into the subquery.
    let where_fold = sel.where_clause.is_some().then(|| {
        let w = &sel.where_clause;
        let fixed = !contains_node(w.as_ref(), &|n| {
            matches!(
                n,
                typedpg_pg_query::NodeRef::ParamRef(_) | typedpg_pg_query::NodeRef::SubLink(_)
            )
        });
        (
            crate::expr::plan_fold::fold_qual(w.as_ref(), snapshot),
            fixed,
        )
    });
    let where_none = matches!(where_fold, None | Some((Folded::Bool(true), _)));
    let where_false = matches!(where_fold, Some((Folded::Bool(false), _)));
    let where_var = matches!(where_fold, Some((Folded::Var, true)));
    let no_clauses = where_none
        && sel.group_clause.is_empty()
        && sel.having_clause.is_none()
        && sel.distinct_clause.is_empty()
        && sel.sort_clause.is_empty()
        && no_limit
        && sel.locking_clause.is_empty()
        && sel.window_clause.is_empty();
    let scan = |o: &TopOutput<'_>| match o {
        TopOutput::Var(v) | TopOutput::FoldedVar(v) => scan_of(v),
        _ => None,
    };
    // Where the scan certainly stays, or a plan node sits above it: a Var
    // of it is the plan's as parsed (as long as this level is the top:
    // pulled up, see `RawColumn::scan_stays`); a folded one is what the
    // level's own folding makes of it.
    let keep = |columns: &mut [RawColumn], outputs: &[TopOutput<'_>]| {
        for (col, o) in columns.iter_mut().zip(outputs) {
            col.planned_top = match o {
                TopOutput::Var(_) if scan(o).is_some() => {
                    col.scan_stays = true;
                    Some(Planned::AsParsed)
                }
                TopOutput::Var(v) | TopOutput::FoldedVar(v) if differs(v) => Some(Planned::Unknown),
                TopOutput::MaybeVar(cs) if cs.iter().any(|c| differs(c)) => Some(Planned::Unknown),
                _ => None,
            };
        }
    };
    if single.is_some()
        && let Some(first) = outputs.iter().find_map(&scan)
    {
        // The level reads a SubqueryScan (pulled up, the level reading
        // this one reads it, with this target list: no qual of this
        // level's is left above it).
        if where_none {
            for (col, o) in columns.iter_mut().zip(&outputs) {
                if let TopOutput::Var(v) = o {
                    col.scan_var = scan_of(v);
                }
            }
        }
        let (len, facts) = (first.len, first.facts);
        // The WHERE reads nothing but the scan's Vars: no pulled-up
        // expression a substitution may fold.
        let where_reads_scan = sel.where_clause.is_some() && {
            let w = &sel.where_clause;
            !contains_node(w.as_ref(), &|n| match n {
                typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                    column_ref_target(cr, scope).is_none_or(|c| !local(c) || scan_of(c).is_none())
                }
                _ => false,
            })
        };
        let trivial = outputs.len() == len
            && outputs
                .iter()
                .enumerate()
                .all(|(i, o)| scan(o).is_some_and(|sv| sv.index == i && sv.len == len));
        if trivial && no_clauses && !facts.kept {
            // The scan is trivial: removed where the subquery allows it.
            for (col, o) in columns.iter_mut().zip(&outputs) {
                col.planned_top = scan(o).and_then(|sv| sv.planned_top);
            }
            return;
        }
        // An ORDER BY on the scan's Vars sorts them in a Sort node above
        // the scan, unless a path of the subquery is sorted already, or
        // a WHERE equates a key to a constant (or is pushed down into the
        // subquery, which may then sort). A key with no collation is
        // sorted by no path of a subquery whose sorts are all collated
        // (its pathkey has no collation; theirs have one): a Sort, or an
        // Incremental Sort, is above the scan. `key_var` tells whether a
        // key is a Var of the scan, and whether it has no collation.
        let no_collation = |o: &TopOutput<'_>| matches!(o, TopOutput::Var(v) if v.no_collation);
        let key_var = |k: Option<&ast::Node>| -> Option<bool> {
            match k {
                Some(ast::Node::AConst(c)) => match &c.val {
                    Some(ast::AConstValue::Integer(i)) => usize::try_from(i.ival)
                        .ok()
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| outputs.get(i))
                        .filter(|o| scan(o).is_some())
                        .map(no_collation),
                    _ => None,
                },
                Some(ast::Node::ColumnRef(cr)) => {
                    let by_name = match cr.fields.as_slice() {
                        [f] => match f.as_ref() {
                            Some(ast::Node::String(s)) => {
                                let named: Vec<_> = columns
                                    .iter()
                                    .zip(&outputs)
                                    .filter(|(c, _)| c.name == s.sval)
                                    .map(|(_, o)| o)
                                    .collect();
                                (!named.is_empty()).then(|| {
                                    named
                                        .iter()
                                        .all(|o| scan(o).is_some())
                                        .then(|| named.iter().all(|o| no_collation(o)))
                                })
                            }
                            _ => Some(None),
                        },
                        _ => None,
                    };
                    by_name.unwrap_or_else(|| {
                        column_ref_target(cr, scope)
                            .filter(|c| local(c) && scan_of(c).is_some())
                            .map(|c| c.no_collation)
                    })
                }
                _ => None,
            }
        };
        let keys: Option<Vec<bool>> = sel
            .sort_clause
            .iter()
            .map(|s| match s.as_ref() {
                Some(ast::Node::SortBy(sb)) => key_var(sb.node.as_ref()),
                _ => None,
            })
            .collect();
        let sorted = !sel.sort_clause.is_empty()
            && sel.where_clause.is_none()
            && sel.group_clause.is_empty()
            && sel.having_clause.is_none()
            && sel.distinct_clause.is_empty()
            && sel.window_clause.is_empty()
            && keys.is_some_and(|keys| {
                facts.unsorted || (facts.collated_sorts && keys.iter().any(|&none| none))
            });
        // trivial_subqueryscan requires a target list as long as the
        // subquery's, of its Vars in order (or Consts equal to its
        // entries), and no qual on the scan; a dummy relation, or a node
        // above the scan (LockRows, Limit, Sort, a Result gating a FALSE
        // qual), has the Vars in its own target list.
        let stays = outputs.len() != len
            || outputs
                .iter()
                .enumerate()
                .any(|(i, o)| scan(o).is_some_and(|sv| sv.index != i || sv.len != len))
            || facts.kept
            || !sel.locking_clause.is_empty()
            || limit_node
            || where_false
            || (where_var && facts.no_pushdown && where_reads_scan)
            || sorted;
        if stays {
            keep(columns, &outputs);
            return;
        }
    } else if let Some(src) = single {
        let all_vars_in_order = outputs.len() == src.columns.len()
            && outputs.iter().zip(&src.columns).all(|(o, c)| match o {
                TopOutput::Var(v) => std::ptr::eq(*v, c),
                _ => false,
            });
        if all_vars_in_order && no_clauses {
            // A pulled-up subquery: its own top level's collations.
            for (col, o) in columns.iter_mut().zip(&outputs) {
                if let TopOutput::Var(v) = o {
                    col.planned_top = v.deferred.planned_top;
                }
            }
            return;
        }
    } else if outputs.iter().any(|o| scan(o).is_some())
        && sel
            .from_clause
            .iter()
            .any(|f| never_removed(f.as_ref(), sel, scope, snapshot))
    {
        // A join with a relation the planner never removes: the scan's
        // Vars are the join's (or a dummy relation's Result's).
        keep(columns, &outputs);
        return;
    }
    // The planner may or may not leave a trivial scan.
    for (col, o) in columns.iter_mut().zip(&outputs) {
        let maybe = match o {
            TopOutput::Var(c) | TopOutput::FoldedVar(c) => differs(c),
            TopOutput::MaybeVar(cs) => cs.iter().any(|c| differs(c)),
            TopOutput::Other => false,
        };
        if maybe {
            col.planned_top = Some(Planned::Unknown);
        }
    }
}

/// Whether FROM item `item` of `sel` holds a relation the planner never
/// removes from the join tree: a table (a partitioned one may be proven
/// empty, which makes the join a dummy relation, not the other side
/// alone) not on the nullable side of an outer join — or on one
/// `join_is_removable` can't drop (see [`nullable_side_kept`]).
fn never_removed(
    item: Option<&ast::Node>,
    sel: &ast::SelectStmt,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> bool {
    match item {
        Some(ast::Node::RangeVar(rv)) => relation_source(rv, scope, snapshot).is_some(),
        Some(ast::Node::JoinExpr(j)) => {
            let side = |n: Option<&ast::Node>| never_removed(n, sel, scope, snapshot);
            let nullable = |n: Option<&ast::Node>| nullable_side_kept(n, j, sel, scope, snapshot);
            match j.jointype {
                ast::JoinType::JoinInner | ast::JoinType::JoinFull => {
                    side(j.larg.as_ref()) || side(j.rarg.as_ref())
                }
                ast::JoinType::JoinLeft => side(j.larg.as_ref()) || nullable(j.rarg.as_ref()),
                ast::JoinType::JoinRight => side(j.rarg.as_ref()) || nullable(j.larg.as_ref()),
                _ => false,
            }
        }
        _ => false,
    }
}

/// The FROM entry of `scope` RangeVar `rv` is, when it is a table,
/// partitioned table or materialized view.
fn relation_source<'a>(
    rv: &ast::RangeVar,
    scope: &'a Scope,
    snapshot: &PgCatalog,
) -> Option<&'a crate::scope::TableSource> {
    use crate::pg_catalog::RelKind;
    let name = rv
        .alias
        .as_ref()
        .map_or(rv.relname.as_str(), |a| a.aliasname.as_str());
    scope.sources.iter().find(|s| {
        s.alias == name
            && matches!(s.kind, crate::scope::SourceKind::Relation)
            && s.relid.is_some_and(|r| {
                snapshot.pg_class.get(&r).is_some_and(|c| {
                    matches!(
                        c.relkind,
                        RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView
                    )
                })
            })
    })
}

/// Whether `item`, the nullable side of outer join `j` of `sel`, certainly
/// stays in the join tree (and so the join does): `join_is_removable`
/// drops only a single relation (two tables inner-joined there make its
/// `min_righthand` no singleton — tables with no unique index, which
/// self-join elimination can't merge either) that is proven distinct for
/// the join: a unique index (`rel_supports_distinctness`) each key column
/// of which a clause of the join equates to the other side or a constant
/// (`relation_has_unique_index_for`), none of its columns used above the
/// join (in the target list or a WHERE qual, which is pushed down).
fn nullable_side_kept(
    item: Option<&ast::Node>,
    j: &ast::JoinExpr,
    sel: &ast::SelectStmt,
    scope: &Scope,
    snapshot: &PgCatalog,
) -> bool {
    let unique_index = |relid| {
        snapshot
            .pg_index
            .values()
            .any(|i| i.indrelid == relid && i.indisunique)
    };
    let rv = match item {
        Some(ast::Node::RangeVar(rv)) => rv,
        Some(ast::Node::JoinExpr(inner)) => {
            let plain = |n: Option<&ast::Node>| match n {
                Some(ast::Node::RangeVar(rv)) => relation_source(rv, scope, snapshot)
                    .and_then(|s| s.relid)
                    .filter(|&r| !unique_index(r)),
                _ => None,
            };
            return inner.jointype == ast::JoinType::JoinInner
                && plain(inner.larg.as_ref()).is_some()
                && plain(inner.rarg.as_ref()).is_some();
        }
        _ => return false,
    };
    let Some(source) = relation_source(rv, scope, snapshot) else {
        return false;
    };
    let Some(relid) = source.relid else {
        return false;
    };
    if j.is_natural {
        return false;
    }
    // The column names the join's clauses mention, whichever entry's.
    let mut mentioned: Vec<&str> = j
        .using_clause
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    if let Some(quals) = j.quals.as_ref() {
        let mut todo: Vec<_> = vec![quals.to_ref()];
        while let Some(n) = todo.pop() {
            if let typedpg_pg_query::NodeRef::ColumnRef(cr) = n
                && let Some(ast::Node::String(s)) = cr.fields.last().and_then(|f| f.as_ref())
            {
                mentioned.push(s.sval.as_str());
            }
            todo.extend(n.children());
        }
    }
    let attributes = snapshot.attributes_of(relid);
    let provable = snapshot.pg_index.values().any(|i| {
        i.indrelid == relid
            && i.indisunique
            && i.indkey
                .iter()
                .take(usize::try_from(i.indnkeyatts).unwrap_or(0))
                .all(|&attnum| {
                    attnum == 0
                        || attributes
                            .iter()
                            .find(|a| a.attnum == attnum)
                            .is_none_or(|a| mentioned.contains(&a.attname.as_str()))
                })
    });
    if !provable {
        return true;
    }
    // A column of the relation read above the join (outside sublinks,
    // whose names may be their own entries').
    let reads = |n: Option<&ast::Node>| {
        contains_node_outside_sublinks(n, &|n| match n {
            typedpg_pg_query::NodeRef::ColumnRef(cr) => {
                let star = cr
                    .fields
                    .last()
                    .is_some_and(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))));
                if star {
                    match cr.fields.as_slice() {
                        [_] => !source.columns.is_empty(),
                        [t, _] => matches!(t.as_ref(),
                            Some(ast::Node::String(s)) if s.sval == source.alias),
                        _ => false,
                    }
                } else {
                    column_ref_target(cr, scope)
                        .is_some_and(|c| source.columns.iter().any(|x| std::ptr::eq(x, c)))
                }
            }
            _ => false,
        })
    };
    sel.target_list.iter().any(|n| reads(n.as_ref())) || reads(sel.where_clause.as_ref())
}

/// Whether `n` holds a node `pred` accepts, not looking into sublinks.
fn contains_node_outside_sublinks(
    n: Option<&ast::Node>,
    pred: &dyn Fn(typedpg_pg_query::NodeRef<'_>) -> bool,
) -> bool {
    let Some(n) = n else {
        return false;
    };
    let mut todo = vec![n.to_ref()];
    while let Some(n) = todo.pop() {
        if matches!(n, typedpg_pg_query::NodeRef::SubLink(_)) {
            continue;
        }
        if pred(n) {
            return true;
        }
        todo.extend(n.children());
    }
    false
}

/// The column of `scope` output expression `n` is once parse analysis and
/// planning are done with it, if certainly one: a same-type cast of a
/// column with no typmod (`coerce_type` returns the Var itself), or a CASE
/// whose branch constants decide (`eval_const_expressions`) — `true`
/// then — reaching one.
fn folds_to_column<'a>(
    n: Option<&ast::Node>,
    scope: &'a Scope,
    snapshot: &PgCatalog,
) -> Option<(bool, &'a ScopeColumn)> {
    use crate::expr::plan_fold::{Folded, case_when};
    let mut folded = false;
    let mut casts = Vec::new();
    let mut n = n;
    loop {
        match n? {
            ast::Node::TypeCast(tc) => {
                let tn = tc.type_name.as_ref()?;
                if !tn.typmods.is_empty() || !tn.array_bounds.is_empty() || tn.setof || tn.pct_type
                {
                    return None;
                }
                casts.push(crate::ddl::util::resolve_type_name(tn, snapshot)?);
                n = tc.arg.as_ref();
            }
            ast::Node::CaseExpr(c) if c.arg.is_none() => {
                let mut next = None;
                for w in &c.args {
                    let Some(ast::Node::CaseWhen(w)) = w.as_ref() else {
                        return None;
                    };
                    match case_when(None, w.expr.as_ref(), snapshot) {
                        Folded::Bool(true) => {
                            next = w.result.as_ref().map(Some);
                            break;
                        }
                        Folded::Bool(false) | Folded::Null => {}
                        _ => return None,
                    }
                }
                n = next.or(c.defresult.as_ref().map(Some))?;
                folded = true;
            }
            ast::Node::ColumnRef(cr) => {
                let c = column_ref_target(cr, scope)?;
                return (c.typmod.is_none() && casts.iter().all(|t| *t == c.type_oid))
                    .then_some((folded, c));
            }
            _ => return None,
        }
    }
}

/// What `sel`, a query level whose FROM entries `scope` holds and whose
/// outputs are `columns`, certainly makes of a SubqueryScan over it,
/// planned on its own (see [`crate::scope::ScanFacts`]).
fn scan_facts(
    sel: &ast::SelectStmt,
    scope: &Scope,
    columns: &[RawColumn],
    snapshot: &PgCatalog,
) -> crate::scope::ScanFacts {
    use crate::expr::plan_fold::Folded;
    use crate::pg_catalog::RelKind;
    if sel.op != SetOperation::SetopNone || !sel.values_lists.is_empty() {
        return Default::default();
    }
    let source_of = |n: Option<&ast::Node>| match n {
        Some(ast::Node::RangeVar(rv)) => {
            let name = rv
                .alias
                .as_ref()
                .map_or(rv.relname.as_str(), |a| a.aliasname.as_str());
            scope
                .sources
                .iter()
                .find(|s| s.alias == name && matches!(s.kind, crate::scope::SourceKind::Relation))
        }
        _ => None,
    };
    let relkind = |s: &crate::scope::TableSource| {
        s.relid
            .and_then(|r| snapshot.pg_class.get(&r))
            .map(|c| c.relkind)
    };
    let plain_tables = !sel.from_clause.is_empty()
        && sel.from_clause.iter().all(|f| {
            source_of(f.as_ref()).is_some_and(|s| matches!(relkind(s), Some(RelKind::Table)))
        });
    // Tables and partitioned tables (with their partitions), one of them
    // a partitioned table with no partition to scan: its relation is a
    // dummy one, and so is the join (`expand_partitioned_rtentry`).
    let tables = !sel.from_clause.is_empty()
        && sel.from_clause.iter().all(|f| {
            matches!(f.as_ref(), Some(ast::Node::RangeVar(rv)) if rv.inh)
                && source_of(f.as_ref()).is_some_and(|s| {
                    matches!(relkind(s), Some(RelKind::Table | RelKind::Partitioned))
                })
        });
    let no_partition = tables
        && sel.from_clause.iter().any(|f| {
            source_of(f.as_ref())
                .and_then(|s| s.relid)
                .is_some_and(|r| {
                    snapshot.pg_class.get(&r).map(|c| c.relkind) == Some(RelKind::Partitioned)
                        && !has_leaf_partition(snapshot, r)
                })
        });
    let calls_in_target = sel.target_list.iter().any(|t| {
        contains_node(t.as_ref(), &|n| {
            matches!(n, typedpg_pg_query::NodeRef::FuncCall(_))
        })
    });
    let has_aggs = crate::grouping::level_info(sel).is_some_and(|l| l.has_aggs);
    let plain_level = sel.group_clause.is_empty()
        && sel.having_clause.is_none()
        && sel.distinct_clause.is_empty()
        && sel.window_clause.is_empty()
        && sel.with_clause.is_none()
        && !has_aggs
        && !calls_in_target;
    // A constant FALSE (or NULL) WHERE over tables: the relation is
    // excluded (`relation_excluded_by_constraints`) or the join a dummy
    // one (`restriction_is_constant_false`), and so the subquery's; the
    // relation reading it is a dummy one too. (A Limit node, a sort,
    // grouping or row marks above it may keep a plan.)
    let no_limit_node = |n: Option<&ast::Node>, zero_ok: bool| match n {
        None => true,
        Some(ast::Node::AConst(c)) if c.isnull => true,
        Some(ast::Node::AConst(c)) => match &c.val {
            Some(ast::AConstValue::Integer(i)) => zero_ok && i.ival == 0,
            _ => false,
        },
        _ => false,
    };
    let dummy = tables
        && plain_level
        && sel.sort_clause.is_empty()
        && sel.locking_clause.is_empty()
        && no_limit_node(sel.limit_count.as_ref(), false)
        && no_limit_node(sel.limit_offset.as_ref(), true)
        && (no_partition
            || (sel.where_clause.is_some()
                && crate::expr::plan_fold::fold_qual(sel.where_clause.as_ref(), snapshot)
                    == Folded::Bool(false)));
    // Row marks: each locked table adds a resjunk `ctid` entry.
    let row_marks = sel.locking_clause.iter().any(|l| match l.as_ref() {
        Some(ast::Node::LockingClause(lc)) => sel.from_clause.iter().any(|f| {
            source_of(f.as_ref()).is_some_and(|s| {
                matches!(relkind(s), Some(RelKind::Table | RelKind::Partitioned))
                    && (lc.locked_rels.is_empty()
                        || lc.locked_rels.iter().any(|r| {
                            matches!(r.as_ref(), Some(ast::Node::RangeVar(rv))
                                if rv.schemaname.is_empty() && rv.relname == s.alias)
                        }))
            })
        }),
        _ => false,
    });
    // A sort key that is a column of the level's entries no output is a
    // resjunk entry (`findTargetlistEntrySQL99` adds one). An unqualified
    // name naming an output is that output (SQL92 rule).
    let resjunk_sort = sel.sort_clause.iter().any(|s| {
        let Some(ast::Node::SortBy(sb)) = s.as_ref() else {
            return false;
        };
        let Some(ast::Node::ColumnRef(cr)) = sb.node.as_ref() else {
            return false;
        };
        if let [f] = cr.fields.as_slice()
            && let Some(ast::Node::String(s)) = f.as_ref()
            && columns.iter().any(|c| c.name == s.sval)
        {
            return false;
        }
        let Some(key) = column_ref_target(cr, scope) else {
            return false;
        };
        !sel.target_list.iter().any(|t| {
            let Some(ast::Node::ResTarget(rt)) = t.as_ref() else {
                return true;
            };
            let mut v = rt.val.as_ref();
            while let Some(ast::Node::TypeCast(tc)) = v {
                v = tc.arg.as_ref();
            }
            match v {
                Some(ast::Node::ColumnRef(t)) => {
                    t.fields
                        .iter()
                        .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))))
                        || column_ref_target(t, scope).is_none_or(|c| std::ptr::eq(c, key))
                }
                _ => false,
            }
        })
    });
    // No path of the level is sorted: no index or inheritance child to
    // scan in order, no join qual to merge join on, nothing it sorts.
    let unsorted = plain_tables
        && plain_level
        && sel.sort_clause.is_empty()
        && (sel.from_clause.len() == 1 || sel.where_clause.is_none())
        && sel.from_clause.iter().all(|f| {
            source_of(f.as_ref())
                .and_then(|s| s.relid)
                .is_some_and(|r| {
                    !snapshot.pg_index.values().any(|i| i.indrelid == r)
                        && !snapshot.pg_inherits.iter().any(|i| i.inhparent == r)
                })
        });
    // Its sorted paths are index scans, or (over partitions or inheritance
    // children) MergeAppends of them or Appends in partition-key order:
    // keys of a collatable type have the index's or the partition key's
    // collation, never none (no join qual a merge join could sort on — a
    // conflicting CASE equated across tables plans to a comparison with no
    // collation — and no ORDER BY, grouping or window of its own).
    let collated_sorts = !sel.from_clause.is_empty()
        && sel.from_clause.iter().all(|f| {
            source_of(f.as_ref()).is_some_and(|s| {
                matches!(
                    relkind(s),
                    Some(RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView)
                )
            })
        })
        && plain_level
        && sel.sort_clause.is_empty()
        && (sel.from_clause.len() == 1 || sel.where_clause.is_none());
    crate::scope::ScanFacts {
        kept: dummy || row_marks || resjunk_sort,
        no_pushdown: sel.limit_count.is_some() || sel.limit_offset.is_some(),
        unsorted,
        collated_sorts,
    }
}

/// Whether partitioned table `rel` has a partition holding rows (a table
/// or a foreign table), at any depth.
fn has_leaf_partition(snapshot: &PgCatalog, rel: crate::oid::PgClassOid) -> bool {
    use crate::pg_catalog::RelKind;
    let mut todo = vec![rel];
    let mut seen = std::collections::HashSet::new();
    while let Some(r) = todo.pop() {
        if !seen.insert(r) {
            continue;
        }
        for i in snapshot.pg_inherits.iter().filter(|i| i.inhparent == r) {
            match snapshot.pg_class.get(&i.inhrelid).map(|c| c.relkind) {
                Some(RelKind::Partitioned) => todo.push(i.inhrelid),
                _ => return true,
            }
        }
    }
    false
}

/// Whether `n` holds a node `pred` accepts.
fn contains_node(
    n: Option<&ast::Node>,
    pred: &dyn Fn(typedpg_pg_query::NodeRef<'_>) -> bool,
) -> bool {
    let mut todo: Vec<_> = n.map(|n| n.to_ref()).into_iter().collect();
    while let Some(n) = todo.pop() {
        if pred(n) {
            return true;
        }
        todo.extend(n.children());
    }
    false
}

/// The column a non-star column reference resolves to, if it names one
/// (not a whole row).
fn column_ref_target<'a>(cr: &ast::ColumnRef, scope: &'a Scope) -> Option<&'a ScopeColumn> {
    let names: Vec<&str> = cr
        .fields
        .iter()
        .map(|f| match f.as_ref() {
            Some(ast::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match names.as_slice() {
        [c] => scope.resolve_column(None, c, None).ok(),
        [t, c] => scope.resolve_column(Some(t), c, None).ok(),
        _ => None,
    }
}

/// Whether a target list holds a `(expr).*`.
fn star_over_expression(target_list: &[Option<ast::Node>]) -> bool {
    target_list.iter().any(|t| {
        matches!(
            t.as_ref(),
            Some(ast::Node::ResTarget(rt))
                if matches!(
                    rt.val.as_ref(),
                    Some(ast::Node::AIndirection(i))
                        if i.indirection.iter().any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))))
                )
        )
    })
}

/// Whether `sel` may be an aggregate query: HAVING, or a call that may be
/// an aggregate of this level in its select list or ORDER BY — directly,
/// or in a sublink over a column this level may provide (PG gives an
/// aggregate the level of its arguments' columns).
fn may_aggregate(sel: &ast::SelectStmt, scope: &Scope, snapshot: &PgCatalog) -> bool {
    use typedpg_pg_query::NodeRef;
    let is_aggregate = |f: &ast::FuncCall| {
        let parts = expr::extract_string_fields(&f.funcname);
        let (schema, name) = match parts.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] => (Some(s.as_str()), n.as_str()),
            _ => return true,
        };
        snapshot
            .find_functions(schema, name)
            .iter()
            .any(|p| matches!(p.prokind, crate::pg_catalog::ProKind::Aggregate))
    };
    // A column reference below `n` that may resolve to this level.
    fn mentions_own(n: NodeRef<'_>, scope: &Scope) -> bool {
        if let NodeRef::ColumnRef(cr) = n {
            let parts = expr::extract_string_fields(&cr.fields);
            return match parts.as_slice() {
                [] => true,
                [one] => {
                    scope.find_source(one).is_some()
                        || scope
                            .sources
                            .iter()
                            .any(|s| s.visible_columns().any(|c| &c.name == one))
                }
                [.., table, _] => scope.find_source(table).is_some(),
            };
        }
        n.children().into_iter().any(|c| mentions_own(c, scope))
    }
    let mentions_own = |n: NodeRef<'_>| mentions_own(n, scope);
    fn walk(
        n: NodeRef<'_>,
        in_sublink: bool,
        is_aggregate: &dyn Fn(&ast::FuncCall) -> bool,
        mentions_own: &dyn Fn(NodeRef<'_>) -> bool,
    ) -> bool {
        let own = match n {
            NodeRef::FuncCall(f) if is_aggregate(f) => !in_sublink || mentions_own(n),
            NodeRef::JsonObjectAgg(_) | NodeRef::JsonArrayAgg(_) | NodeRef::GroupingFunc(_) => {
                !in_sublink || mentions_own(n)
            }
            _ => false,
        };
        let below = in_sublink || matches!(n, NodeRef::SubLink(_));
        own || n
            .children()
            .into_iter()
            .any(|c| walk(c, below, is_aggregate, mentions_own))
    }
    sel.having_clause.is_some()
        || sel.target_list.iter().chain(&sel.sort_clause).any(|n| {
            n.as_ref()
                .is_some_and(|n| walk(n.to_ref(), false, &is_aggregate, &mentions_own))
        })
}

/// What an ORDER BY / GROUP BY / DISTINCT ON item denotes.
enum KeyRef<'a> {
    /// A select-list entry (expanded-target index).
    Target(usize),
    /// An expression of its own.
    Expr(Option<&'a ast::Node>),
}

/// The expression of expanded select-list entry `i`, a column reference for
/// a `*` column.
fn target_expr<'a>(
    targets: &[ExpandedTarget<'a>],
    i: usize,
) -> Option<std::borrow::Cow<'a, ast::Node>> {
    match targets.get(i)? {
        ExpandedTarget::Expr(rt) => rt.val.as_ref().map(std::borrow::Cow::Borrowed),
        ExpandedTarget::StarColumn {
            column: Some((alias, col)),
            ..
        } => Some(std::borrow::Cow::Owned(ast::Node::ColumnRef(NodeBox::new(
            ast::ColumnRef {
                fields: [*alias, *col]
                    .into_iter()
                    .map(|s| {
                        Some(ast::Node::String(NodeBox::new(ast::String {
                            sval: s.into(),
                        })))
                    })
                    .collect(),
                location: -1,
            },
        )))),
        ExpandedTarget::StarColumn { column: None, .. } => None,
    }
}

/// PG's findTargetlistEntrySQL92 for an ORDER BY / GROUP BY / DISTINCT ON
/// item: an integer constant is a select-list position; a bare name is the
/// output column of that name (in GROUP BY only when no input column has
/// it) — `{clause} "a" is ambiguous` (42702) when several output columns
/// of that name have differing expressions; anything else is an expression.
fn sql92_key<'a>(
    node: Option<&'a ast::Node>,
    clause: &str,
    targets: &[ExpandedTarget<'_>],
    columns: &[RawColumn],
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Result<KeyRef<'a>, AnalyzeError> {
    if let Some(ord) = ordinal_of(node) {
        return Ok(match usize::try_from(ord - 1) {
            Ok(i) if i < columns.len() => KeyRef::Target(i),
            _ => KeyRef::Expr(node),
        });
    }
    let Some(ast::Node::ColumnRef(cr)) = node else {
        return Ok(KeyRef::Expr(node));
    };
    let parts = expr::extract_string_fields(&cr.fields);
    let [name] = parts.as_slice() else {
        return Ok(KeyRef::Expr(node));
    };
    if cr.fields.len() != 1
        || (clause == "GROUP BY" && scope.resolve_column(None, name, None).is_ok())
    {
        return Ok(KeyRef::Expr(node));
    }
    let names: Vec<Option<&str>> = columns.iter().map(|c| Some(c.name.as_str())).collect();
    Ok(
        match named_output_column(node, clause, targets, &names, scope, snapshot)? {
            Some(i) => KeyRef::Target(i),
            None => KeyRef::Expr(node),
        },
    )
}

/// The name of each output column of the expanded target list (`targets`,
/// `target_list` expanded), as findTargetlistEntrySQL92 matches a bare name
/// against them: an entry's `resname` ([`target_resname`]), a `*` column's
/// own name, a `(row).*` field's name (`None` where it doesn't resolve).
fn output_column_names<'a>(
    targets: &[ExpandedTarget<'a>],
    target_list: &[Option<ast::Node>],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Vec<Option<std::borrow::Cow<'a, str>>> {
    use std::borrow::Cow;
    let mut out = Vec::with_capacity(targets.len());
    // Each entry contributes the next columns of `targets`: a `(row).*` its
    // fields, which only resolving it names (its columns carry no location
    // to tell them apart by).
    for t in target_list {
        let Some(ast::Node::ResTarget(rt)) = t.as_ref() else {
            continue;
        };
        if rt.val.is_none() || out.len() >= targets.len() {
            continue;
        }
        let rest = &targets[out.len()..];
        // `*` / `t.*`: its columns, maybe none.
        if let Some(ast::Node::ColumnRef(cr)) = rt.val.as_ref()
            && cr
                .fields
                .iter()
                .any(|f| matches!(f.as_ref(), Some(ast::Node::AStar(_))))
        {
            out.extend(rest.iter().map_while(|t| match *t {
                ExpandedTarget::StarColumn {
                    column: Some((_, name)),
                    location,
                } if location == cr.location => Some(Some(Cow::Borrowed(name))),
                _ => None,
            }));
            continue;
        }
        let run = rest
            .iter()
            .take_while(|t| matches!(t, ExpandedTarget::StarColumn { column: None, .. }))
            .count();
        if run == 0 {
            out.push(Some(Cow::Owned(target_resname(rt))));
            continue;
        }
        let fields = match rt.val.as_ref() {
            Some(ast::Node::AIndirection(ind)) => {
                expr::expand_indirection_star(ind, ctx, &mut params.clone())
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        match fields {
            Some(fields) if fields.len() <= run => {
                out.extend(fields.into_iter().map(|(name, _)| Some(Cow::Owned(name))));
            }
            _ => out.extend((0..run).map(|_| None)),
        }
    }
    out.resize(targets.len(), None);
    out
}

/// findTargetlistEntrySQL92's match of a bare name (an ORDER BY, DISTINCT
/// ON or GROUP BY key that is a one-field column reference) against the
/// output columns' `names`, before — for ORDER BY and DISTINCT ON — any
/// input column: the output column of that name, `{clause} "a" is
/// ambiguous` (42702) when several of that name have differing
/// expressions, `None` when none has it (or the key is no bare name).
fn named_output_column<S: AsRef<str>>(
    node: Option<&ast::Node>,
    clause: &str,
    targets: &[ExpandedTarget<'_>],
    names: &[Option<S>],
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Result<Option<usize>, AnalyzeError> {
    let Some(ast::Node::ColumnRef(cr)) = node else {
        return Ok(None);
    };
    let [field] = cr.fields.as_slice() else {
        return Ok(None);
    };
    let Some(ast::Node::String(name)) = field.as_ref() else {
        return Ok(None);
    };
    let matches: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.as_ref().is_some_and(|n| n.as_ref() == name.sval))
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = matches.first() else {
        return Ok(None);
    };
    let first_expr = target_expr(targets, first);
    for &other in &matches[1..] {
        let same = match (&first_expr, target_expr(targets, other)) {
            (Some(a), Some(b)) => {
                grouping::same_level_exprs_equal(Some(a), Some(&b), scope, snapshot)
            }
            _ => false,
        };
        if !same {
            return Err(crate::pgmsg::clause_name_ambiguous(
                clause,
                &name.sval,
                crate::error::SourceSpan::from_node_qname(cr.location),
            )
            .finalize_implicit());
        }
    }
    Ok(Some(first))
}

/// The operator and collation requirements on the sort / group keys of a
/// SELECT level: get_sort_group_operators for each ORDER BY / GROUP BY /
/// DISTINCT [ON] key, each window's PARTITION BY / ORDER BY, and each
/// aggregate's DISTINCT arguments and ORDER BY (42883 when the type has no
/// such operator), then assign_collations' rule that a sort / group key's
/// collation must be determinate (42P21).
fn check_sort_group_keys(
    sel: &ast::SelectStmt,
    targets: &[ExpandedTarget<'_>],
    columns: &[RawColumn],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Result<(), AnalyzeError> {
    use crate::clause::KeyUse;
    let scope = ctx.scope;
    // `(key, operators needed — None for ORDER BY … USING, location)`.
    let mut keys: Vec<(KeyRef<'_>, Option<KeyUse>, i32)> = Vec::new();
    let loc = |n: Option<&ast::Node>| crate::error::node_location(n).unwrap_or(-1);
    fn sort_items(order: &[Option<ast::Node>]) -> Vec<(Option<&ast::Node>, bool)> {
        order
            .iter()
            .filter_map(|o| match o.as_ref()? {
                ast::Node::SortBy(sb) if sb.node.is_some() => Some((
                    sb.node.as_ref(),
                    sb.sortby_dir == ast::SortByDir::SortbyUsing,
                )),
                _ => None,
            })
            .collect()
    }
    for (inner, using) in sort_items(&sel.sort_clause) {
        let key = sql92_key(inner, "ORDER BY", targets, columns, scope, ctx.snapshot)?;
        keys.push((key, (!using).then_some(KeyUse::Sort), loc(inner)));
    }
    let mut stack: Vec<Option<&ast::Node>> =
        sel.group_clause.iter().map(Option::as_ref).rev().collect();
    while let Some(g) = stack.pop() {
        if let Some(ast::Node::GroupingSet(gs)) = g {
            stack.extend(gs.content.iter().map(Option::as_ref).rev());
        } else if let Some(items) = grouping::implicit_row_items(g) {
            stack.extend(items.iter().map(Option::as_ref).rev());
        } else {
            let key = sql92_key(g, "GROUP BY", targets, columns, scope, ctx.snapshot)?;
            keys.push((key, Some(KeyUse::Group), loc(g)));
        }
    }
    let plain_distinct =
        !sel.distinct_clause.is_empty() && sel.distinct_clause.iter().all(|n| n.is_none());
    if plain_distinct {
        for (i, t) in targets.iter().enumerate() {
            let location = match t {
                ExpandedTarget::Expr(rt) => rt.location,
                ExpandedTarget::StarColumn { location, .. } => *location,
            };
            keys.push((KeyRef::Target(i), Some(KeyUse::Group), location));
        }
    } else {
        for d in sel.distinct_clause.iter().filter(|n| n.is_some()) {
            let key = sql92_key(
                d.as_ref(),
                "DISTINCT ON",
                targets,
                columns,
                scope,
                ctx.snapshot,
            )?;
            keys.push((key, Some(KeyUse::Group), loc(d.as_ref())));
        }
    }
    // Windows and aggregates of this level.
    let mut windows: Vec<&ast::WindowDef> = sel
        .window_clause
        .iter()
        .filter_map(|w| match w.as_ref()? {
            ast::Node::WindowDef(wd) => Some(&**wd),
            _ => None,
        })
        .collect();
    let mut level_exprs: Vec<Option<&ast::Node>> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.as_ref()? {
            ast::Node::ResTarget(rt) if rt.val.is_some() => Some(rt.val.as_ref()),
            _ => None,
        })
        .collect();
    level_exprs.extend(sel.having_clause.as_ref().map(Some));
    level_exprs.extend(sort_items(&sel.sort_clause).into_iter().map(|(n, _)| n));
    let mut agg_keys: Vec<(Option<&ast::Node>, KeyUse)> = Vec::new();
    for e in level_exprs {
        visit_same_level(e, &mut |n| {
            // SQL/JSON aggregates: their ORDER BY and window.
            let json_agg = match n {
                Some(ast::Node::JsonObjectAgg(a)) => a.constructor.as_deref(),
                Some(ast::Node::JsonArrayAgg(a)) => a.constructor.as_deref(),
                _ => None,
            };
            if let Some(ctor) = json_agg {
                windows.extend(ctor.over.as_deref());
                agg_keys.extend(
                    sort_items(&ctor.agg_order)
                        .into_iter()
                        .map(|(n, _)| (n, KeyUse::Sort)),
                );
            }
            let Some(ast::Node::FuncCall(fc)) = n else {
                return;
            };
            if let Some(over) = fc.over.as_deref() {
                windows.push(over);
            }
            if fc.agg_distinct {
                agg_keys.extend(fc.args.iter().map(|a| (a.as_ref(), KeyUse::AggDistinct)));
            }
            agg_keys.extend(
                sort_items(&fc.agg_order)
                    .into_iter()
                    .map(|(n, _)| (n, KeyUse::Sort)),
            );
        });
    }
    for wd in windows {
        keys.extend(wd.partition_clause.iter().map(|p| {
            (
                KeyRef::Expr(p.as_ref()),
                Some(KeyUse::Group),
                loc(p.as_ref()),
            )
        }));
        keys.extend(
            sort_items(&wd.order_clause)
                .into_iter()
                .map(|(o, using)| (KeyRef::Expr(o), (!using).then_some(KeyUse::Sort), loc(o))),
        );
    }
    keys.extend(
        agg_keys
            .into_iter()
            .map(|(n, u)| (KeyRef::Expr(n), Some(u), loc(n))),
    );

    // addTargetToSortList / addTargetToGroupList resolve an `unknown` key
    // as `text`, which fails for one that is no literal.
    for (key, _, _) in &keys {
        let node = match key {
            KeyRef::Target(i) if columns.get(*i).is_some_and(|c| c.unknown_value) => {
                target_expr(targets, *i).map(Some)
            }
            KeyRef::Expr(n)
                if expr::infer_expr(*n, ctx, &mut params.clone(), TypeGoal::NONE)
                    .is_ok_and(|t| t.unknown_value) =>
            {
                Some(n.map(std::borrow::Cow::Borrowed))
            }
            _ => None,
        };
        if let Some(node) = node {
            expr::unknown_value_coercion(
                oid::TEXT,
                crate::coerce::CoercionContext::Implicit,
                ctx.snapshot,
                node.as_deref(),
            )?;
        }
    }
    let key_type = |key: &KeyRef<'_>| match key {
        KeyRef::Target(i) => columns.get(*i).map(|c| c.type_oid),
        KeyRef::Expr(n) => {
            let mut scratch = params.clone();
            expr::infer_expr(*n, ctx, &mut scratch, TypeGoal::NONE)
                .ok()
                .map(|t| t.type_oid)
        }
    };
    for (key, usage, location) in &keys {
        if let Some(usage) = usage
            && let Some(t) = key_type(key)
        {
            crate::clause::check_key_operators(ctx.snapshot, t, *usage, Some(*location))?;
        }
    }
    for (key, _, location) in &keys {
        let collatable = key_type(key)
            .and_then(|t| ctx.snapshot.get_type(t))
            .is_some_and(|t| t.typcollation.is_some());
        if !collatable {
            continue;
        }
        let expr = match key {
            KeyRef::Target(i) => target_expr(targets, *i).map(Some),
            KeyRef::Expr(n) => Some(n.map(std::borrow::Cow::Borrowed)),
        };
        if let Some(e) = expr {
            crate::clause::collation_state(e.as_deref(), ctx, params)
                .check_determinate(ctx.snapshot, Some(*location))?;
        }
    }
    Ok(())
}

/// ORDER BY / LIMIT / OFFSET of a bare `VALUES` list (PG's
/// transformValuesClause): they see its columns (`column1`, …) the way a
/// query sees a FROM item, and select-list positions count its columns.
fn values_sort_and_limit(
    sel: &ast::SelectStmt,
    columns: &[RawColumn],
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    outer_tiers: &[Vec<crate::scope::TableSource>],
) -> Result<Vec<crate::scope::TableSource>, AnalyzeError> {
    let alias = crate::scope::hidden_alias("values");
    let cols: Vec<ScopeColumn> = columns
        .iter()
        .map(|c| ScopeColumn {
            deferred: Default::default(),
            name: c.name.clone(),
            type_oid: c.type_oid,
            base_not_null: !c.nullable,
            typmod: c.typmod,
            collation: c.collation,
            no_collation: c.no_collation.is_some(),
            planned_unknown: false,
            table_alias: alias.clone(),
            record_fields: c.record_fields.clone(),
            elem_nullable: c.elem_nullable,
            elem_refine: c.elem_refine.clone(),
            refine: c.refine.clone(),
            origin: None,
            output: None,
        })
        .collect();
    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    scope.outer_tiers.extend(outer_tiers.iter().cloned());
    scope.add_derived(&alias, cols, crate::scope::SourceKind::Other)?;
    let null_ctx = NullabilityContext::default();
    // The sort keys see the VALUES' columns, and so do the sublinks in
    // them: an aggregate there over `column1` is the VALUES level's.
    grouping::with_own_namespace(&scope.sources, || {
        for sort_node in &sel.sort_clause {
            let Some(ast::Node::SortBy(sb)) = sort_node.as_ref() else {
                continue;
            };
            let inner = &sb.node;
            if inner.is_none() {
                continue;
            }
            if let Some(ord) = sql92_position_of(inner.as_ref(), "ORDER BY")? {
                if ord < 1 || ord as usize > columns.len() {
                    return Err(crate::pgmsg::position_not_in_select_list(
                        "ORDER BY",
                        ord,
                        crate::error::node_location(inner.as_ref())
                            .and_then(crate::error::SourceSpan::from_node_token),
                    )
                    .finalize_implicit());
                }
                continue;
            }
            expr::infer_expr(
                inner.as_ref(),
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                params,
                TypeGoal::NONE,
            )?;
        }
        Ok(())
    })?;
    analyze_limit_offset(sel, expr::Ctx::new(&scope, &null_ctx, snapshot), params)?;
    Ok(scope.sources)
}

/// Find window calls (a function's or a JSON aggregate's) and verify that
/// any *named* window they reference (`OVER w` sets `WindowDef.name`;
/// `OVER (w …)` inheritance sets `refname`) is defined in the SELECT's
/// WINDOW clause. SubLinks are skipped — their windows belong to the inner
/// query.
fn check_window_refs(
    node: Option<&ast::Node>,
    defined: &std::collections::HashSet<&str>,
) -> Result<(), AnalyzeError> {
    let mut missing: Option<(&str, i32)> = None;
    visit_same_level(node, &mut |n| {
        let over = match n {
            Some(ast::Node::FuncCall(fc)) => fc.over.as_deref(),
            Some(ast::Node::JsonObjectAgg(a)) => {
                a.constructor.as_deref().and_then(|c| c.over.as_deref())
            }
            Some(ast::Node::JsonArrayAgg(a)) => {
                a.constructor.as_deref().and_then(|c| c.over.as_deref())
            }
            _ => None,
        };
        if missing.is_none()
            && let Some(over) = over
            && let Some(name) = [over.name.as_str(), over.refname.as_str()]
                .into_iter()
                .find(|name| !name.is_empty() && !defined.contains(name))
        {
            missing = Some((name, over.location));
        }
    });
    match missing {
        // PG positions it at the window's name.
        Some((name, location)) => Err(crate::pgmsg::window_does_not_exist(
            name,
            crate::error::SourceSpan::from_node_qname(location),
        )
        .finalize_implicit()),
        None => Ok(()),
    }
}

/// findTargetlistEntrySQL92's constant rule for an ORDER BY / GROUP BY /
/// DISTINCT ON item: an integer constant is a 1-based select-list position
/// (`Some`), any other constant — string, float, boolean, NULL — is
/// `non-integer constant in {clause}` (42601), and anything else is an
/// expression (`None`).
pub(crate) fn sql92_position(node: &ast::Node, clause: &str) -> Result<Option<i64>, AnalyzeError> {
    let ast::Node::AConst(ac) = node else {
        return Ok(None);
    };
    match &ac.val {
        Some(typedpg_pg_query::ast::AConstValue::Integer(i)) if !ac.isnull => {
            Ok(Some(i.ival as i64))
        }
        _ => Err(crate::pgmsg::non_integer_constant(
            clause,
            crate::error::SourceSpan::from_node_token(ac.location),
        )
        .finalize_implicit()),
    }
}

/// [`sql92_position`] of a list cell: a NULL one is no constant.
fn sql92_position_of(node: Option<&ast::Node>, clause: &str) -> Result<Option<i64>, AnalyzeError> {
    node.map_or(Ok(None), |n| sql92_position(n, clause))
}

/// If `node` is a bare integer literal, return its value — GROUP BY / ORDER
/// BY treat those as 1-based projection ordinals.
fn ordinal_of(node: Option<&ast::Node>) -> Option<i64> {
    if let Some(ast::Node::AConst(ac)) = node
        && !ac.isnull
        && let Some(typedpg_pg_query::ast::AConstValue::Integer(i)) = &ac.val
    {
        return Some(i.ival as i64);
    }
    None
}

/// Structural fingerprint of an expression node: its key under PG's
/// `equal()` ([`typedpg_pg_query::Equal::equal_key`]), which skips every
/// source position — `location`, and PG 18's `list_start` / `list_end` of
/// `ARRAY[…]` and `IN (…)` lists among them. Used to compare an ORDER BY
/// expression against the projection entries (PG's "appears in select
/// list" test), where byte positions necessarily differ.
pub(crate) fn node_fingerprint(node: Option<&ast::Node>) -> String {
    use typedpg_pg_query::Equal;
    let mut out = String::new();
    // The key of the `Option<Node>` it borrows.
    match node {
        Some(n) => {
            out.push('?');
            n.equal_key(&mut out);
        }
        None => out.push('~'),
    }
    out
}

/// PG's SELECT DISTINCT rule: an ORDER BY expression must appear in the
/// select list (findTargetlistEntrySQL99) — equal to one of its expanded
/// entries, a `*` column included (a key naming an output column, or its
/// position, is handled by the caller).
fn sort_expr_in_select_list(
    inner: Option<&ast::Node>,
    targets: &[ExpandedTarget<'_>],
    scope: &Scope,
    snapshot: &PgCatalog,
) -> bool {
    // Callers pass a present key.
    let Some(inner) = inner else {
        return false;
    };
    let want = grouping::typed_fingerprint(inner, scope, snapshot);
    (0..targets.len()).any(|i| {
        target_expr(targets, i).is_some_and(|val| {
            grouping::typed_fingerprint(&val, scope, snapshot) == want
                || grouping::same_level_exprs_equal(Some(&val), Some(inner), scope, snapshot)
        })
    })
}

/// Walk one entry from `sel.group_clause`, recursing into `GroupingSet`
/// nodes (`GROUPING SETS`/`ROLLUP`/`CUBE`) to reach the underlying
/// expressions. The walk type-checks parameters and rejects aggregates /
/// window calls inside the grouping expressions (PG forbids those too).
fn walk_group_clause_node(
    group_node: Option<&ast::Node>,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
    select_aliases: &std::collections::HashSet<String>,
    targets: &[ExpandedTarget<'_>],
) -> Result<(), AnalyzeError> {
    let Ctx {
        scope,
        null_ctx,
        snapshot,
        ..
    } = ctx;
    let n_targets = targets.len();
    // A grouping set's members — and the members of an implicit row
    // `(a, b)`, which flatten_grouping_sets turns into a list of grouping
    // expressions — are grouping expressions of their own.
    let members = match group_node {
        Some(ast::Node::GroupingSet(gs)) => Some(&gs.content),
        _ => grouping::implicit_row_items(group_node),
    };
    if let Some(members) = members {
        for inner in members {
            walk_group_clause_node(
                inner.as_ref(),
                expr::Ctx::new(scope, null_ctx, snapshot),
                params,
                select_aliases,
                targets,
            )?;
        }
        // transformGroupingSet: a CUBE over more than 12 elements.
        if let Some(ast::Node::GroupingSet(gs)) = group_node
            && gs.kind == ast::GroupingSetKind::GroupingSetCube
            && gs.content.len() > 12
        {
            return Err(crate::pgmsg::cube_too_many_elements(
                crate::error::SourceSpan::from_node_qname(gs.location),
            )
            .finalize_implicit());
        }
        return Ok(());
    }
    // Integer literals are 1-based projection ordinals (42P10 when out of
    // range); a valid one needs no further walking.
    if let Some(ord) = sql92_position_of(group_node, "GROUP BY")? {
        if ord < 1 || ord as usize > n_targets {
            return Err(crate::pgmsg::position_not_in_select_list(
                "GROUP BY",
                ord,
                crate::error::node_location(group_node)
                    .and_then(crate::error::SourceSpan::from_node_token),
            )
            .finalize_implicit());
        }
        // checkTargetlistEntrySQL92: the referenced target must not
        // contain aggregates (GROUPING included).
        return match targets[ord as usize - 1] {
            ExpandedTarget::Expr(rt) => {
                check_group_target_has_no_aggregates(Some(rt), snapshot)?;
                resolve_unknown_target(rt, snapshot, params)
            }
            ExpandedTarget::StarColumn { .. } => Ok(()),
        };
    }
    // findTargetlistEntrySQL92: a bare name that is no input column
    // (colNameToVar — a FROM entry's name doesn't count) but an output
    // column's name stands for that select-list entry. `GROUP BY s` with
    // `SELECT s::c FROM (…) s` groups by `s::c`, not the whole row `s`.
    if let Some(ast::Node::ColumnRef(cr)) = group_node
        && let [name] = expr::extract_string_fields(&cr.fields).as_slice()
        && cr.fields.len() == 1
        && matches!(
            scope.resolve_column(None, name, None),
            Err(AnalyzeError::UndefinedColumn(_))
        )
        && let Some(target) = targets.iter().find_map(|t| match t {
            ExpandedTarget::Expr(rt) if &target_resname(rt) == name => Some(*rt),
            _ => None,
        })
    {
        return check_group_target_has_no_aggregates(Some(target), snapshot);
    }
    // PG transforms the expression first (bottom-up resolution errors win)
    // and raises the no-aggregates placement error afterwards.
    if let Err(e) = expr::infer_expr(
        group_node,
        expr::Ctx::new(scope, null_ctx, snapshot),
        params,
        TypeGoal::NONE,
    ) {
        if !is_select_alias_reference(group_node, select_aliases, &e) {
            return Err(e);
        }
        let alias = expr::extract_string_fields(match group_node {
            Some(ast::Node::ColumnRef(cr)) => &cr.fields,
            _ => &[],
        });
        let target = targets.iter().find_map(|t| match t {
            ExpandedTarget::Expr(rt) if alias.first() == Some(&target_resname(rt)) => Some(*rt),
            _ => None,
        });
        check_group_target_has_no_aggregates(target, snapshot)?;
        return match target {
            Some(rt) => resolve_unknown_target(rt, snapshot, params),
            None => Ok(()),
        };
    }
    crate::clause::check_level_calls(
        group_node,
        expr::Ctx::new(scope, null_ctx, snapshot),
        "GROUP BY",
        true,
    )?;
    // addTargetToGroupList resolves the key still `unknown` as text,
    // before the next key is transformed.
    expr::resolve_unknown_key(group_node, snapshot, params)
}

/// addTargetToSortList / addTargetToGroupList on a key naming a
/// select-list entry (by ordinal or output name): an entry that is a bare
/// `$N` the select list transformed untyped is still `unknown`, and is
/// resolved as text right there — `inconsistent types deduced` if a
/// clause transformed meanwhile (WHERE, HAVING) typed `$N` otherwise.
fn resolve_unknown_target(
    rt: &ast::ResTarget,
    snapshot: &crate::pg_catalog::PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    match expr::bare_param(rt.val.as_ref(), snapshot) {
        Some(p) if params.is_untyped_output(p.location) => {
            expr::coerce_untyped_param(p, oid::TEXT, snapshot, params)
        }
        _ => Ok(()),
    }
}

/// PG's checkTargetlistEntrySQL92 for GROUP BY: a projection entry referenced
/// by ordinal or alias must not contain aggregate calls — `GROUPING(…)`
/// counts (contain_aggs_of_level) — else `aggregate functions are not
/// allowed in GROUP BY` (42803).
fn check_group_target_has_no_aggregates(
    target: Option<&ast::ResTarget>,
    snapshot: &crate::pg_catalog::PgCatalog,
) -> Result<(), AnalyzeError> {
    if let Some(rt) = target
        && rt.val.is_some()
    {
        let kinds = expr::detect_func_kinds(rt.val.as_ref(), snapshot);
        if kinds.has_aggregate || kinds.has_grouping {
            return Err(crate::error::RawError::new(
                AnalyzeError::GroupingError(
                    "aggregate functions are not allowed in GROUP BY".into(),
                ),
                kinds.aggregate_span(),
                None,
            )
            .finalize_implicit());
        }
    }
    Ok(())
}

/// Returns `true` when `node` is a bare unqualified `ColumnRef` whose name
/// matches one of `aliases` AND the error that infer_expr raised was an
/// `UndefinedColumn`. Used by GROUP BY / ORDER BY to honor PG's rule that
/// a bare identifier in those clauses may reference a select-list alias
/// that isn't visible in the FROM scope. Any other error (type mismatch,
/// undefined function, etc.) is left to propagate.
fn is_select_alias_reference(
    node: Option<&ast::Node>,
    aliases: &std::collections::HashSet<String>,
    err: &AnalyzeError,
) -> bool {
    if !matches!(err, AnalyzeError::UndefinedColumn(_)) {
        return false;
    }
    let Some(ast::Node::ColumnRef(cr)) = node else {
        return false;
    };
    if cr.fields.len() != 1 {
        return false;
    }
    let Some(ast::Node::String(s)) = cr.fields[0].as_ref() else {
        return false;
    };
    aliases.contains(&s.sval)
}

/// PG's `LCS_asString`: the clause's spelling in messages.
/// Where PG positions an error about an INTO clause: its relation.
pub(crate) fn into_span(into: &ast::IntoClause) -> Option<crate::error::SourceSpan> {
    into.rel
        .as_ref()
        .and_then(|rv| crate::error::SourceSpan::from_node_qname(rv.location))
}

pub(crate) fn lock_strength_name(lc: &ast::LockingClause) -> &'static str {
    match lc.strength {
        typedpg_pg_query::ast::LockClauseStrength::LcsForkeyshare => "FOR KEY SHARE",
        typedpg_pg_query::ast::LockClauseStrength::LcsForshare => "FOR SHARE",
        typedpg_pg_query::ast::LockClauseStrength::LcsFornokeyupdate => "FOR NO KEY UPDATE",
        _ => "FOR UPDATE",
    }
}

/// The first `CheckSelectLocking` (analyze.c) violation of `sel`: the
/// construct a locking clause is not allowed with.
fn select_lock_blocker(sel: &ast::SelectStmt, snapshot: &PgCatalog) -> Option<&'static str> {
    if sel.op != SetOperation::SetopNone {
        return Some("UNION/INTERSECT/EXCEPT");
    }
    if !sel.distinct_clause.is_empty() {
        return Some("DISTINCT clause");
    }
    if !sel.group_clause.is_empty() {
        return Some("GROUP BY clause");
    }
    if sel.having_clause.is_some() {
        return Some("HAVING clause");
    }
    let level_exprs: Vec<Option<&ast::Node>> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.as_ref()? {
            ast::Node::ResTarget(rt) if rt.val.is_some() => Some(rt.val.as_ref()),
            _ => None,
        })
        .chain(sel.sort_clause.iter().filter_map(|n| match n.as_ref()? {
            ast::Node::SortBy(sb) if sb.node.is_some() => Some(sb.node.as_ref()),
            _ => None,
        }))
        .collect();
    let kinds: Vec<_> = level_exprs
        .iter()
        .map(|e| expr::detect_func_kinds(*e, snapshot))
        .collect();
    if kinds.iter().any(|k| k.has_aggregate) {
        return Some("aggregate functions");
    }
    if kinds.iter().any(|k| k.has_window) {
        return Some("window functions");
    }
    if !level_srf_calls(sel, snapshot).is_empty() {
        return Some("set-returning functions in the target list");
    }
    None
}

/// The blocker a locking clause pushed down into FROM subquery `sel` would
/// hit: PG's `transformLockingClause` applies an unqualified (or naming)
/// clause to the subquery, which re-runs `CheckSelectLocking` there and
/// pushes further into *its* FROM subqueries.
pub(crate) fn subquery_lock_blocker(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
) -> Option<&'static str> {
    fn from_item_blocker(n: Option<&ast::Node>, snapshot: &PgCatalog) -> Option<&'static str> {
        match n? {
            ast::Node::RangeSubselect(rs) => match rs.subquery.as_ref()? {
                ast::Node::SelectStmt(s) => subquery_lock_blocker(s, snapshot),
                _ => None,
            },
            ast::Node::JoinExpr(j) => from_item_blocker(j.larg.as_ref(), snapshot)
                .or_else(|| from_item_blocker(j.rarg.as_ref(), snapshot)),
            _ => None,
        }
    }
    select_lock_blocker(sel, snapshot).or_else(|| {
        sel.from_clause
            .iter()
            .find_map(|n| from_item_blocker(n.as_ref(), snapshot))
    })
}

/// The WHERE and every join's ON of query level `sel`.
fn level_quals(sel: &ast::SelectStmt) -> Vec<&ast::Node> {
    fn joins<'a>(n: Option<&'a ast::Node>, out: &mut Vec<&'a ast::Node>) {
        if let Some(ast::Node::JoinExpr(j)) = n {
            out.extend(j.quals.as_ref());
            joins(j.larg.as_ref(), out);
            joins(j.rarg.as_ref(), out);
        }
    }
    let mut out: Vec<&ast::Node> = sel.where_clause.iter().collect();
    for item in &sel.from_clause {
        joins(item.as_ref(), &mut out);
    }
    out
}

/// The query of FROM subquery `alias` in `from` (joins searched).
fn from_subselect<'a>(from: &'a [Option<ast::Node>], alias: &str) -> Option<&'a ast::SelectStmt> {
    fn find<'a>(n: Option<&'a ast::Node>, alias: &str) -> Option<&'a ast::SelectStmt> {
        match n? {
            ast::Node::RangeSubselect(rs)
                if rs.alias.as_ref().is_some_and(|a| a.aliasname == alias) =>
            {
                match rs.subquery.as_ref()? {
                    ast::Node::SelectStmt(s) => Some(s),
                    _ => None,
                }
            }
            ast::Node::JoinExpr(j) => {
                find(j.larg.as_ref(), alias).or_else(|| find(j.rarg.as_ref(), alias))
            }
            _ => None,
        }
    }
    from.iter().find_map(|n| find(n.as_ref(), alias))
}

/// Validate a SELECT's locking clauses like PG: `CheckSelectLocking` (the
/// query may not use DISTINCT / GROUP BY / HAVING / aggregates / window
/// functions / target-list SRFs), then `transformLockingClause` — each
/// named entry must be a FROM entry of this level (42P01) and lockable
/// (not a WITH query, function or join; a subquery takes the clause
/// itself), and an unqualified clause applies to every table and
/// subquery. Locking a table on the nullable side of an outer join is the
/// planner's `make_outerjoininfo` error. All but the missing entry are
/// 0A000.
fn check_locking_clause(
    sel: &ast::SelectStmt,
    scope: &Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    use crate::scope::SourceKind;
    let unsupported = |msg: String| {
        crate::error::RawError::new(AnalyzeError::FeatureNotSupported(msg), None, None)
            .finalize_implicit()
    };
    for node in &sel.locking_clause {
        let Some(ast::Node::LockingClause(lc)) = node.as_ref() else {
            continue;
        };
        let clause = lock_strength_name(lc);
        if let Some(blocker) = select_lock_blocker(sel, snapshot) {
            return Err(unsupported(format!(
                "{clause} is not allowed with {blocker}"
            )));
        }
        let mut locked: Vec<&crate::scope::TableSource> = Vec::new();
        if lc.locked_rels.is_empty() {
            locked.extend(
                scope.sources.iter().filter(|s| {
                    matches!(s.kind, SourceKind::Relation | SourceKind::Subquery { .. })
                }),
            );
        }
        for rel in &lc.locked_rels {
            let Some(ast::Node::RangeVar(rv)) = rel.as_ref() else {
                continue;
            };
            if !rv.schemaname.is_empty() || !rv.catalogname.is_empty() {
                return Err(crate::error::RawError::new(
                    AnalyzeError::SyntaxError(format!(
                        "{clause} must specify unqualified relation names"
                    )),
                    crate::error::SourceSpan::from_node_qname(rv.location),
                    None,
                )
                .finalize_implicit());
            }
            let Some(source) = scope.sources.iter().find(|s| s.alias == rv.relname) else {
                return Err(crate::error::RawError::new(
                    AnalyzeError::UndefinedTable(format!(
                        "relation \"{}\" in {clause} clause not found in FROM clause",
                        rv.relname
                    )),
                    crate::error::SourceSpan::from_node_qname(rv.location),
                    None,
                )
                .finalize_implicit());
            };
            let what = match source.kind {
                SourceKind::Cte => Some("a WITH query"),
                SourceKind::Function => Some("a function"),
                SourceKind::Join { .. } => Some("a join"),
                _ => None,
            };
            if let Some(what) = what {
                return Err(unsupported(format!("{clause} cannot be applied to {what}")));
            }
            locked.push(source);
        }
        // applyLockingClause: a locked relation requires
        // ACL_SELECT_FOR_UPDATE (UPDATE).
        for source in &locked {
            match source.kind {
                SourceKind::Relation => {
                    if let Some(relid) = source.relid {
                        crate::privileges::note_lock(relid);
                    }
                }
                // transformLockingClause pushes the clause into a FROM
                // subquery: its relations are locked too.
                SourceKind::Subquery { .. } => {
                    for item in &sel.from_clause {
                        note_subquery_locks(item.as_ref(), Some(&source.alias), scope, snapshot);
                    }
                }
                _ => {}
            }
        }
        for source in &locked {
            if let SourceKind::Subquery {
                lock_blocker: Some(blocker),
                ..
            } = source.kind
            {
                return Err(unsupported(format!(
                    "{clause} is not allowed with {blocker}"
                )));
            }
        }
        for source in &locked {
            let marks_rows = match source.kind {
                SourceKind::Relation => true,
                SourceKind::Subquery { .. } => from_subselect(&sel.from_clause, &source.alias)
                    .is_none_or(|s| super::from::pushed_lock_marks_rows(s, snapshot)),
                _ => false,
            };
            if marks_rows && null_ctx.is_nullable(&source.alias, "", true) {
                return Err(unsupported(format!(
                    "{clause} cannot be applied to the nullable side of an outer join"
                )));
            }
        }
        // Below a locked view or subquery: its own query's blockers and
        // nullable sides (the rewriter / planner's errors).
        for source in &locked {
            match source.lock_error {
                Some(crate::scope::LockBlock::NotAllowedWith(blocker)) => {
                    return Err(unsupported(format!(
                        "{clause} is not allowed with {blocker}"
                    )));
                }
                // Pulled up, the view or subquery's outer joins are this
                // level's: a qual here naming it may reduce them.
                Some(crate::scope::LockBlock::NullableSide)
                    if level_quals(sel)
                        .iter()
                        .any(|q| super::from::qual_may_name(q, &source.alias)) => {}
                Some(crate::scope::LockBlock::NullableSide) => {
                    return Err(unsupported(format!(
                        "{clause} cannot be applied to the nullable side of an outer join"
                    )));
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// LIMIT / OFFSET: coerced to bigint through the shared clause walker, then
/// PG's `checkExprIsVarFree` — a reference to this level's columns is 42P10
/// `argument of LIMIT must not contain variables` (outer references are
/// fine).
pub(crate) fn analyze_limit_offset(
    sel: &ast::SelectStmt,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    // transformSelectStmt: OFFSET first.
    for (limit_node, kind, label) in [
        (
            &sel.limit_offset,
            crate::clause::ClauseKind::Offset,
            "OFFSET",
        ),
        (&sel.limit_count, crate::clause::ClauseKind::Limit, "LIMIT"),
    ] {
        if limit_node.is_none() {
            continue;
        };
        // `preprocess_limit` estimates the bound while planning: a STABLE
        // call over constants runs then.
        expr::plan_fold::with_estimated_limit(limit_node.as_ref(), || {
            crate::clause::coerce_clause_expr(limit_node.as_ref(), ctx, params, kind).map(|_| ())
        })?;
        // The conversion to `bigint` is folded by the planner.
        if expr::plan_time_checks()
            && let Some(e) = crate::fold::conversion_error(
                limit_node.as_ref(),
                crate::pg_catalog::oid::INT8,
                None,
                ctx.snapshot,
            )
            && let Some(e) = expr::plan_time_error(
                crate::error::node_location(limit_node.as_ref()),
                e.at(limit_node.as_ref()),
            )
        {
            return Err(e);
        }
        check_no_srf_in_clause(limit_node.as_ref(), ctx.snapshot, label)?;
        let mut var: Option<i32> = None;
        visit_same_level(limit_node.as_ref(), &mut |e| {
            if var.is_none()
                && let Some(ast::Node::ColumnRef(cr)) = e
                && references_local_column(cr, ctx.scope)
            {
                var = Some(cr.location);
            }
        });
        if let Some(loc) = var {
            return Err(crate::error::RawError::new(
                AnalyzeError::InvalidColumnReference(format!(
                    "argument of {label} must not contain variables"
                )),
                crate::error::SourceSpan::from_node_qname(loc),
                None,
            )
            .finalize_implicit());
        }
    }
    // transformLimitClause: WITH TIES needs a row count, so a NULL constant
    // there is rejected (a NULL at run time only fails when evaluated).
    if sel.limit_option == ast::LimitOption::WithTies
        && let Some(ast::Node::AConst(ac)) = sel.limit_count.as_ref()
        && ac.isnull
    {
        return Err(crate::pgmsg::with_ties_null_row_count(
            crate::error::SourceSpan::from_node_token(ac.location),
        )
        .finalize_implicit());
    }
    Ok(())
}

/// Whether `cr` resolves against this query level's own FROM items (as
/// opposed to an enclosing query's).
pub(crate) fn references_local_column(cr: &ast::ColumnRef, scope: &Scope) -> bool {
    let parts = expr::extract_string_fields(&cr.fields);
    match parts.as_slice() {
        [col] => scope
            .sources
            .iter()
            .any(|s| s.visible_columns().any(|c| &c.name == col)),
        [tbl, ..] => scope.sources.iter().any(|s| &s.alias == tbl),
        [] => false,
    }
}

/// `ORDER BY expr USING op`: PG (`addTargetToSortList`) resolves `op` for
/// the sort expression's type (42883 when it doesn't exist) and requires it
/// to be the `<` or `>` member of a btree operator family (42809). The
/// analyzer has no `pg_amop`, so it accepts the ordering operator names PG's
/// btree families use.
fn check_order_by_using(
    sort_clause: &[Option<ast::Node>],
    target_list: &[Option<ast::Node>],
    columns: &[RawColumn],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    for sort_node in sort_clause {
        let Some(ast::Node::SortBy(sb)) = sort_node.as_ref() else {
            continue;
        };
        if sb.sortby_dir != ast::SortByDir::SortbyUsing {
            continue;
        }
        let inner = &sb.node;
        if inner.is_none() {
            continue;
        }
        let Some(ty) = sort_expr_type(inner.as_ref(), target_list, columns, ctx, params) else {
            continue;
        };
        let op = expr::extract_string_fields(&sb.use_op)
            .pop()
            .unwrap_or_default();
        let span = crate::error::SourceSpan::from_node_token(sb.location);
        if snapshot.find_operator(&op, Some(ty), ty).is_none() {
            let t = crate::ddl::util::format_type_for_message(snapshot, ty);
            return Err(
                crate::pgmsg::operator_does_not_exist(&t, &op, &t, span).finalize_implicit()
            );
        }
        if !matches!(op.as_str(), "<" | ">" | "~<~" | "~>~") {
            return Err(crate::error::RawError::new(
                AnalyzeError::WrongObjectType(format!(
                    "operator {op} is not a valid ordering operator"
                )),
                span,
                Some(
                    "Ordering operators must be \"<\" or \">\" members of btree operator families."
                        .into(),
                ),
            )
            .finalize_implicit());
        }
    }
    Ok(())
}

/// The type of an ORDER BY item: a position or output-column name refers to
/// the target list (`findTargetlistEntrySQL92`), anything else is inferred
/// on a scratch parameter collector.
fn sort_expr_type(
    inner: Option<&ast::Node>,
    target_list: &[Option<ast::Node>],
    columns: &[RawColumn],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Option<PgTypeOid> {
    match sort_target_key(inner, target_list, columns, ctx.scope, ctx.snapshot) {
        SortKey::Target(i) => columns.get(i).map(|c| c.type_oid),
        SortKey::Expr(_) => {
            let mut scratch = params.clone();
            expr::infer_expr(inner, ctx, &mut scratch, TypeGoal::NONE)
                .ok()
                .map(|e| e.type_oid)
        }
    }
}

/// Which target entry an ORDER BY / DISTINCT ON item denotes.
#[derive(Clone, PartialEq, Eq)]
enum SortKey {
    /// An entry of the select list (by position, output name, or equal
    /// expression).
    Target(usize),
    /// A resjunk expression, identified by its location-free fingerprint.
    Expr(String),
}

/// PG's `findTargetlistEntrySQL92`: an integer constant is a position, a
/// bare name matching an output column is that column, and otherwise the
/// expression matches a select-list entry it is equal to.
fn sort_target_key(
    inner: Option<&ast::Node>,
    target_list: &[Option<ast::Node>],
    columns: &[RawColumn],
    scope: &Scope,
    snapshot: &PgCatalog,
) -> SortKey {
    if let Some(ord) = ordinal_of(inner)
        && ord >= 1
    {
        return SortKey::Target(ord as usize - 1);
    }
    if let Some(ast::Node::ColumnRef(cr)) = inner
        && let [name] = expr::extract_string_fields(&cr.fields).as_slice()
        && let Some(i) = columns.iter().position(|c| &c.name == name)
    {
        return SortKey::Target(i);
    }
    // Callers pass a present key.
    let Some(inner) = inner else {
        return SortKey::Expr(node_fingerprint(inner));
    };
    let fp = grouping::typed_fingerprint(inner, scope, snapshot);
    // Only a plain target list (no `*`) lines up with the output columns.
    let has_star = target_list.iter().any(|t| {
        matches!(t.as_ref(), Some(ast::Node::ResTarget(rt))
            if matches!(rt.val.as_ref(),
                Some(ast::Node::ColumnRef(cr)) if cr.fields.iter().any(|f|
                    matches!(f.as_ref(), Some(ast::Node::AStar(_))))))
    });
    if !has_star
        && let Some(i) = target_list.iter().position(|t| {
            matches!(t.as_ref(), Some(ast::Node::ResTarget(rt))
                if rt.val.as_ref().is_some_and(|v| grouping::typed_fingerprint(v, scope, snapshot) == fp))
        })
    {
        return SortKey::Target(i);
    }
    SortKey::Expr(fp)
}

/// `transformDistinctOnClause`: ORDER BY items that are DISTINCT ON items
/// must come first — once an ORDER BY item outside the DISTINCT ON list has
/// been skipped, a later DISTINCT ON item (in ORDER BY or not) is 42P10
/// `SELECT DISTINCT ON expressions must match initial ORDER BY expressions`.
fn check_distinct_on_matches_order_by(
    sel: &ast::SelectStmt,
    columns: &[RawColumn],
    scope: &Scope,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let distinct: Vec<SortKey> = sel
        .distinct_clause
        .iter()
        .filter(|n| n.is_some())
        .map(|n| sort_target_key(n.as_ref(), &sel.target_list, columns, scope, snapshot))
        .collect();
    if distinct.is_empty() || sel.sort_clause.is_empty() {
        return Ok(());
    }
    let err = || {
        crate::error::RawError::new(
            AnalyzeError::InvalidColumnReference(
                "SELECT DISTINCT ON expressions must match initial ORDER BY expressions".into(),
            ),
            None,
            None,
        )
        .finalize_implicit()
    };
    let mut skipped = false;
    let mut covered: Vec<&SortKey> = Vec::new();
    // `addTargetToSortList` drops an item sorting a target the list already
    // sorts (by the same operator or its commutator: ASC after DESC too),
    // so `ORDER BY a, b, 1` with `1` being `a` checks as `ORDER BY a, b`.
    let mut sorted: Vec<SortKey> = Vec::new();
    for sort_node in &sel.sort_clause {
        let Some(ast::Node::SortBy(sb)) = sort_node.as_ref() else {
            continue;
        };
        let inner = &sb.node;
        if inner.is_none() {
            continue;
        }
        let key = sort_target_key(inner.as_ref(), &sel.target_list, columns, scope, snapshot);
        if sb.sortby_dir != ast::SortByDir::SortbyUsing {
            if sorted.contains(&key) {
                continue;
            }
            sorted.push(key.clone());
        }
        if let Some(d) = distinct.iter().find(|d| **d == key) {
            if skipped {
                return Err(err());
            }
            covered.push(d);
        } else {
            skipped = true;
        }
    }
    if skipped && distinct.iter().any(|d| !covered.contains(&d)) {
        return Err(err());
    }
    Ok(())
}

/// The relations a locking clause pushed into FROM subquery `alias` (any
/// subquery when `None`, and every FROM item below the first) locks:
/// transformLockingClause applies it to each relation of the subquery's
/// range table and recurses into its own subqueries, joins included.
fn note_subquery_locks(
    item: Option<&ast::Node>,
    alias: Option<&str>,
    scope: &Scope,
    snapshot: &PgCatalog,
) {
    match item {
        Some(ast::Node::RangeSubselect(rs)) => {
            let name = rs.alias.as_ref().map(|a| a.aliasname.as_str());
            if alias.is_some() && name != alias {
                return;
            }
            if let Some(ast::Node::SelectStmt(inner)) = rs.subquery.as_ref() {
                for item in &inner.from_clause {
                    note_locks_below(item.as_ref(), inner, scope, snapshot);
                }
            }
        }
        Some(ast::Node::JoinExpr(j)) => {
            for side in [j.larg.as_ref(), j.rarg.as_ref()] {
                note_subquery_locks(side, alias, scope, snapshot);
            }
        }
        _ => {}
    }
}

/// A FROM item of a subquery a locking clause was pushed into: a relation
/// is locked, a join or subquery passes the lock on. A WITH query's name
/// (the subquery's own, or the statement's) is no relation.
fn note_locks_below(
    item: Option<&ast::Node>,
    sel: &ast::SelectStmt,
    scope: &Scope,
    snapshot: &PgCatalog,
) {
    match item {
        Some(ast::Node::RangeVar(rv)) => {
            let own_cte = sel.with_clause.as_ref().is_some_and(|w| {
                w.ctes.iter().any(|c| {
                    matches!(c.as_ref(), Some(ast::Node::CommonTableExpr(cte)) if cte.ctename == rv.relname)
                })
            });
            if rv.schemaname.is_empty() && (own_cte || scope.ctes.contains_key(&rv.relname)) {
                return;
            }
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            if let Some(class) = snapshot.resolve_table(schema, &rv.relname) {
                crate::privileges::note_lock(class.oid);
            }
        }
        Some(ast::Node::JoinExpr(j)) => {
            for side in [j.larg.as_ref(), j.rarg.as_ref()] {
                note_locks_below(side, sel, scope, snapshot);
            }
        }
        Some(ast::Node::RangeSubselect(_)) => note_subquery_locks(item, None, scope, snapshot),
        _ => {}
    }
}

/// The FROM item aliases `from` holds that the planner certainly proves
/// empty, so their scans become dummy relations whose security quals are
/// never built (`set_dummy_rel_pathlist`): every item when `where_clause`
/// is a constant FALSE (or NULL, FALSE in a qual), and the sides a
/// constant-FALSE inner `ON` empties.
fn certainly_empty_from_aliases(
    from: &[Option<ast::Node>],
    where_clause: Option<&ast::Node>,
    snapshot: &PgCatalog,
) -> Vec<String> {
    use crate::expr::plan_fold::{Folded, fold_qual};
    let where_empty =
        where_clause.is_some_and(|w| fold_qual(Some(w), snapshot) == Folded::Bool(false));
    let mut out = Vec::new();
    for item in from {
        if where_empty {
            collect_from_aliases(item.as_ref(), &mut out);
        } else {
            mark_empty_from_item(item.as_ref(), snapshot, &mut out);
        }
    }
    out
}

/// The `RangeVar` aliases of `item`, nested joins included; a subquery's own
/// relations are another level's.
fn collect_from_aliases(item: Option<&ast::Node>, out: &mut Vec<String>) {
    match item {
        Some(ast::Node::RangeVar(rv)) => {
            let alias = rv
                .alias
                .as_ref()
                .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
            if !out.contains(&alias) {
                out.push(alias);
            }
        }
        Some(ast::Node::JoinExpr(j)) => {
            for side in [j.larg.as_ref(), j.rarg.as_ref()].into_iter().flatten() {
                collect_from_aliases(Some(side), out);
            }
        }
        _ => {}
    }
}

/// Add to `out` the FROM items of `item` the planner certainly proves empty,
/// and return whether `item` itself is. A constant-FALSE `ON` makes an inner
/// join empty, and for an outer join its nullable side (no row there can
/// match, so the side is a dummy relation); a `FULL` join keeps both sides
/// (unmatched rows of either side still come out). An empty side of an inner
/// join empties the whole join; an empty preserved side of an outer join
/// empties it too.
fn mark_empty_from_item(
    item: Option<&ast::Node>,
    snapshot: &PgCatalog,
    out: &mut Vec<String>,
) -> bool {
    use crate::expr::plan_fold::{Folded, fold_qual};
    use typedpg_pg_query::ast::JoinType;
    let Some(ast::Node::JoinExpr(j)) = item else {
        return false;
    };
    let on_false = j
        .quals
        .as_ref()
        .is_some_and(|q| fold_qual(Some(q), snapshot) == Folded::Bool(false));
    let jointype = j.jointype;
    let mark = |side: Option<&ast::Node>, out: &mut Vec<String>| {
        side.is_some_and(|s| mark_empty_from_item(Some(s), snapshot, out))
    };
    match jointype {
        JoinType::JoinInner | JoinType::JoinSemi | JoinType::JoinAnti => {
            let left = mark(j.larg.as_ref(), out);
            let right = mark(j.rarg.as_ref(), out);
            if on_false || left || right {
                collect_from_aliases(item, out);
                true
            } else {
                false
            }
        }
        JoinType::JoinLeft => {
            let left = mark(j.larg.as_ref(), out);
            if on_false {
                if let Some(right) = j.rarg.as_ref() {
                    collect_from_aliases(Some(right), out);
                }
            } else {
                mark(j.rarg.as_ref(), out);
            }
            if left {
                collect_from_aliases(item, out);
                true
            } else {
                false
            }
        }
        JoinType::JoinRight => {
            let right = mark(j.rarg.as_ref(), out);
            if on_false {
                if let Some(left) = j.larg.as_ref() {
                    collect_from_aliases(Some(left), out);
                }
            } else {
                mark(j.larg.as_ref(), out);
            }
            if right {
                collect_from_aliases(item, out);
                true
            } else {
                false
            }
        }
        // A FULL join keeps both sides, so only its own side restrictions
        // can empty it — and only when both sides are.
        _ => {
            let left = mark(j.larg.as_ref(), out);
            let right = mark(j.rarg.as_ref(), out);
            left && right
        }
    }
}
