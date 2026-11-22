use super::*;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast;

use crate::node_key::NodeKey;

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
    /// The CTE always yields a row (a SELECT whose level does, or a
    /// recursive one whose non-recursive term does).
    pub min_one_row: bool,
}

/// Analyze one CTE body, following PG's `analyzeCTE`. `recursive` is PG's
/// `cterecursive` (the body references the CTE itself, found by
/// [`analyze_with_clause`]'s dependency walk). `outer_tiers` are the
/// enclosing query levels, nearest first: a CTE body is a sub-level of the
/// query owning the WITH (whose own FROM isn't transformed yet), so it may
/// reference them as outer references.
/// Every nullability flag of `columns` (their own, their array elements',
/// their record fields') and their refinements, to tell when a recursive
/// CTE's have settled.
fn nullability_signature(columns: &[ScopeColumn]) -> Signature {
    fn shape(out: &mut Signature, s: Option<&crate::expr::RecordShape>) {
        for f in s.into_iter().flat_map(|s| s.iter()) {
            out.push((
                Some(f.ty.nullable),
                f.ty.elem_nullable,
                f.ty.refine.clone(),
                f.ty.elem_refine.clone(),
            ));
            shape(out, f.ty.record_fields.as_ref());
        }
    }
    let mut out = Vec::new();
    for c in columns {
        out.push((
            Some(c.base_not_null),
            c.elem_nullable,
            c.refine.clone(),
            c.elem_refine.clone(),
        ));
        shape(&mut out, c.record_fields.as_ref());
    }
    out
}

/// See [`nullability_signature`].
type Signature = Vec<(
    Option<bool>,
    Option<bool>,
    crate::refine::Refinement,
    crate::refine::Refinement,
)>;

pub(crate) fn analyze_cte(
    cte: &ast::CommonTableExpr,
    recursive: bool,
    inlined: Option<bool>,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_tiers: &[Vec<crate::scope::TableSource>],
) -> Result<AnalyzedCte, AnalyzeError> {
    let analyze_body = |sel: &ast::SelectStmt,
                        params: &mut ParamCollector,
                        ctes: &HashMap<String, Vec<ScopeColumn>>| {
        analyze_select_with_ctes_and_outer(sel, snapshot, params, ctes, outer_tiers, &[])
    };
    let cte_query = cte
        .ctequery
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("CTE without query".into()))?;

    let owner_depth = QUERY_DEPTH.with(|d| d.get());
    // analyzeCTE types the cycle mark column before the query, which can
    // refer to it.
    let search_cycle = search_cycle_columns(cte, snapshot, params)?;

    // A CTE the planner inlines and then pulls up has its outputs replace
    // the references to them, as planned (see `Planned`); otherwise a
    // reference stays a Var, collated as parsed.
    let pulled_up = inlined == Some(true)
        && matches!(cte_query, ast::Node::SelectStmt(sel)
            if super::deferred::plan_of(sel, snapshot) == super::deferred::Plan::PulledUp);
    let planned_in = |p: crate::expr::Planned| {
        (p == crate::expr::Planned::AsParsed || inlined == Some(false))
            .then_some(crate::expr::Planned::AsParsed)
    };
    // An inlined CTE is a subquery: a trivial scan over it may be removed
    // (`removable`: its level summary says it is, when planned on its own).
    let plan = match (inlined, cte_query) {
        (Some(true), ast::Node::SelectStmt(sel)) => Some(super::deferred::plan_of(sel, snapshot)),
        (Some(false), _) => None,
        _ => Some(super::deferred::Plan::Uncertain),
    };
    let to_scope = |rc: RawColumn, removable: bool| ScopeColumn {
        deferred: crate::scope::Deferred {
            planned_top: plan
                .and_then(|plan| super::from::subquery_planned_top(plan, &rc, removable)),
            subquery_scan: plan == Some(super::deferred::Plan::Separate),
            planned: planned_in(rc.planned).unwrap_or_default(),
            // An inlined CTE is a subquery the planner may pull up.
            substitution: if inlined == Some(false) {
                crate::scope::Substitution::Never
            } else {
                crate::scope::Substitution::Maybe
            },
            ..Default::default()
        },
        name: rc.name,
        type_oid: rc.type_oid,
        base_not_null: !rc.nullable,
        table_alias: cte.ctename.clone(),
        typmod: rc.typmod,
        collation: rc.collation,
        no_collation: rc.no_collation.is_some(),
        planned_unknown: planned_in(rc.planned).is_none(),
        record_fields: rc.record_fields,
        elem_nullable: rc.elem_nullable,
        elem_refine: rc.elem_refine.clone(),
        refine: rc.refine.clone(),
        origin: rc.origin,
        output: None,
    };

    let mut min_one_row = false;
    let mut columns: Vec<ScopeColumn> = match cte_query {
        // A recursive CTE is `non-recursive-term UNION [ALL] recursive-term`
        // (checkWellFormedRecursion made sure). transformSetOperationStmt
        // processes the body's own WITH, then the non-recursive term, whose
        // output fixes the CTE's column names and types
        // (analyzeCTETargetList), and only then the recursive term, which
        // sees the CTE with those columns.
        ast::Node::SelectStmt(sel) if recursive && sel.larg.is_some() && sel.rarg.is_some() => {
            let (Some(larg), Some(rarg)) = (sel.larg.as_ref(), sel.rarg.as_ref()) else {
                unreachable!("guarded above");
            };
            // The body is a query level of its own; its WITH and the two
            // terms sit below the query owning the recursive CTE.
            let _body = QueryLevel::enter();
            let mut body_ctes = existing_ctes.clone();
            if let Some(with) = &sel.with_clause {
                body_ctes = analyze_with_clause(with, snapshot, params, &body_ctes, outer_tiers)?;
            }
            check_set_op_member_locking(larg)?;
            // Both terms are set-operation arms: they keep their unknowns.
            let (mut seed_cols, _) =
                keeping_unknown_outputs(|| analyze_body(larg, params, &body_ctes))?;
            // UNION [ALL] keeps a row of the non-recursive term.
            min_one_row = take_level_summary().min_one_row;
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
                seed_cols
                    .iter()
                    .cloned()
                    .map(|c| to_scope(c, false))
                    .collect(),
                search_cycle.clone(),
                owner_depth,
            );

            check_set_op_member_locking(rarg)?;
            let (rec_cols, _) =
                keeping_unknown_outputs(|| analyze_body(rarg, params, &scopes_with_self))?;
            check_no_aggregates_in_recursive_term(rarg, snapshot)?;
            if seed_cols.len() != rec_cols.len() {
                return Err(crate::pgmsg::set_op_column_count(
                    "UNION",
                    seed_cols.len(),
                    rec_cols.len(),
                    super::set_ops::branch_column_span(rarg, 0),
                )
                .finalize_implicit());
            }

            // PG fixes a recursive CTE's column types from the
            // *non-recursive* term alone. transformSetOperationTree first
            // resolves each column over both terms — select_common_type
            // failing when the types don't meet at all, then
            // select_common_collation (an explicit collation wins, two
            // different explicit ones are an error, two different implicit
            // ones too unless it is UNION ALL, which leaves no collation);
            // then analyzeCTE requires that result to be the non-recursive
            // term's own type, typmod and collation, erroring rather than
            // widening: `recursive query "r" column 1 has type integer in
            // non-recursive term but type numeric overall` (42804), `… has
            // collation "C" in non-recursive term but collation "POSIX"
            // overall` (42P21).
            let collation_name = |c: Option<crate::oid::PgCollationOid>| {
                // PG prints a missing collation's NULL name as glibc does.
                c.and_then(|c| snapshot.pg_collation.get(&c))
                    .map_or_else(|| "(null)".to_string(), |c| c.collname.clone())
            };
            let union_all = sel.all;
            let mut overall = Vec::with_capacity(seed_cols.len());
            for (i, (s, r)) in seed_cols.iter().zip(rec_cols.iter()).enumerate() {
                if s.type_oid == oid::UNKNOWN || r.type_oid == oid::UNKNOWN {
                    overall.push(None);
                    continue;
                }
                let Some(common) =
                    crate::coerce::find_common_type(&[s.type_oid, r.type_oid], snapshot)
                else {
                    let a = crate::ddl::util::format_type_for_message(snapshot, s.type_oid);
                    let b = crate::ddl::util::format_type_for_message(snapshot, r.type_oid);
                    return Err(crate::pgmsg::types_cannot_be_matched(
                        "UNION",
                        &a,
                        &b,
                        &format!(" (column `{}`)", s.name),
                        Some(format!(
                            "cast the recursive term's column to the non-recursive term's type, e.g. `::{a}`"
                        )),
                        super::set_ops::branch_column_span(rarg, i),
                    )
                    .finalize_implicit());
                };
                let typmod = if s.type_oid == r.type_oid && s.typmod == r.typmod {
                    s.typmod
                } else {
                    None
                };
                let merged = super::set_ops::set_op_column_collation(
                    (larg, s),
                    (rarg, r),
                    i,
                    common,
                    union_all,
                    snapshot,
                )?;
                overall.push(Some((common, typmod, merged.collation())));
            }
            for (i, (s, o)) in seed_cols.iter().zip(overall).enumerate() {
                let Some((common, typmod, collation)) = o else {
                    continue;
                };
                if common != s.type_oid || typmod != s.typmod {
                    let format = crate::ddl::util::format_type_with_typmod;
                    return Err(crate::pgmsg::recursive_query_column_type(
                        &cte.ctename,
                        i + 1,
                        &format(snapshot, s.type_oid, s.typmod),
                        &format(snapshot, common, typmod),
                    )
                    .finalize_implicit());
                }
                // A collatable column without one recorded has the type's;
                // one without a collation (a conflict included) is named
                // `(null)`.
                let seed_collation = crate::clause::CollationState::column(
                    snapshot,
                    s.type_oid,
                    s.collation,
                    false,
                    s.no_collation.map(|_| crate::expr::NoCollation::Unset),
                )
                .collation();
                if collation != seed_collation {
                    return Err(crate::pgmsg::recursive_query_column_collation(
                        &cte.ctename,
                        i + 1,
                        &collation_name(seed_collation),
                        &collation_name(collation),
                    )
                    .finalize_implicit());
                }
            }

            let merge = |rec_cols: &[RawColumn]| -> Vec<ScopeColumn> {
                seed_cols
                    .iter()
                    .zip(rec_cols)
                    .map(|(s, r)| {
                        let type_oid =
                            crate::coerce::find_common_type(&[s.type_oid, r.type_oid], snapshot)
                                .unwrap_or(s.type_oid);
                        let typmod = if s.typmod == r.typmod { s.typmod } else { None };
                        // The column's collation is the non-recursive
                        // term's (checked above to be the overall one).
                        let collation = s.collation;
                        ScopeColumn {
                            deferred: Default::default(),
                            name: s.name.clone(),
                            type_oid,
                            // Either arm producing NULL makes the column
                            // nullable.
                            base_not_null: !(s.nullable || r.nullable),
                            typmod,
                            collation,
                            no_collation: s.no_collation.is_some(),
                            planned_unknown: false,
                            table_alias: cte.ctename.clone(),
                            record_fields: crate::expr::merge_set_op_shapes(
                                s.record_fields.as_ref(),
                                r.record_fields.as_ref(),
                                true,
                            ),
                            elem_nullable: crate::expr::merge_elem_nullable([
                                s.elem_nullable,
                                r.elem_nullable,
                            ]),
                            refine: crate::refine::Refinement::either([
                                &s.refine.converted(s.type_oid, type_oid, snapshot),
                                &r.refine.converted(r.type_oid, type_oid, snapshot),
                            ]),
                            elem_refine: crate::refine::Refinement::either([
                                &crate::expr::elem_refine_converted(
                                    &s.elem_refine,
                                    s.type_oid,
                                    type_oid,
                                    snapshot,
                                ),
                                &crate::expr::elem_refine_converted(
                                    &r.elem_refine,
                                    r.type_oid,
                                    type_oid,
                                    snapshot,
                                ),
                            ]),
                            origin: None,
                            output: None,
                        }
                    })
                    .collect()
            };

            // The recursive term above read the CTE as the seed's rows, but
            // it also reads its own output: a NULL it makes in one column
            // flows into the next iteration (`SELECT b, NULL FROM r` turns
            // `a` NULL on the second step). Analyze it again over the
            // columns as they stand until their nullability settles — it
            // only turns on, so this ends; the bound is a safety net, past
            // which nothing is assumed NOT NULL. A refinement can grow step
            // after step (`n + 1` widens a range by one each time): one
            // still changing after a few steps is dropped, so it settles.
            let mut columns = merge(&rec_cols);
            let mut widened = vec![false; columns.len()];
            let mut settled = false;
            for step in 0..32 {
                let mut scopes = body_ctes.clone();
                register_cte(
                    &mut scopes,
                    &cte.ctename,
                    columns.clone(),
                    search_cycle.clone(),
                    owner_depth,
                );
                let (rec_cols, _) =
                    keeping_unknown_outputs(|| analyze_body(rarg, params, &scopes))?;
                let mut next = merge(&rec_cols);
                for ((n, c), widened) in next.iter_mut().zip(&columns).zip(&mut widened) {
                    *widened |=
                        step >= 3 && (n.refine != c.refine || n.elem_refine != c.elem_refine);
                    if *widened {
                        n.refine = crate::refine::Refinement::NONE;
                        n.elem_refine = crate::refine::Refinement::NONE;
                    }
                }
                if nullability_signature(&next) == nullability_signature(&columns) {
                    settled = true;
                    break;
                }
                columns = next;
            }
            if !settled {
                for c in &mut columns {
                    c.base_not_null = false;
                    c.elem_nullable = Some(true);
                    c.refine = crate::refine::Refinement::NONE;
                    c.elem_refine = crate::refine::Refinement::NONE;
                    c.record_fields = None;
                }
            }
            columns
        }
        ast::Node::SelectStmt(sel) => {
            let (mut cols, _) = analyze_body(sel, params, existing_ctes)?;
            let summary = take_level_summary();
            min_one_row = summary.min_one_row;
            resolve_unknown_outputs(sel, &mut cols, params, snapshot)?;
            let cols = apply_cte_column_aliases(&cte.ctename, cols, &cte.aliascolnames)?;
            let mut columns: Vec<ScopeColumn> = cols
                .into_iter()
                .map(|c| to_scope(c, summary.removable_scan))
                .collect();
            // Inlined and pulled up, its outputs replace the references to
            // them, like a FROM subquery's (`resolve::deferred`) — where a
            // reference is never null-extended (see `from`).
            if pulled_up {
                let mut exprs = super::deferred::output_exprs(sel).into_iter();
                for c in &mut columns {
                    c.deferred.substitution = crate::scope::Substitution::Certain;
                    c.deferred.expr = exprs.next().flatten();
                }
            }
            columns
        }
        ast::Node::InsertStmt(_)
        | ast::Node::UpdateStmt(_)
        | ast::Node::DeleteStmt(_)
        | ast::Node::MergeStmt(_) => {
            let (cols, _) = match cte_query {
                ast::Node::InsertStmt(s) => {
                    analyze_insert_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                ast::Node::UpdateStmt(s) => {
                    analyze_update_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                ast::Node::DeleteStmt(s) => {
                    analyze_delete_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                ast::Node::MergeStmt(s) => {
                    analyze_merge_with_outer_ctes(s, snapshot, params, existing_ctes)?
                }
                _ => unreachable!("matched above"),
            };
            // RewriteQuery first rewrites a data-modifying WITH query, which
            // must come out as a single query.
            let target = match cte_query {
                ast::Node::InsertStmt(s) => s.relation.as_ref().map(|r| (r, CmdType::CmdInsert)),
                ast::Node::UpdateStmt(s) => s.relation.as_ref().map(|r| (r, CmdType::CmdUpdate)),
                ast::Node::DeleteStmt(s) => s.relation.as_ref().map(|r| (r, CmdType::CmdDelete)),
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
            // The rows a data-modifying statement returns are not the
            // query's snapshot's: a row it inserts is invisible to the rest
            // of the statement, and so is a parent row inserted beside it.
            cols.into_iter()
                .map(|c| ScopeColumn {
                    origin: None,
                    ..to_scope(c, false)
                })
                .collect()
        }
        _ => {
            return Err(AnalyzeError::Unsupported(
                "CTE with unsupported statement type".into(),
            ));
        }
    };

    // An inlined CTE is planned where its references are (a subquery the
    // planner may pull up): the outputs nothing reads are dropped, and the
    // expressions of the others run.
    if inlined == Some(true)
        && !recursive
        && let Some(group) = crate::privileges::take_level_outputs()
    {
        for (i, column) in columns.iter_mut().enumerate() {
            column.output = Some(crate::privileges::OutputRef { group, index: i });
        }
    }
    check_search_cycle_clauses(cte, recursive, &columns)?;
    Ok(AnalyzedCte {
        columns,
        search_cycle,
        min_one_row,
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
    cte: &ast::CommonTableExpr,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let synthetic = |name: &str, type_oid: PgTypeOid| ScopeColumn {
        deferred: Default::default(),
        name: name.to_owned(),
        type_oid,
        base_not_null: true,
        table_alias: cte.ctename.clone(),
        typmod: None,
        collation: None,
        no_collation: false,
        planned_unknown: false,
        record_fields: None,
        elem_nullable: None,
        elem_refine: crate::refine::Refinement::NONE,
        refine: crate::refine::Refinement::NONE,
        origin: None,
        output: None,
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
        let values: Vec<Option<&ast::Node>> = [
            cycle.cycle_mark_value.as_ref(),
            cycle.cycle_mark_default.as_ref(),
        ]
        .into_iter()
        .filter(|v| v.is_some())
        .collect();
        let mut types = Vec::with_capacity(values.len());
        for v in &values {
            types.push(expr::infer_expr(*v, ctx, params, TypeGoal::NONE)?.type_oid);
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
                crate::pgmsg::types_cannot_be_matched("CYCLE", &a, &b, "", None, None)
                    .finalize_implicit()
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
    cte: &ast::CommonTableExpr,
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
    if let Some(ast::Node::SelectStmt(sel)) = cte.ctequery.as_ref() {
        let is_leaf = |s: Option<&ast::SelectStmt>| {
            s.as_ref().is_none_or(|s| s.op == SetOperation::SetopNone)
        };
        if !is_leaf(sel.larg.as_deref()) {
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
        if !is_leaf(sel.rarg.as_deref()) {
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
    aliases: &[Option<ast::Node>],
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
        .map(|(i, c)| {
            // The alias list is a name the user wrote: it may carry the
            // `!` / `?` nullability annotation.
            let name = names.get(i).cloned().unwrap_or(c.name);
            let nullable = match super::type_resolution::annotation_nullability(&name) {
                Some(nullable) => nullable,
                None => c.nullable,
            };
            RawColumn {
                name,
                nullable,
                ..c
            }
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

/// The CTE-map key marking `name` as a data-modifying CTE that returns
/// exactly one row (see [`insert_returns_one_row`]).
fn one_row_marker(name: &str) -> String {
    format!("\u{1}one-row:{name}")
}

/// Whether CTE `name` of `ctes` returns exactly one row.
pub(crate) fn cte_returns_one_row(ctes: &HashMap<String, Vec<ScopeColumn>>, name: &str) -> bool {
    ctes.contains_key(&one_row_marker(name))
}

/// Whether INSERT … RETURNING `ins` returns exactly one row whenever it
/// succeeds: it inserts one row (one VALUES row without a set-returning
/// function, or DEFAULT VALUES) into a table without rules, and no BEFORE ROW trigger can skip it (return
/// NULL) — nor ON CONFLICT DO NOTHING, nor a DO UPDATE WHERE. ON CONFLICT
/// DO UPDATE without WHERE returns the inserted row or the updated one.
fn insert_returns_one_row(ins: &ast::InsertStmt, snapshot: &PgCatalog) -> bool {
    let one_row = match ins.select_stmt.as_ref() {
        None => true,
        Some(ast::Node::SelectStmt(sel)) => {
            sel.values_lists.len() == 1
                && sel.with_clause.is_none()
                && sel.limit_count.is_none()
                && sel.limit_offset.is_none()
                // A set-returning function in the row (the INSERT's own
                // target list) makes it any number of rows, none included.
                && sel.values_lists.iter().all(|row| match row.as_ref() {
                    Some(ast::Node::List(l)) => count_srf_calls(&l.items, snapshot) == 0,
                    _ => false,
                })
        }
        _ => false,
    };
    let (update, ok_conflict) = match &ins.on_conflict_clause {
        None => (false, true),
        Some(oc) => (
            true,
            oc.action == ast::OnConflictAction::OnconflictUpdate && oc.where_clause.is_none(),
        ),
    };
    let Some(relation) = &ins.relation else {
        return false;
    };
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    let Some(table) = snapshot.resolve_table(schema, &relation.relname) else {
        return false;
    };
    let Some(wt) = WriteTarget::resolve(snapshot, table.oid) else {
        return false;
    };
    let skips = |events: i32| {
        std::iter::once(wt.base)
            .chain(super::from::inheritance_descendants(snapshot, wt.base))
            .any(|r| {
                snapshot.triggers.get(&r).is_some_and(|ts| {
                    ts.iter().any(|t| {
                        t.row
                            && t.timing & crate::ddl::triggers::TRIGGER_TYPE_BEFORE != 0
                            && t.events & events != 0
                    })
                })
            })
    };
    let mut events = crate::ddl::triggers::TRIGGER_TYPE_INSERT;
    if update {
        events |= crate::ddl::triggers::TRIGGER_TYPE_UPDATE;
    }
    one_row && ok_conflict && wt.base == table.oid && !skips(events)
}

/// The CTE-map key marking CTE `name` as always yielding a row.
fn min_one_row_marker(name: &str) -> String {
    format!("\u{1}min-one-row:{name}")
}

/// Whether CTE `name` (as `ctes` resolves it) always yields a row.
pub(crate) fn cte_min_one_row(ctes: &HashMap<String, Vec<ScopeColumn>>, name: &str) -> bool {
    ctes.contains_key(&min_one_row_marker(name)) || cte_returns_one_row(ctes, name)
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
    ctes.remove(&min_one_row_marker(name));
    ctes.remove(&one_row_marker(name));
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

thread_local! {
    /// What the statement owning each WITH clause being analyzed tells of
    /// its CTEs (see [`with_clause_owner`]).
    static WITH_OWNERS: std::cell::RefCell<Vec<(NodeKey<ast::WithClause>, Vec<OwnerFacts>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// What the statement owning a WITH clause tells of one of its CTEs.
struct OwnerFacts {
    /// The CTE's name.
    name: String,
    /// See [`owner_references`].
    references: Option<usize>,
    /// See [`owner_kept_reference`].
    kept_reference: Option<String>,
    /// Whether the statement references the CTE in live code outside the
    /// WITH (see [`owner_cte_refs`]).
    live_reference: Option<bool>,
    /// Whether the statement references the CTE anywhere outside the CTE's
    /// own body (see [`referenced_outside_self`]).
    referenced_outside_self: Option<bool>,
}

/// Run `f` (the analysis of `sel`'s WITH clause) knowing `sel` owns it:
/// how many times the statement references a CTE decides whether the
/// planner inlines it.
pub(crate) fn with_clause_owner<R>(
    sel: &ast::SelectStmt,
    snapshot: &PgCatalog,
    f: impl FnOnce() -> R,
) -> R {
    let Some(with) = sel.with_clause.as_ref() else {
        return f();
    };
    with_statement_owner(with, NodeRef::SelectStmt(sel), snapshot, f)
}

/// [`with_clause_owner`] for `owner`, any statement owning WITH clause
/// `with` (SELECT, INSERT, UPDATE, DELETE, MERGE). The target relation of
/// a data-modifying statement is never a reference to a CTE
/// (`setTargetTable` opens a table), so it isn't counted.
pub(crate) fn with_statement_owner<R>(
    with: &ast::WithClause,
    owner: NodeRef<'_>,
    snapshot: &PgCatalog,
    f: impl FnOnce() -> R,
) -> R {
    let target = match owner {
        NodeRef::InsertStmt(s) => s.relation.as_deref(),
        NodeRef::UpdateStmt(s) => s.relation.as_deref(),
        NodeRef::DeleteStmt(s) => s.relation.as_deref(),
        NodeRef::MergeStmt(s) => s.relation.as_deref(),
        _ => None,
    };
    let names: Vec<&str> = with
        .ctes
        .iter()
        .filter_map(|c| match c.as_ref() {
            Some(ast::Node::CommonTableExpr(c)) => Some(c.ctename.as_str()),
            _ => None,
        })
        .collect();
    let live = owner_cte_refs(owner, &names, snapshot);
    let facts = names
        .iter()
        .copied()
        .enumerate()
        .map(|(index, name)| {
            // `references_in` counts the target relation when it is named
            // so: it isn't a reference.
            let target_named =
                target.is_some_and(|rv| rv.schemaname.is_empty() && rv.relname == name);
            OwnerFacts {
                name: name.to_owned(),
                references: references_in(owner, statement_with(owner), name)
                    .map(|n| n - usize::from(target_named)),
                kept_reference: kept_reference(owner, name),
                live_reference: Some(live.contains(&index)),
                referenced_outside_self: Some(referenced_outside_self(
                    owner, &names, snapshot, index,
                )),
            }
        })
        .collect();
    let key = NodeKey::of(with);
    WITH_OWNERS.with(|o| o.borrow_mut().push((key, facts)));
    let out = f();
    WITH_OWNERS.with(|o| o.borrow_mut().pop());
    out
}

/// The WITH clause statement `n` owns.
fn statement_with(n: NodeRef<'_>) -> Option<&ast::WithClause> {
    match n {
        NodeRef::SelectStmt(s) => s.with_clause.as_deref(),
        NodeRef::InsertStmt(s) => s.with_clause.as_deref(),
        NodeRef::UpdateStmt(s) => s.with_clause.as_deref(),
        NodeRef::DeleteStmt(s) => s.with_clause.as_deref(),
        NodeRef::MergeStmt(s) => s.with_clause.as_deref(),
        _ => None,
    }
}

/// What the statement owning `with` tells of its CTE `name`, by `get`;
/// `None` when the owner isn't known (see [`with_clause_owner`]).
fn owner_fact<T>(
    with: &ast::WithClause,
    name: &str,
    get: impl FnOnce(&OwnerFacts) -> Option<T>,
) -> Option<T> {
    let key = NodeKey::of(with);
    WITH_OWNERS.with(|o| {
        let owners = o.borrow();
        let (_, facts) = owners.iter().rev().find(|(k, _)| *k == key)?;
        get(facts.iter().find(|f| f.name == name)?)
    })
}

/// How many references to CTE `name` the statement owning `with` has
/// (PG's `cterefcount`), as [`references_in`] counts them; `None` when the
/// owner isn't known (see [`with_clause_owner`]).
fn owner_references(with: &ast::WithClause, name: &str) -> Option<usize> {
    owner_fact(with, name, |f| f.references)
}

/// The CTEs of the WITH statement `root` owns that it references outside
/// that WITH's own CTE bodies, in live code: the plan's roots. A reference an
/// inner WITH shadows (`WITH a AS (…) SELECT * FROM (WITH a AS (…) SELECT …
/// FROM a) s`) names the inner CTE, and code the planner certainly folds
/// away (a dead CASE branch, `false AND …`) is never planned. An `EXISTS` /
/// `IN` sublink that `pull_up_sublinks` turns into a join survives the
/// folding — its subquery is planned with the query — when it is a
/// top-level AND argument of a WHERE (or an UPDATE's / DELETE's).
fn owner_cte_refs(
    root: NodeRef<'_>,
    names: &[&str],
    snapshot: &PgCatalog,
) -> std::collections::BTreeSet<usize> {
    let mut out = std::collections::BTreeSet::new();
    let mut scope = RefScope {
        names,
        snapshot,
        shadowed: Vec::new(),
        out: &mut out,
        raw: false,
    };
    scope.walk_owner(root, None);
    out
}

/// Whether the statement references CTE `index` anywhere outside that CTE's
/// own body — PostgreSQL's `cterefcount` for it, without the self-reference
/// (a recursive CTE only it references is discarded).
fn referenced_outside_self(
    root: NodeRef<'_>,
    names: &[&str],
    snapshot: &PgCatalog,
    index: usize,
) -> bool {
    let mut out = std::collections::BTreeSet::new();
    let mut scope = RefScope {
        names,
        snapshot,
        shadowed: Vec::new(),
        out: &mut out,
        raw: true,
    };
    scope.walk_owner(root, Some(index));
    out.contains(&index)
}

/// The CTEs of a WITH a CTE's body references in live code, as indexes:
/// what a planned CTE's body reads reaches the plan through it.
fn cte_body_refs(
    cte: &ast::CommonTableExpr,
    names: &[&str],
    snapshot: &PgCatalog,
) -> std::collections::BTreeSet<usize> {
    let mut out = std::collections::BTreeSet::new();
    let mut scope = RefScope {
        names,
        snapshot,
        shadowed: Vec::new(),
        out: &mut out,
        raw: false,
    };
    scope.walk_node(cte.ctequery.as_ref(), false);
    out
}

/// The state of [`owner_cte_refs`]'s walk: the CTE names being looked for
/// and the inner WITHs shadowing them.
struct RefScope<'a, 'o> {
    names: &'a [&'a str],
    snapshot: &'a PgCatalog,
    /// The CTE names each enclosing inner WITH defines, nearest last.
    shadowed: Vec<std::collections::HashSet<String>>,
    out: &'o mut std::collections::BTreeSet<usize>,
    /// Count every reference, dead code included (PostgreSQL's
    /// `cterefcount`), rather than the live ones the plan reaches.
    raw: bool,
}

impl RefScope<'_, '_> {
    fn is_shadowed(&self, name: &str) -> bool {
        self.shadowed.iter().any(|w| w.contains(name))
    }

    /// `where_and`: the node is a top-level AND argument of a WHERE clause,
    /// where `pull_up_sublinks` makes a sublink a join whatever the clause
    /// folds to.
    fn walk(&mut self, n: NodeRef<'_>, where_and: bool) {
        match n {
            // The CTE body is not the statement's own reference: what a
            // planned CTE's body reads reaches the plan through that CTE
            // (`cte_body_refs`).
            NodeRef::CommonTableExpr(_) => {}
            NodeRef::RangeVar(rv) => {
                if rv.schemaname.is_empty()
                    && !self.is_shadowed(&rv.relname)
                    && let Some(i) = self.names.iter().position(|name| *name == rv.relname)
                {
                    self.out.insert(i);
                }
            }
            // A query of its own: its WITH scopes over the whole statement,
            // and its WHERE folds as the planner sees it.
            NodeRef::SelectStmt(stmt) => {
                self.statement(stmt.with_clause.as_deref(), |scope| {
                    scope.where_clause(stmt.where_clause.as_ref());
                    scope.children_but_with(n);
                });
            }
            NodeRef::InsertStmt(stmt) => {
                self.statement(stmt.with_clause.as_deref(), |scope| {
                    scope.children_but_with(n);
                });
            }
            NodeRef::UpdateStmt(stmt) => {
                self.statement(stmt.with_clause.as_deref(), |scope| {
                    // The WHERE's top-level AND arguments are where
                    // `pull_up_sublinks` applies.
                    scope.where_clause(stmt.where_clause.as_ref());
                    scope.children_but_with(n);
                });
            }
            NodeRef::DeleteStmt(stmt) => {
                self.statement(stmt.with_clause.as_deref(), |scope| {
                    scope.where_clause(stmt.where_clause.as_ref());
                    scope.children_but_with(n);
                });
            }
            NodeRef::MergeStmt(stmt) => {
                self.statement(stmt.with_clause.as_deref(), |scope| {
                    scope.children_but_with(n);
                });
            }
            // Short-circuiting expressions drop what follows their deciding
            // constant; `pull_up_sublinks` reaches the AND arguments of a
            // WHERE (subqueries included).
            NodeRef::BoolExpr(b) => self.bool_expr(b, where_and),
            NodeRef::CaseExpr(c) => self.case_expr(c),
            NodeRef::CoalesceExpr(c) => {
                let mut stopped = false;
                for arg in &c.args {
                    if stopped && !self.raw {
                        continue;
                    }
                    self.walk_node(arg.as_ref(), false);
                    stopped = crate::expr::plan_fold::certainly_non_null_constant(
                        arg.as_ref(),
                        self.snapshot,
                    );
                }
            }
            _ => {
                for child in n.children() {
                    self.walk(child, false);
                }
            }
        }
    }

    /// The children of statement `n` but its WITH clause (handled by
    /// [`Self::statement`]). Its WHERE clause is walked again, as a plain
    /// expression: that walk reaches no more than [`Self::where_clause`]'s.
    fn children_but_with(&mut self, n: NodeRef<'_>) {
        for child in n.children() {
            if !matches!(child, NodeRef::WithClause(_)) {
                self.walk(child, false);
            }
        }
    }

    /// The statement owning the WITH being counted: its own WITH clause's
    /// CTEs are the ones being counted, not inner ones shadowing them. Its
    /// CTE bodies are not the statement's own references — except for a raw
    /// count (`skip`: the CTE whose references are counted), where every
    /// other CTE's body counts too.
    fn walk_owner(&mut self, root: NodeRef<'_>, skip: Option<usize>) {
        let (with, where_clause) = match root {
            NodeRef::SelectStmt(s) => (s.with_clause.as_deref(), s.where_clause.as_ref()),
            NodeRef::InsertStmt(s) => (s.with_clause.as_deref(), None),
            NodeRef::UpdateStmt(s) => (s.with_clause.as_deref(), s.where_clause.as_ref()),
            NodeRef::DeleteStmt(s) => (s.with_clause.as_deref(), s.where_clause.as_ref()),
            NodeRef::MergeStmt(s) => (s.with_clause.as_deref(), None),
            _ => {
                self.walk(root, false);
                return;
            }
        };
        if let (Some(skip), Some(with)) = (skip, with) {
            for (i, cte) in with
                .ctes
                .iter()
                .filter_map(|n| match n.as_ref()? {
                    ast::Node::CommonTableExpr(c) => Some(&**c),
                    _ => None,
                })
                .enumerate()
            {
                if i != skip {
                    self.walk_node(cte.ctequery.as_ref(), false);
                }
            }
        }
        if self.raw {
            for child in root.children() {
                if !matches!(child, NodeRef::WithClause(_)) {
                    self.walk(child, false);
                }
            }
            return;
        }
        self.where_clause(where_clause);
        self.children_but_with(root);
    }

    fn walk_node(&mut self, n: Option<&ast::Node>, where_and: bool) {
        if let Some(inner) = n {
            self.walk(inner.to_ref(), where_and);
        }
    }

    /// Run `main` (the statement's own clauses) with the CTEs of `with` in
    /// scope, and its CTE bodies with the ones before each in scope.
    fn statement(&mut self, with: Option<&ast::WithClause>, main: impl FnOnce(&mut Self)) {
        let Some(with) = with else {
            main(self);
            return;
        };
        let ctes: Vec<&ast::CommonTableExpr> = with
            .ctes
            .iter()
            .filter_map(|n| match n.as_ref()? {
                ast::Node::CommonTableExpr(c) => Some(&**c),
                _ => None,
            })
            .collect();
        if with.recursive {
            self.shadowed
                .push(ctes.iter().map(|c| c.ctename.clone()).collect());
            for cte in &ctes {
                self.walk_node(cte.ctequery.as_ref(), false);
            }
            main(self);
            self.shadowed.pop();
            return;
        }
        let mut earlier = std::collections::HashSet::new();
        for cte in &ctes {
            self.shadowed.push(earlier.clone());
            self.walk_node(cte.ctequery.as_ref(), false);
            self.shadowed.pop();
            earlier.insert(cte.ctename.clone());
        }
        self.shadowed.push(earlier);
        main(self);
        self.shadowed.pop();
    }

    /// The `WHERE` clause: an AND tree whose top arguments
    /// `pull_up_sublinks` reaches before folding.
    fn where_clause(&mut self, clause: Option<&ast::Node>) {
        self.walk_node(clause, true);
    }

    fn bool_expr(&mut self, b: &ast::BoolExpr, where_and: bool) {
        use crate::expr::plan_fold::{Certain, certainly};
        use typedpg_pg_query::ast::BoolExprType;
        if self.raw {
            for arg in &b.args {
                self.walk_node(arg.as_ref(), false);
            }
            return;
        }
        match b.boolop {
            BoolExprType::NotExpr => {
                if let Some(arg) = b.args.first() {
                    self.walk_node(arg.as_ref(), where_and);
                }
            }
            op @ (BoolExprType::AndExpr | BoolExprType::OrExpr) => {
                let and = op == BoolExprType::AndExpr;
                let deciding = b
                    .args
                    .iter()
                    .any(|a| certainly(a.as_ref(), self.snapshot) == Certain::Bool(!and));
                for arg in &b.args {
                    if !deciding {
                        self.walk_node(arg.as_ref(), and && where_and);
                        continue;
                    }
                    if certainly(arg.as_ref(), self.snapshot) == Certain::Bool(!and) {
                        continue;
                    }
                    // A pullable sublink under a false AND argument still
                    // becomes a join: its subquery is planned.
                    if and && where_and && is_pullable_sublink(arg.as_ref()) {
                        self.walk_node(arg.as_ref(), false);
                    }
                }
            }
        }
    }

    fn case_expr(&mut self, c: &ast::CaseExpr) {
        use crate::expr::plan_fold::{Certain, certainly};
        let whens = c.args.iter().filter_map(|w| match w.as_ref() {
            Some(ast::Node::CaseWhen(when)) => Some(&**when),
            _ => None,
        });
        // The CASE's test expression is always evaluated.
        self.walk_node(c.arg.as_ref(), false);
        if self.raw {
            for when in whens {
                self.walk_node(when.expr.as_ref(), false);
                self.walk_node(when.result.as_ref(), false);
            }
            self.walk_node(c.defresult.as_ref(), false);
            return;
        }
        let mut stopped = false;
        for when in whens {
            if stopped {
                continue;
            }
            // A simple CASE's WHENs compare its test; only a searched CASE
            // folds on its conditions (the analyzer leaves the simple form's
            // results live).
            if c.arg.is_none() {
                self.walk_node(when.expr.as_ref(), false);
                match certainly(when.expr.as_ref(), self.snapshot) {
                    Certain::Bool(false) | Certain::Null => continue,
                    Certain::Bool(true) => {
                        self.walk_node(when.result.as_ref(), false);
                        stopped = true;
                        continue;
                    }
                    _ => {}
                }
                self.walk_node(when.result.as_ref(), false);
                continue;
            }
            self.walk_node(when.expr.as_ref(), false);
            self.walk_node(when.result.as_ref(), false);
        }
        if !stopped {
            self.walk_node(c.defresult.as_ref(), false);
        }
    }
}

/// Whether `arg`, a top-level AND argument of a WHERE, is a sublink
/// `pull_up_sublinks` may turn into a join: an `EXISTS` with a WHERE (which
/// must reference the level) or an `IN` whose test expression reads a
/// column. Anything else is left for the folding, and dropped.
fn is_pullable_sublink(arg: Option<&ast::Node>) -> bool {
    use typedpg_pg_query::ast::{BoolExprType, SetOperation, SubLinkType};
    let inner = match arg {
        Some(ast::Node::SubLink(s)) => s,
        Some(ast::Node::BoolExpr(b)) if b.boolop == BoolExprType::NotExpr => {
            match b.args.first().and_then(Option::as_ref) {
                Some(ast::Node::SubLink(s)) => s,
                _ => return false,
            }
        }
        _ => return false,
    };
    match inner.sub_link_type {
        SubLinkType::ExistsSublink => match inner.subselect.as_ref() {
            Some(subselect @ ast::Node::SelectStmt(s)) => {
                // `convert_EXISTS_sublink_to_join` needs a WHERE clause and a
                // reference to the level around it. A subquery naming no
                // column at all (`EXISTS (SELECT 1 FROM …)`) has no outer
                // reference: it stays a sublink, which the folding drops
                // when the clause around it decides.
                s.where_clause.is_some()
                    && reads_column(Some(subselect))
                    && s.with_clause.is_none()
                    && s.group_clause.is_empty()
                    && s.having_clause.is_none()
                    && s.limit_count.is_none()
                    && s.limit_offset.is_none()
                    && s.op == SetOperation::SetopNone
                    && s.values_lists.is_empty()
            }
            _ => false,
        },
        SubLinkType::AnySublink => reads_column(inner.testexpr.as_ref()),
        _ => false,
    }
}

/// Whether the expression reads a column anywhere (a test expression of an
/// `IN` sublink, which `convert_ANY_sublink_to_join` needs).
fn reads_column(n: Option<&ast::Node>) -> bool {
    n.is_some_and(|inner| {
        inner
            .nodes()
            .any(|(n, _)| matches!(n, NodeRef::ColumnRef(_)))
    })
}

/// The alias of a reference to CTE `name` the SELECT (UPDATE, DELETE)
/// owning `with` has among its own FROM (USING) items never null-extended (FROM-list items, sides of
/// inner joins, a LEFT join's left side, a RIGHT join's right one).
fn owner_kept_reference(with: &ast::WithClause, name: &str) -> Option<String> {
    owner_fact(with, name, |f| f.kept_reference.clone())
}

/// The alias of a reference to CTE `name` statement `owner` (a SELECT,
/// UPDATE or DELETE) has among its own FROM (USING) items never
/// null-extended (see [`owner_kept_reference`]).
fn kept_reference(owner: NodeRef<'_>, name: &str) -> Option<String> {
    fn find(n: Option<&ast::Node>, name: &str) -> Option<String> {
        match n? {
            ast::Node::RangeVar(rv) if rv.schemaname.is_empty() && rv.relname == name => Some(
                rv.alias
                    .as_ref()
                    .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone()),
            ),
            ast::Node::JoinExpr(j) => {
                let (left, right) = match j.jointype {
                    ast::JoinType::JoinInner => (true, true),
                    ast::JoinType::JoinLeft => (true, false),
                    ast::JoinType::JoinRight => (false, true),
                    _ => (false, false),
                };
                let side = |s: Option<&ast::Node>, kept: bool| {
                    if kept { find(s, name) } else { None }
                };
                side(j.larg.as_ref(), left).or_else(|| side(j.rarg.as_ref(), right))
            }
            _ => None,
        }
    }
    let from = match owner {
        NodeRef::SelectStmt(sel) => &sel.from_clause,
        NodeRef::UpdateStmt(upd) => &upd.from_clause,
        NodeRef::DeleteStmt(del) => &del.using_clause,
        _ => return None,
    };
    from.iter().find_map(|f| find(f.as_ref(), name))
}

/// How many references to CTE `name` of `with` (the WITH clause `root`
/// owns) the statement `root` has (PG's `cterefcount`, which counts the
/// references in the WITH's own CTE bodies too, read or not): the
/// unqualified relations of that name in it. `None` when that may count
/// what is no reference to it — a relation of that name a nested WITH,
/// or (without RECURSIVE) the CTE's own body or an earlier one's,
/// resolves elsewhere.
pub(crate) fn references_in(
    root: typedpg_pg_query::NodeRef<'_>,
    with: Option<&ast::WithClause>,
    name: &str,
) -> Option<usize> {
    use typedpg_pg_query::NodeRef;
    let with = with?;
    let names_it = |n: NodeRef<'_>| {
        let mut todo = vec![n];
        while let Some(n) = todo.pop() {
            if let NodeRef::RangeVar(rv) = n
                && rv.schemaname.is_empty()
                && rv.relname == name
            {
                return true;
            }
            todo.extend(n.children());
        }
        false
    };
    if !with.recursive {
        for c in &with.ctes {
            let Some(ast::Node::CommonTableExpr(c)) = c.as_ref() else {
                continue;
            };
            if c.ctequery.as_ref().is_some_and(|q| names_it(q.to_ref())) {
                return None;
            }
            if c.ctename == name {
                break;
            }
        }
    }
    let own_with = with;
    let mut count = 0;
    let mut todo = vec![root];
    while let Some(n) = todo.pop() {
        match n {
            NodeRef::RangeVar(rv) if rv.schemaname.is_empty() && rv.relname == name => {
                count += 1;
            }
            // Another WITH naming a CTE so: its references are its own.
            NodeRef::WithClause(w)
                if !std::ptr::eq(own_with, w)
                    && w.ctes.iter().any(|c| {
                        matches!(c.as_ref(),
                            Some(ast::Node::CommonTableExpr(c)) if c.ctename == name)
                    }) =>
            {
                return None;
            }
            _ => {}
        }
        todo.extend(n.children());
    }
    Some(count)
}

/// Whether the planner inlines CTE `cte` (`SS_process_ctes` →
/// `inline_cte`), referenced `references` times (see [`references_in`]):
/// a SELECT neither MATERIALIZED nor recursive, referenced once (or NOT
/// MATERIALIZED) and without volatile functions becomes a FROM subquery;
/// any other is scanned as is. `None` when the analyzer can't tell.
pub(crate) fn inlined(
    cte: &ast::CommonTableExpr,
    recursive: bool,
    references: Option<usize>,
    snapshot: &PgCatalog,
) -> Option<bool> {
    let select = matches!(cte.ctequery.as_ref(), Some(ast::Node::SelectStmt(_)));
    if recursive || !select || cte.ctematerialized == ast::CteMaterialize::Always {
        return Some(false);
    }
    let once = if cte.ctematerialized == ast::CteMaterialize::Never {
        Some(true)
    } else {
        references.map(|n| n <= 1)
    };
    match once {
        Some(false) => Some(false),
        Some(true) if !crate::expr::plan_fold::may_be_volatile(cte.ctequery.as_ref(), snapshot) => {
            Some(true)
        }
        _ => None,
    }
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
/// `outer_tiers` are the enclosing levels the CTE bodies may reference,
/// nearest first (see [`analyze_cte`]).
pub(crate) fn analyze_with_clause(
    with: &ast::WithClause,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_tiers: &[Vec<crate::scope::TableSource>],
) -> Result<HashMap<String, Vec<ScopeColumn>>, AnalyzeError> {
    // A CTE body sees none of the entries of the level owning the WITH.
    crate::grouping::with_visible_entries(&[], || {
        analyze_with_clause_here(with, snapshot, params, outer_ctes, outer_tiers)
    })
}

fn analyze_with_clause_here(
    with: &ast::WithClause,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    outer_ctes: &HashMap<String, Vec<ScopeColumn>>,
    outer_tiers: &[Vec<crate::scope::TableSource>],
) -> Result<HashMap<String, Vec<ScopeColumn>>, AnalyzeError> {
    let ctes: Vec<&ast::CommonTableExpr> = with
        .ctes
        .iter()
        .filter_map(|n| match n.as_ref()? {
            ast::Node::CommonTableExpr(cte) => Some(cte.as_ref()),
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
            if let Some(q) = cte.ctequery.as_ref() {
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
    // Which CTEs reach the plan: those the statement references outside the
    // WITH in live code (a reference a nested WITH shadows or the folding
    // drops doesn't count), the data-modifying ones, which always run, and
    // every one the planner doesn't inline that anything outside its own
    // body references (`SS_process_ctes` plans those before any folding).
    // A CTE a planned one's body references is planned too.
    let names: Vec<&str> = ctes.iter().map(|c| c.ctename.as_str()).collect();
    let mut planned_ctes: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut references_by_cte: Vec<Option<usize>> = Vec::with_capacity(ctes.len());
    let mut inlined_by_cte: Vec<Option<bool>> = Vec::with_capacity(ctes.len());
    for (i, cte) in ctes.iter().enumerate() {
        // PostgreSQL's `cterefcount` counts every syntactic reference,
        // dead code included: it decides inlining, not whether the CTE
        // reaches the plan.
        let references = owner_references(with, &cte.ctename);
        let inlined = inlined(cte, recursive[i], references, snapshot);
        let modifying = matches!(
            cte.ctequery.as_ref(),
            Some(
                ast::Node::InsertStmt(_)
                    | ast::Node::UpdateStmt(_)
                    | ast::Node::DeleteStmt(_)
                    | ast::Node::MergeStmt(_)
            )
        );
        // An unknown owner plans every CTE (never prunes a check).
        let live = owner_fact(with, &cte.ctename, |f| f.live_reference).unwrap_or(true);
        let outside_self =
            owner_fact(with, &cte.ctename, |f| f.referenced_outside_self).unwrap_or(true);
        if modifying || live || (outside_self && inlined != Some(true)) {
            planned_ctes.insert(i);
        }
        references_by_cte.push(references);
        inlined_by_cte.push(inlined);
    }
    // An inlined CTE's body is planned inside the CTEs that reference it.
    loop {
        let mut added = false;
        for i in planned_ctes.clone() {
            for j in cte_body_refs(ctes[i], &names, snapshot) {
                added |= planned_ctes.insert(j);
            }
        }
        if !added {
            break;
        }
    }
    let mut cte_scopes = outer_ctes.clone();
    for i in order {
        let cte = ctes[i];
        let returning = match cte.ctequery.as_ref() {
            Some(ast::Node::InsertStmt(s)) => {
                Some(!crate::resolve::returning_exprs(s.returning_clause.as_deref()).is_empty())
            }
            Some(ast::Node::UpdateStmt(s)) => {
                Some(!crate::resolve::returning_exprs(s.returning_clause.as_deref()).is_empty())
            }
            Some(ast::Node::DeleteStmt(s)) => {
                Some(!crate::resolve::returning_exprs(s.returning_clause.as_deref()).is_empty())
            }
            Some(ast::Node::MergeStmt(s)) => {
                Some(!crate::resolve::returning_exprs(s.returning_clause.as_deref()).is_empty())
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
        // A SELECT CTE nothing references is never planned; what planning
        // one referenced raises is raised at its references (see
        // `deferred`). A data-modifying one always runs. One the planner
        // doesn't inline is scanned as is (a CTE scan, whose Vars keep
        // their parsed collations).
        let references = references_by_cte[i];
        let inlined = inlined_by_cte[i];
        // Inlined, the CTE is a FROM subquery where its one reference is:
        // one of the owner's own FROM items is analyzed as such.
        let reference = match inlined {
            Some(true) => owner_kept_reference(with, &cte.ctename),
            _ => None,
        };
        // A SELECT CTE the plan never holds is not checked: execution checks
        // none of what it would read. It is still analyzed (its names and
        // types are). A data-modifying CTE always runs.
        let unplanned = returning.is_none() && !planned_ctes.contains(&i);
        let (analyzed, planned) = super::deferred::analyze_cte(
            cte,
            recursive[i],
            inlined,
            reference.as_deref(),
            snapshot,
            || {
                let mut body = || {
                    analyze_cte(
                        cte,
                        recursive[i],
                        inlined,
                        snapshot,
                        params,
                        &cte_scopes,
                        outer_tiers,
                    )
                };
                if unplanned {
                    // Not executed: the planner drops it, so its runtime
                    // privileges aren't checked — but parse analysis still
                    // looked up the schemas it names.
                    crate::privileges::not_executed(body)
                } else {
                    body()
                }
            },
        );
        let mut analyzed = analyzed?;
        super::deferred::attach_to_cte(&mut analyzed.columns, planned);
        // A VALUES the executor can't start: a CTE scanned on its own is
        // planned and started wherever it is referenced; an inlined one
        // where its reference is (see `check_values_sort`).
        if let Some(ast::Node::SelectStmt(sel)) = cte.ctequery.as_ref()
            && let Some((e, _)) = super::values_sort_error(sel, snapshot)
        {
            let e = Box::new(e);
            for c in &mut analyzed.columns {
                match inlined {
                    Some(false) if c.deferred.entry.is_none() => c.deferred.entry = Some(e.clone()),
                    Some(true) => c.deferred.started = Some(e.clone()),
                    _ => {}
                }
            }
        }
        // SS_process_ctes plans a SELECT CTE it doesn't inline as soon as
        // anything references it — `cterefcount` counts the references in
        // the WITH's other CTE bodies too, planned or not (`WITH a AS (SELECT
        // 1 / 0), b AS (SELECT * FROM a), d AS (SELECT * FROM a) SELECT 1`
        // fails: `b` and `d` are never planned, `a` is).
        if inlined == Some(false)
            && returning.is_none()
            && references.is_some_and(|n| n > 0)
            && crate::expr::plan_time_checks()
            && let Some(e) = analyzed
                .columns
                .iter()
                .find_map(|c| c.deferred.entry.as_deref())
            && let Some(e) = crate::expr::plan_time_error(Some(cte.location), e.clone())
        {
            return Err(e);
        }
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
        if returning == Some(true)
            && let Some(ast::Node::InsertStmt(ins)) = cte.ctequery.as_ref()
            && insert_returns_one_row(ins, snapshot)
        {
            cte_scopes.insert(one_row_marker(&cte.ctename), Vec::new());
        }
        if analyzed.min_one_row {
            cte_scopes.insert(min_one_row_marker(&cte.ctename), Vec::new());
        }
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
                    if let Some(q) = cte.ctequery.as_ref() {
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
fn inner_ctes(w: &ast::WithClause) -> impl Iterator<Item = &ast::CommonTableExpr> {
    w.ctes.iter().filter_map(|n| match n.as_ref()? {
        ast::Node::CommonTableExpr(c) => Some(c.as_ref()),
        _ => None,
    })
}

fn inner_cte_names(w: &ast::WithClause) -> Vec<String> {
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
                        if let Some(q) = cte.ctequery.as_ref() {
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
                let side = |w: &mut Self, n: Option<&ast::Node>, ctx| {
                    w.context = ctx;
                    let r = match n {
                        Some(n) => w.walk(n.to_ref()),
                        None => Ok(()),
                    };
                    w.context = save;
                    r
                };
                let (l, r) = match j.jointype {
                    JoinType::JoinLeft => (save, outer(save)),
                    JoinType::JoinFull => (outer(save), outer(save)),
                    JoinType::JoinRight => (outer(save), save),
                    _ => (save, save),
                };
                side(self, j.larg.as_ref(), l)?;
                side(self, j.rarg.as_ref(), r)?;
                side(self, j.quals.as_ref(), save)?;
            }
            NodeRef::SubLink(sl) => {
                // The subquery is independent of the outer context.
                self.context = RecursionContext::Sublink;
                if let Some(s) = sl.subselect.as_ref() {
                    self.walk(s.to_ref())?;
                }
                self.context = save;
                if let Some(t) = sl.testexpr.as_ref() {
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
    fn select_stmt(&mut self, stmt: &ast::SelectStmt) -> Result<(), AnalyzeError> {
        let save = self.context;
        let op = stmt.op;
        if save != RecursionContext::Ok
            || !matches!(op, SetOperation::SetopIntersect | SetOperation::SetopExcept)
        {
            for c in typedpg_pg_query::NodeRef::SelectStmt(stmt).children() {
                self.walk(c)?;
            }
            return Ok(());
        }
        let arg = |w: &mut Self, s: Option<&ast::SelectStmt>| match s {
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
        arg(self, stmt.larg.as_deref())?;
        if op == SetOperation::SetopExcept {
            self.context = RecursionContext::Except;
        }
        arg(self, stmt.rarg.as_deref())?;
        self.context = save;
        for n in stmt
            .sort_clause
            .iter()
            .chain(std::iter::once(&stmt.limit_offset))
            .chain(std::iter::once(&stmt.limit_count))
            .chain(stmt.locking_clause.iter())
        {
            if let Some(n) = n.as_ref() {
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
fn check_well_formed_recursion(cte: &ast::CommonTableExpr) -> Result<(), AnalyzeError> {
    let invalid = |msg: String| {
        crate::error::RawError::new(AnalyzeError::InvalidRecursion(msg), None, None)
            .finalize_implicit()
    };
    let Some(ast::Node::SelectStmt(sel)) = cte.ctequery.as_ref() else {
        return Err(invalid(format!(
            "recursive query \"{}\" must not contain data-modifying statements",
            cte.ctename
        )));
    };
    if sel.op != SetOperation::SetopUnion {
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
            if let Some(c) = c.as_ref() {
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
    rarg: &ast::SelectStmt,
    snapshot: &PgCatalog,
) -> Result<(), AnalyzeError> {
    let has_agg = rarg
        .target_list
        .iter()
        .filter_map(|t| match t.as_ref()? {
            ast::Node::ResTarget(rt) => Some(&rt.val),
            _ => None,
        })
        .chain(std::iter::once(&rarg.having_clause))
        .any(|n| expr::detect_func_kinds(n.as_ref(), snapshot).has_aggregate);
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
