use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// CTE
// ──────────────────────────────────────────────────────────────────────────────

/// One analyzed CTE: its columns, and the columns its SEARCH / CYCLE
/// clauses append (`addRangeTableEntryForCTE` adds them to every reference,
/// but a reference from below the WITH's own query level doesn't expand
/// them in `*`).
pub(crate) struct AnalyzedCte {
    pub columns: Vec<ScopeColumn>,
    pub search_cycle: Vec<ScopeColumn>,
}

/// Analyze one CTE body, following PG's `analyzeCTE`. `recursive` is PG's
/// `cterecursive` (the body references the CTE itself, found by
/// [`analyze_with_clause`]'s dependency walk). `outer_sources` are the FROM
/// entries of the enclosing query levels: a CTE body is a sub-level of the
/// query owning the WITH (whose own FROM isn't transformed yet), so it may
/// reference them as outer references.
pub(crate) fn analyze_cte(
    cte: &protobuf::CommonTableExpr,
    recursive: bool,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_sources: &[crate::scope::TableSource],
) -> Result<AnalyzedCte, AnalyzeError> {
    let analyze_body = |sel: &protobuf::SelectStmt,
                        params: &mut ParamCollector,
                        ctes: &HashMap<String, Vec<ScopeColumn>>| {
        analyze_select_with_ctes_and_outer(sel, snapshot, params, ctes, &[], outer_sources, &[])
    };
    let cte_query = cte
        .ctequery
        .as_ref()
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Unsupported("CTE without query".into()))?;

    let owner_depth = QUERY_DEPTH.with(|d| d.get());
    // analyzeCTE types the cycle mark column before the query, which can
    // refer to it.
    let search_cycle = search_cycle_columns(cte, snapshot, params)?;

    let to_scope = |rc: RawColumn| ScopeColumn {
        name: rc.name,
        type_oid: rc.type_oid,
        base_not_null: !rc.nullable,
        table_alias: cte.ctename.clone(),
        typmod: rc.typmod,
        collation: rc.collation,
        record_fields: rc.record_fields,
    };

    let columns: Vec<ScopeColumn> = match cte_query {
        // A recursive CTE is `non-recursive-term UNION [ALL] recursive-term`
        // (checkWellFormedRecursion made sure). transformSetOperationStmt
        // processes the body's own WITH, then the non-recursive term, whose
        // output fixes the CTE's column names and types
        // (analyzeCTETargetList), and only then the recursive term, which
        // sees the CTE with those columns.
        node::Node::SelectStmt(sel) if recursive && sel.larg.is_some() && sel.rarg.is_some() => {
            let (Some(larg), Some(rarg)) = (sel.larg.as_ref(), sel.rarg.as_ref()) else {
                unreachable!("guarded above");
            };
            // The body is a query level of its own; its WITH and the two
            // terms sit below the query owning the recursive CTE.
            let _body = QueryLevel::enter();
            let mut body_ctes = existing_ctes.clone();
            if let Some(with) = &sel.with_clause {
                body_ctes = analyze_with_clause(with, snapshot, params, &body_ctes, outer_sources)?;
            }
            check_set_op_member_locking(larg)?;
            let (mut seed_cols, _) = analyze_body(larg, params, &body_ctes)?;
            // analyzeCTETargetList: a recursive CTE exposes an `unknown`
            // column (an untyped literal or parameter) as text before the
            // recursive term is looked at.
            resolve_unknown_outputs(larg, &mut seed_cols, params, snapshot)?;
            let seed_cols = apply_cte_column_aliases(&cte.ctename, seed_cols, &cte.aliascolnames)?;

            // Register the CTE against its seed types (plus the SEARCH /
            // CYCLE columns every reference carries) so the recursive term
            // can resolve `FROM r`.
            let mut scopes_with_self = body_ctes.clone();
            register_cte(
                &mut scopes_with_self,
                &cte.ctename,
                seed_cols.iter().cloned().map(to_scope).collect(),
                search_cycle.clone(),
                owner_depth,
            );

            check_set_op_member_locking(rarg)?;
            let (rec_cols, _) = analyze_body(rarg, params, &scopes_with_self)?;
            check_no_aggregates_in_recursive_term(rarg, snapshot)?;
            if seed_cols.len() != rec_cols.len() {
                return Err(crate::pgmsg::set_op_column_count(
                    "UNION",
                    seed_cols.len(),
                    rec_cols.len(),
                )
                .finalize_implicit());
            }

            // PG fixes a recursive CTE's column types from the
            // *non-recursive* term alone; if common-type resolution with the
            // recursive term lands anywhere else, it errors rather than
            // widening (SQLSTATE 42804): `recursive query "r" column 1 has
            // type integer in non-recursive term but type numeric overall`.
            for (i, (s, r)) in seed_cols.iter().zip(rec_cols.iter()).enumerate() {
                if s.type_oid == oid::UNKNOWN || r.type_oid == oid::UNKNOWN {
                    continue;
                }
                let common = crate::coerce::find_common_type(&[s.type_oid, r.type_oid], snapshot)
                    .unwrap_or(s.type_oid);
                if common != s.type_oid {
                    let seed_ty = crate::ddl::util::format_type_for_message(snapshot, s.type_oid);
                    let overall = crate::ddl::util::format_type_for_message(snapshot, common);
                    return Err(crate::pgmsg::recursive_query_column_type(
                        &cte.ctename,
                        i + 1,
                        &seed_ty,
                        &overall,
                    )
                    .finalize_implicit());
                }
            }

            seed_cols
                .into_iter()
                .zip(rec_cols)
                .map(|(s, r)| {
                    let type_oid =
                        crate::coerce::find_common_type(&[s.type_oid, r.type_oid], snapshot)
                            .unwrap_or(s.type_oid);
                    let typmod = if s.typmod == r.typmod { s.typmod } else { None };
                    // Recursive CTE arms only keep the collation when both
                    // arms agree — otherwise PG drops it (same shape as the
                    // typmod merge above).
                    let collation = if s.collation == r.collation {
                        s.collation
                    } else {
                        None
                    };
                    ScopeColumn {
                        name: s.name,
                        type_oid,
                        // Either arm producing NULL makes the column nullable.
                        base_not_null: !(s.nullable || r.nullable),
                        typmod,
                        collation,
                        table_alias: cte.ctename.clone(),
                        record_fields: s.record_fields,
                    }
                })
                .collect()
        }
        node::Node::SelectStmt(sel) => {
            let (mut cols, _) = analyze_body(sel, params, existing_ctes)?;
            resolve_unknown_outputs(sel, &mut cols, params, snapshot)?;
            let cols = apply_cte_column_aliases(&cte.ctename, cols, &cte.aliascolnames)?;
            cols.into_iter().map(to_scope).collect()
        }
        node::Node::InsertStmt(_)
        | node::Node::UpdateStmt(_)
        | node::Node::DeleteStmt(_)
        | node::Node::MergeStmt(_) => {
            let (cols, _) = match cte_query {
                node::Node::InsertStmt(s) => {
                    analyze_insert_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                node::Node::UpdateStmt(s) => {
                    analyze_update_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                node::Node::DeleteStmt(s) => {
                    analyze_delete_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                node::Node::MergeStmt(s) => {
                    analyze_merge_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                _ => unreachable!("matched above"),
            };
            // RewriteQuery first rewrites a data-modifying WITH query, which
            // must come out as a single query.
            let target = match cte_query {
                node::Node::InsertStmt(s) => s.relation.as_ref().map(|r| (r, CmdType::CmdInsert)),
                node::Node::UpdateStmt(s) => s.relation.as_ref().map(|r| (r, CmdType::CmdUpdate)),
                node::Node::DeleteStmt(s) => s.relation.as_ref().map(|r| (r, CmdType::CmdDelete)),
                _ => None,
            };
            if let Some((rv, cmd)) = target
                && let Some(class) = snapshot.resolve_table(
                    (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str()),
                    &rv.relname,
                )
            {
                check_with_query_rules(snapshot, class.oid, cmd)?;
            }
            // analyzeCTETargetList names a data-modifying CTE's RETURNING
            // columns through the alias list too.
            let cols = apply_cte_column_aliases(&cte.ctename, cols, &cte.aliascolnames)?;
            cols.into_iter().map(to_scope).collect()
        }
        _ => {
            return Err(AnalyzeError::Unsupported(
                "CTE with unsupported statement type".into(),
            ));
        }
    };

    check_search_cycle_clauses(cte, recursive, &columns)?;
    Ok(AnalyzedCte {
        columns,
        search_cycle,
    })
}

/// The columns a CTE's SEARCH / CYCLE clauses add, typed as
/// `addRangeTableEntryForCTE` types them:
///
/// - `SEARCH DEPTH FIRST BY k SET ord` adds `ord record[]` (the path of
///   visited rows), `BREADTH FIRST` adds `ord record`;
/// - `CYCLE k SET mark [TO v DEFAULT d] USING path` adds `mark` typed as the
///   common type of `v` and `d` (`CYCLE types X and Y cannot be matched`
///   otherwise; the grammar defaults them to `true` / `false`) and
///   `path record[]`.
///
/// All added columns are NOT NULL.
fn search_cycle_columns(
    cte: &protobuf::CommonTableExpr,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let synthetic = |name: &str, type_oid: PgTypeOid| ScopeColumn {
        name: name.to_owned(),
        type_oid,
        base_not_null: true,
        table_alias: cte.ctename.clone(),
        typmod: None,
        collation: None,
        record_fields: None,
    };
    let record_array = snapshot.array_type_of(oid::RECORD).unwrap_or(oid::UNKNOWN);
    let mut added: Vec<ScopeColumn> = Vec::new();
    if let Some(search) = cte.search_clause.as_ref()
        && !search.search_seq_column.is_empty()
    {
        let seq_type = if search.search_breadth_first {
            oid::RECORD
        } else {
            record_array
        };
        added.push(synthetic(&search.search_seq_column, seq_type));
    }
    if let Some(cycle) = cte.cycle_clause.as_ref() {
        let scope = Scope::default();
        let null_ctx = NullabilityContext::default();
        let ctx = expr::Ctx::new(&scope, &null_ctx, snapshot);
        let values: Vec<&protobuf::Node> = [
            cycle.cycle_mark_value.as_deref(),
            cycle.cycle_mark_default.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut types = Vec::with_capacity(values.len());
        for v in &values {
            types.push(expr::infer_expr(v, ctx, params, TypeGoal::NONE)?.type_oid);
        }
        let mark_type = if types.is_empty() {
            oid::BOOL
        } else if types.iter().all(|&t| t == oid::UNKNOWN) {
            // select_common_type resolves all-unknown inputs to text.
            oid::TEXT
        } else {
            let concrete: Vec<PgTypeOid> = types
                .iter()
                .copied()
                .filter(|&t| t != oid::UNKNOWN)
                .collect();
            crate::coerce::find_common_type(&concrete, snapshot).ok_or_else(|| {
                let a = crate::ddl::util::format_type_for_message(snapshot, concrete[0]);
                let b = crate::ddl::util::format_type_for_message(
                    snapshot,
                    *concrete.last().unwrap_or(&concrete[0]),
                );
                crate::pgmsg::types_cannot_be_matched("CYCLE", &a, &b, "", None).finalize_implicit()
            })?
        };
        // Coercing the values to the mark type validates literal content
        // (`TO 1 DEFAULT 'x'` → invalid input syntax for type integer).
        for v in values {
            expr::coerce_unknown_to(v, ctx, params, mark_type)?;
        }
        // analyzeCTE compares cycle marks with the type's equality operator.
        crate::clause::check_key_operators(
            snapshot,
            mark_type,
            crate::clause::KeyUse::Group,
            None,
        )?;
        added.push(synthetic(&cycle.cycle_mark_column, mark_type));
        added.push(synthetic(&cycle.cycle_path_column, record_array));
    }
    Ok(added)
}

/// analyzeCTE's checks of the SEARCH / CYCLE clauses, run once the query is
/// analyzed: the CTE must be recursive with plain SELECTs on both sides of
/// its UNION, the listed columns must be distinct CTE columns, and the
/// added column names must be new and distinct.
fn check_search_cycle_clauses(
    cte: &protobuf::CommonTableExpr,
    recursive: bool,
    columns: &[ScopeColumn],
) -> Result<(), AnalyzeError> {
    let search = cte.search_clause.as_ref();
    let cycle = cte.cycle_clause.as_ref();
    if search.is_none() && cycle.is_none() {
        return Ok(());
    }
    let syntax = |msg: String| {
        crate::error::RawError::new(AnalyzeError::SyntaxError(msg), None, None).finalize_implicit()
    };
    let duplicate = |msg: String| {
        crate::error::RawError::new(AnalyzeError::DuplicateColumn(msg), None, None)
            .finalize_implicit()
    };
    if !recursive {
        return Err(syntax("WITH query is not recursive".into()));
    }
    if let Some(node::Node::SelectStmt(sel)) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref())
    {
        let is_leaf = |s: &Option<Box<protobuf::SelectStmt>>| {
            s.as_ref()
                .is_none_or(|s| s.op == SetOperation::SetopNone as i32)
        };
        if !is_leaf(&sel.larg) {
            return Err(crate::error::RawError::new(
                AnalyzeError::FeatureNotSupported(
                    "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT"
                        .into(),
                ),
                None,
                None,
            )
            .finalize_implicit());
        }
        if !is_leaf(&sel.rarg) {
            return Err(syntax(
                "with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT"
                    .into(),
            ));
        }
    }
    let is_column = |name: &str| columns.iter().any(|c| c.name == name);
    if let Some(search) = search {
        let mut seen: Vec<String> = Vec::new();
        for name in expr::extract_string_fields(&search.search_col_list) {
            if !is_column(&name) {
                return Err(syntax(format!(
                    "search column \"{name}\" not in WITH query column list"
                )));
            }
            if seen.contains(&name) {
                return Err(duplicate(format!(
                    "search column \"{name}\" specified more than once"
                )));
            }
            seen.push(name);
        }
        if is_column(&search.search_seq_column) {
            return Err(syntax(format!(
                "search sequence column name \"{}\" already used in WITH query column list",
                search.search_seq_column
            )));
        }
    }
    if let Some(cycle) = cycle {
        let mut seen: Vec<String> = Vec::new();
        for name in expr::extract_string_fields(&cycle.cycle_col_list) {
            if !is_column(&name) {
                return Err(syntax(format!(
                    "cycle column \"{name}\" not in WITH query column list"
                )));
            }
            if seen.contains(&name) {
                return Err(duplicate(format!(
                    "cycle column \"{name}\" specified more than once"
                )));
            }
            seen.push(name);
        }
        if is_column(&cycle.cycle_mark_column) {
            return Err(syntax(format!(
                "cycle mark column name \"{}\" already used in WITH query column list",
                cycle.cycle_mark_column
            )));
        }
        if is_column(&cycle.cycle_path_column) {
            return Err(syntax(format!(
                "cycle path column name \"{}\" already used in WITH query column list",
                cycle.cycle_path_column
            )));
        }
        if cycle.cycle_mark_column == cycle.cycle_path_column {
            return Err(syntax(
                "cycle mark column name and cycle path column name are the same".into(),
            ));
        }
    }
    if let (Some(search), Some(cycle)) = (search, cycle) {
        if search.search_seq_column == cycle.cycle_mark_column {
            return Err(syntax(
                "search sequence column name and cycle mark column name are the same".into(),
            ));
        }
        if search.search_seq_column == cycle.cycle_path_column {
            return Err(syntax(
                "search sequence column name and cycle path column name are the same".into(),
            ));
        }
    }
    Ok(())
}

/// Rename `cols` using the `aliascolnames` from `WITH name(col1, col2) AS …`
/// if present. PG uses positional matching; if the CTE has fewer aliases
/// than columns, the trailing columns keep their inner names, and more
/// aliases than columns is 42P10 (`analyzeCTETargetList`).
fn apply_cte_column_aliases(
    cte_name: &str,
    cols: Vec<RawColumn>,
    aliases: &[protobuf::Node],
) -> Result<Vec<RawColumn>, AnalyzeError> {
    let names = expr::extract_string_fields(aliases);
    if names.len() > cols.len() {
        return Err(crate::error::RawError::new(
            AnalyzeError::InvalidColumnReference(format!(
                "WITH query \"{cte_name}\" has {} columns available but {} columns specified",
                cols.len(),
                names.len()
            )),
            None,
            None,
        )
        .finalize_implicit());
    }
    Ok(cols
        .into_iter()
        .enumerate()
        .map(|(i, c)| RawColumn {
            name: names.get(i).cloned().unwrap_or(c.name),
            ..c
        })
        .collect())
}

// ──────────────────────────────────────────────────────────────────────────────
// WITH-clause rules
// ──────────────────────────────────────────────────────────────────────────────

thread_local! {
    /// Nesting depth of the query being analyzed: 1 for the statement
    /// itself, more inside subqueries, sublinks, set-operation arms and CTE
    /// bodies. PG only accepts a data-modifying CTE at depth 1.
    static QUERY_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII marker for one query level (see [`QUERY_DEPTH`]).
pub(crate) struct QueryLevel;

impl QueryLevel {
    pub(crate) fn enter() -> Self {
        QUERY_DEPTH.with(|d| d.set(d.get() + 1));
        crate::grouping::enter_query_level();
        QueryLevel
    }
}

impl Drop for QueryLevel {
    fn drop(&mut self) {
        QUERY_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        crate::grouping::leave_query_level();
    }
}

/// The CTE-map key marking `name` as a data-modifying CTE without
/// RETURNING: it may be defined but not referenced.
fn no_returning_marker(name: &str) -> String {
    format!("\u{1}no-returning:{name}")
}

/// PG (`addRangeTableEntryForCTE`, 0A000): referencing a data-modifying CTE
/// that has no RETURNING clause.
pub(crate) fn check_cte_reference(
    ctes: &HashMap<String, Vec<ScopeColumn>>,
    name: &str,
) -> Result<(), AnalyzeError> {
    if ctes.contains_key(&no_returning_marker(name)) {
        return Err(crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(format!(
                "WITH query \"{name}\" does not have a RETURNING clause"
            )),
            None,
            None,
        )
        .finalize_implicit());
    }
    Ok(())
}

/// The CTE-map key holding the SEARCH / CYCLE columns of CTE `name`.
fn search_cycle_marker(name: &str) -> String {
    format!("\u{1}search-cycle:{name}")
}

/// The CTE-map key recording that CTE `name` belongs to the query level at
/// nesting depth `depth` (see [`QUERY_DEPTH`]).
fn level_marker(depth: u32, name: &str) -> String {
    format!("\u{1}cte-level:{depth}:{name}")
}

/// Put CTE `name` into `ctes` as defined by the query level at `depth`,
/// replacing whatever an enclosing level called that.
fn register_cte(
    ctes: &mut HashMap<String, Vec<ScopeColumn>>,
    name: &str,
    columns: Vec<ScopeColumn>,
    search_cycle: Vec<ScopeColumn>,
    depth: u32,
) {
    ctes.retain(|k, _| {
        !k.strip_prefix("\u{1}cte-level:")
            .and_then(|rest| rest.split_once(':'))
            .is_some_and(|(_, n)| n == name)
    });
    ctes.remove(&search_cycle_marker(name));
    if !search_cycle.is_empty() {
        ctes.insert(search_cycle_marker(name), search_cycle);
        ctes.insert(level_marker(depth, name), Vec::new());
    }
    ctes.insert(name.to_owned(), columns);
}

/// The columns a reference to CTE `name` exposes, as
/// `addRangeTableEntryForCTE` builds them: the CTE's own columns (which a
/// FROM alias list may rename), then its SEARCH / CYCLE columns and whether
/// `*` expands those — only when the reference sits on the query level
/// that owns the WITH (`ctelevelsup == 0`).
pub(crate) fn cte_reference_columns(
    ctes: &HashMap<String, Vec<ScopeColumn>>,
    name: &str,
) -> Option<(Vec<ScopeColumn>, Vec<ScopeColumn>, bool)> {
    let columns = ctes.get(name)?.clone();
    let search_cycle = ctes
        .get(&search_cycle_marker(name))
        .cloned()
        .unwrap_or_default();
    let depth = QUERY_DEPTH.with(|d| d.get());
    let same_level = ctes.contains_key(&level_marker(depth, name));
    Some((columns, search_cycle, same_level))
}

/// Analyze a `WITH` clause on top of the CTEs already visible, following
/// PG's `transformWithClause` / `analyzeCTE`: names must be unique within
/// the clause (42712), a data-modifying CTE is only allowed on the
/// top-level statement (0A000). Without RECURSIVE each CTE is analyzed in
/// order, seeing the ones before it; with RECURSIVE every CTE sees every
/// other, so they are analyzed in dependency order (mutual recursion is
/// 0A000) after `checkWellFormedRecursion` validated the self-referencing
/// ones.
///
/// `outer_sources` are the enclosing levels' FROM entries the CTE bodies
/// may reference (see [`analyze_cte`]).
pub(crate) fn analyze_with_clause(
    with: &protobuf::WithClause,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_sources: &[crate::scope::TableSource],
) -> Result<HashMap<String, Vec<ScopeColumn>>, AnalyzeError> {
    let ctes: Vec<&protobuf::CommonTableExpr> = with
        .ctes
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::CommonTableExpr(cte) => Some(cte.as_ref()),
            _ => None,
        })
        .collect();
    for (i, cte) in ctes.iter().enumerate() {
        if ctes[..i].iter().any(|c| c.ctename == cte.ctename) {
            return Err(crate::error::RawError::new(
                AnalyzeError::DuplicateAlias(format!(
                    "WITH query name \"{}\" specified more than once",
                    cte.ctename
                )),
                None,
                None,
            )
            .finalize_implicit());
        }
    }

    // transformWithClause: for WITH RECURSIVE, find each item's references
    // to the others (and to itself), sort the items so none comes before
    // one it depends on, and check the self-referencing ones' shape.
    let mut order: Vec<usize> = (0..ctes.len()).collect();
    let mut recursive = vec![false; ctes.len()];
    if with.recursive {
        let names: Vec<&str> = ctes.iter().map(|c| c.ctename.as_str()).collect();
        let mut depends_on: Vec<std::collections::BTreeSet<usize>> =
            vec![Default::default(); ctes.len()];
        for (i, cte) in ctes.iter().enumerate() {
            let mut walker = CteRefWalker::new(&names);
            if let Some(q) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref()) {
                walker.walk(q.to_ref())?;
            }
            for j in walker.found {
                if j == i {
                    recursive[i] = true;
                } else {
                    depends_on[i].insert(j);
                }
            }
        }
        order = topological_sort(depends_on)?;
        for &i in &order {
            if recursive[i] {
                check_well_formed_recursion(ctes[i])?;
            }
        }
    }

    let top_level = QUERY_DEPTH.with(|d| d.get()) <= 1;
    let depth = QUERY_DEPTH.with(|d| d.get());
    let mut cte_scopes = outer_ctes.clone();
    for i in order {
        let cte = ctes[i];
        let returning = match cte.ctequery.as_deref().and_then(|q| q.node.as_ref()) {
            Some(node::Node::InsertStmt(s)) => {
                Some(!crate::resolve::returning_exprs(&s.returning_clause).is_empty())
            }
            Some(node::Node::UpdateStmt(s)) => {
                Some(!crate::resolve::returning_exprs(&s.returning_clause).is_empty())
            }
            Some(node::Node::DeleteStmt(s)) => {
                Some(!crate::resolve::returning_exprs(&s.returning_clause).is_empty())
            }
            Some(node::Node::MergeStmt(s)) => {
                Some(!crate::resolve::returning_exprs(&s.returning_clause).is_empty())
            }
            _ => None,
        };
        if returning.is_some() && !top_level {
            return Err(crate::error::RawError::new(
                AnalyzeError::FeatureNotSupported(
                    "WITH clause containing a data-modifying statement must be at the top level"
                        .into(),
                ),
                None,
                None,
            )
            .finalize_implicit());
        }
        let analyzed = analyze_cte(
            cte,
            recursive[i],
            snapshot,
            params,
            &cte_scopes,
            outer_sources,
        )?;
        let marker = no_returning_marker(&cte.ctename);
        if returning == Some(false) {
            cte_scopes.insert(marker, Vec::new());
        } else {
            cte_scopes.remove(&marker);
        }
        register_cte(
            &mut cte_scopes,
            &cte.ctename,
            analyzed.columns,
            analyzed.search_cycle,
            depth,
        );
    }
    Ok(cte_scopes)
}

/// PG's `TopologicalSort` over the WITH RECURSIVE items: repeatedly take the
/// first remaining item without unresolved dependencies; if none is left,
/// the items depend on each other in a cycle.
fn topological_sort(
    mut depends_on: Vec<std::collections::BTreeSet<usize>>,
) -> Result<Vec<usize>, AnalyzeError> {
    let mut items: Vec<usize> = (0..depends_on.len()).collect();
    for i in 0..items.len() {
        let Some(j) = (i..items.len()).find(|&j| depends_on[items[j]].is_empty()) else {
            return Err(crate::error::RawError::new(
                AnalyzeError::FeatureNotSupported(
                    "mutual recursion between WITH items is not implemented".into(),
                ),
                None,
                None,
            )
            .finalize_implicit());
        };
        items.swap(i, j);
        let done = items[i];
        for &k in &items[i + 1..] {
            depends_on[k].remove(&done);
        }
    }
    Ok(items)
}

/// PG's `makeDependencyGraphWalker`: collects the references (unqualified
/// relation names) a WITH RECURSIVE item makes to the items of its WITH
/// clause — `found` holds their indexes — skipping names an inner WITH
/// redefines at that point.
struct CteRefWalker<'a> {
    names: &'a [&'a str],
    innerwiths: Vec<Vec<String>>,
    found: std::collections::BTreeSet<usize>,
}

impl<'a> CteRefWalker<'a> {
    fn new(names: &'a [&'a str]) -> Self {
        CteRefWalker {
            names,
            innerwiths: Vec::new(),
            found: Default::default(),
        }
    }

    fn walk(&mut self, n: typedpg_pg_query::NodeRef<'_>) -> Result<(), AnalyzeError> {
        use typedpg_pg_query::NodeRef;
        let with = match n {
            NodeRef::RangeVar(rv) => {
                if rv.schemaname.is_empty()
                    && !captured(&self.innerwiths, &rv.relname)
                    && let Some(i) = self.names.iter().position(|&c| c == rv.relname)
                {
                    self.found.insert(i);
                }
                return Ok(());
            }
            NodeRef::WithClause(_) => return Ok(()),
            NodeRef::SelectStmt(s) => s.with_clause.as_ref(),
            NodeRef::InsertStmt(s) => s.with_clause.as_ref(),
            NodeRef::UpdateStmt(s) => s.with_clause.as_ref(),
            NodeRef::DeleteStmt(s) => s.with_clause.as_ref(),
            NodeRef::MergeStmt(s) => s.with_clause.as_ref(),
            _ => None,
        };
        match with {
            // WalkInnerWith: the WITH's names are visible to its own items
            // (all of them with RECURSIVE, the earlier ones otherwise) and to
            // the statement.
            Some(w) => {
                let cte_names = inner_cte_names(w);
                self.innerwiths.push(if w.recursive {
                    cte_names.clone()
                } else {
                    Vec::new()
                });
                for (cte, name) in inner_ctes(w).zip(cte_names) {
                    if let Some(q) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref()) {
                        self.walk(q.to_ref())?;
                    }
                    if !w.recursive
                        && let Some(top) = self.innerwiths.last_mut()
                    {
                        top.push(name);
                    }
                }
                for c in n.children() {
                    self.walk(c)?;
                }
                self.innerwiths.pop();
            }
            None => {
                for c in n.children() {
                    self.walk(c)?;
                }
            }
        }
        Ok(())
    }
}

/// The CTEs of a WITH clause.
fn inner_ctes(w: &protobuf::WithClause) -> impl Iterator<Item = &protobuf::CommonTableExpr> {
    w.ctes.iter().filter_map(|n| match n.node.as_ref()? {
        node::Node::CommonTableExpr(c) => Some(c.as_ref()),
        _ => None,
    })
}

fn inner_cte_names(w: &protobuf::WithClause) -> Vec<String> {
    inner_ctes(w).map(|c| c.ctename.clone()).collect()
}

/// Whether an inner WITH in scope defines `name`.
fn captured(innerwiths: &[Vec<String>], name: &str) -> bool {
    innerwiths.iter().any(|w| w.iter().any(|n| n == name))
}

/// Where a recursive self-reference sits (PG's `RecursionContext`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecursionContext {
    Ok,
    NonRecursiveTerm,
    Sublink,
    OuterJoin,
    Intersect,
    Except,
}

impl RecursionContext {
    fn complaint(self) -> &'static str {
        match self {
            RecursionContext::Ok => "",
            RecursionContext::NonRecursiveTerm => "within its non-recursive term",
            RecursionContext::Sublink => "within a subquery",
            RecursionContext::OuterJoin => "within an outer join",
            RecursionContext::Intersect => "within INTERSECT",
            RecursionContext::Except => "within EXCEPT",
        }
    }
}

/// PG's `checkWellFormedRecursionWalker`: every reference to the CTE `name`
/// is counted; one outside an `Ok` context, or a second one, is 42P19.
struct RecursionWalker<'a> {
    name: &'a str,
    innerwiths: Vec<Vec<String>>,
    selfrefcount: usize,
    context: RecursionContext,
}

impl RecursionWalker<'_> {
    fn error(&self, msg: String) -> AnalyzeError {
        crate::error::RawError::new(AnalyzeError::InvalidRecursion(msg), None, None)
            .finalize_implicit()
    }

    fn walk(&mut self, n: typedpg_pg_query::NodeRef<'_>) -> Result<(), AnalyzeError> {
        use typedpg_pg_query::NodeRef;
        let save = self.context;
        match n {
            NodeRef::RangeVar(rv) => {
                if !rv.schemaname.is_empty()
                    || captured(&self.innerwiths, &rv.relname)
                    || rv.relname != self.name
                {
                    return Ok(());
                }
                if self.context != RecursionContext::Ok {
                    return Err(self.error(format!(
                        "recursive reference to query \"{}\" must not appear {}",
                        self.name,
                        self.context.complaint()
                    )));
                }
                self.selfrefcount += 1;
                if self.selfrefcount > 1 {
                    return Err(self.error(format!(
                        "recursive reference to query \"{}\" must not appear more than once",
                        self.name
                    )));
                }
            }
            NodeRef::SelectStmt(stmt) => match &stmt.with_clause {
                Some(w) => {
                    let cte_names = inner_cte_names(w);
                    self.innerwiths.push(if w.recursive {
                        cte_names.clone()
                    } else {
                        Vec::new()
                    });
                    for (cte, name) in inner_ctes(w).zip(cte_names) {
                        if let Some(q) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref()) {
                            self.walk(q.to_ref())?;
                        }
                        if !w.recursive
                            && let Some(top) = self.innerwiths.last_mut()
                        {
                            top.push(name);
                        }
                    }
                    self.select_stmt(stmt)?;
                    self.innerwiths.pop();
                }
                None => self.select_stmt(stmt)?,
            },
            NodeRef::WithClause(_) => {}
            NodeRef::JoinExpr(j) => {
                let outer = |ctx| {
                    if ctx == RecursionContext::Ok {
                        RecursionContext::OuterJoin
                    } else {
                        ctx
                    }
                };
                let side = |w: &mut Self, n: &Option<Box<protobuf::Node>>, ctx| {
                    w.context = ctx;
                    let r = match n.as_deref().and_then(|n| n.node.as_ref()) {
                        Some(n) => w.walk(n.to_ref()),
                        None => Ok(()),
                    };
                    w.context = save;
                    r
                };
                let (l, r) = match JoinType::try_from(j.jointype) {
                    Ok(JoinType::JoinLeft) => (save, outer(save)),
                    Ok(JoinType::JoinFull) => (outer(save), outer(save)),
                    Ok(JoinType::JoinRight) => (outer(save), save),
                    _ => (save, save),
                };
                side(self, &j.larg, l)?;
                side(self, &j.rarg, r)?;
                side(self, &j.quals, save)?;
            }
            NodeRef::SubLink(sl) => {
                // The subquery is independent of the outer context.
                self.context = RecursionContext::Sublink;
                if let Some(s) = sl.subselect.as_deref().and_then(|n| n.node.as_ref()) {
                    self.walk(s.to_ref())?;
                }
                self.context = save;
                if let Some(t) = sl.testexpr.as_deref().and_then(|n| n.node.as_ref()) {
                    self.walk(t.to_ref())?;
                }
            }
            _ => {
                for c in n.children() {
                    self.walk(c)?;
                }
            }
        }
        Ok(())
    }

    /// `checkWellFormedSelectStmt`: a SELECT without looking at its WITH.
    /// Only ALL makes INTERSECT unsafe, and EXCEPT is unsafe on its right
    /// side always, on its left side with ALL.
    fn select_stmt(&mut self, stmt: &protobuf::SelectStmt) -> Result<(), AnalyzeError> {
        let save = self.context;
        let op = SetOperation::try_from(stmt.op).unwrap_or(SetOperation::SetopNone);
        if save != RecursionContext::Ok
            || !matches!(op, SetOperation::SetopIntersect | SetOperation::SetopExcept)
        {
            for c in typedpg_pg_query::NodeRef::SelectStmt(stmt).children() {
                self.walk(c)?;
            }
            return Ok(());
        }
        let arg = |w: &mut Self, s: &Option<Box<protobuf::SelectStmt>>| match s {
            Some(s) => w.walk(typedpg_pg_query::NodeRef::SelectStmt(s)),
            None => Ok(()),
        };
        if stmt.all {
            self.context = if op == SetOperation::SetopIntersect {
                RecursionContext::Intersect
            } else {
                RecursionContext::Except
            };
        }
        arg(self, &stmt.larg)?;
        if op == SetOperation::SetopExcept {
            self.context = RecursionContext::Except;
        }
        arg(self, &stmt.rarg)?;
        self.context = save;
        for n in stmt
            .sort_clause
            .iter()
            .chain(stmt.limit_offset.as_deref())
            .chain(stmt.limit_count.as_deref())
            .chain(stmt.locking_clause.iter())
        {
            if let Some(n) = n.node.as_ref() {
                self.walk(n.to_ref())?;
            }
        }
        Ok(())
    }
}

/// PG's `checkWellFormedRecursion` for one self-referencing CTE: it must be
/// a `non-recursive-term UNION [ALL] recursive-term` SELECT; a WITH on it
/// may not reference it; no ORDER BY / OFFSET / LIMIT / FOR UPDATE on top;
/// the non-recursive term may not reference it; and the recursive term
/// references it exactly once, outside sublinks, outer-join nullable sides,
/// INTERSECT ALL and EXCEPT.
fn check_well_formed_recursion(cte: &protobuf::CommonTableExpr) -> Result<(), AnalyzeError> {
    let invalid = |msg: String| {
        crate::error::RawError::new(AnalyzeError::InvalidRecursion(msg), None, None)
            .finalize_implicit()
    };
    let Some(node::Node::SelectStmt(sel)) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref())
    else {
        return Err(invalid(format!(
            "recursive query \"{}\" must not contain data-modifying statements",
            cte.ctename
        )));
    };
    if sel.op != SetOperation::SetopUnion as i32 {
        return Err(invalid(format!(
            "recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] \
             recursive-term",
            cte.ctename
        )));
    }
    let mut w = RecursionWalker {
        name: &cte.ctename,
        innerwiths: Vec::new(),
        selfrefcount: 0,
        context: RecursionContext::Sublink,
    };
    if let Some(with) = &sel.with_clause {
        for c in &with.ctes {
            if let Some(c) = c.node.as_ref() {
                w.walk(c.to_ref())?;
            }
        }
    }
    let unsupported = |what: &str| {
        crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(format!(
                "{what} in a recursive query is not implemented"
            )),
            None,
            None,
        )
        .finalize_implicit()
    };
    if !sel.sort_clause.is_empty() {
        return Err(unsupported("ORDER BY"));
    }
    if sel.limit_offset.is_some() {
        return Err(unsupported("OFFSET"));
    }
    if sel.limit_count.is_some() {
        return Err(unsupported("LIMIT"));
    }
    if !sel.locking_clause.is_empty() {
        return Err(unsupported("FOR UPDATE/SHARE"));
    }
    for (arg, context) in [
        (&sel.larg, RecursionContext::NonRecursiveTerm),
        (&sel.rarg, RecursionContext::Ok),
    ] {
        w.innerwiths.clear();
        w.selfrefcount = 0;
        w.context = context;
        if let Some(s) = arg {
            w.walk(typedpg_pg_query::NodeRef::SelectStmt(s))?;
        }
    }
    Ok(())
}

/// PG (parse_agg.c, 42P19): no aggregates in the recursive term.
fn check_no_aggregates_in_recursive_term(
    rarg: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let has_agg = rarg
        .target_list
        .iter()
        .filter_map(|t| match t.node.as_ref()? {
            node::Node::ResTarget(rt) => rt.val.as_deref(),
            _ => None,
        })
        .chain(rarg.having_clause.as_deref())
        .any(|n| expr::detect_func_kinds(n, snapshot).has_aggregate);
    if has_agg {
        return Err(crate::error::RawError::new(
            AnalyzeError::InvalidRecursion(
                "aggregate functions are not allowed in a recursive query's recursive term".into(),
            ),
            None,
            None,
        )
        .finalize_implicit());
    }
    Ok(())
}
