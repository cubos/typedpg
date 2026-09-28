use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// FROM clause processing
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn process_from_clause(
    from_clause: &[protobuf::Node],
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    for node in from_clause {
        process_from_item(node, scope, null_ctx, snapshot, cte_scopes, params)?;
    }
    Ok(())
}

pub(crate) fn process_from_item(
    node: &protobuf::Node,
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let inner = node
        .node
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("empty FROM item".into()))?;

    match inner {
        node::Node::RangeVar(rv) => {
            let alias = rv
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or(&rv.relname);

            // Check CTEs first.
            if rv.schemaname.is_empty()
                && let Some(cte_cols) = cte_scopes.get(&rv.relname)
            {
                check_cte_reference(cte_scopes, &rv.relname)?;
                let cols: Vec<ScopeColumn> = cte_cols
                    .iter()
                    .cloned()
                    .map(|mut c| {
                        c.table_alias = alias.to_owned();
                        c
                    })
                    .collect();
                scope.add_derived(alias, cols, crate::scope::SourceKind::Cte)?;
                apply_alias_column_names(scope, rv.alias.as_ref())?;
                return Ok(());
            }

            let schema = if rv.schemaname.is_empty() {
                None
            } else {
                Some(rv.schemaname.as_str())
            };
            scope.add_table(
                snapshot,
                schema,
                &rv.relname,
                alias,
                crate::error::SourceSpan::from_node_qname(rv.location),
            )?;
            apply_alias_column_names(scope, rv.alias.as_ref())?;
        }
        node::Node::JoinExpr(join) => {
            process_join_expr(join, scope, null_ctx, snapshot, cte_scopes, params)?;
        }
        node::Node::RangeSubselect(sub) => {
            // PG 16+ accepts an unaliased subquery and gives it no
            // referencable name.
            let alias_owned = match sub.alias.as_ref() {
                Some(a) => a.aliasname.clone(),
                None => crate::scope::hidden_alias("subquery"),
            };
            let alias = alias_owned.as_str();

            // `AS foo(a, b, c)` overrides the subquery's own output names.
            // Common in information_schema views that rename columns at the
            // FROM boundary instead of in the SELECT list.
            let col_aliases: Vec<String> = sub
                .alias
                .as_ref()
                .map(|a| {
                    a.colnames
                        .iter()
                        .filter_map(|n| match n.node.as_ref()? {
                            node::Node::String(s) => Some(s.sval.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            if let Some(subquery) = &sub.subquery
                && let Some(node::Node::SelectStmt(sel)) = subquery.node.as_ref()
            {
                // A LATERAL subquery inherits the visible FROM items to its
                // left — including the enclosing SELECT's scope we already
                // built — so column refs like `s.oid` inside
                // `JOIN LATERAL (… s.oid …)` resolve properly. Without
                // LATERAL the same aliases are *shadowed*: not resolvable,
                // but tracked so a stray reference produces PG's exact
                // `invalid reference to FROM-clause entry for table "x"`
                // diagnostic instead of a generic missing-column message.
                // Lateral visibility is transitive: a LATERAL subquery nested
                // inside another one still sees the outermost lateral refs,
                // so pass the enclosing scope's own lateral tier along too.
                let visible = scope.lateral_visible();
                let (lateral_sources, shadowed_sources): (Vec<_>, Vec<_>) = if sub.lateral {
                    (visible, Vec::new())
                } else {
                    (Vec::new(), visible)
                };
                let (mut cols, _) = analyze_select_with_ctes_and_outer(
                    sel,
                    snapshot,
                    params,
                    cte_scopes,
                    &lateral_sources,
                    &[],
                    &shadowed_sources,
                )?;
                resolve_unknown_outputs(sel, &mut cols, params);
                let mut scope_cols: Vec<ScopeColumn> = cols
                    .into_iter()
                    .map(|rc| ScopeColumn {
                        name: rc.name,
                        type_oid: rc.type_oid,
                        base_not_null: !rc.nullable,
                        table_alias: alias.to_owned(),
                        typmod: rc.typmod,
                        collation: rc.collation,
                        record_fields: rc.record_fields,
                    })
                    .collect();
                // PG rejects more aliases than columns (42P10).
                if col_aliases.len() > scope_cols.len() {
                    return Err(crate::pgmsg::too_many_column_aliases(
                        alias,
                        scope_cols.len(),
                        col_aliases.len(),
                    )
                    .finalize_implicit());
                }
                for (i, alias_name) in col_aliases.iter().enumerate() {
                    if let Some(c) = scope_cols.get_mut(i) {
                        c.name = alias_name.clone();
                    }
                }
                scope.add_derived(
                    alias,
                    scope_cols,
                    crate::scope::SourceKind::Subquery {
                        lock_blocker: subquery_lock_blocker(sel, snapshot),
                    },
                )?;
            }
        }
        node::Node::RangeFunction(rf) => {
            process_range_function(rf, scope, null_ctx, snapshot, params)?;
        }
        node::Node::RangeTableSample(ts) => {
            // `TABLESAMPLE` only changes how rows are picked at runtime —
            // it does not affect the relation's column shape or
            // nullability. Process the wrapped `relation`, then the
            // sampling method and its arguments.
            let relation = ts.relation.as_ref().ok_or_else(|| {
                AnalyzeError::Unsupported("RangeTableSample without relation".into())
            })?;
            process_from_item(relation, scope, null_ctx, snapshot, cte_scopes, params)?;
            process_tablesample(ts, scope, snapshot, params)?;
        }
        node::Node::JsonTable(jt) => {
            process_json_table(jt, scope, null_ctx, snapshot, params)?;
        }
        _ => {
            return Err(AnalyzeError::Unsupported(format!(
                "FROM item type: {:?}",
                std::mem::discriminant(inner)
            )));
        }
    }
    Ok(())
}

/// `FROM func(args)` / `ROWS FROM (f1(…), f2(…) AS (…))` — a function RTE.
///
/// Mirrors PG's `transformRangeFunction` (parse_clause.c) and
/// `addRangeTableEntryForFunction` (parse_relation.c):
/// - the SQL-standard multi-argument `unnest(a, b, …)` (unqualified, no
///   decoration, no column definition list) is rewritten into one
///   `pg_catalog.unnest()` call per argument;
/// - an outer column definition list (`f() AS x(a int)`) attaches to the
///   single function, and is rejected with several functions or with
///   `WITH ORDINALITY`;
/// - each function contributes its columns: OUT parameters, a named
///   composite's attributes, the column definition list for `record`, or
///   one scalar column (named after the alias when it is the only function);
/// - with several functions the shorter results are padded with NULL, so
///   every column but `ordinality` is nullable;
/// - `WITH ORDINALITY` appends `ordinality bigint NOT NULL`, and the alias'
///   column list renames positionally (42P10 when it is too long).
fn process_range_function(
    rf: &protobuf::RangeFunction,
    scope: &mut Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let mut funcs: Vec<RteFunction<'_>> = Vec::with_capacity(rf.functions.len());
    for item in &rf.functions {
        let Some(node::Node::List(pair)) = item.node.as_ref() else {
            return Err(AnalyzeError::Unsupported(
                "RangeFunction without function call".into(),
            ));
        };
        let call = match pair.items.first().and_then(|n| n.node.as_ref()) {
            Some(node::Node::FuncCall(fc)) => fc.as_ref(),
            _ => {
                return Err(AnalyzeError::Unsupported(
                    "RangeFunction item is not a FuncCall".into(),
                ));
            }
        };
        let coldeflist: &[protobuf::Node] = match pair.items.get(1).and_then(|n| n.node.as_ref()) {
            Some(node::Node::List(l)) => &l.items,
            _ => &[],
        };
        if coldeflist.is_empty() && is_sql_standard_unnest(call) {
            for arg in &call.args {
                let mut single = call.clone();
                single.funcname = ["pg_catalog", "unnest"]
                    .into_iter()
                    .map(|s| protobuf::Node {
                        node: Some(node::Node::String(protobuf::String { sval: s.into() })),
                    })
                    .collect();
                single.args = vec![arg.clone()];
                funcs.push(RteFunction {
                    call: std::borrow::Cow::Owned(single),
                    coldeflist: &[],
                });
            }
            continue;
        }
        funcs.push(RteFunction {
            call: std::borrow::Cow::Borrowed(call),
            coldeflist,
        });
    }

    if !rf.coldeflist.is_empty() {
        if funcs.len() != 1 {
            return Err(if rf.is_rowsfrom {
                crate::pgmsg::rows_from_multiple_coldeflist()
            } else {
                crate::pgmsg::unnest_multiple_coldeflist()
            }
            .finalize_implicit());
        }
        if rf.ordinality {
            return Err(crate::pgmsg::ordinality_with_coldeflist().finalize_implicit());
        }
        if !funcs[0].coldeflist.is_empty() {
            return Err(crate::pgmsg::multiple_coldeflists().finalize_implicit());
        }
        funcs[0].coldeflist = &rf.coldeflist;
    }

    // The RTE's name: the alias, else the first function's FigureColname.
    let alias_owned = match rf.alias.as_ref() {
        Some(a) => a.aliasname.clone(),
        None => funcs
            .first()
            .and_then(|f| expr::extract_string_fields(&f.call.funcname).pop())
            .unwrap_or_else(|| "_srf".into()),
    };
    let alias = alias_owned.as_str();

    let arg_scope = srf_arg_scope(scope);
    let arg_ctx = expr::Ctx::new(&arg_scope, null_ctx, snapshot);
    let nfuncs = funcs.len();
    let mut cols: Vec<ScopeColumn> = Vec::new();
    for f in &funcs {
        cols.extend(function_rte_columns(f, rf, alias, nfuncs, arg_ctx, params)?);
    }
    if nfuncs > 1 {
        // nodeFunctionscan.c pads every function that runs out of rows
        // first with NULLs.
        for c in &mut cols {
            c.base_not_null = false;
        }
    }

    // WITH ORDINALITY appends a trailing BIGINT NOT NULL row number. Do this
    // before the alias override so `AS t(val, ord)` can rename the ordinality
    // column too.
    if rf.ordinality {
        cols.push(ScopeColumn {
            name: "ordinality".into(),
            type_oid: oid::INT8,
            base_not_null: true,
            table_alias: alias.to_owned(),
            typmod: None,
            collation: None,
            record_fields: None,
        });
    }

    // `buildRelationAliases`: the alias' column list renames positionally
    // and may not be longer than the column list.
    let col_aliases = rf
        .alias
        .as_ref()
        .map(|a| expr::extract_string_fields(&a.colnames))
        .unwrap_or_default();
    if col_aliases.len() > cols.len() {
        return Err(
            crate::pgmsg::too_many_column_aliases(alias, cols.len(), col_aliases.len())
                .finalize_implicit(),
        );
    }
    for (c, alias_name) in cols.iter_mut().zip(col_aliases) {
        c.name = alias_name;
    }

    scope.add_derived(alias, cols, crate::scope::SourceKind::Function)?;
    Ok(())
}

/// One function of a function RTE after `transformRangeFunction`'s
/// preprocessing: the call and the column definition list attached to it.
struct RteFunction<'a> {
    call: std::borrow::Cow<'a, protobuf::FuncCall>,
    coldeflist: &'a [protobuf::Node],
}

/// PG rewrites `unnest(a, b, …)` into `ROWS FROM (unnest(a), unnest(b), …)`
/// only for the undecorated, unqualified SQL-standard spelling.
fn is_sql_standard_unnest(fc: &protobuf::FuncCall) -> bool {
    matches!(expr::extract_string_fields(&fc.funcname).as_slice(), [n] if n == "unnest")
        && fc.args.len() > 1
        && fc.agg_order.is_empty()
        && fc.agg_filter.is_none()
        && fc.over.is_none()
        && !fc.agg_star
        && !fc.agg_distinct
        && !fc.func_variadic
}

/// The contiguous run of `scope.sources` contributed by one side of a JOIN.
///
/// FROM processing appends sources in traversal order, so a JOIN's sides are
/// always adjacent runs; addressing them through captured spans (instead of
/// loose `left_start`/`left_end`/`right_end` indices) keeps the nullability
/// promotion and USING merging phrased as "the left side's sources".
#[derive(Clone, Copy)]
struct SourceSpan {
    start: usize,
    end: usize,
}

impl SourceSpan {
    /// Run `f` (which may append sources to the scope) and capture the span
    /// of sources it added.
    fn capture(
        scope: &mut Scope,
        f: impl FnOnce(&mut Scope) -> Result<(), AnalyzeError>,
    ) -> Result<SourceSpan, AnalyzeError> {
        let start = scope.sources.len();
        f(scope)?;
        Ok(SourceSpan {
            start,
            end: scope.sources.len(),
        })
    }

    /// Union with an adjacent later span (left side `.to(right side)`).
    fn to(self, other: SourceSpan) -> SourceSpan {
        SourceSpan {
            start: self.start,
            end: other.end,
        }
    }

    fn sources(self, scope: &Scope) -> &[crate::scope::TableSource] {
        &scope.sources[self.start..self.end]
    }
}

/// `a JOIN b ON …` / `USING (…)` / `NATURAL …`, following PG's
/// `transformFromClauseItem` (parse_clause.c): process both sides (the right
/// side's LATERAL items may reference the left side only for INNER / LEFT
/// joins), walk the ON clause, build the USING / NATURAL merged columns, and
/// apply outer-join nullability to the null-padded side(s).
fn process_join_expr(
    join: &protobuf::JoinExpr,
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    // Fail loudly on unknown join kinds rather than defaulting to INNER,
    // which would silently produce wrong nullability for outer joins the
    // parser couldn't classify.
    let join_type = JoinType::try_from(join.jointype)
        .map_err(|_| AnalyzeError::UnsupportedJoinType(join.jointype))?;

    let left = SourceSpan::capture(scope, |scope| match &join.larg {
        Some(larg) => process_from_item(larg, scope, null_ctx, snapshot, cte_scopes, params),
        None => Ok(()),
    })?;
    // PG exposes the left side to a LATERAL right side, but a reference to
    // it is an error unless the join is INNER or LEFT.
    let lateral_ok = matches!(join_type, JoinType::JoinInner | JoinType::JoinLeft);
    let blocked_before = scope.lateral_blocked_aliases.clone();
    if !lateral_ok {
        let left_aliases: Vec<String> = left
            .sources(scope)
            .iter()
            .map(|s| s.alias.clone())
            .collect();
        scope.lateral_blocked_aliases.extend(left_aliases);
    }
    let right = SourceSpan::capture(scope, |scope| match &join.rarg {
        Some(rarg) => process_from_item(rarg, scope, null_ctx, snapshot, cte_scopes, params),
        None => Ok(()),
    });
    scope.lateral_blocked_aliases = blocked_before;
    let right = right?;

    // Walk the ON clause *before* applying outer-join nullability:
    // PG evaluates `ON` on paired rows where right-side columns are
    // still NOT NULL (for LEFT JOIN), and only null-pads non-matches
    // afterwards. Without this walk, `$N` parameters used only in
    // `ON` are never registered with the collector and `into_sorted`
    // reports a spurious "parameter gap".
    if let Some(quals) = &join.quals {
        // Shares WHERE's machinery: resolution errors first, then
        // the no-aggregates placement rule, then PG's clause wording
        // (`argument of JOIN/ON must be type boolean, not type X`).
        crate::clause::coerce_clause_expr(
            quals,
            expr::Ctx::new(scope, null_ctx, snapshot),
            params,
            crate::clause::ClauseKind::JoinOn,
        )?;
        check_no_srf_in_clause(quals, snapshot, "JOIN conditions")?;
    }

    // `JOIN … USING (cols)` / `NATURAL JOIN` merge the join columns: the
    // output has ONE column per name (placed before both sides' remaining
    // columns in `*`), an unqualified reference resolves to it without
    // ambiguity, and the constituents stay reachable qualified (`a.id`)
    // and via `a.*`. Built before this join's own null-padding is applied:
    // the merged value's nullability comes from the constituents as they
    // are *inside* the join.
    let using_names: Vec<String> = if join.is_natural {
        // PG: every left-side output column name that also names a
        // right-side output column, in left-side order.
        let right_names: std::collections::HashSet<String> =
            output_column_names(scope, right).into_iter().collect();
        let mut names: Vec<String> = Vec::new();
        for n in output_column_names(scope, left) {
            if right_names.contains(&n) && !names.contains(&n) {
                names.push(n);
            }
        }
        names
    } else {
        expr::extract_string_fields(&join.using_clause)
    };
    let merged = if using_names.is_empty() {
        None
    } else {
        Some(merge_using_columns(
            scope,
            null_ctx,
            snapshot,
            &using_names,
            left,
            right,
            join_type,
        )?)
    };

    // Apply JOIN nullability.
    match join_type {
        JoinType::JoinLeft => {
            let right_aliases = nullability::collect_aliases(right.sources(scope));
            null_ctx.mark_all_nullable(&right_aliases);
        }
        JoinType::JoinRight => {
            let left_aliases = nullability::collect_aliases(left.sources(scope));
            null_ctx.mark_all_nullable(&left_aliases);
        }
        JoinType::JoinFull => {
            let all_aliases = nullability::collect_aliases(left.to(right).sources(scope));
            null_ctx.mark_all_nullable(&all_aliases);
        }
        JoinType::JoinInner => {} // No nullability change.
        other => return Err(AnalyzeError::UnsupportedJoinType(other as i32)),
    }

    if let Some(merged) = merged {
        // `USING (…) AS j` (PG 14) names the merged columns; otherwise they
        // live in an unreferencable synthetic source.
        let alias = match join.join_using_alias.as_ref() {
            Some(a) => a.aliasname.clone(),
            None => crate::scope::hidden_alias("join"),
        };
        let columns: Vec<ScopeColumn> = merged
            .into_iter()
            .map(|mut c| {
                c.table_alias = alias.clone();
                c
            })
            .collect();
        if scope.sources.iter().any(|s| s.alias == alias) {
            return Err(crate::pgmsg::duplicate_table_alias(&alias).finalize_implicit());
        }
        scope.sources.insert(
            left.start,
            crate::scope::TableSource {
                kind: crate::scope::SourceKind::Join,
                ..crate::scope::TableSource::derived(&alias, columns)
            },
        );
    }
    Ok(())
}

/// The output column names of one join side, as PG's `l_colnames` /
/// `r_colnames`: every source's columns minus those an inner USING join
/// merged away.
fn output_column_names(scope: &Scope, span: SourceSpan) -> Vec<String> {
    span.sources(scope)
        .iter()
        .flat_map(|s| s.visible_columns().map(|c| c.name.clone()))
        .collect()
}

/// Build the merged columns for `JOIN USING` / `NATURAL JOIN` and record
/// the constituents in their sources' `join_hidden` so unqualified
/// resolution and the bare `*` skip them. The caller splices the merged
/// columns in *before* both join sides — exactly where PG puts them in `*`
/// expansion (`SELECT * FROM a JOIN b USING (id)` is `id, <a-rest>,
/// <b-rest>`).
///
/// Merged-column semantics mirrored from PG's `transformFromClauseItem`:
/// - a name listed twice → `column name "x" appears more than once in USING
///   clause` (42701);
/// - a name missing on a side → `column "x" specified in USING clause does
///   not exist in left/right table` (42703), present twice → `common column
///   name "x" appears more than once in left/right table` (42702);
/// - the join qual `l = r` must resolve (`transformJoinUsingClause`), else
///   `operator does not exist: X = Y` (42883);
/// - type: the sides' common type (`buildMergedJoinVar`), else `JOIN/USING
///   types X and Y cannot be matched` (42804);
/// - nullability: the left value for LEFT, the right one for RIGHT,
///   `COALESCE(l, r)` for FULL (NOT NULL only when both sides are), and for
///   INNER either NOT NULL side suffices (the strict `=` discards NULLs).
///   Each side is taken with the nullability it has inside this join.
fn merge_using_columns(
    scope: &mut Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    using_names: &[String],
    left: SourceSpan,
    right: SourceSpan,
    join_type: JoinType,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    // `(source index, column)` of the unique visible column named `name`.
    let find_col = |scope: &Scope,
                    span: SourceSpan,
                    name: &str,
                    side: &str|
     -> Result<(usize, ScopeColumn), AnalyzeError> {
        let mut found: Option<(usize, ScopeColumn)> = None;
        for (i, s) in scope.sources[span.start..span.end].iter().enumerate() {
            for c in s.visible_columns().filter(|c| c.name == name) {
                if found.is_some() {
                    return Err(
                        crate::pgmsg::using_column_ambiguous(name, side).finalize_implicit()
                    );
                }
                found = Some((span.start + i, c.clone()));
            }
        }
        found.ok_or_else(|| crate::pgmsg::using_column_missing(name, side).finalize_implicit())
    };

    let mut merged: Vec<ScopeColumn> = Vec::with_capacity(using_names.len());
    let mut hide: Vec<(usize, String)> = Vec::new();
    for (i, name) in using_names.iter().enumerate() {
        if using_names[..i].contains(name) {
            return Err(crate::pgmsg::using_column_listed_twice(name).finalize_implicit());
        }
        let (l_idx, l) = find_col(scope, left, name, "left")?;
        let (r_idx, r) = find_col(scope, right, name, "right")?;

        let lt = crate::ddl::util::format_type_for_message(snapshot, l.type_oid);
        let rt = crate::ddl::util::format_type_for_message(snapshot, r.type_oid);
        if l.type_oid != oid::UNKNOWN
            && r.type_oid != oid::UNKNOWN
            && snapshot
                .find_operator("=", Some(l.type_oid), r.type_oid)
                .is_none()
        {
            return Err(
                crate::pgmsg::operator_does_not_exist(&lt, "=", &rt, None).finalize_implicit()
            );
        }
        let type_oid = if l.type_oid == r.type_oid {
            l.type_oid
        } else {
            crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot).ok_or_else(
                || crate::pgmsg::join_using_types_mismatch(&lt, &rt).finalize_implicit(),
            )?
        };
        let l_not_null = !null_ctx.is_nullable(&l.table_alias, &l.name, l.base_not_null);
        let r_not_null = !null_ctx.is_nullable(&r.table_alias, &r.name, r.base_not_null);
        let base_not_null = match join_type {
            JoinType::JoinLeft => l_not_null,
            JoinType::JoinRight => r_not_null,
            JoinType::JoinFull => l_not_null && r_not_null,
            _ => l_not_null || r_not_null,
        };
        merged.push(ScopeColumn {
            name: name.clone(),
            type_oid,
            base_not_null,
            typmod: if l.typmod == r.typmod { l.typmod } else { None },
            collation: if l.collation == r.collation {
                l.collation
            } else {
                None
            },
            // Set by the caller once the source's alias is known.
            table_alias: String::new(),
            record_fields: None,
        });
        hide.push((l_idx, name.clone()));
        hide.push((r_idx, name.clone()));
    }
    for (idx, name) in hide {
        scope.sources[idx].join_hidden.insert(name);
    }
    Ok(merged)
}

/// Apply a FROM item's column-alias list (`users AS t(a, b, c)`) to the
/// just-added source: rename positionally, and mirror PG's 42P10 rejection
/// when more aliases than columns are given (`table "t" has N columns
/// available but M columns specified`).
fn apply_alias_column_names(
    scope: &mut Scope,
    alias_node: Option<&pg_query::protobuf::Alias>,
) -> Result<(), AnalyzeError> {
    let Some(a) = alias_node else {
        return Ok(());
    };
    let colnames = expr::extract_string_fields(&a.colnames);
    if colnames.is_empty() {
        return Ok(());
    }
    let Some(src) = scope.sources.last_mut() else {
        return Ok(());
    };
    if colnames.len() > src.columns.len() {
        return Err(crate::pgmsg::too_many_column_aliases(
            &src.alias,
            src.columns.len(),
            colnames.len(),
        )
        .finalize_implicit());
    }
    for (i, name) in colnames.into_iter().enumerate() {
        if let Some(c) = src.columns.get_mut(i) {
            c.name = name;
        }
    }
    Ok(())
}

/// The scope a FROM function's arguments are resolved in: every FROM item
/// to its left (PG treats function RTEs as implicitly LATERAL), the lateral
/// refs this level received, and the correlated outer sources.
fn srf_arg_scope(scope: &Scope) -> Scope {
    let mut arg_scope = Scope::default();
    arg_scope
        .sources
        .extend(scope.sources.iter().cloned().map(|mut s| {
            s.lateral_blocked |= scope.lateral_blocked_aliases.contains(&s.alias);
            s
        }));
    arg_scope
        .lateral_sources
        .extend(scope.lateral_sources.clone());
    arg_scope.outer_sources.extend(scope.outer_sources.clone());
    arg_scope.ctes = scope.ctes.clone();
    arg_scope
}

/// Infer the SRF's argument types (so overload resolution can pick the right
/// function) and their nullability.
///
/// PG always treats a function-call FROM item as LATERAL: the `LATERAL`
/// keyword on a `RangeFunction` is a noise word, because the args can already
/// refer to earlier FROM items implicitly. So we copy the visible sources
/// unconditionally, not just when `rf.lateral` is set. We also propagate
/// `outer_sources` so SRF args inside a correlated sublink can reach aliases
/// bound by the enclosing query (e.g. `pg_stats_ext` does
/// `(SELECT … FROM unnest(s.stxkeys) …)` where `s` is from the outer FROM).
fn infer_srf_arg_types(
    func_call: &protobuf::FuncCall,
    arg_ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(Vec<PgTypeOid>, Vec<bool>), AnalyzeError> {
    let mut arg_types = Vec::with_capacity(func_call.args.len());
    let mut arg_nullable = Vec::with_capacity(func_call.args.len());
    for arg in &func_call.args {
        // Any error transforming an argument aborts the query, as in PG's
        // `transformRangeFunction`: an unknown column, a FROM item that isn't
        // visible yet (`FROM f(t.c), t`), the left side of a RIGHT / FULL join.
        let e = expr::infer_expr(arg, arg_ctx, params, crate::expr::TypeGoal::NONE)?;
        let (t, n) = (e.type_oid, e.nullable);
        arg_types.push(t);
        arg_nullable.push(n);
    }
    Ok((arg_types, arg_nullable))
}

/// Nullability of the elements a strict `pg_catalog` set-returning function
/// emits, or `None` when `resolved` is not one (the caller keeps its
/// scalar-function rules).
///
/// A strict SRF is never *called* with a NULL argument — PG yields zero
/// rows instead (`ExecMakeTableFunctionResult` / `ExecMakeFunctionResultSet`
/// short-circuit on `fn_strict`), so the arguments' nullability says nothing
/// about the output. What matters is whether the function itself emits SQL
/// NULLs: `unnest(anyarray)` returns the array's elements, which may be NULL
/// even when the array column is NOT NULL, and `json[b]_array_elements_text`
/// maps a JSON `null` to SQL NULL. Every other strict catalog SRF
/// (`generate_series`, `generate_subscripts`, `jsonb_array_elements`,
/// `regexp_split_to_table`, `unnest(anymultirange)`, …) never does.
///
/// `unnest(ARRAY[e1, e2, …])` over a literal constructor whose elements are
/// all NOT NULL is still provably NOT NULL; the elements are re-inferred
/// against a throwaway parameter collector so the peek has no side effects.
pub(crate) fn srf_elements_nullable(
    resolved: &functions::ResolvedFunction,
    name: &str,
    args: &[protobuf::Node],
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Option<bool> {
    if !(resolved.is_set_returning && resolved.is_strict && resolved.schema == "pg_catalog") {
        return None;
    }
    let array_unnest = name == "unnest"
        && ctx
            .snapshot
            .get_type(resolved.arg_types.first().copied().unwrap_or(oid::UNKNOWN))
            .is_some_and(|t| t.typname == "anyarray");
    if array_unnest {
        let elements_not_null = match args
            .first()
            .map(functions::call_arg_value)
            .and_then(|a| a.node.as_ref())
        {
            Some(node::Node::AArrayExpr(arr)) => arr.elements.iter().all(|e| {
                let mut scratch = params.clone();
                expr::infer_expr(e, ctx, &mut scratch, TypeGoal::NONE).is_ok_and(|t| !t.nullable)
            }),
            _ => false,
        };
        return Some(!elements_not_null);
    }
    Some(matches!(
        name,
        "json_array_elements_text" | "jsonb_array_elements_text"
    ))
}

/// Columns one function contributes to a function RTE, following
/// `addRangeTableEntryForFunction`'s `get_expr_result_type` classes: OUT
/// parameters and named composites expose their own row (a column
/// definition list is redundant there), `record` requires one, and a
/// scalar result is a single column (a column definition list is not
/// allowed).
fn function_rte_columns(
    f: &RteFunction<'_>,
    rf: &protobuf::RangeFunction,
    alias: &str,
    nfuncs: usize,
    arg_ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let snapshot = arg_ctx.snapshot;
    let func_call: &protobuf::FuncCall = &f.call;
    let func_name_parts = expr::extract_string_fields(&func_call.funcname);
    let (schema, name) = match func_name_parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedFunction(format!(
                "invalid function name in FROM: {func_name_parts:?}"
            )));
        }
    };
    let (arg_types, arg_nullable) = infer_srf_arg_types(func_call, arg_ctx, params)?;
    // nodeFunctionscan.c evaluates only the top-level call as a set.
    if func_call
        .args
        .iter()
        .any(|a| count_srf_calls(std::slice::from_ref(a), snapshot) > 0)
    {
        return Err(crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(
                "set-returning functions must appear at top level of FROM".into(),
            ),
            None,
            None,
        )
        .finalize_implicit());
    }
    let any_arg_nullable = arg_nullable.iter().any(|&n| n);

    let resolved = functions::resolve_function(
        snapshot,
        schema,
        name,
        &arg_types,
        &functions::CallNotation::of(func_call)?,
        false,
        crate::error::SourceSpan::from_node_qname(func_call.location),
    )?;
    let has_coldeflist = !f.coldeflist.is_empty();

    // A strict catalog SRF with a single OUT column (`jsonb_array_elements`
    // → `value`) emits exactly its elements; wider OUT rows keep the
    // conservative per-column default.
    let single_out_not_null = (resolved.out_args.len() == 1)
        .then(|| srf_elements_nullable(&resolved, name, &func_call.args, arg_ctx, params))
        .flatten()
        .map(|nullable| !nullable);

    if !resolved.out_args.is_empty() {
        if has_coldeflist {
            return Err(crate::pgmsg::coldeflist_redundant_out_params().finalize_implicit());
        }
        return Ok(resolved
            .out_args
            .iter()
            .map(|f| ScopeColumn {
                name: f.name.clone(),
                type_oid: f.type_oid,
                base_not_null: single_out_not_null.unwrap_or(f.not_null),
                typmod: None,
                collation: None,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect());
    }
    if let Some(typrelid) = snapshot.get_type(resolved.return_type_oid).and_then(|t| {
        (t.typtype == TypType::Composite)
            .then_some(t.typrelid)
            .flatten()
    }) {
        if has_coldeflist {
            return Err(crate::pgmsg::coldeflist_redundant_composite().finalize_implicit());
        }
        return Ok(snapshot
            .attributes_of(typrelid)
            .iter()
            .map(|f| ScopeColumn {
                name: f.attname.clone(),
                type_oid: f.atttypid,
                // A row-type *value* does not enforce its relation's NOT
                // NULL constraints — `jsonb_populate_record(NULL::t, '{}')`
                // and a `RETURNS SETOF t` function can both yield NULL
                // fields, and a NULL row reads as all-NULL fields. PG's
                // function RTE (`addRangeTableEntryForFunction`) takes only
                // the tuple descriptor's names and types.
                base_not_null: false,
                typmod: snapshot.effective_typmod(f.atttypid, f.atttypmod),
                collation: f.attcollation,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect());
    }
    if resolved.return_type_oid == oid::RECORD {
        if !has_coldeflist {
            return Err(crate::pgmsg::coldeflist_required().finalize_implicit());
        }
        return coldeflist_columns(f.coldeflist, alias, snapshot);
    }
    if has_coldeflist {
        return Err(crate::pgmsg::coldeflist_only_for_record().finalize_implicit());
    }

    // A strict catalog SRF's elements are NOT NULL unless the function
    // emits NULLs itself (`unnest` of an array with NULL elements); other
    // catalog functions follow their derived builtin nullability.
    let not_null = match srf_elements_nullable(&resolved, name, &func_call.args, arg_ctx, params) {
        Some(nullable) => !nullable,
        None => {
            resolved.schema == "pg_catalog"
                && !functions::builtin_result_nullable(
                    &resolved,
                    &[any_arg_nullable],
                    func_call.func_variadic,
                )
        }
    };
    // `chooseScalarFunctionAlias`: a lone scalar function's column is named
    // after the RTE alias (`FROM generate_series(1, 3) AS g` exposes `g`);
    // with several functions each column keeps its function's name.
    let col_name = if nfuncs == 1 { alias } else { name };
    Ok(vec![ScopeColumn {
        name: col_name.to_owned(),
        type_oid: resolved.return_type_oid,
        base_not_null: not_null,
        table_alias: alias.to_owned(),
        typmod: None,
        collation: None,
        record_fields: None,
    }])
}

/// The columns a column definition list (`AS x(a int, b varchar(3))`)
/// declares for a `record`-returning function: PG resolves each type with
/// its typmod (`typenameTypeIdAndMod`), then rejects duplicate names
/// (`CheckAttributeNamesTypes`, 42701). The values are unconstrained, so
/// every column is nullable.
fn coldeflist_columns(
    coldeflist: &[protobuf::Node],
    alias: &str,
    snapshot: &PgCatalog,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let mut cols: Vec<ScopeColumn> = Vec::with_capacity(coldeflist.len());
    for n in coldeflist {
        let Some(node::Node::ColumnDef(cd)) = n.node.as_ref() else {
            continue;
        };
        let tn = cd
            .type_name
            .as_ref()
            .ok_or_else(|| AnalyzeError::Unsupported("column definition without a type".into()))?;
        let type_oid = crate::ddl::util::lookup_type_name(tn, snapshot).map_err(|e| match e {
            crate::ddl::DdlError::TypeNotFound(msg) => AnalyzeError::UndefinedType(msg),
            other => AnalyzeError::Invalid(other.to_string()),
        })?;
        let typmod = crate::typmod::encode(snapshot, type_oid, &tn.typmods)
            .map_err(|e| AnalyzeError::Invalid(e.to_string()))?;
        cols.push(ScopeColumn {
            name: cd.colname.clone(),
            type_oid,
            base_not_null: false,
            typmod: snapshot.effective_typmod(type_oid, typmod),
            collation: None,
            table_alias: alias.to_owned(),
            record_fields: None,
        });
    }
    for (i, c) in cols.iter().enumerate() {
        if cols[..i].iter().any(|p| p.name == c.name) {
            return Err(crate::pgmsg::duplicate_column_name(&c.name).finalize_implicit());
        }
    }
    Ok(cols)
}

/// `TABLESAMPLE method (args) [REPEATABLE (seed)]` — PG's
/// `transformRangeTableSample` (parse_clause.c): the method names a
/// `tsm_handler` function (42704 otherwise), the argument count must match
/// the handler's parameter list (2202H), each argument is coerced to its
/// parameter type and REPEATABLE's seed to double precision (42804 with
/// PG's `argument of TABLESAMPLE must be type …` wording). The arguments
/// can't reference the sampled relation or other FROM items.
fn process_tablesample(
    ts: &protobuf::RangeTableSample,
    scope: &Scope,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let method_parts = expr::extract_string_fields(&ts.method);
    let method = method_parts.join(".");
    let (schema, name) = match method_parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => (None, method.as_str()),
    };
    let is_handler = snapshot.find_functions(schema, name).iter().any(|p| {
        snapshot
            .get_type(p.prorettype)
            .is_some_and(|t| t.typname == "tsm_handler")
    });
    if !is_handler {
        return Err(crate::error::RawError::new(
            AnalyzeError::UndefinedObject(format!("tablesample method {method} does not exist")),
            None,
            None,
        )
        .finalize_implicit());
    }
    // The handlers' `TsmRoutine.parameterTypes`: the core methods and the
    // contrib `tsm_system_*` ones.
    let param_types: &[PgTypeOid] = match name {
        "system_rows" => &[oid::INT8],
        "system_time" => &[oid::FLOAT8],
        _ => &[oid::FLOAT4],
    };
    if ts.args.len() != param_types.len() {
        return Err(AnalyzeError::Invalid(format!(
            "tablesample method {method} requires {} argument{}, not {}",
            param_types.len(),
            if param_types.len() == 1 { "" } else { "s" },
            ts.args.len()
        )));
    }
    let mut arg_scope = Scope {
        ctes: scope.ctes.clone(),
        ..Scope::default()
    };
    arg_scope.shadowed_sources = scope.sources.clone();
    let null_ctx = NullabilityContext::default();
    let ctx = expr::Ctx::new(&arg_scope, &null_ctx, snapshot);
    let coerce = |arg: &protobuf::Node,
                  target: PgTypeOid,
                  clause: &str,
                  params: &mut ParamCollector|
     -> Result<(), AnalyzeError> {
        match expr::infer_expr(arg, ctx, params, TypeGoal::assignment(target)) {
            Err(AnalyzeError::TypeMismatch { .. }) => {
                let mut scratch = params.clone();
                let actual = expr::infer_expr(arg, ctx, &mut scratch, TypeGoal::NONE)
                    .map(|e| e.type_oid)
                    .unwrap_or(oid::UNKNOWN);
                let want = crate::ddl::util::format_type_for_message(snapshot, target);
                let got = crate::ddl::util::format_type_for_message(snapshot, actual);
                Err(crate::error::RawError::new(
                    AnalyzeError::DatatypeMismatch(format!(
                        "argument of {clause} must be type {want}, not type {got}"
                    )),
                    None,
                    None,
                )
                .finalize_implicit())
            }
            other => other.map(|_| ()),
        }
    };
    for (arg, &target) in ts.args.iter().zip(param_types) {
        coerce(arg, target, "TABLESAMPLE", params)?;
    }
    if let Some(seed) = ts.repeatable.as_deref() {
        coerce(seed, oid::FLOAT8, "REPEATABLE", params)?;
    }
    Ok(())
}

/// `JSON_TABLE(ctx, path [AS name] [PASSING …] COLUMNS (…)) [AS alias(…)]`
/// (PG 17), following `transformJsonTable` (parse_jsontable.c):
///
/// - the context item sees the FROM items to its left (JSON_TABLE is always
///   LATERAL); it must be json / jsonb or a string type read as JSON (an
///   untyped parameter becomes text), otherwise `cannot cast type X to
///   jsonb`. PASSING values are plain expressions;
/// - columns: `FOR ORDINALITY` is integer; regular, `FORMAT JSON` and
///   `EXISTS` columns take their declared type (with typmod); `NESTED PATH`
///   contributes its own columns. Column and path names must be unique
///   (42712 `duplicate JSON_TABLE column or path name`). OMIT QUOTES with a
///   wrapper is 42601, and a DEFAULT behavior must be a constant expression
///   coercible to the column (42804 / the input function's error);
/// - every column is nullable (no match, ON EMPTY / ON ERROR, sibling
///   nested paths) except the top-level ordinality;
/// - the RTE is named by the alias (default `json_table`), whose column
///   list renames positionally.
fn process_json_table(
    jt: &protobuf::JsonTable,
    scope: &mut Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let arg_scope = srf_arg_scope(scope);
    let ctx = expr::Ctx::new(&arg_scope, null_ctx, snapshot);

    if let Some(item) = jt
        .context_item
        .as_deref()
        .and_then(|v| v.raw_expr.as_deref())
    {
        let t = expr::infer_expr(item, ctx, params, TypeGoal::NONE)?;
        if t.type_oid == oid::UNKNOWN {
            if let Some(node::Node::ParamRef(p)) = item.node.as_ref()
                && params.get(p.number) == oid::UNKNOWN
            {
                params.record(p.number, oid::TEXT);
            }
        } else {
            let base = snapshot.unwrap_domain(t.type_oid);
            let ok = snapshot.get_type(base).is_some_and(|te| {
                te.typcategory == TypCategory::String
                    || te.typname == "json"
                    || te.typname == "jsonb"
            });
            if !ok {
                let from = crate::ddl::util::format_type_for_message(snapshot, t.type_oid);
                return Err(AnalyzeError::Invalid(format!(
                    "cannot cast type {from} to jsonb"
                )));
            }
        }
    }
    for arg in &jt.passing {
        if let Some(node::Node::JsonArgument(ja)) = arg.node.as_ref()
            && let Some(val) = ja.val.as_deref().and_then(|v| v.raw_expr.as_deref())
        {
            let t = expr::infer_expr(val, ctx, params, TypeGoal::NONE)?;
            if t.type_oid == oid::UNKNOWN
                && let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && params.get(p.number) == oid::UNKNOWN
            {
                params.record(p.number, oid::TEXT);
            }
        }
    }

    // Column and path names share one namespace.
    let mut names: Vec<String> = Vec::new();
    if let Some(ps) = jt.pathspec.as_deref()
        && !ps.name.is_empty()
    {
        names.push(ps.name.clone());
    }
    let alias_owned = jt
        .alias
        .as_ref()
        .map(|a| a.aliasname.clone())
        .unwrap_or_else(|| "json_table".to_owned());
    let alias = alias_owned.as_str();
    let mut cols: Vec<ScopeColumn> = Vec::new();
    json_table_columns(&jt.columns, true, alias, &mut names, &mut cols, ctx, params)?;

    let col_aliases = jt
        .alias
        .as_ref()
        .map(|a| expr::extract_string_fields(&a.colnames))
        .unwrap_or_default();
    if col_aliases.len() > cols.len() {
        return Err(
            crate::pgmsg::too_many_column_aliases(alias, cols.len(), col_aliases.len())
                .finalize_implicit(),
        );
    }
    for (c, n) in cols.iter_mut().zip(col_aliases) {
        c.name = n;
    }
    scope.add_derived(alias, cols, crate::scope::SourceKind::Other)
}

/// The columns of one JSON_TABLE `COLUMNS (…)` list, recursing into
/// `NESTED PATH` lists.
fn json_table_columns(
    columns: &[protobuf::Node],
    top_level: bool,
    alias: &str,
    names: &mut Vec<String>,
    out: &mut Vec<ScopeColumn>,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    use protobuf::JsonTableColumnType as Kind;
    let snapshot = ctx.snapshot;
    let claim = |name: &str, names: &mut Vec<String>| -> Result<(), AnalyzeError> {
        if names.iter().any(|n| n == name) {
            return Err(crate::error::RawError::new(
                AnalyzeError::DuplicateAlias(format!(
                    "duplicate JSON_TABLE column or path name: {name}"
                )),
                None,
                None,
            )
            .finalize_implicit());
        }
        names.push(name.to_owned());
        Ok(())
    };
    for n in columns {
        let Some(node::Node::JsonTableColumn(col)) = n.node.as_ref() else {
            continue;
        };
        let kind = Kind::try_from(col.coltype).unwrap_or(Kind::Undefined);
        if kind == Kind::JtcNested {
            if let Some(ps) = col.pathspec.as_deref()
                && !ps.name.is_empty()
            {
                claim(&ps.name, names)?;
            }
            json_table_columns(&col.columns, false, alias, names, out, ctx, params)?;
            continue;
        }
        claim(&col.name, names)?;
        let (type_oid, typmod) = if kind == Kind::JtcForOrdinality {
            (oid::INT4, None)
        } else {
            let tn = col.type_name.as_ref().ok_or_else(|| {
                AnalyzeError::Unsupported("JSON_TABLE column without a type".into())
            })?;
            let t = crate::ddl::util::lookup_type_name(tn, snapshot).map_err(|e| match e {
                crate::ddl::DdlError::TypeNotFound(msg) => AnalyzeError::UndefinedType(msg),
                other => AnalyzeError::Invalid(other.to_string()),
            })?;
            let m = crate::typmod::encode(snapshot, t, &tn.typmods)
                .map_err(|e| AnalyzeError::Invalid(e.to_string()))?;
            (t, m)
        };
        let with_wrapper = matches!(
            protobuf::JsonWrapper::try_from(col.wrapper),
            Ok(protobuf::JsonWrapper::JswConditional | protobuf::JsonWrapper::JswUnconditional)
        );
        if with_wrapper
            && protobuf::JsonQuotes::try_from(col.quotes) == Ok(protobuf::JsonQuotes::JsQuotesOmit)
        {
            return Err(crate::error::RawError::new(
                AnalyzeError::SyntaxError(
                    "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used"
                        .into(),
                ),
                None,
                None,
            )
            .finalize_implicit());
        }
        for behavior in [col.on_empty.as_deref(), col.on_error.as_deref()]
            .into_iter()
            .flatten()
        {
            let Some(default) = behavior.expr.as_deref() else {
                continue;
            };
            // transformJsonBehavior: the DEFAULT must not reference
            // columns or parameters.
            let mut volatile = false;
            visit_same_level(default, &mut |e| {
                if matches!(
                    e.node.as_ref(),
                    Some(
                        node::Node::ColumnRef(_) | node::Node::ParamRef(_) | node::Node::SubLink(_)
                    )
                ) {
                    volatile = true;
                }
            });
            if volatile {
                return Err(crate::error::RawError::new(
                    AnalyzeError::DatatypeMismatch(
                        "can only specify a constant, non-aggregate function, or operator \
                         expression for DEFAULT"
                            .into(),
                    ),
                    None,
                    None,
                )
                .finalize_implicit());
            }
            expr::coerce_unknown_to(default, ctx, params, type_oid)?;
        }
        out.push(ScopeColumn {
            name: col.name.clone(),
            type_oid,
            base_not_null: top_level && kind == Kind::JtcForOrdinality,
            typmod: snapshot.effective_typmod(type_oid, typmod),
            collation: None,
            table_alias: alias.to_owned(),
            record_fields: None,
        });
    }
    Ok(())
}
