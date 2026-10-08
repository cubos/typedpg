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
        with_rows_kept(true, || {
            process_from_item(node, scope, null_ctx, snapshot, cte_scopes, params)
        })?;
    }
    Ok(())
}

thread_local! {
    /// The FROM item being processed is never null-extended in this
    /// level's rows: it is a FROM-list item, or a side of joins that
    /// preserve it all the way up.
    static ROWS_KEPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn rows_kept() -> bool {
    ROWS_KEPT.with(|k| k.get())
}

fn with_rows_kept<R>(kept: bool, f: impl FnOnce() -> R) -> R {
    let before = ROWS_KEPT.with(|k| k.replace(kept));
    let out = f();
    ROWS_KEPT.with(|k| k.set(before));
    out
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
            check_rangevar_catalog(rv)?;
            let alias = rv
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or(&rv.relname);

            // Check CTEs first.
            if rv.schemaname.is_empty()
                && let Some((cte_cols, search_cycle, expand)) =
                    cte_reference_columns(cte_scopes, &rv.relname)
            {
                check_cte_reference(cte_scopes, &rv.relname)?;
                let realias = |cols: Vec<ScopeColumn>| -> Vec<ScopeColumn> {
                    cols.into_iter()
                        .map(|mut c| {
                            c.table_alias = alias.to_owned();
                            c
                        })
                        .collect()
                };
                // Each reference scans the CTE's rows anew.
                let mut cte_cols = realias(cte_cols);
                fresh_scans(&mut cte_cols);
                scope.add_derived(alias, cte_cols, crate::scope::SourceKind::Cte)?;
                // An alias list renames only the CTE's own columns.
                apply_alias_column_names(scope, rv.alias.as_ref())?;
                // addRangeTableEntryForCTE appends the SEARCH / CYCLE
                // columns; below the WITH's own level they are left out of
                // `*` (but stay referencable by name).
                if let Some(src) = scope.sources.last_mut() {
                    if expand {
                        src.columns.extend(realias(search_cycle));
                    } else {
                        src.system_columns.extend(realias(search_cycle));
                    }
                    src.min_one_row = cte_min_one_row(cte_scopes, &rv.relname);
                    null_ctx.register_origins(&src.alias, &src.columns, false);
                }
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
            if let Some(class) = snapshot.resolve_table(schema, &rv.relname)
                && let Some(src) = scope.sources.last_mut()
            {
                src.lock_error = view_lock_error(class, snapshot);
                src.partial_scan =
                    !rv.inh && class.relkind == crate::pg_catalog::RelKind::Partitioned;
                // A scan of an inheritance parent returns its children's
                // rows too: a column is NOT NULL only if it is in each of
                // them (a parent's `NOT NULL NO INHERIT` isn't).
                let descendants = if rv.inh {
                    inheritance_descendants(snapshot, class.oid)
                } else {
                    Vec::new()
                };
                src.inherits_rows = !descendants.is_empty()
                    && class.relkind != crate::pg_catalog::RelKind::Partitioned;
                for c in src.columns.iter_mut().filter(|c| c.base_not_null) {
                    c.base_not_null = descendants.iter().all(|&d| {
                        snapshot
                            .attributes_of(d)
                            .iter()
                            .find(|a| a.attname == c.name)
                            .is_some_and(|a| snapshot.attr_never_null(a))
                    });
                }
                // The row a statement writes through the view whose body
                // this is: not a stored row (see `with_written_relation`).
                let written = super::take_written_relation(class.oid);
                if let Some(events) = &written
                    && class.relkind == crate::pg_catalog::RelKind::View
                {
                    let attrs = super::written_row_attrs(
                        snapshot,
                        class.oid,
                        snapshot.attributes_of(class.oid),
                        events,
                    );
                    for c in &mut src.columns {
                        c.base_not_null &= attrs
                            .iter()
                            .find(|a| a.attname == c.name)
                            .is_some_and(|a| snapshot.attr_never_null(a));
                    }
                }
                // Where each column's values come from: this scan of the
                // table, or what a view reads — nowhere a foreign key can
                // be trusted, for the written row.
                if written.is_some() {
                    // No origin.
                } else if class.relkind == crate::pg_catalog::RelKind::View {
                    // A view's columns are NOT NULL as its query was
                    // analyzed when it was defined, maybe relying on a
                    // foreign key. Under row locking, a concurrently
                    // updated row it reads is re-fetched and its query
                    // re-evaluated for it against the statement's snapshot
                    // (EvalPlanQual), which need not hold the row a new
                    // key references: only what the query proves under
                    // row locking holds.
                    if crate::nonnull::row_locking() {
                        let locked = crate::ddl::views::reanalyze_view(snapshot, class.oid);
                        for (i, c) in src.columns.iter_mut().enumerate() {
                            c.base_not_null &= locked
                                .as_ref()
                                .and_then(|cols| cols.get(i))
                                .is_some_and(|rc| !rc.nullable);
                        }
                    }
                    if let Some(cols) = view_columns(snapshot, class) {
                        for (c, rc) in src.columns.iter_mut().zip(cols) {
                            // What the query makes of it now (calls read
                            // as their bodies, see `expr::inline`) holds
                            // as well as what was found at CREATE VIEW.
                            c.base_not_null |= !rc.nullable;
                            c.origin = rc.origin;
                            // Both hold: the column's type (a domain) and
                            // what the view's query makes of it.
                            c.elem_nullable = match (rc.elem_nullable, c.elem_nullable) {
                                (Some(false), _) | (_, Some(false)) => Some(false),
                                (a, b) => a.or(b),
                            };
                            c.refine = c.refine.and(&rc.refine);
                        }
                    }
                } else {
                    let scan = crate::scope::fresh_scan_id();
                    let (with_children, all_rows) = (src.inherits_rows, !src.partial_scan);
                    for c in src.columns.iter_mut() {
                        c.origin = Some(crate::scope::Origin {
                            scan,
                            relid: class.oid,
                            column: c.name.clone(),
                            base_not_null: c.base_not_null,
                            with_children,
                            all_rows,
                        });
                    }
                }

                // Every row of a table satisfies its CHECK constraints
                // (named by the table's own column names: not with an
                // alias list renaming them).
                let renamed = rv.alias.as_ref().is_some_and(|a| !a.colnames.is_empty());
                if !renamed
                    && matches!(
                        class.relkind,
                        crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
                    )
                    && let Some(checks) =
                        crate::nonnull::checks::RelationChecks::of(snapshot, class.oid)
                {
                    let base: HashMap<String, bool> = src
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.base_not_null))
                        .collect();
                    let alias = src.alias.clone();
                    null_ctx.register_checks(&alias, checks, base);
                } else if !renamed
                    && let Some(rows) = super::WriteTarget::view_rows(snapshot, class.oid)
                    && let Some(checks) =
                        crate::nonnull::checks::RelationChecks::of(snapshot, rows.base)
                {
                    // A view's rows are its base table's, whose CHECK
                    // constraints hold over the plain columns it exposes.
                    let checks = checks.renamed(|b| rows.target_column(b).map(str::to_owned));
                    let base: HashMap<String, bool> = src
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.base_not_null))
                        .collect();
                    let alias = src.alias.clone();
                    null_ctx.register_checks(&alias, checks, base);
                }
            }
            apply_alias_column_names(scope, rv.alias.as_ref())?;
            if let Some(src) = scope.sources.last() {
                // A table entry is its scan's home; a view's columns come
                // from scans inside it.
                let home = src
                    .relid
                    .and_then(|r| snapshot.pg_class.get(&r))
                    .is_some_and(|c| c.relkind != crate::pg_catalog::RelKind::View);
                null_ctx.register_origins(&src.alias, &src.columns, home);
            }
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
                // The lateral tier this level received belongs to the
                // enclosing level: a non-LATERAL subquery still reaches it
                // as an outer reference, only this level's own items are
                // off limits.
                let mut enclosing = scope.enclosing_sources();
                let (lateral_sources, mut shadowed_sources): (Vec<_>, Vec<_>) = if sub.lateral {
                    // This level's own entries carry its nullability down.
                    let mut visible = scope.lateral_visible();
                    let own = scope.sources.len();
                    let baked = null_ctx.bake_sources(&visible[..own]);
                    visible.splice(..own, baked);
                    (visible, Vec::new())
                } else {
                    enclosing.splice(0..0, scope.lateral_sources.iter().cloned());
                    (Vec::new(), scope.sources.clone())
                };
                // Entries this level already can't reference stay
                // unreferencable below it: PG's errorMissingRTE finds them
                // in any enclosing range table.
                shadowed_sources.extend(scope.shadowed_sources.iter().cloned());
                // Every FROM subquery still sees the enclosing query levels
                // as outer references.
                // A LATERAL subquery that is never null-extended: what its
                // WHERE proves of the entries to its left holds for every
                // row of this level (see `nonnull::capture_correlation`).
                let capture = sub.lateral && rows_kept();
                let (analyzed, correlated) = crate::nonnull::capture_correlation(sel, || {
                    analyze_select_with_ctes_and_outer(
                        sel,
                        snapshot,
                        params,
                        cte_scopes,
                        &lateral_sources,
                        &enclosing,
                        &shadowed_sources,
                    )
                });
                let (mut cols, _) = analyzed?;
                let summary = take_level_summary();
                if capture {
                    null_ctx.add_where_facts(
                        correlated.restricted_to(&crate::nonnull::own_aliases(scope)),
                    );
                }
                resolve_unknown_outputs(sel, &mut cols, params, snapshot)?;
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
                        elem_nullable: rc.elem_nullable,
                        refine: rc.refine.clone(),
                        origin: rc.origin,
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
                if let Some(src) = scope.sources.last_mut() {
                    src.lock_error = pushed_lock_error(sel, snapshot);
                    src.min_one_row = summary.min_one_row;
                    null_ctx.register_origins(&src.alias, &src.columns, false);
                }
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
            if let Some(src) = scope.sources.last_mut() {
                src.partial_scan = true;
                for o in src.columns.iter_mut().filter_map(|c| c.origin.as_mut()) {
                    o.all_rows = false;
                }
            }
            // transformFromClauseItem: only a plain table, partitioned
            // table or materialized view can be sampled (not a view,
            // sequence, foreign table or WITH query).
            let sampleable = match relation.node.as_ref() {
                Some(node::Node::RangeVar(rv))
                    if !(rv.schemaname.is_empty() && cte_scopes.contains_key(&rv.relname)) =>
                {
                    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                    snapshot
                        .resolve_table(schema, &rv.relname)
                        .is_some_and(|c| {
                            matches!(
                                c.relkind,
                                crate::pg_catalog::RelKind::Table
                                    | crate::pg_catalog::RelKind::MaterializedView
                                    | crate::pg_catalog::RelKind::Partitioned
                            )
                        })
                }
                _ => false,
            };
            if !sampleable {
                return Err(crate::error::RawError::new(
                    AnalyzeError::FeatureNotSupported(
                        "TABLESAMPLE clause can only be applied to tables and materialized views"
                            .into(),
                    ),
                    None,
                    None,
                )
                .finalize_implicit());
            }
            process_tablesample(ts, scope, snapshot, params)?;
        }
        node::Node::JsonTable(jt) => {
            process_json_table(jt, scope, null_ctx, snapshot, params)?;
        }
        _ => {
            return Err(crate::error::RawError::unsupported(
                format!(
                    "typedpg does not support {} in FROM yet",
                    crate::error::node_kind(inner)
                ),
                crate::error::expr_span(node),
                None,
            )
            .finalize_implicit());
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
    null_ctx: &mut NullabilityContext,
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
    let log = crate::nonnull::StrictLog::default();
    let arg_ctx = expr::Ctx::new(&arg_scope, null_ctx, snapshot).logging_strictness(&log);
    let nfuncs = funcs.len();
    let mut cols: Vec<ScopeColumn> = Vec::new();
    let mut whole_row = crate::scope::WholeRow::Record;
    let mut rows = RteFunctionRows::default();
    // EXPR_KIND_FROM_FUNCTION: no aggregate or window call of this level,
    // in the call itself or in a sublink of its arguments.
    crate::grouping::with_clause(Some("functions in FROM"), || {
        for f in &funcs {
            cols.extend(function_rte_columns(
                f,
                alias,
                nfuncs,
                arg_ctx,
                params,
                &mut whole_row,
                &mut rows,
            )?);
            let call = protobuf::Node {
                node: Some(node::Node::FuncCall(Box::new(f.call.clone().into_owned()))),
            };
            crate::clause::check_level_calls(&call, arg_ctx, "functions in FROM", true)?;
        }
        Ok::<(), AnalyzeError>(())
    })?;
    // makeWholeRowVar: several functions or WITH ORDINALITY always make an
    // anonymous record.
    if nfuncs != 1 || rf.ordinality {
        whole_row = crate::scope::WholeRow::Record;
    }
    if nfuncs > 1 && !srfs_have_equal_static_rows(funcs.iter().map(|f| &*f.call), arg_ctx, params) {
        // nodeFunctionscan.c pads every function that runs out of rows
        // first with NULLs — unless they all yield as many.
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
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
            origin: None,
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

    // ExecMakeTableFunctionResult calls a strict set-returning function
    // with a NULL argument not at all: no row. So wherever a row of a lone
    // one is, its arguments were non-NULL (several functions pad each
    // other's missing rows with NULLs).
    if nfuncs == 1 && rows.strict_srf {
        let own = crate::nonnull::own_aliases(scope);
        let facts = funcs[0]
            .call
            .args
            .iter()
            .fold(crate::nonnull::Facts::default(), |acc, a| {
                acc.union(crate::nonnull::nonnullable(
                    a, false, &arg_scope, &log, snapshot,
                ))
            })
            .restricted_to(&own);
        null_ctx.add_entry_facts(alias, facts);
    }
    scope.add_derived(alias, cols, crate::scope::SourceKind::Function)?;
    if let Some(src) = scope.sources.last_mut() {
        src.whole_row = whole_row;
        src.min_one_row = rows.min_one_row;
    }
    Ok(())
}

/// What a function RTE's functions guarantee about its rows.
#[derive(Default)]
struct RteFunctionRows {
    /// One of them yields a row whatever happens (`generate_series` over
    /// constant bounds that aren't empty).
    min_one_row: bool,
    /// The (last) function is a strict set-returning one.
    strict_srf: bool,
}

/// `generate_series(start, stop [, step])` over integer constants that
/// make a non-empty series.
fn constant_series_is_non_empty(args: &[protobuf::Node]) -> bool {
    let int = |n: &protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(protobuf::AConst {
            val: Some(protobuf::a_const::Val::Ival(i)),
            isnull: false,
            ..
        })) => Some(i64::from(i.ival)),
        _ => None,
    };
    let vals: Option<Vec<i64>> = args.iter().map(int).collect();
    match vals.as_deref() {
        Some([start, stop]) => start <= stop,
        Some([start, stop, step]) => (*step > 0 && start <= stop) || (*step < 0 && start >= stop),
        _ => false,
    }
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

    let kept = rows_kept();
    let left = SourceSpan::capture(scope, |scope| match &join.larg {
        Some(larg) => with_rows_kept(
            kept && matches!(join_type, JoinType::JoinInner | JoinType::JoinLeft),
            || process_from_item(larg, scope, null_ctx, snapshot, cte_scopes, params),
        ),
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
        Some(rarg) => with_rows_kept(
            kept && matches!(join_type, JoinType::JoinInner | JoinType::JoinRight),
            || process_from_item(rarg, scope, null_ctx, snapshot, cte_scopes, params),
        ),
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
    let mut on_facts = crate::nonnull::Facts::default();
    let mut equalities: Option<Vec<(crate::nonnull::Col, crate::nonnull::Col)>> = None;
    if let Some(quals) = &join.quals {
        // transformJoinOnClause: the ON clause sees just the join's two
        // sides (plus outer levels); the FROM items beside the join are in
        // the range table but not referencable from it.
        let mut on_scope = scope.clone();
        let side_sources: Vec<crate::scope::TableSource> = left.to(right).sources(scope).to_vec();
        on_scope.shadowed_sources.extend(
            scope.sources[..left.start]
                .iter()
                .chain(&scope.sources[right.end..])
                .cloned(),
        );
        on_scope.sources = side_sources;
        // Shares WHERE's machinery: resolution errors first, then
        // the no-aggregates placement rule, then PG's clause wording
        // (`argument of JOIN/ON must be type boolean, not type X`).
        let log = crate::nonnull::StrictLog::default();
        crate::clause::coerce_clause_expr(
            quals,
            expr::Ctx::new(&on_scope, null_ctx, snapshot).logging_strictness(&log),
            params,
            crate::clause::ClauseKind::JoinOn,
        )?;
        check_no_srf_in_clause(quals, snapshot, "JOIN conditions")?;
        on_facts = crate::nonnull::nonnullable(quals, true, &on_scope, &log, snapshot);
        equalities = on_equalities(quals, &on_scope, &log, snapshot);
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
        let (merged, facts, parts) = merge_using_columns(
            scope,
            null_ctx,
            snapshot,
            &using_names,
            left,
            right,
            join_type,
        )?;
        on_facts = std::mem::take(&mut on_facts).union(facts);
        equalities = parts
            .iter()
            .map(|p| p.eq_strict.then(|| (p.left.0.clone(), p.right.0.clone())))
            .collect();
        Some((merged, parts))
    };

    // Apply JOIN nullability.
    let kind = match join_type {
        JoinType::JoinLeft => nullability::JoinKind::Left,
        JoinType::JoinRight => nullability::JoinKind::Right,
        JoinType::JoinFull => nullability::JoinKind::Full,
        JoinType::JoinInner => nullability::JoinKind::Inner,
        other => return Err(AnalyzeError::UnsupportedJoinType(other as i32)),
    };
    let fk = equalities
        .as_deref()
        .and_then(|eqs| fk_match(eqs, scope, left, right, null_ctx, snapshot));
    // `ON true` to a side that always yields a row (for each row of the
    // other, when LATERAL): that side always matches.
    let on_true = join.quals.as_deref().is_some_and(is_true_const);
    let always = |side: SourceSpan| on_true && matches!(side.sources(scope), [s] if s.min_one_row);
    let always_matches = (always(left), always(right));
    // A FULL join's row has one side or the other: what each side has
    // non-NULL whenever its row is there.
    let full_sides = (kind == nullability::JoinKind::Full).then(|| {
        let side_cols = |side: SourceSpan| -> Vec<crate::nonnull::Col> {
            side.sources(scope)
                .iter()
                .filter(|s| !null_ctx.alias_is_nullable(&s.alias))
                .flat_map(|s| s.columns.iter())
                .filter(|c| !null_ctx.is_nullable(&c.table_alias, &c.name, c.base_not_null))
                .map(|c| (c.table_alias.clone(), c.name.clone()))
                .collect()
        };
        [side_cols(left), side_cols(right)]
    });
    let join_idx = null_ctx.record_join(
        kind,
        &nullability::collect_aliases(left.sources(scope)),
        &nullability::collect_aliases(right.sources(scope)),
        on_facts,
        fk,
        always_matches,
        full_sides,
    );

    let merged_inserted = merged.is_some();
    if let Some((merged, parts)) = merged {
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
            return Err(crate::pgmsg::duplicate_table_alias(&alias, None).finalize_implicit());
        }
        null_ctx.record_merged(
            parts
                .into_iter()
                .map(|p| {
                    (
                        (alias.clone(), p.name),
                        nullability::Merged {
                            join: join_idx,
                            left: p.left,
                            right: p.right,
                            inside_not_null: p.inside_not_null,
                            eq_strict: p.eq_strict,
                        },
                    )
                })
                .collect(),
        );
        scope.sources.insert(
            left.start,
            crate::scope::TableSource {
                kind: crate::scope::SourceKind::Join,
                ..crate::scope::TableSource::derived(&alias, columns)
            },
        );
    }

    if let Some(alias) = &join.alias {
        let end = right.end + usize::from(merged_inserted);
        alias_join(scope, null_ctx, alias, left.start, end)?;
    }
    Ok(())
}

/// Every table inheriting from `relid`, directly or not (partitions too).
pub(crate) fn inheritance_descendants(
    snapshot: &PgCatalog,
    relid: crate::oid::PgClassOid,
) -> Vec<crate::oid::PgClassOid> {
    let mut out = Vec::new();
    let mut todo = vec![relid];
    while let Some(r) = todo.pop() {
        for i in snapshot.pg_inherits.iter().filter(|i| i.inhparent == r) {
            if !out.contains(&i.inhrelid) {
                out.push(i.inhrelid);
                todo.push(i.inhrelid);
            }
        }
    }
    out
}

/// The column pairs an ON clause equates, when it is nothing but an AND
/// of strict `a.x = b.y` comparisons between plain columns — or `IS NOT
/// DISTINCT FROM`, the same where the columns are non-NULL (what a
/// foreign key needs of its child keys anyway), and either side may be
/// cast to its own type (a no-op).
fn on_equalities(
    quals: &protobuf::Node,
    on_scope: &Scope,
    log: &crate::nonnull::StrictLog,
    snapshot: &PgCatalog,
) -> Option<Vec<(crate::nonnull::Col, crate::nonnull::Col)>> {
    let mut cs = Vec::new();
    conjuncts(quals, &mut cs);
    let column = |n: &protobuf::Node| -> Option<crate::nonnull::Col> {
        let n = match n.node.as_ref()? {
            // `col::its_type` (no typmod: a length coercion is no no-op).
            node::Node::TypeCast(tc) => {
                let tn = tc.type_name.as_ref()?;
                let arg = tc.arg.as_deref()?;
                let (_, t) = crate::nonnull::plain_column(arg, on_scope)?;
                if !tn.typmods.is_empty()
                    || crate::ddl::util::resolve_type_name(tn, snapshot) != Some(t)
                {
                    return None;
                }
                arg
            }
            _ => n,
        };
        Some(crate::nonnull::plain_column(n, on_scope)?.0)
    };
    cs.into_iter()
        .map(|c| {
            let Some(node::Node::AExpr(e)) = c.node.as_ref() else {
                return None;
            };
            if !matches!(
                protobuf::AExprKind::try_from(e.kind),
                Ok(protobuf::AExprKind::AexprOp | protobuf::AExprKind::AexprNotDistinct)
            ) || expr::extract_string_fields(&e.name).join(".") != "="
                || !log.is_strict(e.location, crate::nonnull::StrictNode::Op)
            {
                return None;
            }
            let l = column(e.lexpr.as_deref()?)?;
            let r = column(e.rexpr.as_deref()?)?;
            Some((l, r))
        })
        .collect()
}

/// The foreign key a join follows, when its join condition is exactly the
/// equalities of a foreign key from the rows of an entry on one side (the
/// child) into the other side, which must keep every row of the
/// referenced table (the parent): the table itself scanned in full, a
/// view, subquery or CTE passing all of its rows through, or a join tree
/// keeping every row of one of those ([`NullabilityContext::side_keeps_rows_of`]).
/// The child's key columns may come through a subquery, CTE, view or
/// aliased join too, as long as they are passed through from one scan of
/// the child table (see [`crate::scope::Origin`]). The constraint is
/// assumed to hold, `NOT VALID` or deferrable as it may be; a `NOT
/// ENFORCED` one isn't, nor one into a table with row security.
///
/// A join of a table's rows to the same table on the same columns is
/// matched just the same: each row finds itself (a btree `=` is
/// reflexive).
fn fk_match(
    equalities: &[(crate::nonnull::Col, crate::nonnull::Col)],
    scope: &Scope,
    left: SourceSpan,
    right: SourceSpan,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
) -> Option<nullability::FkMatch> {
    let in_span =
        |side: SourceSpan, alias: &str| side.sources(scope).iter().any(|s| s.alias == alias);
    let col_of = |side: SourceSpan, c: &crate::nonnull::Col| {
        side.sources(scope)
            .iter()
            .find(|s| s.alias == c.0)?
            .columns
            .iter()
            .find(|sc| sc.name == c.1)
    };
    // The origins of `cols` (all in `side`), when all from one scan.
    let one_scan = |side: SourceSpan, cols: &[&crate::nonnull::Col]| {
        let origins: Vec<&crate::scope::Origin> = cols
            .iter()
            .map(|c| col_of(side, c)?.origin.as_ref())
            .collect::<Option<_>>()?;
        let first = *origins.first()?;
        origins
            .iter()
            .all(|o| o.scan == first.scan)
            .then_some(origins)
    };
    let try_side = |parent_side: SourceSpan, child_side: SourceSpan, parent_is_left: bool| {
        // (child column, parent column) pairs, and whether the parent's
        // column is the left operand.
        let mut pairs: Vec<(&crate::nonnull::Col, &crate::nonnull::Col)> = Vec::new();
        let mut parent_left: Vec<bool> = Vec::new();
        for (a, b) in equalities {
            if in_span(parent_side, &a.0) && in_span(child_side, &b.0) {
                pairs.push((b, a));
                parent_left.push(true);
            } else if in_span(parent_side, &b.0) && in_span(child_side, &a.0) {
                pairs.push((a, b));
                parent_left.push(false);
            } else {
                return None;
            }
        }
        let parent_alias = &pairs.first()?.1.0;
        let child_alias = &pairs.first()?.0.0;
        if pairs
            .iter()
            .any(|(c, p)| &c.0 != child_alias || &p.0 != parent_alias)
        {
            return None;
        }
        let parent_origins = one_scan(
            parent_side,
            &pairs.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
        )?;
        let child_origins = one_scan(
            child_side,
            &pairs.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        )?;
        let (parent, child) = (parent_origins[0], child_origins[0]);
        if !parent.all_rows || !fk_parent_ok(snapshot, parent.relid) {
            return None;
        }
        // The parent's entry keeps its rows through the side's joins.
        let side_aliases: std::collections::HashSet<String> = parent_side
            .sources(scope)
            .iter()
            .map(|s| s.alias.clone())
            .collect();
        if !null_ctx.side_keeps_rows_of(&side_aliases, parent_alias) {
            return None;
        }
        let wanted: std::collections::BTreeSet<(String, String)> = child_origins
            .iter()
            .zip(&parent_origins)
            .map(|(c, p)| (c.column.clone(), p.column.clone()))
            .collect();
        let typed: Option<Vec<FkEquality<'_>>> = pairs
            .iter()
            .zip(child_origins.iter().zip(&parent_origins))
            .zip(&parent_left)
            .map(|(((c, p), (co, po)), &parent_left)| {
                Some(FkEquality {
                    parent_col: &po.column,
                    child_col: &co.column,
                    parent_type: col_of(parent_side, p)?.type_oid,
                    child_type: col_of(child_side, c)?.type_oid,
                    parent_left,
                })
            })
            .collect();
        let follows = !child.with_children
            && fk_follows(snapshot, child.relid, parent.relid, &wanted)
            && typed
                .is_some_and(|eqs| fk_equalities_hold(snapshot, child.relid, parent.relid, &eqs));
        // A row joined to its own table on its own columns finds itself.
        let itself = || {
            scan_contains(parent, child)
                && pairs
                    .iter()
                    .zip(child_origins.iter().zip(&parent_origins))
                    .all(|((c, _), (co, po))| {
                        co.column == po.column
                            && col_of(child_side, c)
                                .is_some_and(|sc| reflexive_eq(snapshot, sc.type_oid))
                    })
        };
        if !follows && !itself() {
            return None;
        }
        let child_cols: Vec<(crate::nonnull::Col, bool)> = pairs
            .iter()
            .map(|(c, _)| {
                let b = col_of(child_side, c).is_some_and(|sc| sc.base_not_null);
                ((*c).clone(), b)
            })
            .collect();
        let child_present = child_cols
            .iter()
            .all(|((a, c), b)| !null_ctx.is_nullable(a, c, *b));
        Some(nullability::FkMatch {
            parent_is_left,
            child_cols,
            child_present,
        })
    };
    if equalities.is_empty() || crate::nonnull::row_locking() {
        return None;
    }
    try_side(right, left, false).or_else(|| try_side(left, right, true))
}

/// `(a JOIN b …) AS j [(c1, …)]`, as PG's `transformFromClauseItem` /
/// `addRangeTableEntryForJoin`: the join becomes one FROM entry `j` whose
/// columns are its output — merged USING columns, then the left side's,
/// then the right side's — renamed by the alias list, with the join's
/// nullability baked in. The entries inside are no longer referencable
/// (`a.x` → `invalid reference to FROM-clause entry for table "a"`), and a
/// name repeated across the sides is ambiguous even as `j.id`.
fn alias_join(
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    alias: &protobuf::Alias,
    start: usize,
    end: usize,
) -> Result<(), AnalyzeError> {
    let inner: Vec<crate::scope::TableSource> = scope.sources.drain(start..end).collect();
    // What each column is inside the join: its nullability follows that
    // column's, as later quals narrow it or reduce the join it's in.
    let inner_cols: Vec<(String, String, bool)> = inner
        .iter()
        .flat_map(|s| s.visible_columns())
        .map(|c| (c.table_alias.clone(), c.name.clone(), c.base_not_null))
        .collect();
    let mut columns: Vec<ScopeColumn> = inner
        .iter()
        .flat_map(|s| s.visible_columns())
        .map(|c| ScopeColumn {
            base_not_null: !null_ctx.is_nullable(&c.table_alias, &c.name, c.base_not_null),
            table_alias: alias.aliasname.clone(),
            ..c.clone()
        })
        .collect();
    // A table's rows are all in the join only if its joins keep them.
    let inner_aliases: std::collections::HashSet<String> =
        inner.iter().map(|s| s.alias.clone()).collect();
    for (c, (ia, _, _)) in columns.iter_mut().zip(&inner_cols) {
        if let Some(o) = &mut c.origin {
            o.all_rows &= null_ctx.side_keeps_rows_of(&inner_aliases, ia);
        }
    }
    let colnames = expr::extract_string_fields(&alias.colnames);
    if colnames.len() > columns.len() {
        return Err(crate::pgmsg::too_many_join_column_aliases(
            &alias.aliasname,
            columns.len(),
            colnames.len(),
        )
        .finalize_implicit());
    }
    for (c, name) in columns.iter_mut().zip(colnames) {
        c.name = name;
    }
    if scope.sources.iter().any(|s| s.alias == alias.aliasname) {
        return Err(crate::pgmsg::duplicate_table_alias(&alias.aliasname, None).finalize_implicit());
    }
    // A name repeated across the sides can't be referenced (it's
    // ambiguous): only unique names are mapped.
    for (c, (ia, ic, ibase)) in columns.iter().zip(inner_cols) {
        if columns.iter().filter(|o| o.name == c.name).count() == 1 {
            null_ctx.record_aliased((alias.aliasname.clone(), c.name.clone()), (ia, ic), ibase);
        }
    }
    scope.shadowed_sources.extend(
        inner
            .into_iter()
            .filter(|s| !crate::scope::is_hidden_alias(&s.alias)),
    );
    null_ctx.register_origins(&alias.aliasname, &columns, false);
    scope.sources.insert(
        start,
        crate::scope::TableSource {
            kind: crate::scope::SourceKind::Join,
            ..crate::scope::TableSource::derived(&alias.aliasname, columns)
        },
    );
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
) -> Result<(Vec<ScopeColumn>, crate::nonnull::Facts, Vec<MergedParts>), AnalyzeError> {
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
    let mut facts = crate::nonnull::Facts::default();
    let mut parts: Vec<MergedParts> = Vec::new();
    let mut hide: Vec<(usize, String)> = Vec::new();
    for (i, name) in using_names.iter().enumerate() {
        if using_names[..i].contains(name) {
            return Err(crate::pgmsg::using_column_listed_twice(name).finalize_implicit());
        }
        let (l_idx, l) = find_col(scope, left, name, "left")?;
        let (r_idx, r) = find_col(scope, right, name, "right")?;

        let lt = crate::ddl::util::format_type_for_message(snapshot, l.type_oid);
        let rt = crate::ddl::util::format_type_for_message(snapshot, r.type_oid);
        let eq = snapshot.find_operator("=", Some(l.type_oid), r.type_oid);
        if l.type_oid != oid::UNKNOWN && r.type_oid != oid::UNKNOWN && eq.is_none() {
            // PG gives this one no position.
            let err = crate::pgmsg::operator_does_not_exist(&lt, "=", &rt, None);
            return Err(crate::expr::operators::with_cast_note(
                err, snapshot, "=", l.type_oid, r.type_oid,
            )
            .finalize_implicit());
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
        // The join qual `l = r`: when its operator is strict, a joined
        // pair has both sides non-NULL.
        let eq_strict = eq
            .as_ref()
            .and_then(|op| op.code)
            .and_then(|f| snapshot.pg_proc.get(&f))
            .is_some_and(|f| f.proisstrict);
        if eq_strict {
            facts = facts
                .union(crate::nonnull::Facts::column(&l.table_alias, &l.name))
                .union(crate::nonnull::Facts::column(&r.table_alias, &r.name));
        }
        parts.push(MergedParts {
            name: name.clone(),
            left: ((l.table_alias.clone(), l.name.clone()), l.base_not_null),
            right: ((r.table_alias.clone(), r.name.clone()), r.base_not_null),
            inside_not_null: (l_not_null, r_not_null),
            eq_strict,
        });
        let base_not_null = match join_type {
            JoinType::JoinLeft => l_not_null,
            JoinType::JoinRight => r_not_null,
            JoinType::JoinFull => l_not_null && r_not_null,
            _ => l_not_null || r_not_null || eq_strict,
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
            elem_nullable: crate::expr::merge_elem_nullable([l.elem_nullable, r.elem_nullable]),
            refine: crate::refine::Refinement::either([
                &l.refine.converted(l.type_oid, type_oid, snapshot),
                &r.refine.converted(r.type_oid, type_oid, snapshot),
            ]),
            origin: None,
        });
        hide.push((l_idx, name.clone()));
        hide.push((r_idx, name.clone()));
    }
    for (idx, name) in hide {
        scope.sources[idx].join_hidden.insert(name);
    }
    Ok((merged, facts, parts))
}

/// A `JOIN USING` merged column's constituents (with their own NOT NULL)
/// and whether its `l = r` is strict.
struct MergedParts {
    name: String,
    left: (crate::nonnull::Col, bool),
    right: (crate::nonnull::Col, bool),
    inside_not_null: (bool, bool),
    eq_strict: bool,
}

/// Apply a FROM item's column-alias list (`users AS t(a, b, c)`) to the
/// just-added source: rename positionally, and mirror PG's 42P10 rejection
/// when more aliases than columns are given (`table "t" has N columns
/// available but M columns specified`).
fn apply_alias_column_names(
    scope: &mut Scope,
    alias_node: Option<&typedpg_pg_query::protobuf::Alias>,
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

/// RangeVarGetRelidExtended's catalog check: a relation qualified by a
/// database name (`db.schema.rel`) must be in the current database, which
/// the analyzer cannot know — every catalog qualifier is taken as another
/// database (`crate::pgmsg::cross_database_reference`). PG quotes the
/// whole name here.
pub(crate) fn check_rangevar_catalog(rv: &protobuf::RangeVar) -> Result<(), AnalyzeError> {
    if rv.catalogname.is_empty() {
        return Ok(());
    }
    let name = format!("\"{}.{}.{}\"", rv.catalogname, rv.schemaname, rv.relname);
    Err(crate::pgmsg::cross_database_reference(
        &name,
        crate::error::SourceSpan::from_node_qname(rv.location),
    )
    .finalize_implicit())
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
    arg_scope.shadowed_sources = scope.shadowed_sources.clone();
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
) -> Result<Vec<crate::expr::ExprType>, AnalyzeError> {
    let mut exprs = Vec::with_capacity(func_call.args.len());
    for arg in &func_call.args {
        // Any error transforming an argument aborts the query, as in PG's
        // `transformRangeFunction`: an unknown column, a FROM item that isn't
        // visible yet (`FROM f(t.c), t`), the left side of a RIGHT / FULL join.
        exprs.push(expr::infer_expr(
            arg,
            arg_ctx,
            params,
            crate::expr::TypeGoal::NONE,
        )?);
    }
    Ok(exprs)
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
        // What the argument's type says of its elements: `ARRAY[a, b]` over
        // NOT NULL values, `ARRAY(SELECT nn_col …)`, … (a multidimensional
        // array's are unknown: its sub-arrays being there says nothing of
        // their elements).
        let elements_not_null = args
            .first()
            .map(functions::call_arg_value)
            .is_some_and(|a| {
                let mut scratch = params.clone();
                expr::infer_expr(a, ctx, &mut scratch, TypeGoal::NONE)
                    .is_ok_and(|t| t.elem_nullable == Some(false))
            });
        return Some(!elements_not_null);
    }
    Some(matches!(
        name,
        "json_array_elements_text" | "jsonb_array_elements_text"
    ))
}

/// The number of rows a set-returning call yields, when the call alone
/// fixes it whatever the data: `pg_catalog.unnest` of an `ARRAY[…]`
/// constructor over non-array values (one row per element, NULL or not)
/// or of an array literal (one per element, every dimension unrolled),
/// and `pg_catalog.generate_series` over integer literals. `None` for
/// anything else.
pub(crate) fn static_srf_rows(
    fc: &protobuf::FuncCall,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Option<u64> {
    if fc.over.is_some()
        || fc.func_variadic
        || fc.agg_star
        || fc.agg_filter.is_some()
        || !fc.agg_order.is_empty()
        || fc.args.iter().any(|a| {
            !matches!(a.node.as_ref(), Some(node::Node::NamedArgExpr(_)))
                && count_srf_calls(std::slice::from_ref(a), ctx.snapshot) > 0
        })
    {
        return None;
    }
    let parts = expr::extract_string_fields(&fc.funcname);
    let (schema, name) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return None,
    };
    let mut scratch = params.clone();
    let mut types = Vec::with_capacity(fc.args.len());
    for a in &fc.args {
        types.push(
            expr::infer_expr(a, ctx, &mut scratch, TypeGoal::NONE)
                .ok()?
                .type_oid,
        );
    }
    let resolved = functions::resolve_function(
        ctx.snapshot,
        schema,
        name,
        &types,
        &functions::CallNotation::of(fc).ok()?,
        false,
        None,
    )
    .ok()?;
    if resolved.schema != "pg_catalog" || !resolved.is_set_returning {
        return None;
    }
    let args: Vec<&protobuf::Node> = fc.args.iter().map(functions::call_arg_value).collect();
    match resolved.signature.as_str() {
        "unnest(anyarray)" => static_array_len(args.first()?, ctx, params),
        "generate_series(int4,int4)"
        | "generate_series(int8,int8)"
        | "generate_series(int4,int4,int4)"
        | "generate_series(int8,int8,int8)" => {
            let int = |n: &protobuf::Node| -> Option<i128> {
                crate::nonnull::literal(n)
                    .filter(|l| l.is_integer())
                    .and_then(|l| l.text.parse::<i128>().ok())
            };
            let start = int(args.first()?)?;
            let stop = int(args.get(1)?)?;
            let step = match args.get(2) {
                Some(s) => int(s)?,
                None => 1,
            };
            let rows = match step {
                0 => return None,
                s if s > 0 && stop >= start => (stop - start) / s + 1,
                s if s < 0 && start >= stop => (start - stop) / -s + 1,
                _ => 0,
            };
            u64::try_from(rows).ok()
        }
        _ => None,
    }
}

/// The number of elements of an array expression whose length doesn't
/// depend on the data (see [`static_srf_rows`]); a cast to an array type
/// keeps it (an element conversion maps elements one to one).
fn static_array_len(node: &protobuf::Node, ctx: Ctx<'_>, params: &ParamCollector) -> Option<u64> {
    match node.node.as_ref()? {
        node::Node::TypeCast(c) => static_array_len(c.arg.as_deref()?, ctx, params),
        node::Node::AArrayExpr(a) => {
            // `ARRAY[x, y]` over arrays builds a multidimensional array.
            let mut scratch = params.clone();
            for e in &a.elements {
                if matches!(e.node.as_ref(), Some(node::Node::AArrayExpr(_))) {
                    return None;
                }
                let t = expr::infer_expr(e, ctx, &mut scratch, TypeGoal::NONE).ok()?;
                if t.type_oid == oid::UNKNOWN
                    && !matches!(e.node.as_ref(), Some(node::Node::AConst(_)))
                {
                    return None;
                }
                if crate::coerce::element_type(ctx.snapshot.unwrap_domain(t.type_oid), ctx.snapshot)
                    .is_some()
                {
                    return None;
                }
            }
            u64::try_from(a.elements.len()).ok()
        }
        node::Node::AConst(c) if !c.isnull => match &c.val {
            // (`box[]` literals are `;`-delimited.)
            Some(protobuf::a_const::Val::Sval(sv)) if !sv.sval.contains(';') => {
                let mut n: u64 = 0;
                crate::array_input::parse_array(&sv.sval, b',', &mut |_| {
                    n += 1;
                    Ok(())
                })
                .ok()?;
                Some(n)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether set-returning calls run in lockstep never pad: each yields the
/// same, statically known number of rows ([`static_srf_rows`]).
pub(crate) fn srfs_have_equal_static_rows<'a>(
    calls: impl IntoIterator<Item = &'a protobuf::FuncCall>,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> bool {
    let mut rows = None;
    for fc in calls {
        let Some(n) = static_srf_rows(fc, ctx, params) else {
            return false;
        };
        if rows.is_some_and(|r| r != n) {
            return false;
        }
        rows = Some(n);
    }
    rows.is_some()
}

/// Whether a SELECT's input always has a row, whatever the data: no WHERE,
/// and a FROM list (none at all is one row) of items that always do —
/// a VALUES list, a FROM-less SELECT, a function of statically known
/// non-zero length, or a join keeping such a side. What an aggregate
/// without GROUP BY then reads is never empty. `ctx` sees the FROM list.
pub(crate) fn select_input_never_empty(
    sel: &protobuf::SelectStmt,
    ctx: Ctx<'_>,
    params: &ParamCollector,
) -> bool {
    sel.where_clause.is_none()
        && sel
            .from_clause
            .iter()
            .all(|item| from_item_never_empty(item, ctx, params))
}

fn from_item_never_empty(item: &protobuf::Node, ctx: Ctx<'_>, params: &ParamCollector) -> bool {
    match item.node.as_ref() {
        Some(node::Node::RangeFunction(rf)) => rf.functions.iter().any(|f| {
            let Some(node::Node::List(pair)) = f.node.as_ref() else {
                return false;
            };
            let Some(node::Node::FuncCall(fc)) = pair.items.first().and_then(|n| n.node.as_ref())
            else {
                return false;
            };
            if is_sql_standard_unnest(fc) {
                // ROWS FROM (unnest(a), unnest(b), …): as long as the longest.
                return fc.args.iter().any(|arg| {
                    let mut single = (**fc).clone();
                    single.args = vec![arg.clone()];
                    static_srf_rows(&single, ctx, params).is_some_and(|n| n > 0)
                });
            }
            static_srf_rows(fc, ctx, params).is_some_and(|n| n > 0)
        }),
        Some(node::Node::RangeSubselect(rs)) => {
            match rs.subquery.as_deref().and_then(|s| s.node.as_ref()) {
                Some(node::Node::SelectStmt(s)) => {
                    s.op == protobuf::SetOperation::SetopNone as i32
                        && s.limit_count.is_none()
                        && s.limit_offset.is_none()
                        && (!s.values_lists.is_empty()
                            || (s.from_clause.is_empty()
                                && s.where_clause.is_none()
                                && s.having_clause.is_none()
                                && s.group_clause.is_empty()
                                && level_srf_calls(s, ctx.snapshot).is_empty()))
                }
                _ => false,
            }
        }
        Some(node::Node::JoinExpr(j)) => {
            let side = |n: &Option<Box<protobuf::Node>>| {
                n.as_deref()
                    .is_some_and(|n| from_item_never_empty(n, ctx, params))
            };
            match protobuf::JoinType::try_from(j.jointype) {
                Ok(protobuf::JoinType::JoinInner) => {
                    j.quals.is_none()
                        && !j.is_natural
                        && j.using_clause.is_empty()
                        && side(&j.larg)
                        && side(&j.rarg)
                }
                Ok(protobuf::JoinType::JoinLeft) => side(&j.larg),
                Ok(protobuf::JoinType::JoinRight) => side(&j.rarg),
                Ok(protobuf::JoinType::JoinFull) => side(&j.larg) || side(&j.rarg),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Which OUT columns of a strict `pg_catalog` set-returning function with
/// several of them can be NULL in a row it emits, for the functions whose
/// C implementation says (`None` for the rest): `each_worker[_jsonb]`
/// fills the key from the object's key string, always, and the value
/// with the member's JSON value — JSON `null` included, as a json/jsonb
/// datum — except for the `_text` variants, which map a JSON `null` to
/// SQL NULL. (A NULL argument yields no row: the functions are strict.)
pub(crate) fn srf_out_columns_nullable(
    resolved: &functions::ResolvedFunction,
) -> Option<&'static [bool]> {
    if !(resolved.is_set_returning && resolved.is_strict && resolved.schema == "pg_catalog") {
        return None;
    }
    match resolved.signature.as_str() {
        "jsonb_each(jsonb)" | "json_each(json)" => Some(&[false, false]),
        "jsonb_each_text(jsonb)" | "json_each_text(json)" => Some(&[false, true]),
        _ => None,
    }
}

/// Columns one function contributes to a function RTE, following
/// `addRangeTableEntryForFunction`'s `get_expr_result_type` classes: OUT
/// parameters and named composites expose their own row (a column
/// definition list is redundant there), `record` requires one, and a
/// scalar result is a single column (a column definition list is not
/// allowed).
///
/// `whole_row` receives what a whole-row reference to a single-function RTE
/// yields: its named composite type, or the scalar value itself.
fn function_rte_columns(
    f: &RteFunction<'_>,
    alias: &str,
    nfuncs: usize,
    arg_ctx: Ctx<'_>,
    params: &mut ParamCollector,
    whole_row: &mut crate::scope::WholeRow,
    rows: &mut RteFunctionRows,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let snapshot = arg_ctx.snapshot;
    let func_call: &protobuf::FuncCall = &f.call;
    let func_name_parts = expr::extract_string_fields(&func_call.funcname);
    let (schema, name) = expr::deconstruct_qualified_name(
        &func_name_parts,
        crate::error::SourceSpan::from_node_qname(func_call.location),
    )?;
    let arg_exprs = infer_srf_arg_types(func_call, arg_ctx, params)?;
    let arg_types: Vec<PgTypeOid> = arg_exprs.iter().map(|e| e.type_oid).collect();
    let arg_nullable: Vec<bool> = arg_exprs.iter().map(|e| e.nullable).collect();
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
    let resolved = functions::resolve_function(
        snapshot,
        schema,
        name,
        &arg_types,
        &functions::CallNotation::of(func_call)?,
        false,
        crate::error::SourceSpan::from_node_qname(func_call.location),
    )?;
    // Coerce untyped arguments to the chosen signature (pins `$N`,
    // validates literal contents), as for a call anywhere else.
    expr::backfill_call_args(func_call, &arg_types, &resolved, arg_ctx, params)?;
    // An argument whose coercion can map it to NULL is a NULL argument.
    let arg_nullable: Vec<bool> = arg_nullable
        .iter()
        .zip(expr::arg_coercions_nullable(
            &arg_types, &resolved, snapshot,
        ))
        .map(|(&n, c)| n || c)
        .collect();
    // Strict in the arguments as written: not a variadic call packing them
    // into an array, nor one coercing them through a non-strict cast.
    rows.strict_srf = resolved.is_set_returning
        && resolved.is_strict
        && resolved.nvargs == 0
        && arg_types
            .iter()
            .zip(&resolved.arg_types)
            .all(|(&a, &d)| arg_ctx.coercion_is_strict(a, d));
    rows.min_one_row |= resolved.schema == "pg_catalog"
        && name == "generate_series"
        && !func_call.func_variadic
        && constant_series_is_non_empty(&func_call.args);
    let has_coldeflist = !f.coldeflist.is_empty();

    // A strict catalog SRF with a single OUT column (`jsonb_array_elements`
    // → `value`) emits exactly its elements; wider OUT rows are known per
    // column for some (`jsonb_each`) and keep the conservative default
    // otherwise.
    let single_out_not_null = (resolved.out_args.len() == 1)
        .then(|| srf_elements_nullable(&resolved, name, &func_call.args, arg_ctx, params))
        .flatten()
        .map(|nullable| !nullable);
    let out_columns_nullable =
        srf_out_columns_nullable(&resolved).filter(|n| n.len() == resolved.out_args.len());

    if !resolved.out_args.is_empty() {
        if has_coldeflist {
            return Err(crate::pgmsg::coldeflist_redundant_out_params().finalize_implicit());
        }
        // A single OUT parameter makes the function scalar
        // (TYPEFUNC_SCALAR): a whole-row reference is its value, not a
        // record of it.
        if resolved.out_args.len() == 1 {
            *whole_row = crate::scope::WholeRow::Scalar;
        }
        return Ok(resolved
            .out_args
            .iter()
            .enumerate()
            .map(|(i, f)| ScopeColumn {
                name: f.name.clone(),
                type_oid: f.type_oid,
                base_not_null: match out_columns_nullable {
                    Some(nullable) => !nullable[i],
                    None => single_out_not_null.unwrap_or(f.not_null),
                },
                typmod: None,
                collation: None,
                table_alias: alias.to_owned(),
                record_fields: None,
                elem_nullable: None,
                refine: crate::refine::Refinement::NONE,
                origin: None,
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
        *whole_row = crate::scope::WholeRow::Composite(resolved.return_type_oid);
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
                elem_nullable: None,
                refine: crate::refine::Refinement::NONE,
                origin: None,
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
                    &resolved.in_declared_order(&arg_nullable, false),
                    func_call.func_variadic,
                )
        }
    };
    // `chooseScalarFunctionAlias`: a lone scalar function's column is named
    // after the RTE alias (`FROM generate_series(1, 3) AS g` exposes `g`);
    // with several functions each column keeps its function's name.
    let col_name = if nfuncs == 1 { alias } else { name };
    *whole_row = crate::scope::WholeRow::Scalar;
    Ok(vec![ScopeColumn {
        name: col_name.to_owned(),
        type_oid: resolved.return_type_oid,
        base_not_null: not_null,
        table_alias: alias.to_owned(),
        typmod: None,
        collation: None,
        record_fields: None,
        elem_nullable: None,
        // `generate_series` of finite bounds and step (see
        // `refine::of_builtin_call`).
        refine: snapshot
            .pg_proc
            .get(&resolved.oid)
            .map(|p| {
                crate::refine::of_builtin_call(p, &arg_exprs.iter().collect::<Vec<_>>(), snapshot)
            })
            .unwrap_or_default(),
        origin: None,
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
        let typmod = crate::typmod::encode(snapshot, type_oid, tn)
            .map_err(|e| AnalyzeError::Invalid(e.to_string()))?;
        cols.push(ScopeColumn {
            name: cd.colname.clone(),
            type_oid,
            base_not_null: false,
            typmod: snapshot.effective_typmod(type_oid, typmod),
            collation: None,
            table_alias: alias.to_owned(),
            record_fields: None,
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
            origin: None,
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
    arg_scope
        .shadowed_sources
        .extend(scope.shadowed_sources.iter().cloned());
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
    // The arguments are EXPR_KIND_FROM_FUNCTION expressions: no aggregates
    // or window functions.
    for (arg, &target) in ts.args.iter().zip(param_types) {
        coerce(arg, target, "TABLESAMPLE", params)?;
        crate::clause::check_no_aggregates_or_windows(arg, snapshot, "functions in FROM")?;
    }
    if let Some(seed) = ts.repeatable.as_deref() {
        coerce(seed, oid::FLOAT8, "REPEATABLE", params)?;
        crate::clause::check_no_aggregates_or_windows(seed, snapshot, "functions in FROM")?;
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

    json_table_path_literal(jt.pathspec.as_deref(), ctx, params)?;

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

/// A JSON_TABLE path (the row pattern, a column's `PATH` / `EXISTS PATH`,
/// a `NESTED PATH`): the grammar only admits a string constant, which
/// transformJsonTable makes a `jsonpath` constant — so its content is
/// parsed by jsonpath_in at parse time, like any path argument.
fn json_table_path_literal(
    pathspec: Option<&protobuf::JsonTablePathSpec>,
    ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    const JSONPATH: PgTypeOid = PgTypeOid::from_raw(4072);
    match pathspec.and_then(|ps| ps.string.as_deref()) {
        Some(path) => expr::coerce_unknown_to(path, ctx, params, JSONPATH),
        None => Ok(()),
    }
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
        json_table_path_literal(col.pathspec.as_deref(), ctx, params)?;
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
            let m = crate::typmod::encode(snapshot, t, tn)
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
            elem_nullable: None,
            refine: crate::refine::Refinement::NONE,
            origin: None,
        });
    }
    Ok(())
}

/// What a locking clause pushed into query `sel` — a FROM subquery or a
/// view's definition — runs into below the level that names it:
/// `CheckSelectLocking` on it and its nested subqueries (the parser's
/// `transformLockingClause` for subqueries, `grouping_planner` for views,
/// whose queries only appear after the rewriter's `markQueryForLocking`),
/// the same for the views it reads, and `make_outerjoininfo`'s refusal of
/// a marked relation or subquery on an outer join's nullable side.
pub(crate) fn pushed_lock_error(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
) -> Option<crate::scope::LockBlock> {
    use crate::scope::LockBlock;
    if let Some(b) = subquery_lock_blocker(sel, snapshot) {
        return Some(LockBlock::NotAllowedWith(b));
    }
    let ctes: Vec<String> = sel
        .with_clause
        .as_ref()
        .map(|w| {
            w.ctes
                .iter()
                .filter_map(|c| match c.node.as_ref()? {
                    node::Node::CommonTableExpr(c) => Some(c.ctename.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    fn item(
        n: &protobuf::Node,
        nullable: bool,
        ctes: &[String],
        snapshot: &PgCatalog,
    ) -> Option<crate::scope::LockBlock> {
        use crate::scope::LockBlock;
        match n.node.as_ref()? {
            node::Node::RangeVar(rv) => {
                if rv.schemaname.is_empty() && ctes.contains(&rv.relname) {
                    return None;
                }
                let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                let class = snapshot.resolve_table(schema, &rv.relname)?;
                if nullable {
                    return Some(LockBlock::NullableSide);
                }
                view_lock_error(class, snapshot)
            }
            node::Node::RangeSubselect(rs) => {
                if nullable {
                    return Some(LockBlock::NullableSide);
                }
                match rs.subquery.as_deref()?.node.as_ref()? {
                    node::Node::SelectStmt(s) => pushed_lock_error(s, snapshot),
                    _ => None,
                }
            }
            node::Node::RangeTableSample(ts) => {
                item(ts.relation.as_deref()?, nullable, ctes, snapshot)
            }
            node::Node::JoinExpr(j) => {
                let (l, r) = match JoinType::try_from(j.jointype) {
                    Ok(JoinType::JoinLeft) => (nullable, true),
                    Ok(JoinType::JoinRight) => (true, nullable),
                    Ok(JoinType::JoinFull) => (true, true),
                    _ => (nullable, nullable),
                };
                j.larg
                    .as_deref()
                    .and_then(|n| item(n, l, ctes, snapshot))
                    .or_else(|| j.rarg.as_deref().and_then(|n| item(n, r, ctes, snapshot)))
            }
            // Functions, VALUES, JSON_TABLE… are unaffected by FOR UPDATE.
            _ => None,
        }
    }
    sel.from_clause
        .iter()
        .find_map(|n| item(n, false, &ctes, snapshot))
}

/// [`pushed_lock_error`] for a view's stored query (`None` for a table).
pub(crate) fn view_lock_error(
    class: &crate::pg_catalog::PgClass,
    snapshot: &PgCatalog,
) -> Option<crate::scope::LockBlock> {
    if class.relkind != crate::pg_catalog::RelKind::View {
        return None;
    }
    match crate::ddl::views::view_query(snapshot, class.oid)?.node? {
        node::Node::SelectStmt(s) => pushed_lock_error(&s, snapshot),
        _ => None,
    }
}
