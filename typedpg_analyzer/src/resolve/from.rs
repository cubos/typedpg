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
                let cols: Vec<ScopeColumn> = cte_cols
                    .iter()
                    .cloned()
                    .map(|mut c| {
                        c.table_alias = alias.to_owned();
                        c
                    })
                    .collect();
                scope.add_virtual_table(alias, cols)?;
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
            let alias = sub
                .alias
                .as_ref()
                .map(|a| a.aliasname.as_str())
                .unwrap_or("_subquery");

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
                let visible: Vec<_> = scope
                    .sources
                    .iter()
                    .chain(scope.lateral_sources.iter())
                    .cloned()
                    .collect();
                let (lateral_sources, shadowed_sources): (Vec<_>, Vec<_>) = if sub.lateral {
                    (visible, Vec::new())
                } else {
                    (Vec::new(), visible)
                };
                let (cols, _) = analyze_select_with_ctes_and_outer(
                    sel,
                    snapshot,
                    params,
                    cte_scopes,
                    &lateral_sources,
                    &[],
                    &shadowed_sources,
                )?;
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
                scope.add_virtual_table(alias, scope_cols)?;
            }
        }
        node::Node::RangeFunction(rf) => {
            process_range_function(rf, scope, null_ctx, snapshot, params)?;
        }
        node::Node::RangeTableSample(ts) => {
            // `TABLESAMPLE` only changes how rows are picked at runtime —
            // it does not affect the relation's column shape or
            // nullability. Pass through to the wrapped `relation` and
            // ignore method/args/repeatable.
            let relation = ts.relation.as_ref().ok_or_else(|| {
                AnalyzeError::Unsupported("RangeTableSample without relation".into())
            })?;
            return process_from_item(relation, scope, null_ctx, snapshot, cte_scopes, params);
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

    scope.add_virtual_table(alias, cols)?;
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

/// `a JOIN b ON …` / `USING (…)` / `NATURAL …`: process both sides, walk the
/// ON clause, apply outer-join nullability to the null-padded side(s), and
/// merge USING/NATURAL columns.
fn process_join_expr(
    join: &protobuf::JoinExpr,
    scope: &mut Scope,
    null_ctx: &mut NullabilityContext,
    snapshot: &PgCatalog,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let left = SourceSpan::capture(scope, |scope| match &join.larg {
        Some(larg) => process_from_item(larg, scope, null_ctx, snapshot, cte_scopes, params),
        None => Ok(()),
    })?;
    let right = SourceSpan::capture(scope, |scope| match &join.rarg {
        Some(rarg) => process_from_item(rarg, scope, null_ctx, snapshot, cte_scopes, params),
        None => Ok(()),
    })?;

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
    }

    // Apply JOIN nullability. Fail loudly on unknown join kinds rather
    // than defaulting to INNER, which would silently produce wrong
    // nullability for outer joins the parser couldn't classify.
    let join_type = JoinType::try_from(join.jointype)
        .map_err(|_| AnalyzeError::UnsupportedJoinType(join.jointype))?;

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

    // `JOIN … USING (cols)` / `NATURAL JOIN` merge the join columns:
    // the output has ONE column per name (placed before both sides'
    // remaining columns in `*`), an unqualified reference resolves
    // to it without ambiguity, and the constituents stay reachable
    // qualified (`a.id`) and via `a.*`.
    let using_names: Vec<String> = if join.is_natural {
        // Common column names, in left-side column order.
        let right_names: std::collections::HashSet<&str> = right
            .sources(scope)
            .iter()
            .flat_map(|s| s.columns.iter().map(|c| c.name.as_str()))
            .collect();
        left.sources(scope)
            .iter()
            .flat_map(|s| s.columns.iter().map(|c| c.name.clone()))
            .filter(|n| right_names.contains(n.as_str()))
            .collect()
    } else {
        expr::extract_string_fields(&join.using_clause)
    };
    if !using_names.is_empty() {
        merge_using_columns(scope, snapshot, &using_names, left, right, join_type)?;
    }
    Ok(())
}

/// Build the merged columns for `JOIN USING` / `NATURAL JOIN` and splice
/// them into the scope as a synthetic empty-alias source placed *before*
/// both join sides — which is exactly where PG puts them in `*` expansion
/// (`SELECT * FROM a JOIN b USING (id)` is `id, <a-rest>, <b-rest>`). The
/// constituent columns are recorded in `scope.join_hidden` so unqualified
/// resolution and the bare `*` skip them.
///
/// Merged-column semantics mirrored from PG:
/// - missing name → `column "x" specified in USING clause does not exist
///   in left/right table` (42703);
/// - type: the sides' common type, else `JOIN/USING types X and Y cannot
///   be matched` (42804);
/// - nullability: the merged value is the left side's for INNER/LEFT (the
///   preserved side), the right's for RIGHT, and `COALESCE(l, r)` for FULL
///   — all computed from the columns' *base* nullability (the outer-join
///   promotion applies to the constituent aliases, not the merged copy).
fn merge_using_columns(
    scope: &mut Scope,
    snapshot: &PgCatalog,
    using_names: &[String],
    left: SourceSpan,
    right: SourceSpan,
    join_type: JoinType,
) -> Result<(), AnalyzeError> {
    let find_col = |span: SourceSpan, name: &str| -> Option<(String, ScopeColumn)> {
        scope.sources[span.start..span.end].iter().find_map(|s| {
            s.columns
                .iter()
                .find(|c| c.name == name)
                .map(|c| (s.alias.clone(), c.clone()))
        })
    };

    let mut merged: Vec<ScopeColumn> = Vec::with_capacity(using_names.len());
    for name in using_names {
        let Some((l_alias, l)) = find_col(left, name) else {
            return Err(crate::pgmsg::using_column_missing(name, "left").finalize_implicit());
        };
        let Some((r_alias, r)) = find_col(right, name) else {
            return Err(crate::pgmsg::using_column_missing(name, "right").finalize_implicit());
        };

        let type_oid = if l.type_oid == r.type_oid {
            l.type_oid
        } else {
            crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot).ok_or_else(
                || {
                    let lt = crate::ddl::util::format_type_for_message(snapshot, l.type_oid);
                    let rt = crate::ddl::util::format_type_for_message(snapshot, r.type_oid);
                    crate::pgmsg::join_using_types_mismatch(&lt, &rt).finalize_implicit()
                },
            )?
        };
        let base_not_null = match join_type {
            JoinType::JoinRight => r.base_not_null,
            JoinType::JoinFull => l.base_not_null || r.base_not_null,
            _ => l.base_not_null,
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
            // The synthetic source's (empty) alias — never referenced
            // qualified.
            table_alias: String::new(),
            record_fields: None,
        });
        scope.join_hidden.insert((l_alias, name.clone()));
        scope.join_hidden.insert((r_alias, name.clone()));
    }

    scope.sources.insert(
        left.start,
        crate::scope::TableSource {
            alias: String::new(),
            columns: merged,
            system_columns: Vec::new(),
            source_qn: None,
        },
    );
    Ok(())
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
    arg_scope.sources.extend(scope.sources.clone());
    arg_scope
        .lateral_sources
        .extend(scope.lateral_sources.clone());
    arg_scope.outer_sources.extend(scope.outer_sources.clone());
    arg_scope.join_hidden = scope.join_hidden.clone();
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
    rf: &protobuf::RangeFunction,
    func_call: &protobuf::FuncCall,
    arg_ctx: Ctx<'_>,
    params: &mut ParamCollector,
) -> Result<(Vec<PgTypeOid>, Vec<bool>), AnalyzeError> {
    let mut arg_types = Vec::with_capacity(func_call.args.len());
    let mut arg_nullable = Vec::with_capacity(func_call.args.len());
    for arg in &func_call.args {
        let (t, n) = match expr::infer_expr(arg, arg_ctx, params, crate::expr::TypeGoal::NONE) {
            Ok(e) => (e.type_oid, e.nullable),
            // `FROM a, f(a.col)` without LATERAL — PG rejects with `invalid
            // reference to FROM-clause entry for table "a"`. The scope we
            // built above is empty precisely so this fails; don't let the
            // old `.unwrap_or(UNKNOWN)` swallow it. Likewise a qualifier
            // naming a FROM item that isn't visible yet (`FROM f(t.c), t`):
            // `missing FROM-clause entry for table "t"`.
            Err(e @ (AnalyzeError::UndefinedColumn(_) | AnalyzeError::UndefinedTable(_)))
                if !rf.lateral =>
            {
                return Err(e);
            }
            Err(_) => (oid::UNKNOWN, true),
        };
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
    let (arg_types, arg_nullable) = infer_srf_arg_types(rf, func_call, arg_ctx, params)?;
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
