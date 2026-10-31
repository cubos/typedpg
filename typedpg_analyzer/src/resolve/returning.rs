use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// RETURNING (PG 18: OLD / NEW rows)
// ──────────────────────────────────────────────────────────────────────────────

/// Which of RETURNING's OLD / NEW rows some returned row may lack.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReturningRows {
    /// A returned row may have no old version: a plain INSERT, the insert
    /// arm of ON CONFLICT DO UPDATE, a MERGE with an INSERT action.
    pub old_may_be_null: bool,
    /// A returned row may have no new version: DELETE, a MERGE with a
    /// DELETE action.
    pub new_may_be_null: bool,
    /// The target's columns the statement's WHERE proves non-NULL in the
    /// old row (an UPDATE's or DELETE's): so are OLD's.
    pub old_proven: std::collections::HashSet<String>,
    /// Those an UPDATE's new row keeps: so are NEW's.
    pub new_proven: std::collections::HashSet<String>,
}

/// PG's `transformReturningClause` (parser/analyze.c): resolve a RETURNING
/// list against `scope` — the statement's namespace, which holds the target
/// relation under `target_alias` — plus the OLD / NEW rows.
///
/// Each `WITH (OLD AS o, NEW AS n)` option names its row (twice is 42601,
/// a name already in the namespace is 42712); a row the options leave
/// unnamed is added as `old` / `new` unless a relation of that name is
/// already visible, which then masks it. The rows are copies of the target
/// entry reachable only by name (`addNSItemForReturning` adds them
/// table-only), so unqualified names and the bare `*` still mean the
/// target. They exist only for the RETURNING list.
pub(crate) fn resolve_returning(
    clause: &Option<protobuf::ReturningClause>,
    target_alias: &str,
    rows: ReturningRows,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Vec<RawColumn>, AnalyzeError> {
    let Some(clause) = clause else {
        return Ok(Vec::new());
    };
    let scope = returning_scope(clause, target_alias, &rows, ctx.scope)?;

    // EXPR_KIND_RETURNING / EXPR_KIND_MERGE_RETURNING.
    for target in &clause.exprs {
        if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
            && let Some(val) = &rt.val
        {
            crate::clause::check_no_aggregates_or_windows(val, ctx.snapshot, "RETURNING")?;
            check_no_srf_in_clause(val, ctx.snapshot, "RETURNING")?;
        }
    }
    // (Only a bare `$N` changes anything here: any other parameter is typed
    // alike by the walk below.)
    let is_bare_param = |t: &protobuf::Node| {
        matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
            if rt.val.as_deref().and_then(|v| expr::bare_param(v, ctx.snapshot)).is_some())
    };
    if clause.exprs.iter().any(is_bare_param) {
        resolve_unknown_returning_params(
            clause,
            expr::Ctx::new(&scope, ctx.null_ctx, ctx.snapshot),
            params,
        )?;
    }

    let columns = crate::grouping::with_clause(Some("RETURNING"), || {
        resolve_target_list(
            &clause.exprs,
            expr::Ctx::new(&scope, ctx.null_ctx, ctx.snapshot),
            params,
        )
    })?;
    // A nonempty list that expanded to nothing (stars over a zero-column
    // table) would read as "no RETURNING".
    if columns.is_empty()
        && let Some(first) = clause.exprs.first()
    {
        let span =
            crate::error::node_location(first).and_then(crate::error::SourceSpan::from_node_token);
        return Err(crate::pgmsg::returning_without_columns(span).finalize_implicit());
    }
    // transformReturningClause's resolveTargetListUnknowns.
    super::select::resolve_unknown_values(&clause.exprs, &columns, ctx.snapshot)?;
    Ok(columns)
}

/// The parameter types RETURNING deduces, where PG transforms it before
/// the analyzer resolves it: ahead of an UPDATE's SET list
/// (transformUpdateStmt). Errors are left to [`resolve_returning`].
pub(crate) fn type_returning_params(
    clause: &Option<protobuf::ReturningClause>,
    target_alias: &str,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let Some(clause) = clause else {
        return Ok(());
    };
    let Ok(scope) = returning_scope(clause, target_alias, &ReturningRows::default(), ctx.scope)
    else {
        return Ok(());
    };
    resolve_unknown_returning_params(
        clause,
        expr::Ctx::new(&scope, ctx.null_ctx, ctx.snapshot),
        params,
    )
}

/// transformReturningClause transforms the list like a select list, then
/// resolves its still-untyped outputs as `text` right away
/// (resolveTargetListUnknowns): a bare `$N` untyped when transformed is
/// deduced `text` — or, typed meanwhile by a later entry, inconsistent.
fn resolve_unknown_returning_params(
    clause: &protobuf::ReturningClause,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let items: Vec<&protobuf::Node> = clause
        .exprs
        .iter()
        .filter_map(|t| match t.node.as_ref() {
            Some(node::Node::ResTarget(rt)) => rt.val.as_deref(),
            _ => None,
        })
        .collect();
    let untyped = expr::replay_transform(&items, ctx, params);
    for (item, untyped) in items.into_iter().zip(untyped) {
        if untyped && let Some(p) = expr::bare_param(item, ctx.snapshot) {
            expr::coerce_untyped_param(p, oid::TEXT, ctx.snapshot, params)?;
        }
    }
    Ok(())
}

/// The namespace RETURNING resolves in: `scope` plus the OLD / NEW rows.
fn returning_scope(
    clause: &protobuf::ReturningClause,
    target_alias: &str,
    rows: &ReturningRows,
    scope: &Scope,
) -> Result<Scope, AnalyzeError> {
    let mut scope = scope.clone();
    let Some(target) = scope
        .sources
        .iter()
        .find(|s| s.alias == target_alias)
        .cloned()
    else {
        return Err(AnalyzeError::Unsupported(format!(
            "internal: RETURNING target \"{target_alias}\" is not in scope"
        )));
    };

    let mut old_named = false;
    let mut new_named = false;
    for option in &clause.options {
        let Some(node::Node::ReturningOption(opt)) = option.node.as_ref() else {
            continue;
        };
        let span = crate::error::SourceSpan::from_node_qname(opt.location);
        let (named, kind, null_row, old) = match protobuf::ReturningOptionKind::try_from(opt.option)
        {
            Ok(protobuf::ReturningOptionKind::ReturningOptionOld) => {
                (&mut old_named, "OLD", rows.old_may_be_null, true)
            }
            Ok(protobuf::ReturningOptionKind::ReturningOptionNew) => {
                (&mut new_named, "NEW", rows.new_may_be_null, false)
            }
            _ => {
                return Err(AnalyzeError::Unsupported(format!(
                    "unrecognized returning option: {}",
                    opt.option
                )));
            }
        };
        if *named {
            return Err(crate::pgmsg::returning_option_repeated(kind, span).finalize_implicit());
        }
        *named = true;
        if namespace_has(&scope, &opt.value) {
            return Err(crate::pgmsg::duplicate_table_alias(&opt.value, span).finalize_implicit());
        }
        scope
            .sources
            .push(row_copy(&target, &opt.value, null_row, old, rows));
    }
    for (named, name, null_row, old) in [
        (old_named, "old", rows.old_may_be_null, true),
        (new_named, "new", rows.new_may_be_null, false),
    ] {
        if !named && !namespace_has(&scope, name) {
            scope
                .sources
                .push(row_copy(&target, name, null_row, old, rows));
        }
    }
    Ok(scope)
}

/// RETURNING's OLD / NEW row `alias`, a copy of the target entry knowing
/// what the WHERE proved of its row.
fn row_copy(
    target: &crate::scope::TableSource,
    alias: &str,
    null_row: bool,
    old: bool,
    rows: &ReturningRows,
) -> crate::scope::TableSource {
    let mut copy = target.table_only_copy(alias, null_row);
    let proven = if old {
        &rows.old_proven
    } else {
        &rows.new_proven
    };
    if !null_row {
        for c in &mut copy.columns {
            c.base_not_null |= proven.contains(&c.name);
        }
    }
    copy
}

/// PG's `refnameNamespaceItem(pstate, NULL, name, …)` without walking up:
/// whether a relation of this name is visible at the current level — its
/// own entries, plus a rule action's OLD / NEW (which PG adds to the
/// action's own level and we carry as outer references).
fn namespace_has(scope: &Scope, name: &str) -> bool {
    scope
        .sources
        .iter()
        .chain(scope.outer_sources.iter())
        .any(|s| s.alias == name)
}
