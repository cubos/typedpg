use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// SELECT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_select(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, &HashMap::new(), &[], &[], &[])
}

/// Like [`analyze_select`] but seeds the initial scope with `outer_sources`
/// as **correlated** references — a subquery's column lookup tries its local
/// FROM first and only falls back to these outer sources when nothing
/// matched. Used for `EXISTS (...)`, scalar sublinks, and `IN (SELECT ...)`,
/// where PG's lexical rule says inner aliases shadow outer ones.
pub(crate) fn analyze_correlated_select(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_scope: &crate::scope::Scope,
) -> AnalyzeResult {
    // Everything the enclosing level can see — its own FROM plus any
    // lateral refs it received — is reachable from the sublink as a
    // correlated (fallback-only) reference; and so is what *it* reaches
    // that way, the levels further out (PG searches the whole ParseState
    // chain). Those come behind the enclosing level's entries: an entry of
    // the same name there hides them, and so does a column of the same name
    // for unqualified references.
    let mut outer: Vec<crate::scope::TableSource> = outer_scope
        .sources
        .iter()
        .chain(outer_scope.lateral_sources.iter())
        .cloned()
        .collect();
    let near_aliases: std::collections::HashSet<String> =
        outer.iter().map(|s| s.alias.clone()).collect();
    let near_columns: std::collections::HashSet<String> = outer
        .iter()
        .flat_map(|s| s.visible_columns().map(|c| c.name.clone()))
        .collect();
    let rule_pseudo = crate::scope::Scope::default().outer_sources;
    for far in &outer_scope.outer_sources {
        if near_aliases.contains(&far.alias) || rule_pseudo.iter().any(|p| p.alias == far.alias) {
            continue;
        }
        let mut far = far.clone();
        for c in &far.columns {
            if near_columns.contains(&c.name) {
                far.join_hidden.insert(c.name.clone());
            }
        }
        outer.push(far);
    }
    let (mut cols, p) = analyze_select_with_ctes_and_outer(
        sel,
        snapshot,
        params,
        &outer_scope.ctes,
        &[],
        &outer,
        // Unreferencable entries stay so inside the sublink (errorMissingRTE
        // searches every enclosing range table).
        &outer_scope.shadowed_sources,
    )?;
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
    sel: &protobuf::SelectStmt,
    cols: &mut [RawColumn],
    params: &mut ParamCollector,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    expr::resolve_untyped_output_params(sel, snapshot, params)?;
    let direct = sel.op == SetOperation::SetopNone as i32
        && sel.values_lists.is_empty()
        && sel.target_list.len() == cols.len();
    for (i, col) in cols.iter_mut().enumerate() {
        if col.type_oid != oid::UNKNOWN {
            continue;
        }
        col.type_oid = oid::TEXT;
        if direct
            && let Some(node::Node::ResTarget(rt)) = sel.target_list[i].node.as_ref()
            && let Some(node::Node::ParamRef(p)) = rt.val.as_deref().and_then(|v| v.node.as_ref())
            && params.get(p.number) == oid::UNKNOWN
        {
            params.record(p.number, oid::TEXT);
        }
    }
    Ok(())
}

pub(crate) fn analyze_select_with_ctes(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    analyze_select_with_ctes_and_outer(sel, snapshot, params, outer_ctes, &[], &[], &[])
}

/// Core SELECT analyzer.
///
/// Three flavours of outer scope, mirroring PG's distinction:
/// - `lateral_sources`: pre-visible aliases for `LATERAL` subqueries —
///   merged into the local FROM scope so the inner query sees them as if
///   they were declared locally.
/// - `correlated_sources`: pre-visible aliases for plain sublinks
///   (`EXISTS`, scalar, `IN`, `ANY`/`ALL`) — only consulted as a fallback
///   when local resolution fails, so an inner alias of the same name
///   shadows the outer one.
/// - `shadowed_sources`: aliases visible in the enclosing FROM but
///   *unreachable* from inside this query (non-LATERAL FROM subquery). Not
///   used for resolution — only to upgrade the diagnostic from a generic
///   missing-column to PG's `invalid reference to FROM-clause entry for
///   table "x"` when the SQL tries to reach across the boundary.
pub(crate) fn analyze_select_with_ctes_and_outer(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    lateral_sources: &[crate::scope::TableSource],
    correlated_sources: &[crate::scope::TableSource],
    shadowed_sources: &[crate::scope::TableSource],
) -> AnalyzeResult {
    let _level = QueryLevel::enter();
    // Start with outer CTEs (from parent WITH clause).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();

    // Process this SELECT's own CTEs (before UNION check, since WITH wraps UNION).
    if let Some(with) = &sel.with_clause {
        // The CTE bodies see the enclosing levels (LATERAL or correlated).
        let outer: Vec<_> = lateral_sources
            .iter()
            .chain(correlated_sources)
            .cloned()
            .collect();
        cte_scopes = analyze_with_clause(with, snapshot, params, &cte_scopes, &outer)?;
    }

    // Handle UNION/INTERSECT/EXCEPT. The set operation is a query level of
    // its own, without a FROM clause; its arms are the levels below.
    // Outer references reach the arms and the VALUES rows below like any
    // subquery's.
    let outer: Vec<crate::scope::TableSource> = lateral_sources
        .iter()
        .chain(correlated_sources)
        .cloned()
        .collect();
    if sel.op != SetOperation::SetopNone as i32 {
        grouping::register_level(sel, &[], &[]);
        return analyze_set_operation(sel, snapshot, params, &cte_scopes, &outer, shadowed_sources);
    }

    // Handle `VALUES (…), (…), …` — a `SelectStmt` without a FROM/target
    // list, carrying rows in `values_lists`. Column types are derived from
    // the first row; names default to `column1`/`column2`/… (PG convention)
    // and are typically overridden by a `AS alias(col1, col2)` column list
    // at the RangeSubselect that wraps the VALUES.
    // The rows see the LATERAL and correlated outer levels like any
    // expression of this level (`LATERAL (VALUES (v.a))`).
    if !sel.values_lists.is_empty() {
        grouping::register_level(sel, &[], &[]);
        let mut values_scope = Scope {
            ctes: cte_scopes.clone(),
            ..Scope::default()
        };
        values_scope
            .lateral_sources
            .extend(lateral_sources.iter().cloned());
        values_scope
            .outer_sources
            .extend(correlated_sources.iter().cloned());
        values_scope
            .shadowed_sources
            .extend(shadowed_sources.iter().cloned());
        let columns = analyze_values_lists(&sel.values_lists, snapshot, params, &values_scope)?;
        values_sort_and_limit(sel, &columns, snapshot, params, &cte_scopes, &outer)?;
        return Ok((columns, None));
    }

    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    // LATERAL: outer aliases resolve like outer references — the subquery's
    // own FROM wins first, they're excluded from `*` expansion, and two
    // lateral sources sharing a column name are ambiguous among themselves
    // (their own tier). Correlated sublinks: outer aliases are only a
    // fallback so an inner alias of the same name shadows correctly.
    // Shadowed: aliases live only as a hint for the diagnostic when the SQL
    // reaches across the boundary.
    scope
        .lateral_sources
        .extend(lateral_sources.iter().cloned());
    scope
        .outer_sources
        .extend(correlated_sources.iter().cloned());
    scope
        .shadowed_sources
        .extend(shadowed_sources.iter().cloned());
    let mut null_ctx = NullabilityContext::default();
    null_ctx.has_group_by = !sel.group_clause.is_empty();
    null_ctx.srfs_in_lockstep = count_srf_calls(&sel.target_list, snapshot) > 1;
    null_ctx.window_frames = sel
        .window_clause
        .iter()
        .filter_map(|w| match w.node.as_ref()? {
            node::Node::WindowDef(wd) => Some((wd.name.clone(), wd.frame_options)),
            _ => None,
        })
        .collect();

    // Process FROM clause. An aggregate of this level inside it (in a
    // LATERAL subquery) is PG's EXPR_KIND_FROM_SUBSELECT error; JOIN
    // conditions and function arguments name their own clause.
    grouping::with_clause(Some("FROM clause of their own query level"), || {
        process_from_clause(
            &sel.from_clause,
            &mut scope,
            &mut null_ctx,
            snapshot,
            &cte_scopes,
            params,
        )
    })?;
    grouping::register_level(sel, &scope.sources, &scope.shadowed_sources);

    // PG transforms the target list right after FROM; note which bare `$N`
    // outputs it sees untyped before WHERE & co. can type them.
    expr::note_untyped_output_params(
        &sel.target_list,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    );

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
    let expansion = grouping::expand_grouping_sets(&sel.group_clause, &scope, &targets);
    null_ctx.grouping_omitted = expansion.omitted;
    null_ctx.has_empty_grouping_set = expansion.has_empty_set;

    // Process WHERE clause — PG uses COERCION_ASSIGNMENT + BOOL goal, and
    // emits its own wording on mismatch: `argument of WHERE must be type
    // boolean, not type X`. Catch the generic coerce error and rewrite to
    // PG's exact message so `pglite_sanity` matches.
    if let Some(where_clause) = &sel.where_clause {
        // PG rejects aggregate / window function calls inside WHERE (they
        // reference the post-aggregation row, not the pre-aggregation one) —
        // but only after the expression itself resolves; the ordering lives
        // in `coerce_bool_clause`.
        crate::clause::coerce_clause_expr(
            where_clause,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::Where,
        )?;
        check_no_srf_in_clause(where_clause, snapshot, "WHERE")?;
    }

    // Collect select-list aliases so GROUP BY / ORDER BY can fall back to
    // them when a bare identifier doesn't resolve against the FROM scope.
    // PG accepts `SELECT name AS n FROM t GROUP BY n ORDER BY n`; without
    // this fallback, propagating errors from those walks would regress
    // legitimate queries.
    let select_aliases: std::collections::HashSet<String> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.node.as_ref()? {
            node::Node::ResTarget(rt) if !rt.name.is_empty() => Some(rt.name.clone()),
            _ => None,
        })
        .collect();

    // Process GROUP BY expressions — no type expectation, but we still need
    // to walk them so any parameters referenced are collected and typed
    // and column refs validated. `GroupingSet` nodes (`GROUPING SETS` /
    // `ROLLUP` / `CUBE`) are not real expressions; recurse into their
    // `content` to reach the underlying column references and
    // aggregate-rejection checks.
    grouping::with_clause(Some("GROUP BY"), || {
        for group_node in &sel.group_clause {
            walk_group_clause_node(
                group_node,
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                params,
                &select_aliases,
                &targets,
            )?;
        }
        Ok::<(), AnalyzeError>(())
    })?;

    // Process HAVING clause — same boolean goal as WHERE, but aggregates
    // are of course allowed there.
    if let Some(having) = &sel.having_clause {
        crate::clause::coerce_clause_expr(
            having,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::Having,
        )?;
        check_no_srf_in_clause(having, snapshot, "HAVING")?;
    }

    // Process ORDER BY expressions. Sort items are wrapped in `SortBy` nodes
    // — we walk the inner expression so parameters referenced there (e.g.
    // `ORDER BY embedding <=> $embedding`) get their types inferred from
    // operator context and any column refs are validated. A bare
    // identifier may name a select-list alias that isn't in the FROM
    // scope (PG resolution rule); suppress `UndefinedColumn` only in that
    // exact shape so typos still surface. Integer literals are *ordinals*:
    // they reference a projection position and must be in range (42P10).
    let n_targets = targets.len();
    // `SELECT DISTINCT` (the plain form parses as one empty node) restricts
    // ORDER BY to expressions that appear in the select list.
    let plain_distinct =
        !sel.distinct_clause.is_empty() && sel.distinct_clause.iter().all(|n| n.node.is_none());
    for sort_node in &sel.sort_clause {
        let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref() else {
            continue;
        };
        let Some(inner) = sb.node.as_deref() else {
            continue;
        };
        if let Some(ord) = sql92_position(inner, "ORDER BY")? {
            if ord < 1 || ord as usize > n_targets {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "ORDER BY",
                    ord,
                    crate::error::node_location(inner)
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            continue;
        }
        if let Err(e) = expr::infer_expr(
            inner,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            TypeGoal::NONE,
        ) && !is_select_alias_reference(inner, &select_aliases, &e)
        {
            return Err(e);
        }
        if plain_distinct && !sort_expr_in_select_list(inner, &sel.target_list, &select_aliases) {
            return Err(crate::pgmsg::distinct_order_by_not_in_select_list(
                crate::error::node_location(inner)
                    .and_then(crate::error::SourceSpan::from_node_qname),
            )
            .finalize_implicit());
        }
    }

    // Process `DISTINCT ON (…)` expressions the same way — they resolve
    // like ORDER BY items (select-list aliases allowed). A plain `DISTINCT`
    // parses as a single empty node; skip it. Without this walk, a `$N`
    // referenced only inside DISTINCT ON was never registered with the
    // collector and analysis died on the param-count invariant.
    for distinct_node in &sel.distinct_clause {
        if let Some(ord) = sql92_position(distinct_node, "DISTINCT ON")? {
            if ord < 1 || ord as usize > n_targets {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "DISTINCT ON",
                    ord,
                    crate::error::node_location(distinct_node)
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            continue;
        }
        if distinct_node.node.is_some()
            && let Err(e) = expr::infer_expr(
                distinct_node,
                expr::Ctx::new(&scope, &null_ctx, snapshot),
                params,
                TypeGoal::NONE,
            )
            && !is_select_alias_reference(distinct_node, &select_aliases, &e)
        {
            return Err(e);
        }
    }

    // Named-window references: `OVER w` (and `OVER (w …)` inheritance) must
    // name a window defined in this SELECT's WINDOW clause — PG (42704):
    // `window "w" does not exist`. Window calls only appear in the target
    // list and ORDER BY.
    let defined_windows: std::collections::HashSet<&str> = sel
        .window_clause
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::WindowDef(w) if !w.name.is_empty() => Some(w.name.as_str()),
            _ => None,
        })
        .collect();
    for t in &sel.target_list {
        if let Some(node::Node::ResTarget(rt)) = t.node.as_ref()
            && let Some(val) = &rt.val
        {
            check_window_refs(val, &defined_windows)?;
        }
    }
    for sort_node in &sel.sort_clause {
        if let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref()
            && let Some(inner) = sb.node.as_deref()
        {
            check_window_refs(inner, &defined_windows)?;
        }
    }
    // The WINDOW clause's own definitions, and the inline windows that
    // inherit from them (PG's transformWindowDefinitions).
    expr::check_window_clause(sel, expr::Ctx::new(&scope, &null_ctx, snapshot), params)?;

    analyze_limit_offset(sel, expr::Ctx::new(&scope, &null_ctx, snapshot), params)?;

    // Resolve target list (SELECT expressions) — no type expectation.
    let (columns, explicit_collations) = resolve_target_list_explicit(
        &sel.target_list,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
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
        .chain(sel.where_clause.as_deref())
        .chain(sel.having_clause.as_deref())
        .chain(sel.sort_clause.iter())
        .chain(sel.group_clause.iter())
    {
        check_srf_nesting(n, snapshot)?;
    }

    check_order_by_using(
        &sel.sort_clause,
        &sel.target_list,
        &columns,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;
    check_distinct_on_matches_order_by(sel, &columns)?;
    check_sort_group_keys(
        sel,
        &targets,
        &columns,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;

    // `FOR UPDATE [OF …]` (and FOR SHARE / NO KEY UPDATE / KEY SHARE) —
    // PG checks it last in transformSelectStmt.
    check_locking_clause(sel, &scope, &null_ctx, snapshot)?;

    // Aggregate ownership and parseCheckAggregates — which PG runs after
    // everything above.
    grouping::finish_select_level(sel, &scope, &targets, snapshot)?;

    Ok((columns, None))
}

/// What an ORDER BY / GROUP BY / DISTINCT ON item denotes.
enum KeyRef<'a> {
    /// A select-list entry (expanded-target index).
    Target(usize),
    /// An expression of its own.
    Expr(&'a protobuf::Node),
}

/// The expression of expanded select-list entry `i`, a column reference for
/// a `*` column.
fn target_expr(targets: &[ExpandedTarget<'_>], i: usize) -> Option<protobuf::Node> {
    match targets.get(i)? {
        ExpandedTarget::Expr(rt) => rt.val.as_deref().cloned(),
        ExpandedTarget::StarColumn {
            column: Some((alias, col)),
            ..
        } => Some(protobuf::Node {
            node: Some(node::Node::ColumnRef(protobuf::ColumnRef {
                fields: [*alias, *col]
                    .into_iter()
                    .map(|s| protobuf::Node {
                        node: Some(node::Node::String(protobuf::String { sval: s.into() })),
                    })
                    .collect(),
                location: -1,
            })),
        }),
        ExpandedTarget::StarColumn { column: None, .. } => None,
    }
}

/// PG's findTargetlistEntrySQL92 for an ORDER BY / GROUP BY / DISTINCT ON
/// item: an integer constant is a select-list position; a bare name is the
/// output column of that name (in GROUP BY only when no input column has
/// it) — `{clause} "a" is ambiguous` (42702) when several output columns
/// of that name have differing expressions; anything else is an expression.
fn sql92_key<'a>(
    node: &'a protobuf::Node,
    clause: &str,
    targets: &[ExpandedTarget<'_>],
    columns: &[RawColumn],
    scope: &Scope,
) -> Result<KeyRef<'a>, AnalyzeError> {
    if let Some(ord) = ordinal_of(node) {
        return Ok(match usize::try_from(ord - 1) {
            Ok(i) if i < columns.len() => KeyRef::Target(i),
            _ => KeyRef::Expr(node),
        });
    }
    let Some(node::Node::ColumnRef(cr)) = node.node.as_ref() else {
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
    let matches: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| &c.name == name)
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = matches.first() else {
        return Ok(KeyRef::Expr(node));
    };
    let first_expr = target_expr(targets, first);
    for &other in &matches[1..] {
        let same = match (&first_expr, target_expr(targets, other)) {
            (Some(a), Some(b)) => grouping::same_level_exprs_equal(a, &b, scope),
            _ => false,
        };
        if !same {
            return Err(crate::pgmsg::clause_name_ambiguous(
                clause,
                name,
                crate::error::SourceSpan::from_node_qname(cr.location),
            )
            .finalize_implicit());
        }
    }
    Ok(KeyRef::Target(first))
}

/// The operator and collation requirements on the sort / group keys of a
/// SELECT level: get_sort_group_operators for each ORDER BY / GROUP BY /
/// DISTINCT [ON] key, each window's PARTITION BY / ORDER BY, and each
/// aggregate's DISTINCT arguments and ORDER BY (42883 when the type has no
/// such operator), then assign_collations' rule that a sort / group key's
/// collation must be determinate (42P21).
fn check_sort_group_keys(
    sel: &protobuf::SelectStmt,
    targets: &[ExpandedTarget<'_>],
    columns: &[RawColumn],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Result<(), AnalyzeError> {
    use crate::clause::KeyUse;
    let scope = ctx.scope;
    // `(key, operators needed — None for ORDER BY … USING, location)`.
    let mut keys: Vec<(KeyRef<'_>, Option<KeyUse>, i32)> = Vec::new();
    let loc = |n: &protobuf::Node| crate::error::node_location(n).unwrap_or(-1);
    fn sort_items(order: &[protobuf::Node]) -> Vec<(&protobuf::Node, bool)> {
        order
            .iter()
            .filter_map(|o| match o.node.as_ref()? {
                node::Node::SortBy(sb) => Some((
                    sb.node.as_deref()?,
                    sb.sortby_dir() == protobuf::SortByDir::SortbyUsing,
                )),
                _ => None,
            })
            .collect()
    }
    for (inner, using) in sort_items(&sel.sort_clause) {
        let key = sql92_key(inner, "ORDER BY", targets, columns, scope)?;
        keys.push((key, (!using).then_some(KeyUse::Sort), loc(inner)));
    }
    let mut stack: Vec<&protobuf::Node> = sel.group_clause.iter().rev().collect();
    while let Some(g) = stack.pop() {
        if let Some(node::Node::GroupingSet(gs)) = g.node.as_ref() {
            stack.extend(gs.content.iter().rev());
        } else if let Some(items) = grouping::implicit_row_items(g) {
            stack.extend(items.iter().rev());
        } else {
            let key = sql92_key(g, "GROUP BY", targets, columns, scope)?;
            keys.push((key, Some(KeyUse::Group), loc(g)));
        }
    }
    let plain_distinct =
        !sel.distinct_clause.is_empty() && sel.distinct_clause.iter().all(|n| n.node.is_none());
    if plain_distinct {
        for (i, t) in targets.iter().enumerate() {
            let location = match t {
                ExpandedTarget::Expr(rt) => rt.location,
                ExpandedTarget::StarColumn { location, .. } => *location,
            };
            keys.push((KeyRef::Target(i), Some(KeyUse::Group), location));
        }
    } else {
        for d in sel.distinct_clause.iter().filter(|n| n.node.is_some()) {
            let key = sql92_key(d, "DISTINCT ON", targets, columns, scope)?;
            keys.push((key, Some(KeyUse::Group), loc(d)));
        }
    }
    // Windows and aggregates of this level.
    let mut windows: Vec<&protobuf::WindowDef> = sel
        .window_clause
        .iter()
        .filter_map(|w| match w.node.as_ref()? {
            node::Node::WindowDef(wd) => Some(&**wd),
            _ => None,
        })
        .collect();
    let mut level_exprs: Vec<&protobuf::Node> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.node.as_ref()? {
            node::Node::ResTarget(rt) => rt.val.as_deref(),
            _ => None,
        })
        .collect();
    level_exprs.extend(sel.having_clause.as_deref());
    level_exprs.extend(sort_items(&sel.sort_clause).into_iter().map(|(n, _)| n));
    let mut agg_keys: Vec<(&protobuf::Node, KeyUse)> = Vec::new();
    for e in level_exprs {
        visit_same_level(e, &mut |n| {
            // SQL/JSON aggregates: their ORDER BY and window.
            let json_agg = match n.node.as_ref() {
                Some(node::Node::JsonObjectAgg(a)) => a.constructor.as_deref(),
                Some(node::Node::JsonArrayAgg(a)) => a.constructor.as_deref(),
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
            let Some(node::Node::FuncCall(fc)) = n.node.as_ref() else {
                return;
            };
            if let Some(over) = fc.over.as_deref() {
                windows.push(over);
            }
            if fc.agg_distinct {
                agg_keys.extend(fc.args.iter().map(|a| (a, KeyUse::AggDistinct)));
            }
            agg_keys.extend(
                sort_items(&fc.agg_order)
                    .into_iter()
                    .map(|(n, _)| (n, KeyUse::Sort)),
            );
        });
    }
    for wd in windows {
        keys.extend(
            wd.partition_clause
                .iter()
                .map(|p| (KeyRef::Expr(p), Some(KeyUse::Group), loc(p))),
        );
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

    let key_type = |key: &KeyRef<'_>| match key {
        KeyRef::Target(i) => columns.get(*i).map(|c| c.type_oid),
        KeyRef::Expr(n) => {
            let mut scratch = params.clone();
            expr::infer_expr(n, ctx, &mut scratch, TypeGoal::NONE)
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
            KeyRef::Target(i) => target_expr(targets, *i),
            KeyRef::Expr(n) => Some((*n).clone()),
        };
        if let Some(e) = expr {
            crate::clause::collation_state(&e, ctx, params)
                .check_determinate(ctx.snapshot, Some(*location))?;
        }
    }
    Ok(())
}

/// ORDER BY / LIMIT / OFFSET of a bare `VALUES` list (PG's
/// transformValuesClause): they see its columns (`column1`, …) the way a
/// query sees a FROM item, and select-list positions count its columns.
fn values_sort_and_limit(
    sel: &protobuf::SelectStmt,
    columns: &[RawColumn],
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    outer: &[crate::scope::TableSource],
) -> Result<(), AnalyzeError> {
    let alias = crate::scope::hidden_alias("values");
    let cols: Vec<ScopeColumn> = columns
        .iter()
        .map(|c| ScopeColumn {
            name: c.name.clone(),
            type_oid: c.type_oid,
            base_not_null: !c.nullable,
            typmod: c.typmod,
            collation: c.collation,
            table_alias: alias.clone(),
            record_fields: c.record_fields.clone(),
            elem_nullable: c.elem_nullable,
        })
        .collect();
    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    scope.outer_sources.extend(outer.iter().cloned());
    scope.add_derived(&alias, cols, crate::scope::SourceKind::Other)?;
    let null_ctx = NullabilityContext::default();
    for sort_node in &sel.sort_clause {
        let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref() else {
            continue;
        };
        let Some(inner) = sb.node.as_deref() else {
            continue;
        };
        if let Some(ord) = sql92_position(inner, "ORDER BY")? {
            if ord < 1 || ord as usize > columns.len() {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "ORDER BY",
                    ord,
                    crate::error::node_location(inner)
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            continue;
        }
        expr::infer_expr(
            inner,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            TypeGoal::NONE,
        )?;
    }
    analyze_limit_offset(sel, expr::Ctx::new(&scope, &null_ctx, snapshot), params)
}

/// Recursively find window-function calls and verify that any *named*
/// window they reference (`OVER w` sets `WindowDef.name`; `OVER (w …)`
/// inheritance sets `refname`) is defined in the SELECT's WINDOW clause.
/// SubLinks are skipped — their windows belong to the inner query.
fn check_window_refs(
    node: &protobuf::Node,
    defined: &std::collections::HashSet<&str>,
) -> Result<(), AnalyzeError> {
    let Some(inner) = node.node.as_ref() else {
        return Ok(());
    };
    let check_name = |name: &str, location: i32| -> Result<(), AnalyzeError> {
        if !name.is_empty() && !defined.contains(name) {
            // PG positions it at the window's name.
            let span = crate::error::SourceSpan::from_node_qname(location);
            return Err(crate::pgmsg::window_does_not_exist(name, span).finalize_implicit());
        }
        Ok(())
    };
    match inner {
        node::Node::FuncCall(fc) => {
            if let Some(over) = &fc.over {
                check_name(&over.name, over.location)?;
                check_name(&over.refname, over.location)?;
            }
            for arg in &fc.args {
                check_window_refs(arg, defined)?;
            }
            if let Some(f) = &fc.agg_filter {
                check_window_refs(f, defined)?;
            }
        }
        node::Node::AExpr(e) => {
            if let Some(l) = &e.lexpr {
                check_window_refs(l, defined)?;
            }
            if let Some(r) = &e.rexpr {
                check_window_refs(r, defined)?;
            }
        }
        node::Node::BoolExpr(b) => {
            for a in &b.args {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::TypeCast(c) => {
            if let Some(a) = &c.arg {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::NamedArgExpr(na) => {
            if let Some(a) = &na.arg {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::CaseExpr(c) => {
            for w in &c.args {
                check_window_refs(w, defined)?;
            }
            if let Some(d) = &c.defresult {
                check_window_refs(d, defined)?;
            }
        }
        node::Node::CaseWhen(w) => {
            if let Some(e) = &w.expr {
                check_window_refs(e, defined)?;
            }
            if let Some(r) = &w.result {
                check_window_refs(r, defined)?;
            }
        }
        node::Node::CoalesceExpr(c) => {
            for a in &c.args {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::MinMaxExpr(m) => {
            for a in &m.args {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::NullTest(t) => {
            if let Some(a) = &t.arg {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::BooleanTest(t) => {
            if let Some(a) = &t.arg {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::AArrayExpr(a) => {
            for e in &a.elements {
                check_window_refs(e, defined)?;
            }
        }
        node::Node::RowExpr(r) => {
            for a in &r.args {
                check_window_refs(a, defined)?;
            }
        }
        node::Node::List(l) => {
            for i in &l.items {
                check_window_refs(i, defined)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// findTargetlistEntrySQL92's constant rule for an ORDER BY / GROUP BY /
/// DISTINCT ON item: an integer constant is a 1-based select-list position
/// (`Some`), any other constant — string, float, boolean, NULL — is
/// `non-integer constant in {clause}` (42601), and anything else is an
/// expression (`None`).
pub(crate) fn sql92_position(
    node: &protobuf::Node,
    clause: &str,
) -> Result<Option<i64>, AnalyzeError> {
    let Some(node::Node::AConst(ac)) = node.node.as_ref() else {
        return Ok(None);
    };
    match &ac.val {
        Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)) if !ac.isnull => {
            Ok(Some(i.ival as i64))
        }
        _ => Err(crate::pgmsg::non_integer_constant(
            clause,
            crate::error::SourceSpan::from_node_token(ac.location),
        )
        .finalize_implicit()),
    }
}

/// If `node` is a bare integer literal, return its value — GROUP BY / ORDER
/// BY treat those as 1-based projection ordinals.
fn ordinal_of(node: &protobuf::Node) -> Option<i64> {
    if let Some(node::Node::AConst(ac)) = node.node.as_ref()
        && !ac.isnull
        && let Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)) = &ac.val
    {
        return Some(i.ival as i64);
    }
    None
}

/// Structural fingerprint of an expression node with the `location` fields
/// neutralized — `Debug` output with every `location: N` span removed. Used
/// to compare an ORDER BY expression against the projection entries (PG's
/// "appears in select list" test), where byte positions necessarily differ.
pub(crate) fn node_fingerprint(node: &protobuf::Node) -> String {
    let dbg = format!("{node:?}");
    let mut out = String::with_capacity(dbg.len());
    let mut rest = dbg.as_str();
    while let Some(pos) = rest.find("location: ") {
        out.push_str(&rest[..pos]);
        rest = &rest[pos + "location: ".len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '-')
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// PG's SELECT DISTINCT rule: an ORDER BY expression must appear in the
/// select list — as a structurally equal expression or as a select-list
/// alias (ordinals are handled by the caller).
fn sort_expr_in_select_list(
    inner: &protobuf::Node,
    target_list: &[protobuf::Node],
    select_aliases: &std::collections::HashSet<String>,
) -> bool {
    if let Some(node::Node::ColumnRef(cr)) = inner.node.as_ref() {
        let parts = expr::extract_string_fields(&cr.fields);
        if let [single] = parts.as_slice()
            && select_aliases.contains(single)
        {
            return true;
        }
    }
    let want = node_fingerprint(inner);
    target_list.iter().any(|t| {
        if let Some(node::Node::ResTarget(rt)) = t.node.as_ref()
            && let Some(val) = &rt.val
        {
            node_fingerprint(val) == want
        } else {
            false
        }
    })
}

/// Walk one entry from `sel.group_clause`, recursing into `GroupingSet`
/// nodes (`GROUPING SETS`/`ROLLUP`/`CUBE`) to reach the underlying
/// expressions. The walk type-checks parameters and rejects aggregates /
/// window calls inside the grouping expressions (PG forbids those too).
fn walk_group_clause_node(
    group_node: &protobuf::Node,
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
    let members = match group_node.node.as_ref() {
        Some(node::Node::GroupingSet(gs)) => Some(&gs.content),
        _ => grouping::implicit_row_items(group_node),
    };
    if let Some(members) = members {
        for inner in members {
            walk_group_clause_node(
                inner,
                expr::Ctx::new(scope, null_ctx, snapshot),
                params,
                select_aliases,
                targets,
            )?;
        }
        // transformGroupingSet: a CUBE over more than 12 elements.
        if let Some(node::Node::GroupingSet(gs)) = group_node.node.as_ref()
            && gs.kind == protobuf::GroupingSetKind::GroupingSetCube as i32
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
    if let Some(ord) = sql92_position(group_node, "GROUP BY")? {
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
            ExpandedTarget::Expr(rt) => check_group_target_has_no_aggregates(Some(rt), snapshot),
            ExpandedTarget::StarColumn { .. } => Ok(()),
        };
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
        let alias = expr::extract_string_fields(match group_node.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) => &cr.fields,
            _ => &[],
        });
        let target = targets.iter().find_map(|t| match t {
            ExpandedTarget::Expr(rt) if Some(&rt.name) == alias.first() => Some(*rt),
            _ => None,
        });
        return check_group_target_has_no_aggregates(target, snapshot);
    }
    crate::clause::check_level_calls(
        group_node,
        expr::Ctx::new(scope, null_ctx, snapshot),
        "GROUP BY",
        true,
    )?;
    Ok(())
}

/// PG's checkTargetlistEntrySQL92 for GROUP BY: a projection entry referenced
/// by ordinal or alias must not contain aggregate calls — `GROUPING(…)`
/// counts (contain_aggs_of_level) — else `aggregate functions are not
/// allowed in GROUP BY` (42803).
fn check_group_target_has_no_aggregates(
    target: Option<&protobuf::ResTarget>,
    snapshot: &crate::pg_catalog::PgCatalog,
) -> Result<(), AnalyzeError> {
    if let Some(rt) = target
        && let Some(val) = &rt.val
    {
        let kinds = expr::detect_func_kinds(val, snapshot);
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
    node: &protobuf::Node,
    aliases: &std::collections::HashSet<String>,
    err: &AnalyzeError,
) -> bool {
    if !matches!(err, AnalyzeError::UndefinedColumn(_)) {
        return false;
    }
    let Some(node::Node::ColumnRef(cr)) = node.node.as_ref() else {
        return false;
    };
    if cr.fields.len() != 1 {
        return false;
    }
    let Some(node::Node::String(s)) = cr.fields[0].node.as_ref() else {
        return false;
    };
    aliases.contains(&s.sval)
}

/// PG's `LCS_asString`: the clause's spelling in messages.
pub(crate) fn lock_strength_name(lc: &protobuf::LockingClause) -> &'static str {
    match lc.strength() {
        typedpg_pg_query::protobuf::LockClauseStrength::LcsForkeyshare => "FOR KEY SHARE",
        typedpg_pg_query::protobuf::LockClauseStrength::LcsForshare => "FOR SHARE",
        typedpg_pg_query::protobuf::LockClauseStrength::LcsFornokeyupdate => "FOR NO KEY UPDATE",
        _ => "FOR UPDATE",
    }
}

/// The first `CheckSelectLocking` (analyze.c) violation of `sel`: the
/// construct a locking clause is not allowed with.
fn select_lock_blocker(sel: &protobuf::SelectStmt, snapshot: &PgCatalog) -> Option<&'static str> {
    if sel.op != SetOperation::SetopNone as i32 {
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
    let level_exprs: Vec<&protobuf::Node> = sel
        .target_list
        .iter()
        .filter_map(|t| match t.node.as_ref()? {
            node::Node::ResTarget(rt) => rt.val.as_deref(),
            _ => None,
        })
        .chain(
            sel.sort_clause
                .iter()
                .filter_map(|n| match n.node.as_ref()? {
                    node::Node::SortBy(sb) => sb.node.as_deref(),
                    _ => None,
                }),
        )
        .collect();
    let kinds: Vec<_> = level_exprs
        .iter()
        .map(|e| expr::detect_func_kinds(e, snapshot))
        .collect();
    if kinds.iter().any(|k| k.has_aggregate) {
        return Some("aggregate functions");
    }
    if kinds.iter().any(|k| k.has_window) {
        return Some("window functions");
    }
    if count_srf_calls(&sel.target_list, snapshot) > 0 {
        return Some("set-returning functions in the target list");
    }
    None
}

/// The blocker a locking clause pushed down into FROM subquery `sel` would
/// hit: PG's `transformLockingClause` applies an unqualified (or naming)
/// clause to the subquery, which re-runs `CheckSelectLocking` there and
/// pushes further into *its* FROM subqueries.
pub(crate) fn subquery_lock_blocker(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
) -> Option<&'static str> {
    fn from_item_blocker(n: &protobuf::Node, snapshot: &PgCatalog) -> Option<&'static str> {
        match n.node.as_ref()? {
            node::Node::RangeSubselect(rs) => match rs.subquery.as_deref()?.node.as_ref()? {
                node::Node::SelectStmt(s) => subquery_lock_blocker(s, snapshot),
                _ => None,
            },
            node::Node::JoinExpr(j) => j
                .larg
                .as_deref()
                .and_then(|l| from_item_blocker(l, snapshot))
                .or_else(|| {
                    j.rarg
                        .as_deref()
                        .and_then(|r| from_item_blocker(r, snapshot))
                }),
            _ => None,
        }
    }
    select_lock_blocker(sel, snapshot).or_else(|| {
        sel.from_clause
            .iter()
            .find_map(|n| from_item_blocker(n, snapshot))
    })
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
    sel: &protobuf::SelectStmt,
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
        let Some(node::Node::LockingClause(lc)) = node.node.as_ref() else {
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
            let Some(node::Node::RangeVar(rv)) = rel.node.as_ref() else {
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
                SourceKind::Join => Some("a join"),
                _ => None,
            };
            if let Some(what) = what {
                return Err(unsupported(format!("{clause} cannot be applied to {what}")));
            }
            locked.push(source);
        }
        for source in &locked {
            if let SourceKind::Subquery {
                lock_blocker: Some(blocker),
            } = source.kind
            {
                return Err(unsupported(format!(
                    "{clause} is not allowed with {blocker}"
                )));
            }
        }
        for source in &locked {
            if matches!(
                source.kind,
                SourceKind::Relation | SourceKind::Subquery { .. }
            ) && null_ctx.is_nullable(&source.alias, "", true)
            {
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
    sel: &protobuf::SelectStmt,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for (limit_node, kind, label) in [
        (&sel.limit_count, crate::clause::ClauseKind::Limit, "LIMIT"),
        (
            &sel.limit_offset,
            crate::clause::ClauseKind::Offset,
            "OFFSET",
        ),
    ] {
        let Some(limit_node) = limit_node else {
            continue;
        };
        crate::clause::coerce_clause_expr(limit_node, ctx, params, kind)?;
        check_no_srf_in_clause(limit_node, ctx.snapshot, label)?;
        let mut var: Option<i32> = None;
        visit_same_level(limit_node, &mut |e| {
            if var.is_none()
                && let Some(node::Node::ColumnRef(cr)) = e.node.as_ref()
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
    if sel.limit_option == protobuf::LimitOption::WithTies as i32
        && let Some(node::Node::AConst(ac)) =
            sel.limit_count.as_deref().and_then(|n| n.node.as_ref())
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
fn references_local_column(cr: &protobuf::ColumnRef, scope: &Scope) -> bool {
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
    sort_clause: &[protobuf::Node],
    target_list: &[protobuf::Node],
    columns: &[RawColumn],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Result<(), AnalyzeError> {
    let snapshot = ctx.snapshot;
    for sort_node in sort_clause {
        let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref() else {
            continue;
        };
        if sb.sortby_dir() != protobuf::SortByDir::SortbyUsing {
            continue;
        }
        let Some(inner) = sb.node.as_deref() else {
            continue;
        };
        let Some(ty) = sort_expr_type(inner, target_list, columns, ctx, params) else {
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
    inner: &protobuf::Node,
    target_list: &[protobuf::Node],
    columns: &[RawColumn],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Option<PgTypeOid> {
    match sort_target_key(inner, target_list, columns) {
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
#[derive(PartialEq, Eq)]
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
    inner: &protobuf::Node,
    target_list: &[protobuf::Node],
    columns: &[RawColumn],
) -> SortKey {
    if let Some(ord) = ordinal_of(inner)
        && ord >= 1
    {
        return SortKey::Target(ord as usize - 1);
    }
    if let Some(node::Node::ColumnRef(cr)) = inner.node.as_ref()
        && let [name] = expr::extract_string_fields(&cr.fields).as_slice()
        && let Some(i) = columns.iter().position(|c| &c.name == name)
    {
        return SortKey::Target(i);
    }
    let fp = node_fingerprint(inner);
    // Only a plain target list (no `*`) lines up with the output columns.
    let has_star = target_list.iter().any(|t| {
        matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
            if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
                Some(node::Node::ColumnRef(cr)) if cr.fields.iter().any(|f|
                    matches!(f.node.as_ref(), Some(node::Node::AStar(_))))))
    });
    if !has_star
        && let Some(i) = target_list.iter().position(|t| {
            matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
                if rt.val.as_deref().is_some_and(|v| node_fingerprint(v) == fp))
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
    sel: &protobuf::SelectStmt,
    columns: &[RawColumn],
) -> Result<(), AnalyzeError> {
    let distinct: Vec<SortKey> = sel
        .distinct_clause
        .iter()
        .filter(|n| n.node.is_some())
        .map(|n| sort_target_key(n, &sel.target_list, columns))
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
    for sort_node in &sel.sort_clause {
        let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref() else {
            continue;
        };
        let Some(inner) = sb.node.as_deref() else {
            continue;
        };
        let key = sort_target_key(inner, &sel.target_list, columns);
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
