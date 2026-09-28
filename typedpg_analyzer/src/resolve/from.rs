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

/// `FROM func(args)` — resolve the SRF and populate `scope` with its output
/// columns. Handles three cases:
/// - Function has `out_args` (TABLE/OUT) → one scope column per out_arg.
/// - Function returns a registered composite type → expand the composite's
///   fields as scope columns.
/// - Otherwise (scalar or plain `record`) → a single scope column named after
///   the relation alias (the function's name when unaliased, as in PG), typed
///   with its return OID.
///
/// Also honors `WITH ORDINALITY` by adding a trailing `ordinality BIGINT NOT NULL`
/// column when the flag is set.
fn process_range_function(
    rf: &protobuf::RangeFunction,
    scope: &mut Scope,
    null_ctx: &NullabilityContext,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let _ = rf.lateral;
    // Each entry in `functions` is a 2-element `List` — [FuncCall, coldeflist].
    // We support only the simple form: a single function call, no explicit
    // column definitions. `ROWS FROM (…)` with multiple functions or user-
    // supplied coldeflists are rarer and fall through to Unsupported so we
    // don't silently lose column shape.
    let list = rf
        .functions
        .first()
        .and_then(|n| n.node.as_ref())
        .and_then(|n| {
            if let node::Node::List(l) = n {
                Some(l)
            } else {
                None
            }
        })
        .ok_or_else(|| AnalyzeError::Unsupported("RangeFunction without function call".into()))?;

    let func_call_node = list
        .items
        .first()
        .ok_or_else(|| AnalyzeError::Unsupported("RangeFunction function list is empty".into()))?;
    let func_call = match func_call_node.node.as_ref() {
        Some(node::Node::FuncCall(fc)) => fc,
        _ => {
            return Err(AnalyzeError::Unsupported(
                "RangeFunction item is not a FuncCall".into(),
            ));
        }
    };

    // Alias: `FROM f() AS t(col1, col2)` gives aliases both for the relation
    // and for its columns. Fall back to the function's last name component.
    let func_name_parts = expr::extract_string_fields(&func_call.funcname);
    let default_alias = func_name_parts
        .last()
        .cloned()
        .unwrap_or_else(|| "_srf".into());
    let alias_owned = rf
        .alias
        .as_ref()
        .map(|a| a.aliasname.clone())
        .unwrap_or_else(|| default_alias.clone());
    let alias = alias_owned.as_str();
    let col_aliases: Vec<String> = rf
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

    let arg_scope = srf_arg_scope(scope);
    let arg_ctx = expr::Ctx::new(&arg_scope, null_ctx, snapshot);
    let (arg_types, arg_nullable) = infer_srf_arg_types(rf, func_call, arg_ctx, params)?;
    let any_arg_nullable = arg_nullable.iter().any(|&n| n);

    let (schema, name) = match func_name_parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => {
            return Err(AnalyzeError::UndefinedFunction(format!(
                "invalid function name in FROM: {func_name_parts:?}"
            )));
        }
    };

    // `unnest(arr1, arr2, …)` in FROM is a special PG-only multi-array form
    // (parsed as a regular FuncCall but transformed by PG into ROWS FROM
    // (unnest(arr1), unnest(arr2), …)). Each argument contributes one column
    // of its element type, aligned row-wise (zip with NULL-padding).
    let is_pg_unnest = (schema.is_none() || schema == Some("pg_catalog")) && name == "unnest";
    let mut cols: Vec<ScopeColumn> = if is_pg_unnest && arg_types.len() > 1 {
        unnest_multi_arg_columns(&arg_types, alias, snapshot, func_call)?
    } else {
        srf_function_columns(
            SrfCall { schema, name },
            SrfArgs {
                types: &arg_types,
                any_nullable: any_arg_nullable,
            },
            alias,
            rf,
            func_call,
            arg_ctx,
            params,
        )?
    };

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

    // User-supplied column aliases override the names above, in order.
    for (i, alias_name) in col_aliases.iter().enumerate() {
        if let Some(c) = cols.get_mut(i) {
            c.name = alias_name.clone();
        }
    }

    scope.add_virtual_table(alias, cols)?;
    Ok(())
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

/// Columns for the multi-argument `unnest(arr1, arr2, …)` FROM form: each
/// argument contributes one column of its element type.
fn unnest_multi_arg_columns(
    arg_types: &[PgTypeOid],
    alias: &str,
    snapshot: &PgCatalog,
    func_call: &protobuf::FuncCall,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let mut col_specs = Vec::with_capacity(arg_types.len());
    for (i, &type_oid) in arg_types.iter().enumerate() {
        let type_entry = snapshot.get_type(type_oid).ok_or_else(|| {
            AnalyzeError::UndefinedType(format!(
                "internal: unnest argument {} has unknown type OID {}",
                i + 1,
                type_oid.get()
            ))
        })?;
        let elem = (type_entry.typcategory == TypCategory::Array)
            .then_some(type_entry.typelem)
            .flatten()
            .ok_or_else(|| {
                // Match PG's wording: when an `unnest` arg isn't an array, PG's
                // resolver emits `function pg_catalog.unnest(<type>) does not
                // exist` (it searches for a single-arg overload of the
                // offending type). Mirror that and append our richer detail.
                let typname = type_entry.typname.clone();
                crate::functions::undefined_function_error(
                    snapshot,
                    Some("pg_catalog"),
                    "unnest",
                    format!(
                        "function pg_catalog.unnest({typname}) does not exist \
                         (unnest argument {} is not an array)",
                        i + 1,
                    ),
                    crate::error::SourceSpan::from_node_qname(func_call.location),
                )
            })?;
        // PG rewrites the multi-argument form into `ROWS FROM (unnest(a),
        // unnest(b), …)`, which pads the shorter results with NULL
        // (`ExecFunctionScan` / nodeFunctionscan.c), and the elements
        // themselves may be NULL — every column is nullable.
        col_specs.push(ScopeColumn {
            name: "unnest".to_owned(),
            type_oid: elem,
            base_not_null: false,
            table_alias: alias.to_owned(),
            typmod: None,
            collation: None,
            record_fields: None,
        });
    }
    Ok(col_specs)
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

/// The resolved name of a function called in FROM.
struct SrfCall<'a> {
    schema: Option<&'a str>,
    name: &'a str,
}

/// The inferred argument types of an SRF call plus whether any is nullable.
struct SrfArgs<'a> {
    types: &'a [PgTypeOid],
    any_nullable: bool,
}

/// Columns for an ordinary set-returning / OUT-arg / composite-returning
/// function in FROM: resolve the overload, then expand its row shape (OUT
/// args, composite attributes, or a single scalar column).
fn srf_function_columns(
    call: SrfCall<'_>,
    args: SrfArgs<'_>,
    alias: &str,
    rf: &protobuf::RangeFunction,
    func_call: &protobuf::FuncCall,
    arg_ctx: Ctx<'_>,
    params: &ParamCollector,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let snapshot = arg_ctx.snapshot;
    let SrfCall { schema, name } = call;
    let resolved = functions::resolve_function(
        snapshot,
        schema,
        name,
        args.types,
        &functions::CallNotation::of(func_call)?,
        false,
        crate::error::SourceSpan::from_node_qname(func_call.location),
    )?;

    // A strict catalog SRF with a single OUT column (`jsonb_array_elements`
    // → `value`) emits exactly its elements; wider OUT rows keep the
    // conservative per-column default.
    let single_out_not_null = (resolved.out_args.len() == 1)
        .then(|| srf_elements_nullable(&resolved, name, &func_call.args, arg_ctx, params))
        .flatten()
        .map(|nullable| !nullable);

    // Build the scope columns.
    if !resolved.out_args.is_empty() {
        Ok(resolved
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
            .collect())
    } else if let Some(typrelid) = snapshot.get_type(resolved.return_type_oid).and_then(|t| {
        (t.typtype == TypType::Composite)
            .then_some(t.typrelid)
            .flatten()
    }) {
        Ok(snapshot
            .attributes_of(typrelid)
            .iter()
            .map(|f| ScopeColumn {
                name: f.attname.clone(),
                type_oid: f.atttypid,
                base_not_null: f.attnotnull || snapshot.type_is_not_null(f.atttypid),
                typmod: snapshot.effective_typmod(f.atttypid, f.atttypmod),
                collation: f.attcollation,
                table_alias: alias.to_owned(),
                record_fields: None,
            })
            .collect())
    } else if resolved.return_type_oid == oid::RECORD && rf.coldeflist.is_empty() {
        // `RETURNS RECORD` without OUT args needs a column-definition list at
        // the call site so PG knows what shape to expose. Mirror PG's exact
        // wording so the analyzer's diagnostic is pg_sanity-aligned.
        Err(AnalyzeError::Invalid(
            "a column definition list is required for functions returning \"record\"".to_owned(),
        ))
    } else {
        // A strict catalog SRF's elements are NOT NULL unless the function
        // emits NULLs itself (`unnest` of an array with NULL elements); other
        // catalog functions follow their derived builtin nullability.
        let strict_not_null =
            match srf_elements_nullable(&resolved, name, &func_call.args, arg_ctx, params) {
                Some(nullable) => !nullable,
                None => {
                    resolved.schema == "pg_catalog"
                        && !functions::builtin_result_nullable(
                            &resolved,
                            &[args.any_nullable],
                            func_call.func_variadic,
                        )
                }
            };
        // PG names a lone scalar function's column after the alias:
        // `FROM generate_series(1, 3) AS g` exposes column `g`. With several
        // `ROWS FROM` functions each column keeps its function's name.
        let col_name = if rf.functions.len() == 1 { alias } else { name };
        Ok(vec![ScopeColumn {
            name: col_name.to_owned(),
            type_oid: resolved.return_type_oid,
            base_not_null: strict_not_null,
            table_alias: alias.to_owned(),
            typmod: None,
            collation: None,
            record_fields: None,
        }])
    }
}
