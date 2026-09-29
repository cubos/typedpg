use super::*;

// ──────────────────────────────────────────────────────────────────────────────
// UNION / INTERSECT / EXCEPT
// ──────────────────────────────────────────────────────────────────────────────

pub(crate) fn analyze_set_operation(
    sel: &protobuf::SelectStmt,
    snapshot: &PgCatalog,
    params: &mut ParamCollector,
    cte_scopes: &HashMap<String, Vec<ScopeColumn>>,
) -> AnalyzeResult {
    let left = sel
        .larg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without left side".into()))?;
    let right = sel
        .rarg
        .as_ref()
        .ok_or_else(|| AnalyzeError::Unsupported("UNION without right side".into()))?;

    check_set_op_member_locking(left)?;
    let (left_cols, _) = analyze_select_with_ctes(left, snapshot, params, cte_scopes)?;
    check_set_op_member_locking(right)?;
    let (right_cols, _) = analyze_select_with_ctes(right, snapshot, params, cte_scopes)?;

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
            if let Some(node::Node::ResTarget(rt)) = target.node.as_ref()
                && let Some(val) = &rt.val
                && let Some(node::Node::ParamRef(p)) = val.node.as_ref()
                && own_cols.get(i).is_some_and(|c| c.type_oid == oid::UNKNOWN)
                && let Some(peer) = peer_cols.get(i)
                && peer.type_oid != oid::UNKNOWN
                && params.get(p.number) == oid::UNKNOWN
            {
                params.record(p.number, snapshot.unwrap_domain(peer.type_oid));
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
        // UNION arms only carry collation forward when both sides agree
        // — same shape as the typmod merge above. Mirrors PG's collation
        // derivation rule that conflicting branches produce an
        // indeterminate (None) collation.
        let collation = if l.collation == r.collation {
            l.collation
        } else {
            None
        };
        columns.push(RawColumn {
            name: l.name,
            type_oid,
            nullable: l.nullable || r.nullable,
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
        if let Some(node::Node::AConst(ac)) = inner.node.as_ref()
            && let Some(typedpg_pg_query::protobuf::a_const::Val::Ival(i)) = &ac.val
        {
            let ord = i.ival as i64;
            if ord < 1 || ord as usize > columns.len() {
                return Err(crate::pgmsg::position_not_in_select_list(
                    "ORDER BY",
                    ord,
                    crate::error::node_location(inner)
                        .and_then(crate::error::SourceSpan::from_node_token),
                )
                .finalize_implicit());
            }
            continue;
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
