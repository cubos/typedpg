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
        append_search_cycle_columns(cte, &mut unified, snapshot);
        return Ok(unified);
    }

    match cte_query {
        node::Node::SelectStmt(sel) => {
            let (cols, _) = analyze_select_with_ctes(sel, snapshot, params, existing_ctes)?;
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

/// Append synthetic columns introduced by `SEARCH BREADTH/DEPTH FIRST BY
/// … SET col` and `CYCLE … SET mark USING path` clauses on a recursive
/// CTE. PG defines each clause as adding one or two named, NOT NULL
/// columns to the CTE's output:
///
/// - `SEARCH BFS BY k SET ord` → `ord record NOT NULL` (a row of
///   `(integer, k...)` PG materializes during recursion).
/// - `CYCLE k SET is_cycle USING path` → `is_cycle <mark_type> NOT NULL`
///   (defaults to bool when the user didn't specify `TO/DEFAULT`) and
///   `path record[] NOT NULL` (an array of `(k...)` rows).
///
/// Without this, downstream `SELECT id, ord, is_cycle, path FROM cte`
/// fails with `column "ord" does not exist`.
fn append_search_cycle_columns(
    cte: &protobuf::CommonTableExpr,
    cols: &mut Vec<ScopeColumn>,
    snapshot: &PgCatalog,
) {
    if let Some(search) = cte.search_clause.as_ref()
        && !search.search_seq_column.is_empty()
    {
        cols.push(ScopeColumn {
            name: search.search_seq_column.clone(),
            type_oid: oid::RECORD,
            base_not_null: true,
            table_alias: cte.ctename.clone(),
            typmod: None,
            collation: None,
            record_fields: None,
        });
    }
    if let Some(cycle) = cte.cycle_clause.as_ref() {
        if !cycle.cycle_mark_column.is_empty() {
            // PG: when `TO/DEFAULT` are omitted the mark column is bool;
            // otherwise the AST exposes the inferred type via `cycle_mark_type`.
            let mark_oid = PgTypeOid::new(cycle.cycle_mark_type).unwrap_or(oid::BOOL);
            cols.push(ScopeColumn {
                name: cycle.cycle_mark_column.clone(),
                type_oid: mark_oid,
                base_not_null: true,
                table_alias: cte.ctename.clone(),
                typmod: None,
                collation: None,
                record_fields: None,
            });
        }
        if !cycle.cycle_path_column.is_empty() {
            // The path column is `record[]` — let `array_type_of(RECORD)`
            // walk the snapshot's `pg_type.typarray` link instead of
            // hardcoding the OID, mirroring how PG resolves the
            // automatic `_record` array type.
            let path_oid = snapshot.array_type_of(oid::RECORD).unwrap_or(oid::UNKNOWN);
            cols.push(ScopeColumn {
                name: cycle.cycle_path_column.clone(),
                type_oid: path_oid,
                base_not_null: true,
                table_alias: cte.ctename.clone(),
                typmod: None,
                collation: None,
                record_fields: None,
            });
        }
    }
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
