use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// CTE
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_cte(
    cte: &protobuf::CommonTableExpr,
    with_recursive: bool,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    existing_ctes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<Vec<ScopeColumn>, AnalyzeError> {
    let cte_query = cte
        .ctequery
        .as_ref()
        .and_then(|n| n.node.as_ref())
        .ok_or_else(|| AnalyzeError::Unsupported("CTE without query".into()))?;

    // `WITH RECURSIVE` — the recursive branch references the CTE by name, so
    // we have to seed the scope before analyzing it. pg_query's AST doesn't
    // set `cterecursive` on individual CTEs without full parse analysis, so
    // we rely on the enclosing `WithClause.recursive` flag (true when the
    // user wrote `WITH RECURSIVE`) plus the UNION shape of the inner query.
    // We (1) analyze the seed arm alone to type the CTE's columns,
    // (2) register those columns in a temporary scope, (3) analyze the
    // recursive arm against that scope, (4) unify the two arms' column
    // types via `find_common_type` — matching PG's common-type resolution.
    if with_recursive
        && let node::Node::SelectStmt(sel) = cte_query
        && sel.op != SetOperation::SetopNone as i32
        && let (Some(larg), Some(rarg)) = (sel.larg.as_ref(), sel.rarg.as_ref())
    {
        let (seed_cols, _) = analyze_select_with_ctes(larg, snapshot, params, existing_ctes)?;
        let seed_cols = apply_cte_column_aliases(seed_cols, &cte.aliascolnames);

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

        let (rec_cols, _) = analyze_select_with_ctes(rarg, snapshot, params, &scopes_with_self)?;
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
            let (mut cols, _) = analyze_select_with_ctes(sel, snapshot, params, existing_ctes)?;
            resolve_unknown_outputs(sel, &mut cols, params);
            let cols = apply_cte_column_aliases(cols, &cte.aliascolnames);
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
/// than columns, the trailing columns keep their inner names.
fn apply_cte_column_aliases(cols: Vec<RawColumn>, aliases: &[protobuf::Node]) -> Vec<RawColumn> {
    if aliases.is_empty() {
        return cols;
    }
    let names: Vec<String> = aliases
        .iter()
        .filter_map(|n| match n.node.as_ref()? {
            node::Node::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    cols.into_iter()
        .enumerate()
        .map(|(i, c)| RawColumn {
            name: names.get(i).cloned().unwrap_or(c.name),
            type_oid: c.type_oid,
            nullable: c.nullable,
            typmod: c.typmod,
            collation: c.collation,
            record_fields: c.record_fields,
        })
        .collect()
}
