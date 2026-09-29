use super::*;

// ──────────────────────────────────────────────────────────────────────────
// Minimization — shrink a failing query while preserving the divergence kind.
// ──────────────────────────────────────────────────────────────────────────

/// Candidate reductions of `sql` produced by structural edits to the parsed
/// SELECT (drop a projection, drop a clause, unwrap a binary expr).
pub(crate) fn reductions(sql: &str) -> Vec<String> {
    // `sql` is in the analyzer's named form; typedpg_pg_query needs positional.
    let Ok(parsed) = typedpg_pg_query::parse(&named_to_positional(sql)) else {
        return Vec::new();
    };
    // The wrapper `ParseResult` isn't `Clone`, but the inner protobuf message
    // is — work on it and deparse via the free function.
    let proto = parsed.protobuf;
    let mut out = Vec::new();

    // Work on a fresh clone per reduction so edits don't compound.
    for stmt_idx in 0..proto.stmts.len() {
        let make = |edit: &dyn Fn(&mut protobuf::SelectStmt)| -> Option<String> {
            let mut clone = proto.clone();
            let raw = clone.stmts.get_mut(stmt_idx)?;
            let node = raw.stmt.as_mut()?.node.as_mut()?;
            if let NodeEnum::SelectStmt(sel) = node {
                edit(sel.as_mut());
            } else {
                return None;
            }
            typedpg_pg_query::deparse(&clone)
                .ok()
                .map(|s| positional_to_named(&s))
        };

        // Drop the last projection (keep at least one).
        if let Some(s) = make(&|sel| {
            if sel.target_list.len() > 1 {
                sel.target_list.pop();
            }
        }) {
            out.push(s);
        }
        // Clear each optional clause independently.
        for clear in [
            (|sel: &mut protobuf::SelectStmt| sel.where_clause = None)
                as fn(&mut protobuf::SelectStmt),
            |sel| sel.having_clause = None,
            |sel| sel.group_clause.clear(),
            |sel| sel.sort_clause.clear(),
            |sel| sel.distinct_clause.clear(),
            |sel| sel.limit_count = None,
            |sel| sel.limit_offset = None,
            |sel| sel.from_clause.clear(),
        ] {
            if let Some(s) = make(&clear) {
                out.push(s);
            }
        }
    }
    out
}

/// Greedily shrink `sql` while the same `kind` of divergence still reproduces.
/// Bounded by a fixed number of oracle calls to keep PG round-trips in check.
pub(crate) fn minimize(db: &PgCatalog, sql: &str, kind: DivergenceKind) -> String {
    let mut best = sql.to_string();
    let mut budget = 80u32;
    let mut improved = true;
    while improved && budget > 0 {
        improved = false;
        for cand in reductions(&best) {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if cand.len() >= best.len() {
                continue;
            }
            if let (_, Some(div)) = db.analyze_checked(&cand)
                && div.kind == kind
            {
                best = cand;
                improved = true;
                break;
            }
        }
    }
    best
}
