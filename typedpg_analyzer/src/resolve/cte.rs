use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// CTE
// ──────────────────────────────────────────────────────────────────────────────

/// Analyze one CTE body. `outer_sources` are the FROM entries of the
/// enclosing query levels: a CTE body is a sub-level of the query owning
/// the WITH (whose own FROM isn't transformed yet), so it may reference
/// them as outer references.
pub(crate) fn analyze_cte(
    cte: &protobuf::CommonTableExpr,
    with_recursive: bool,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_sources: &[crate::scope::TableSource],
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
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

    // `WITH RECURSIVE` — the recursive branch references the CTE by name, so
    // we have to seed the scope before analyzing it. typedpg_pg_query's AST doesn't
    // set `cterecursive` on individual CTEs without full parse analysis, so
    // we rely on the enclosing `WithClause.recursive` flag (true when the
    // user wrote `WITH RECURSIVE`) plus the UNION shape of the inner query.
    // We (1) analyze the seed arm alone to type the CTE's columns,
    // (2) register those columns in a temporary scope, (3) analyze the
    // recursive arm against that scope, (4) unify the two arms' column
    // types via `find_common_type` — matching PG's common-type resolution.
    let self_referencing = with_recursive && is_self_referencing(cte);
    if self_referencing && let node::Node::SelectStmt(sel) = cte_query {
        check_well_formed_recursion(cte, sel)?;
    }
    if self_referencing
        && let node::Node::SelectStmt(sel) = cte_query
        && let (Some(larg), Some(rarg)) = (sel.larg.as_ref(), sel.rarg.as_ref())
    {
        let (seed_cols, _) = analyze_body(larg, params, existing_ctes)?;
        let seed_cols = apply_cte_column_aliases(&cte.ctename, seed_cols, &cte.aliascolnames)?;

        // Register the CTE against its seed types so the recursive arm can
        // resolve `FROM t`.
        let mut scopes_with_self = existing_ctes.clone();
        let self_scope: Vec<ScopeColumn> = seed_cols
            .iter()
            .cloned()
            .map(|rc| ScopeColumn {
                name: rc.name,
                type_oid: rc.type_oid,
                base_not_null: !rc.nullable,
                table_alias: cte.ctename.clone(),
                typmod: rc.typmod,
                collation: rc.collation,
                record_fields: rc.record_fields,
            })
            .collect();
        scopes_with_self.insert(cte.ctename.clone(), self_scope);

        let (rec_cols, _) = analyze_body(rarg, params, &scopes_with_self)?;
        check_no_aggregates_in_recursive_term(rarg, snapshot)?;
        if seed_cols.len() != rec_cols.len() {
            return Err(AnalyzeError::Unsupported(
                "recursive CTE branches have different column counts".into(),
            ));
        }

        // PG fixes a recursive CTE's column types from the *non-recursive*
        // term alone; if common-type resolution with the recursive term
        // lands anywhere else, it errors rather than widening (SQLSTATE
        // 42804): `recursive query "r" column 1 has type integer in
        // non-recursive term but type numeric overall`.
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

        let mut unified: Vec<ScopeColumn> = seed_cols
            .into_iter()
            .zip(rec_cols)
            .map(|(s, r)| {
                let type_oid = crate::coerce::find_common_type(&[s.type_oid, r.type_oid], snapshot)
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
            .collect();
        append_search_cycle_columns(cte, &mut unified, snapshot, params)?;
        return Ok(unified);
    }

    // PG (42601): SEARCH / CYCLE need a recursive query.
    if cte.search_clause.is_some() || cte.cycle_clause.is_some() {
        return Err(crate::error::RawError::new(
            AnalyzeError::SyntaxError("WITH query is not recursive".into()),
            None,
            None,
        )
        .finalize_implicit());
    }

    match cte_query {
        node::Node::SelectStmt(sel) => {
            let (mut cols, _) = analyze_body(sel, params, existing_ctes)?;
            resolve_unknown_outputs(sel, &mut cols, params);
            let cols = apply_cte_column_aliases(&cte.ctename, cols, &cte.aliascolnames)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    typmod: rc.typmod,
                    collation: rc.collation,
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::InsertStmt(ins) => {
            let (cols, _) = analyze_insert_with_outer_ctes(ins, snapshot, params, existing_ctes)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    typmod: rc.typmod,
                    collation: rc.collation,
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::UpdateStmt(upd) => {
            let (cols, _) = analyze_update_with_outer_ctes(upd, snapshot, params, existing_ctes)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    typmod: rc.typmod,
                    collation: rc.collation,
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::DeleteStmt(del) => {
            let (cols, _) = analyze_delete_with_outer_ctes(del, snapshot, params, existing_ctes)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    typmod: rc.typmod,
                    collation: rc.collation,
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        node::Node::MergeStmt(merge) => {
            let (cols, _) = analyze_merge_with_outer_ctes(merge, snapshot, params, existing_ctes)?;
            Ok(cols
                .into_iter()
                .map(|rc| ScopeColumn {
                    name: rc.name,
                    type_oid: rc.type_oid,
                    base_not_null: !rc.nullable,
                    table_alias: cte.ctename.clone(),
                    typmod: rc.typmod,
                    collation: rc.collation,
                    record_fields: rc.record_fields,
                })
                .collect())
        }
        _ => Err(AnalyzeError::Unsupported(
            "CTE with unsupported statement type".into(),
        )),
    }
}

/// Validate a recursive CTE's `SEARCH` / `CYCLE` clauses and append the
/// columns they add, following PG's `analyzeCTE` (parse_cte.c):
///
/// - every `SEARCH … BY` / `CYCLE` column must be one of the CTE's columns;
/// - the mark and path column names must differ;
/// - `SEARCH DEPTH FIRST BY k SET ord` adds `ord record[]` (the path of
///   visited rows), `BREADTH FIRST` adds `ord record`;
/// - `CYCLE k SET mark [TO v DEFAULT d] USING path` adds `mark` typed as the
///   common type of `v` and `d` (`CYCLE types X and Y cannot be matched`
///   otherwise; the grammar defaults them to `true` / `false`) and
///   `path record[]`.
///
/// All added columns are NOT NULL.
fn append_search_cycle_columns(
    cte: &protobuf::CommonTableExpr,
    cols: &mut Vec<ScopeColumn>,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<(), AnalyzeError> {
    let syntax = |msg: String| {
        crate::error::RawError::new(AnalyzeError::SyntaxError(msg), None, None).finalize_implicit()
    };
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
    if let Some(search) = cte.search_clause.as_ref() {
        for name in expr::extract_string_fields(&search.search_col_list) {
            if !cols.iter().any(|c| c.name == name) {
                return Err(syntax(format!(
                    "search column \"{name}\" not in WITH query column list"
                )));
            }
        }
        if !search.search_seq_column.is_empty() {
            let seq_type = if search.search_breadth_first {
                oid::RECORD
            } else {
                record_array
            };
            added.push(synthetic(&search.search_seq_column, seq_type));
        }
    }
    if let Some(cycle) = cte.cycle_clause.as_ref() {
        for name in expr::extract_string_fields(&cycle.cycle_col_list) {
            if !cols.iter().any(|c| c.name == name) {
                return Err(syntax(format!(
                    "cycle column \"{name}\" not in WITH query column list"
                )));
            }
        }
        if cycle.cycle_mark_column == cycle.cycle_path_column {
            return Err(syntax(
                "cycle mark column name and cycle path column name are the same".into(),
            ));
        }
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
        added.push(synthetic(&cycle.cycle_mark_column, mark_type));
        added.push(synthetic(&cycle.cycle_path_column, record_array));
    }
    cols.extend(added);
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
        QueryLevel
    }
}

impl Drop for QueryLevel {
    fn drop(&mut self) {
        QUERY_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
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

/// Analyze a `WITH` clause on top of the CTEs already visible, following
/// PG's `transformWithClause` / `analyzeCTE`: names must be unique within
/// the clause (42712), a data-modifying CTE is only allowed on the
/// top-level statement (0A000), and each CTE is analyzed in order, seeing
/// the ones before it.
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
    let top_level = QUERY_DEPTH.with(|d| d.get()) <= 1;
    let mut cte_scopes = outer_ctes.clone();
    for cte in ctes {
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
        let cte_columns = analyze_cte(
            cte,
            with.recursive,
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
        cte_scopes.insert(cte.ctename.clone(), cte_columns);
    }
    Ok(cte_scopes)
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

/// PG's `checkWellFormedRecursionWalker` over one query: every reference to
/// the CTE `name` is counted; one outside an `Ok` context, or a second one,
/// is 42P19. With `check` off it only counts (self-reference detection).
struct RecursionWalker<'a> {
    name: &'a str,
    check: bool,
    refs: usize,
}

impl RecursionWalker<'_> {
    fn error(&self, msg: String) -> AnalyzeError {
        crate::error::RawError::new(AnalyzeError::InvalidRecursion(msg), None, None)
            .finalize_implicit()
    }

    fn reference(&mut self, ctx: RecursionContext) -> Result<(), AnalyzeError> {
        if self.check {
            if ctx != RecursionContext::Ok {
                return Err(self.error(format!(
                    "recursive reference to query \"{}\" must not appear {}",
                    self.name,
                    ctx.complaint()
                )));
            }
            if self.refs > 0 {
                return Err(self.error(format!(
                    "recursive reference to query \"{}\" must not appear more than once",
                    self.name
                )));
            }
        }
        self.refs += 1;
        Ok(())
    }

    fn select(
        &mut self,
        sel: &protobuf::SelectStmt,
        ctx: RecursionContext,
    ) -> Result<(), AnalyzeError> {
        // A WITH inside the query that redefines the name shadows it.
        if let Some(with) = &sel.with_clause {
            if with.ctes.iter().any(|n| {
                matches!(n.node.as_ref(), Some(node::Node::CommonTableExpr(c)) if c.ctename == self.name)
            }) {
                return Ok(());
            }
            for n in &with.ctes {
                if let Some(node::Node::CommonTableExpr(c)) = n.node.as_ref()
                    && let Some(node::Node::SelectStmt(s)) =
                        c.ctequery.as_deref().and_then(|q| q.node.as_ref())
                {
                    self.select(s, ctx)?;
                }
            }
        }
        match SetOperation::try_from(sel.op) {
            Ok(SetOperation::SetopNone) | Err(_) => {}
            Ok(op) => {
                let (l, r) = match op {
                    SetOperation::SetopIntersect => {
                        (RecursionContext::Intersect, RecursionContext::Intersect)
                    }
                    SetOperation::SetopExcept => (ctx, RecursionContext::Except),
                    _ => (ctx, ctx),
                };
                if let Some(larg) = &sel.larg {
                    self.select(larg, if ctx == RecursionContext::Ok { l } else { ctx })?;
                }
                if let Some(rarg) = &sel.rarg {
                    self.select(rarg, if ctx == RecursionContext::Ok { r } else { ctx })?;
                }
                return Ok(());
            }
        }
        for item in &sel.from_clause {
            self.visit_from_item(item, ctx)?;
        }
        let exprs = sel
            .target_list
            .iter()
            .chain(sel.where_clause.as_deref())
            .chain(sel.group_clause.iter())
            .chain(sel.having_clause.as_deref())
            .chain(sel.sort_clause.iter());
        for e in exprs {
            self.expr(e)?;
        }
        for row in &sel.values_lists {
            self.expr(row)?;
        }
        Ok(())
    }

    fn visit_from_item(
        &mut self,
        n: &protobuf::Node,
        ctx: RecursionContext,
    ) -> Result<(), AnalyzeError> {
        match n.node.as_ref() {
            Some(node::Node::RangeVar(rv)) => {
                if rv.schemaname.is_empty() && rv.relname == self.name {
                    self.reference(ctx)?;
                }
            }
            Some(node::Node::JoinExpr(j)) => {
                let outer = RecursionContext::OuterJoin;
                let pick = |side_nullable: bool| {
                    if side_nullable && ctx == RecursionContext::Ok {
                        outer
                    } else {
                        ctx
                    }
                };
                let (l_null, r_null) = match JoinType::try_from(j.jointype) {
                    Ok(JoinType::JoinLeft) => (false, true),
                    Ok(JoinType::JoinRight) => (true, false),
                    Ok(JoinType::JoinFull) => (true, true),
                    _ => (false, false),
                };
                if let Some(l) = j.larg.as_deref() {
                    self.visit_from_item(l, pick(l_null))?;
                }
                if let Some(r) = j.rarg.as_deref() {
                    self.visit_from_item(r, pick(r_null))?;
                }
                if let Some(q) = j.quals.as_deref() {
                    self.expr(q)?;
                }
            }
            Some(node::Node::RangeSubselect(rs)) => {
                if let Some(node::Node::SelectStmt(s)) =
                    rs.subquery.as_deref().and_then(|q| q.node.as_ref())
                {
                    self.select(s, ctx)?;
                }
            }
            Some(node::Node::RangeFunction(rf)) => {
                for f in &rf.functions {
                    self.expr(f)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn expr(&mut self, n: &protobuf::Node) -> Result<(), AnalyzeError> {
        let mut sublinks: Vec<&protobuf::SelectStmt> = Vec::new();
        visit_same_level(n, &mut |e| {
            if let Some(node::Node::SubLink(sl)) = e.node.as_ref()
                && let Some(node::Node::SelectStmt(s)) =
                    sl.subselect.as_deref().and_then(|q| q.node.as_ref())
            {
                sublinks.push(s);
            }
        });
        for s in sublinks {
            self.select(s, RecursionContext::Sublink)?;
        }
        Ok(())
    }
}

/// Whether a `WITH RECURSIVE` CTE's body references the CTE itself (PG's
/// `cterecursive`).
fn is_self_referencing(cte: &protobuf::CommonTableExpr) -> bool {
    let Some(node::Node::SelectStmt(sel)) = cte.ctequery.as_deref().and_then(|q| q.node.as_ref())
    else {
        return false;
    };
    let mut w = RecursionWalker {
        name: &cte.ctename,
        check: false,
        refs: 0,
    };
    w.select(sel, RecursionContext::Ok).is_ok() && w.refs > 0
}

/// PG's `checkWellFormedRecursion`: a self-referencing CTE must be
/// `non-recursive-term UNION [ALL] recursive-term` without ORDER BY /
/// OFFSET / LIMIT / FOR UPDATE, the non-recursive term may not reference
/// it, and the recursive term references it exactly once, outside
/// sublinks, outer-join nullable sides, INTERSECT and EXCEPT.
fn check_well_formed_recursion(
    cte: &protobuf::CommonTableExpr,
    sel: &protobuf::SelectStmt,
) -> Result<(), AnalyzeError> {
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
    if sel.op != SetOperation::SetopUnion as i32 {
        return Err(crate::error::RawError::new(
            AnalyzeError::InvalidRecursion(format!(
                "recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] \
                 recursive-term",
                cte.ctename
            )),
            None,
            None,
        )
        .finalize_implicit());
    }
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
    let mut w = RecursionWalker {
        name: &cte.ctename,
        check: true,
        refs: 0,
    };
    if let Some(larg) = &sel.larg {
        w.select(larg, RecursionContext::NonRecursiveTerm)?;
    }
    if let Some(rarg) = &sel.rarg {
        w.select(rarg, RecursionContext::Ok)?;
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
