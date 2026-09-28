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
    // correlated (fallback-only) reference.
    let outer: Vec<_> = outer_scope
        .sources
        .iter()
        .chain(outer_scope.lateral_sources.iter())
        .cloned()
        .collect();
    analyze_select_with_ctes_and_outer(sel, snapshot, params, &outer_scope.ctes, &[], &outer, &[])
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
    // Start with outer CTEs (from parent WITH clause).
    let mut cte_scopes: HashMap<String, Vec<ScopeColumn>> = outer_ctes.clone();

    // Process this SELECT's own CTEs (before UNION check, since WITH wraps UNION).
    if let Some(with) = &sel.with_clause {
        for cte_node in &with.ctes {
            if let Some(node::Node::CommonTableExpr(cte)) = cte_node.node.as_ref() {
                let cte_columns = analyze_cte(cte, with.recursive, snapshot, params, &cte_scopes)?;
                cte_scopes.insert(cte.ctename.clone(), cte_columns);
            }
        }
    }

    // Handle UNION/INTERSECT/EXCEPT.
    if sel.op != SetOperation::SetopNone as i32 {
        return analyze_set_operation(sel, snapshot, params, &cte_scopes);
    }

    // Handle `VALUES (…), (…), …` — a `SelectStmt` without a FROM/target
    // list, carrying rows in `values_lists`. Column types are derived from
    // the first row; names default to `column1`/`column2`/… (PG convention)
    // and are typically overridden by a `AS alias(col1, col2)` column list
    // at the RangeSubselect that wraps the VALUES.
    if !sel.values_lists.is_empty() {
        return Ok((
            analyze_values_lists(&sel.values_lists, snapshot, params, &cte_scopes)?,
            None,
        ));
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

    // Process FROM clause.
    process_from_clause(
        &sel.from_clause,
        &mut scope,
        &mut null_ctx,
        snapshot,
        &cte_scopes,
        params,
    )?;

    // `FOR UPDATE OF alias` (and FOR SHARE / NO KEY UPDATE / KEY SHARE):
    // every named relation must be a FROM entry of *this* query level.
    check_locking_clause(&sel.locking_clause, &scope)?;

    // Expand `GROUPING SETS` / `ROLLUP` / `CUBE`: promote columns that
    // some grouping set omits to nullable, and remember whether any
    // grouping set is empty (drives aggregate-result nullability).
    let expansion = grouping::expand_grouping_sets(&sel.group_clause, &scope);
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
    for group_node in &sel.group_clause {
        walk_group_clause_node(
            group_node,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            &select_aliases,
            sel.target_list.len(),
        )?;
    }

    // Process HAVING clause — same boolean goal as WHERE, but aggregates
    // are of course allowed there.
    if let Some(having) = &sel.having_clause {
        crate::clause::coerce_clause_expr(
            having,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::Having,
        )?;
    }

    // Process ORDER BY expressions. Sort items are wrapped in `SortBy` nodes
    // — we walk the inner expression so parameters referenced there (e.g.
    // `ORDER BY embedding <=> $embedding`) get their types inferred from
    // operator context and any column refs are validated. A bare
    // identifier may name a select-list alias that isn't in the FROM
    // scope (PG resolution rule); suppress `UndefinedColumn` only in that
    // exact shape so typos still surface. Integer literals are *ordinals*:
    // they reference a projection position and must be in range (42P10).
    let n_targets = sel.target_list.len();
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
        if let Some(ord) = ordinal_of(inner) {
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

    // Process LIMIT / OFFSET — int8 coercion, placement rule, and PG's
    // wording all live in the shared clause walker.
    for (limit_node, kind) in [
        (&sel.limit_count, crate::clause::ClauseKind::Limit),
        (&sel.limit_offset, crate::clause::ClauseKind::Offset),
    ] {
        let Some(limit_node) = limit_node else {
            continue;
        };
        crate::clause::coerce_clause_expr(
            limit_node,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            kind,
        )?;
    }

    // Resolve target list (SELECT expressions) — no type expectation.
    let columns = resolve_target_list(
        &sel.target_list,
        expr::Ctx::new(&scope, &null_ctx, snapshot),
        params,
    )?;

    // In a grouped query, every projected/HAVING/ORDER BY column must be
    // grouped or aggregated (PG SQLSTATE 42803). Checked after the target list
    // so undefined-column errors surface first, matching PG's order.
    crate::grouping::check_grouping(sel, &scope, snapshot)?;

    Ok((columns, None))
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
    let check_name = |name: &str| -> Result<(), AnalyzeError> {
        if !name.is_empty() && !defined.contains(name) {
            return Err(crate::pgmsg::window_does_not_exist(name).finalize_implicit());
        }
        Ok(())
    };
    match inner {
        node::Node::FuncCall(fc) => {
            if let Some(over) = &fc.over {
                check_name(&over.name)?;
                check_name(&over.refname)?;
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

/// If `node` is a bare integer literal, return its value — GROUP BY / ORDER
/// BY treat those as 1-based projection ordinals.
fn ordinal_of(node: &protobuf::Node) -> Option<i64> {
    if let Some(node::Node::AConst(ac)) = node.node.as_ref()
        && !ac.isnull
        && let Some(pg_query::protobuf::a_const::Val::Ival(i)) = &ac.val
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
    n_targets: usize,
) -> Result<(), AnalyzeError> {
    let Ctx {
        scope,
        null_ctx,
        snapshot,
    } = ctx;
    if let Some(node::Node::GroupingSet(gs)) = group_node.node.as_ref() {
        for inner in &gs.content {
            walk_group_clause_node(
                inner,
                expr::Ctx::new(scope, null_ctx, snapshot),
                params,
                select_aliases,
                n_targets,
            )?;
        }
        return Ok(());
    }
    // Integer literals are 1-based projection ordinals (42P10 when out of
    // range); a valid one needs no further walking.
    if let Some(ord) = ordinal_of(group_node) {
        if ord < 1 || ord as usize > n_targets {
            return Err(crate::pgmsg::position_not_in_select_list(
                "GROUP BY",
                ord,
                crate::error::node_location(group_node)
                    .and_then(crate::error::SourceSpan::from_node_token),
            )
            .finalize_implicit());
        }
        return Ok(());
    }
    // PG transforms the expression first (bottom-up resolution errors win)
    // and raises the no-aggregates placement error afterwards.
    if let Err(e) = expr::infer_expr(
        group_node,
        expr::Ctx::new(scope, null_ctx, snapshot),
        params,
        TypeGoal::NONE,
    ) && !is_select_alias_reference(group_node, select_aliases, &e)
    {
        return Err(e);
    }
    crate::clause::check_no_aggregates_or_windows(group_node, snapshot, "GROUP BY")?;
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

/// Validate `FOR UPDATE OF a, b` (and the other lock strengths): PG
/// requires every named relation to be an entry of the current query
/// level's FROM clause — `relation "x" in FOR UPDATE clause not found in
/// FROM clause` (SQLSTATE 42P01) otherwise.
fn check_locking_clause(
    locking_clause: &[protobuf::Node],
    scope: &Scope,
) -> Result<(), AnalyzeError> {
    for node in locking_clause {
        let Some(node::Node::LockingClause(lc)) = node.node.as_ref() else {
            continue;
        };
        let clause = match lc.strength() {
            pg_query::protobuf::LockClauseStrength::LcsForkeyshare => "FOR KEY SHARE",
            pg_query::protobuf::LockClauseStrength::LcsForshare => "FOR SHARE",
            pg_query::protobuf::LockClauseStrength::LcsFornokeyupdate => "FOR NO KEY UPDATE",
            _ => "FOR UPDATE",
        };
        for rel in &lc.locked_rels {
            let Some(node::Node::RangeVar(rv)) = rel.node.as_ref() else {
                continue;
            };
            if scope.find_source(&rv.relname).is_none() {
                return Err(crate::error::RawError::new(
                    AnalyzeError::UndefinedTable(format!(
                        "relation \"{}\" in {clause} clause not found in FROM clause",
                        rv.relname
                    )),
                    crate::error::SourceSpan::from_node_qname(rv.location),
                    None,
                )
                .finalize_implicit());
            }
        }
    }
    Ok(())
}
