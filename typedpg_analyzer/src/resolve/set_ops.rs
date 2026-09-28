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

    let (left_cols, _) = analyze_select_with_ctes(left, snapshot, params, cte_scopes)?;
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

    let mut columns = Vec::with_capacity(left_cols.len());
    for (l, r) in left_cols.into_iter().zip(right_cols) {
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
            (None, false) => l.type_oid,
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

    Ok((columns, None))
}
