use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// RETURNING (PG 18: OLD / NEW rows)
// ──────────────────────────────────────────────────────────────────────────────

/// Which of RETURNING's OLD / NEW rows some returned row may lack.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ReturningRows {
    /// A returned row may have no old version: a plain INSERT, the insert
    /// arm of ON CONFLICT DO UPDATE, a MERGE with an INSERT action.
    pub old_may_be_null: bool,
    /// A returned row may have no new version: DELETE, a MERGE with a
    /// DELETE action.
    pub new_may_be_null: bool,
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
    let mut scope = ctx.scope.clone();
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
        let (named, kind, null_row) = match protobuf::ReturningOptionKind::try_from(opt.option) {
            Ok(protobuf::ReturningOptionKind::ReturningOptionOld) => {
                (&mut old_named, "OLD", rows.old_may_be_null)
            }
            Ok(protobuf::ReturningOptionKind::ReturningOptionNew) => {
                (&mut new_named, "NEW", rows.new_may_be_null)
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
            .push(target.table_only_copy(&opt.value, null_row));
    }
    for (named, name, null_row) in [
        (old_named, "old", rows.old_may_be_null),
        (new_named, "new", rows.new_may_be_null),
    ] {
        if !named && !namespace_has(&scope, name) {
            scope.sources.push(target.table_only_copy(name, null_row));
        }
    }

    // EXPR_KIND_RETURNING / EXPR_KIND_MERGE_RETURNING.
    for target in &clause.exprs {
        if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
            && let Some(val) = &rt.val
        {
            crate::clause::check_no_aggregates_or_windows(val, ctx.snapshot, "RETURNING")?;
            check_no_srf_in_clause(val, ctx.snapshot, "RETURNING")?;
        }
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
    Ok(columns)
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
