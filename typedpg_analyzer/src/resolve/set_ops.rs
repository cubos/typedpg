use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// UNION / INTERSECT / EXCEPT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_set_operation(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
    outer: &[crate::scope::TableSource],
    shadowed: &[crate::scope::TableSource],
) -> AnalyzeResult {
    let left = sel
        .larg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without left side".into()))?;
    let right = sel
        .rarg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without right side".into()))?;

    // The arms are subqueries of the set operation: outer references reach
    // them as correlated ones, and the FROM entries a non-LATERAL subquery
    // can't see stay unreachable (PG's `invalid reference to FROM-clause
    // entry`).
    check_set_op_member_locking(left)?;
    let (left_cols, _) = analyze_select_with_ctes_and_outer(
        left,
        snapshot,
        params,
        cte_scopes,
        &[],
        outer,
        shadowed,
    )?;
    check_set_op_member_locking(right)?;
    let (right_cols, _) = analyze_select_with_ctes_and_outer(
        right,
        snapshot,
        params,
        cte_scopes,
        &[],
        outer,
        shadowed,
    )?;

    // PG names the operation in both error messages below.
    let op_label = match protobuf::SetOperation::try_from(sel.op) {
        Ok(protobuf::SetOperation::SetopIntersect) => "INTERSECT",
        Ok(protobuf::SetOperation::SetopExcept) => "EXCEPT",
        _ => "UNION",
    };

    // PG's CheckSelectLocking: a set operation can't be locked (0A000).
    if let Some(node::Node::LockingClause(lc)) =
        sel.locking_clause.first().and_then(|n| n.node.as_ref())
    {
        return Err(crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(format!(
                "{} is not allowed with UNION/INTERSECT/EXCEPT",
                lock_strength_name(lc)
            )),
            None,
            None,
        )
        .finalize_implicit());
    }

    if left_cols.len() != right_cols.len() {
        return Err(
            crate::pgmsg::set_op_column_count(op_label, left_cols.len(), right_cols.len())
                .finalize_implicit(),
        );
    }

    // A bare `$N` projected by one branch adopts the column's reconciled
    // type — PG's Describe reports `SELECT $1 UNION ALL SELECT age` with $1
    // as the other branch's type, not text. Pin direct ParamRef targets
    // before the per-column merge below.
    for (branch, own_cols, peer_cols) in [
        (left, &left_cols, &right_cols),
        (right, &right_cols, &left_cols),
    ] {
        for (i, target) in branch.target_list.iter().enumerate() {
            // A bare `$N` the arm transformed while `$N` was untyped stays
            // `unknown` even if a later clause of the arm deduced a type,
            // and its coercion then fails with 42P08
            // (`expr::note_untyped_output_params`).
            if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
                && let Some(val) = &rt.val
                && let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && (own_cols.get(i).is_some_and(|c| c.type_oid == oid::UNKNOWN)
                    || params.is_untyped_output(p.location))
                && let Some(peer) = peer_cols.get(i)
                && peer.type_oid != oid::UNKNOWN
            {
                let target = snapshot.unwrap_domain(peer.type_oid);
                if let Err(prev) = params.coerce_untyped(p.number, target) {
                    return Err(expr::inconsistent_param_error(
                        p.number, prev, target, p.location, snapshot,
                    ));
                }
            }
        }
    }

    // PG keeps an arm's untyped string literal as `unknown` (set-operation
    // arms don't resolve unknowns) and coerces it to the other arm's type,
    // running that type's input function on the literal (22P02 for
    // `SELECT 1 UNION SELECT 'x'`).
    let left_lits: Vec<Option<&str>> = (0..left_cols.len())
        .map(|i| arm_unknown_literal(left, i))
        .collect();
    let right_lits: Vec<Option<&str>> = (0..right_cols.len())
        .map(|i| arm_unknown_literal(right, i))
        .collect();

    let mut columns = Vec::with_capacity(left_cols.len());
    for (i, (mut l, mut r)) in left_cols.into_iter().zip(right_cols).enumerate() {
        if left_lits[i].is_some() {
            l.type_oid = oid::UNKNOWN;
        }
        if right_lits[i].is_some() {
            r.type_oid = oid::UNKNOWN;
        }
        let target = if l.type_oid == oid::UNKNOWN {
            r.type_oid
        } else {
            l.type_oid
        };
        if target != oid::UNKNOWN {
            for lit in [left_lits[i], right_lits[i]].into_iter().flatten() {
                if let Err(msg) = crate::literal_input::validate(lit, target, snapshot) {
                    return Err(
                        crate::error::RawError::invalid_literal(msg, None).finalize_implicit()
                    );
                }
            }
        }
        // When both sides carry concrete types (not UNKNOWN), their common
        // type must exist — PG rejects `SELECT 1 UNION SELECT 'x'` with
        // `UNION types integer and text cannot be matched`.
        let common = crate::coerce::find_common_type(&[l.type_oid, r.type_oid], snapshot);
        let both_concrete = l.type_oid != oid::UNKNOWN && r.type_oid != oid::UNKNOWN;
        let type_oid = match (common, both_concrete) {
            (Some(t), _) => t,
            (None, true) => {
                // PG (SQLSTATE 42804): `UNION types A and B cannot be
                // matched` — INTERSECT/EXCEPT use their own name. Use
                // `Invalid` to keep `TypeMismatch::Display`'s generic prefix
                // from leaking.
                let a = crate::ddl::util::format_type_for_message(snapshot, l.type_oid);
                let b = crate::ddl::util::format_type_for_message(snapshot, r.type_oid);
                return Err(crate::pgmsg::types_cannot_be_matched(
                    op_label,
                    &a,
                    &b,
                    &format!(" (column `{}`)", l.name),
                    Some(format!(
                        "cast both sides to a common type, e.g. `{}::{a}`",
                        l.name
                    )),
                )
                .finalize_implicit());
            }
            // Both arms unknown: select_common_type resolves to text.
            (None, false) if l.type_oid == oid::UNKNOWN && r.type_oid == oid::UNKNOWN => oid::TEXT,
            (None, false) => target,
        };
        let typmod = if l.typmod == r.typmod { l.typmod } else { None };
        // transformSetOperationTree: every set operation but UNION ALL
        // compares the rows, so the column type needs an equality operator.
        let union_all = op_label == "UNION" && sel.all;
        if !union_all {
            crate::clause::check_key_operators(
                snapshot,
                type_oid,
                crate::clause::KeyUse::Group,
                None,
            )?;
        }
        // select_common_collation over the two arms' columns: an explicit
        // collation (a COLLATE in a plain arm's select list) wins, two
        // different explicit ones are an error, and two different implicit
        // ones leave the column without a collation — an error too unless
        // it is UNION ALL, which never compares.
        let arm_state = |arm: &protobuf::SelectStmt, c: &RawColumn| {
            let explicit = arm.op == SetOperation::SetopNone as i32
                && crate::grouping::level_info(arm)
                    .is_some_and(|l| l.explicit_collations.get(i).copied().unwrap_or(false));
            crate::clause::CollationState::column(snapshot, type_oid, c.collation, explicit)
        };
        let (ls, rs) = (arm_state(left, &l), arm_state(right, &r));
        if let (Some(a), Some(b)) = (ls.explicit(), rs.explicit())
            && a != b
        {
            let name = |c| {
                snapshot
                    .pg_collation
                    .get(&c)
                    .map(|c| c.collname.clone())
                    .unwrap_or_default()
            };
            return Err(crate::pgmsg::collation_mismatch_explicit(
                &name(a),
                &name(b),
            ));
        }
        let merged = ls.merged(rs);
        if !union_all {
            merged.check_determinate(snapshot, None)?;
        }
        let collation = merged.collation();
        columns.push(RawColumn {
            name: l.name,
            type_oid,
            // EXCEPT only emits left rows; INTERSECT only left rows equal
            // (NULLs not distinct) to a right one — a NULL there needs
            // both sides to have one.
            nullable: match op_label {
                "EXCEPT" => l.nullable,
                "INTERSECT" => l.nullable && r.nullable,
                _ => l.nullable || r.nullable,
            },
            typmod,
            collation,
            record_fields: None,
        });
    }

    set_operation_sort_and_limit(sel, &columns, snapshot, params, cte_scopes)?;

    Ok((columns, None))
}

/// ORDER BY / LIMIT / OFFSET of a set operation (`transformSetOperationStmt`):
/// ORDER BY sees only the result columns, by name or position — anything
/// else is 0A000 `invalid UNION/INTERSECT/EXCEPT ORDER BY clause`; LIMIT and
/// OFFSET see no columns at all.
fn set_operation_sort_and_limit(
    sel: &protobuf::SelectStmt,
    columns: &[RawColumn],
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> Result<(), AnalyzeError> {
    let alias = crate::scope::hidden_alias("setop");
    let result_cols: Vec<ScopeColumn> = columns
        .iter()
        .map(|c| ScopeColumn {
            name: c.name.clone(),
            type_oid: c.type_oid,
            base_not_null: !c.nullable,
            typmod: c.typmod,
            collation: c.collation,
            table_alias: alias.clone(),
            record_fields: c.record_fields.clone(),
        })
        .collect();
    let mut scope = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    scope.add_derived(&alias, result_cols, crate::scope::SourceKind::Other)?;
    let null_ctx = NullabilityContext::default();
    for sort_node in &sel.sort_clause {
        let Some(node::Node::SortBy(sb)) = sort_node.node.as_ref() else {
            continue;
        };
        let Some(inner) = sb.node.as_deref() else {
            continue;
        };
        let sorts = sb.sortby_dir() != protobuf::SortByDir::SortbyUsing;
        if let Some(ord) = sql92_position(inner, "ORDER BY")? {
            if ord < 1 || ord as usize > columns.len() {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "ORDER BY",
                    ord,
                    crate::error::node_location(inner)
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            if sorts {
                crate::clause::check_key_operators(
                    snapshot,
                    columns[ord as usize - 1].type_oid,
                    crate::clause::KeyUse::Sort,
                    crate::error::node_location(inner),
                )?;
            }
            continue;
        }
        // findTargetlistEntrySQL92: a name matches the result columns;
        // several of that name (distinct columns) are ambiguous.
        if let Some(node::Node::ColumnRef(cr)) = inner.node.as_ref()
            && let [name] = expr::extract_string_fields(&cr.fields).as_slice()
            && cr.fields.len() == 1
        {
            let mut named = columns.iter().filter(|c| &c.name == name);
            if let Some(col) = named.next() {
                if named.next().is_some() {
                    return Err(crate::pgmsg::clause_name_ambiguous(
                        "ORDER BY",
                        name,
                        crate::error::SourceSpan::from_node_qname(cr.location),
                    )
                    .finalize_implicit());
                }
                if sorts {
                    crate::clause::check_key_operators(
                        snapshot,
                        col.type_oid,
                        crate::clause::KeyUse::Sort,
                        Some(cr.location),
                    )?;
                }
            }
        }
        expr::infer_expr(
            inner,
            expr::Ctx::new(&scope, &null_ctx, snapshot),
            params,
            TypeGoal::NONE,
        )?;
        let is_result_column = matches!(inner.node.as_ref(),
            Some(node::Node::ColumnRef(cr)) if cr.fields.len() == 1);
        if !is_result_column {
            return Err(crate::error::RawError::new(
                AnalyzeError::FeatureNotSupported(
                    "invalid UNION/INTERSECT/EXCEPT ORDER BY clause".into(),
                ),
                crate::error::node_location(inner)
                    .and_then(crate::error::SourceSpan::from_node_token),
                Some(
                    "Add the expression/function to every SELECT, or move the UNION into a \
                     FROM clause."
                        .into(),
                ),
            )
            .finalize_implicit());
        }
    }
    let empty = Scope {
        ctes: cte_scopes.clone(),
        ..Scope::default()
    };
    analyze_limit_offset(sel, expr::Ctx::new(&empty, &null_ctx, snapshot), params)
}

/// The untyped string literal an arm projects at position `i`, when the arm
/// is a plain SELECT whose target entries line up with its output.
fn arm_unknown_literal(arm: &protobuf::SelectStmt, i: usize) -> Option<&str> {
    if arm.op != SetOperation::SetopNone as i32 || !arm.values_lists.is_empty() {
        return None;
    }
    let has_star = arm.target_list.iter().any(|t| {
        matches!(t.node.as_ref(), Some(node::Node::ResTarget(rt))
            if matches!(rt.val.as_deref().and_then(|v| v.node.as_ref()),
                Some(node::Node::ColumnRef(cr)) if cr.fields.iter().any(|f|
                    matches!(f.node.as_ref(), Some(node::Node::AStar(_))))))
    });
    if has_star {
        return None;
    }
    let Some(node::Node::ResTarget(rt)) = arm.target_list.get(i)?.node.as_ref() else {
        return None;
    };
    match rt.val.as_deref()?.node.as_ref()? {
        node::Node::AConst(ac) if !ac.isnull => match &ac.val {
            Some(typedpg_pg_query::protobuf::a_const::Val::Sval(sv)) => Some(sv.sval.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// transformSetOperationTree: no member of a set-operation tree may carry
/// a locking clause (0A000), checked as each member is reached.
pub(crate) fn check_set_op_member_locking(arm: &protobuf::SelectStmt) -> Result<(), AnalyzeError> {
    match arm.locking_clause.first().and_then(|n| n.node.as_ref()) {
        Some(node::Node::LockingClause(lc)) => Err(crate::error::RawError::new(
            AnalyzeError::FeatureNotSupported(format!(
                "{} is not allowed with UNION/INTERSECT/EXCEPT",
                lock_strength_name(lc)
            )),
            None,
            None,
        )
        .finalize_implicit()),
        _ => Ok(()),
    }
}
