//! A small SQL printer for the differential fuzzer's mutated parse trees.
//!
//! The fuzzer mutates a parsed seed's AST and needs SQL back to
//! hand to the oracle. libpg_query's deparser would do it, but typedpg
//! doesn't link it; this printer covers exactly the raw parse trees the
//! seeds, the generators and the mutations produce.
//!
//! It is deliberately conservative: whenever it meets a node kind or field
//! value it does not support, [`print`] returns `None` instead of writing
//! something that would reparse to a different tree. Parentheses around
//! subexpressions are free — they don't survive into PostgreSQL's raw parse
//! tree — so binary and unary expressions are always parenthesized.

use typedpg_core::quote_identifier;
use typedpg_pg_query::ast::Node;
use typedpg_pg_query::ast::{
    self, AExprKind, BoolExprType, BoolTestType, CoercionForm, GroupingSetKind, JoinType,
    LimitOption, MergeMatchKind, MinMaxOp, NullTestType, SetOperation, SortByDir, SortByNulls,
    SqlValueFunctionOp, SubLinkType,
};

/// Render `tree` as SQL: its statements, joined by `"; "`.
///
/// Returns `None` if any part of the tree isn't supported: the caller then
/// skips the tree rather than feeding the oracle a wrong query.
pub(crate) fn print(tree: &ast::ParseResult) -> Option<String> {
    let mut stmts = Vec::with_capacity(tree.stmts.len());
    for raw in &tree.stmts {
        stmts.push(statement(raw.stmt.as_ref())?);
    }
    Some(stmts.join("; "))
}

/// One statement (a CTE body included): the node kinds the fuzzer produces.
fn statement(node: Option<&ast::Node>) -> Option<String> {
    match node? {
        Node::SelectStmt(sel) => select_stmt(sel),
        Node::InsertStmt(ins) => insert_stmt(ins),
        Node::UpdateStmt(upd) => update_stmt(upd),
        Node::DeleteStmt(del) => delete_stmt(del),
        Node::MergeStmt(mrg) => merge_stmt(mrg),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Statements.
// ──────────────────────────────────────────────────────────────────────────

fn select_stmt(sel: &ast::SelectStmt) -> Option<String> {
    // A set operation: both arms are selects of their own, parenthesized so
    // their grouping survives.
    if sel.op != SetOperation::SetopNone {
        let op = match sel.op {
            SetOperation::SetopUnion => "UNION",
            SetOperation::SetopIntersect => "INTERSECT",
            SetOperation::SetopExcept => "EXCEPT",
            _ => return None,
        };
        let mut sql = String::new();
        if let Some(with) = sel.with_clause.as_ref() {
            sql.push_str(&with_clause(with)?);
            sql.push(' ');
        }
        let larg = sel.larg.as_deref()?;
        let rarg = sel.rarg.as_deref()?;
        sql.push_str(&format!(
            "({}) {op}{} ({})",
            select_stmt(larg)?,
            if sel.all { " ALL" } else { "" },
            select_stmt(rarg)?,
        ));
        sql.push_str(&sort_and_limit(sel)?);
        return Some(sql);
    }

    if !sel.values_lists.is_empty() {
        let rows: Vec<String> = sel
            .values_lists
            .iter()
            .map(|row| {
                let Node::List(list) = row.as_ref()? else {
                    return None;
                };
                let exprs: Vec<String> = list
                    .items
                    .iter()
                    .map(|n| expr(n.as_ref()))
                    .collect::<Option<_>>()?;
                Some(format!("({})", exprs.join(", ")))
            })
            .collect::<Option<_>>()?;
        let mut sql = String::new();
        if let Some(with) = sel.with_clause.as_ref() {
            sql.push_str(&with_clause(with)?);
            sql.push(' ');
        }
        sql.push_str("VALUES ");
        sql.push_str(&rows.join(", "));
        sql.push_str(&sort_and_limit(sel)?);
        return Some(sql);
    }

    if sel.into_clause.is_some() {
        // `SELECT … INTO` doesn't appear in fuzzer trees.
        return None;
    }
    if !sel.locking_clause.is_empty() {
        return None;
    }

    let mut sql = String::new();
    if let Some(with) = sel.with_clause.as_ref() {
        sql.push_str(&with_clause(with)?);
        sql.push(' ');
    }
    sql.push_str("SELECT ");

    if !sel.distinct_clause.is_empty() {
        if sel.distinct_clause.len() == 1 && sel.distinct_clause[0].is_none() {
            sql.push_str("DISTINCT ");
        } else {
            let items: Vec<String> = sel
                .distinct_clause
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<_>>()?;
            sql.push_str(&format!("DISTINCT ON ({}) ", items.join(", ")));
        }
    }

    let targets: Vec<String> = sel
        .target_list
        .iter()
        .map(|n| res_target(n.as_ref()))
        .collect::<Option<_>>()?;
    if targets.is_empty() {
        return None;
    }
    sql.push_str(&targets.join(", "));

    if !sel.from_clause.is_empty() {
        let from: Vec<String> = sel
            .from_clause
            .iter()
            .map(|n| from_item(n.as_ref()))
            .collect::<Option<_>>()?;
        sql.push_str(" FROM ");
        sql.push_str(&from.join(", "));
    }
    if let where_clause @ Some(_) = &sel.where_clause {
        sql.push_str(" WHERE ");
        sql.push_str(&expr(where_clause.as_ref())?);
    }
    if !sel.group_clause.is_empty() {
        let groups: Vec<String> = sel
            .group_clause
            .iter()
            .map(|n| grouping_element(n.as_ref()))
            .collect::<Option<_>>()?;
        sql.push_str(" GROUP BY ");
        if sel.group_distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&groups.join(", "));
    }
    if let having @ Some(_) = &sel.having_clause {
        sql.push_str(" HAVING ");
        sql.push_str(&expr(having.as_ref())?);
    }
    if !sel.window_clause.is_empty() {
        let windows: Vec<String> = sel
            .window_clause
            .iter()
            .map(|w| {
                let Node::WindowDef(w) = w.as_ref()? else {
                    return None;
                };
                if w.name.is_empty() {
                    return None;
                }
                Some(format!(
                    "{} AS {}",
                    quote_identifier(&w.name),
                    window_spec(w)?
                ))
            })
            .collect::<Option<_>>()?;
        sql.push_str(" WINDOW ");
        sql.push_str(&windows.join(", "));
    }
    sql.push_str(&sort_and_limit(sel)?);
    Some(sql)
}

/// `ORDER BY …` and the `LIMIT`/`OFFSET` (or `FETCH FIRST`) tail.
fn sort_and_limit(sel: &ast::SelectStmt) -> Option<String> {
    let mut sql = String::new();
    if !sel.sort_clause.is_empty() {
        let sorts: Vec<String> = sel
            .sort_clause
            .iter()
            .map(|n| sort_by(n.as_ref()))
            .collect::<Option<_>>()?;
        sql.push_str(" ORDER BY ");
        sql.push_str(&sorts.join(", "));
    }
    if let count @ Some(_) = &sel.limit_count {
        if sel.limit_option == LimitOption::WithTies {
            sql.push_str(&format!(
                " FETCH FIRST {} ROWS WITH TIES",
                expr(count.as_ref())?
            ));
        } else {
            sql.push_str(&format!(" LIMIT {}", expr(count.as_ref())?));
        }
    }
    // `LIMIT ALL` needs no case of its own: the grammar stores it as a NULL
    // constant count (`LIMIT NULL` parses the same), and a select without a
    // LIMIT has no count and `LimitOption::Count`, PostgreSQL's default.
    if let offset @ Some(_) = &sel.limit_offset {
        sql.push_str(&format!(" OFFSET {}", expr(offset.as_ref())?));
    }
    Some(sql)
}

fn insert_stmt(ins: &ast::InsertStmt) -> Option<String> {
    let mut sql = String::new();
    if let Some(with) = ins.with_clause.as_ref() {
        sql.push_str(&with_clause(with)?);
        sql.push(' ');
    }
    sql.push_str("INSERT INTO ");
    sql.push_str(&range_var(ins.relation.as_ref()?)?);
    if !ins.cols.is_empty() {
        let cols: Vec<String> = ins
            .cols
            .iter()
            .map(|c| {
                let Node::ResTarget(target) = c.as_ref()? else {
                    return None;
                };
                if target.name.is_empty() {
                    return None;
                }
                Some(quote_identifier(&target.name))
            })
            .collect::<Option<_>>()?;
        sql.push_str(&format!(" ({})", cols.join(", ")));
    }
    sql.push(' ');
    sql.push_str(&statement(ins.select_stmt.as_ref())?);
    if let Some(on_conflict) = ins.on_conflict_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&on_conflict_clause(on_conflict)?);
    }
    if let Some(returning) = ins.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn update_stmt(upd: &ast::UpdateStmt) -> Option<String> {
    let mut sql = String::new();
    if let Some(with) = upd.with_clause.as_ref() {
        sql.push_str(&with_clause(with)?);
        sql.push(' ');
    }
    sql.push_str("UPDATE ");
    sql.push_str(&range_var(upd.relation.as_ref()?)?);
    let sets: Vec<String> = upd
        .target_list
        .iter()
        .map(|n| set_target(n.as_ref()))
        .collect::<Option<_>>()?;
    if sets.is_empty() {
        return None;
    }
    sql.push_str(" SET ");
    sql.push_str(&sets.join(", "));
    if !upd.from_clause.is_empty() {
        let from: Vec<String> = upd
            .from_clause
            .iter()
            .map(|n| from_item(n.as_ref()))
            .collect::<Option<_>>()?;
        sql.push_str(" FROM ");
        sql.push_str(&from.join(", "));
    }
    if let where_clause @ Some(_) = &upd.where_clause {
        sql.push_str(" WHERE ");
        sql.push_str(&expr(where_clause.as_ref())?);
    }
    if let Some(returning) = upd.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn delete_stmt(del: &ast::DeleteStmt) -> Option<String> {
    let mut sql = String::new();
    if let Some(with) = del.with_clause.as_ref() {
        sql.push_str(&with_clause(with)?);
        sql.push(' ');
    }
    sql.push_str("DELETE FROM ");
    sql.push_str(&range_var(del.relation.as_ref()?)?);
    if !del.using_clause.is_empty() {
        let using: Vec<String> = del
            .using_clause
            .iter()
            .map(|n| from_item(n.as_ref()))
            .collect::<Option<_>>()?;
        sql.push_str(" USING ");
        sql.push_str(&using.join(", "));
    }
    if let where_clause @ Some(_) = &del.where_clause {
        sql.push_str(" WHERE ");
        sql.push_str(&expr(where_clause.as_ref())?);
    }
    if let Some(returning) = del.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn merge_stmt(mrg: &ast::MergeStmt) -> Option<String> {
    let mut sql = String::new();
    if let Some(with) = mrg.with_clause.as_ref() {
        sql.push_str(&with_clause(with)?);
        sql.push(' ');
    }
    sql.push_str("MERGE INTO ");
    sql.push_str(&range_var(mrg.relation.as_ref()?)?);
    sql.push_str(" USING ");
    sql.push_str(&from_item(mrg.source_relation.as_ref())?);
    sql.push_str(" ON ");
    sql.push_str(&expr(mrg.join_condition.as_ref())?);
    for when in &mrg.merge_when_clauses {
        sql.push(' ');
        sql.push_str(&merge_when_clause(when.as_ref())?);
    }
    if let Some(returning) = mrg.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn merge_when_clause(node: Option<&ast::Node>) -> Option<String> {
    let Node::MergeWhenClause(when) = node? else {
        return None;
    };
    let mut sql = String::from("WHEN ");
    sql.push_str(match when.match_kind {
        MergeMatchKind::MergeWhenMatched => "MATCHED",
        MergeMatchKind::MergeWhenNotMatchedBySource => "NOT MATCHED BY SOURCE",
        MergeMatchKind::MergeWhenNotMatchedByTarget => "NOT MATCHED",
    });
    if let condition @ Some(_) = &when.condition {
        sql.push_str(" AND ");
        sql.push_str(&expr(condition.as_ref())?);
    }
    sql.push_str(" THEN ");
    match when.command_type {
        ast::CmdType::CmdUpdate => {
            let sets: Vec<String> = when
                .target_list
                .iter()
                .map(|n| set_target(n.as_ref()))
                .collect::<Option<_>>()?;
            if sets.is_empty() {
                return None;
            }
            sql.push_str("UPDATE SET ");
            sql.push_str(&sets.join(", "));
        }
        ast::CmdType::CmdDelete => sql.push_str("DELETE"),
        ast::CmdType::CmdNothing => sql.push_str("DO NOTHING"),
        ast::CmdType::CmdInsert => {
            sql.push_str("INSERT");
            if !when.target_list.is_empty() {
                let cols: Vec<String> = when
                    .target_list
                    .iter()
                    .map(|c| {
                        let Node::ResTarget(target) = c.as_ref()? else {
                            return None;
                        };
                        (!target.name.is_empty()).then(|| quote_identifier(&target.name))
                    })
                    .collect::<Option<_>>()?;
                sql.push_str(&format!(" ({})", cols.join(", ")));
            }
            let values: Vec<String> = when
                .values
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<_>>()?;
            sql.push_str(&format!(" VALUES ({})", values.join(", ")));
        }
        _ => return None,
    }
    Some(sql)
}

fn on_conflict_clause(clause: &ast::OnConflictClause) -> Option<String> {
    let mut sql = String::from("ON CONFLICT");
    if let Some(infer) = clause.infer.as_deref() {
        let elems: Vec<String> = infer
            .index_elems
            .iter()
            .map(|n| index_elem(n.as_ref()))
            .collect::<Option<_>>()?;
        sql.push_str(&format!(" ({})", elems.join(", ")));
        if let where_clause @ Some(_) = &infer.where_clause {
            sql.push_str(" WHERE ");
            sql.push_str(&expr(where_clause.as_ref())?);
        }
    }
    match clause.action {
        ast::OnConflictAction::OnconflictNothing => sql.push_str(" DO NOTHING"),
        ast::OnConflictAction::OnconflictUpdate => {
            let sets: Vec<String> = clause
                .target_list
                .iter()
                .map(|n| set_target(n.as_ref()))
                .collect::<Option<_>>()?;
            if sets.is_empty() {
                return None;
            }
            sql.push_str(" DO UPDATE SET ");
            sql.push_str(&sets.join(", "));
            if let where_clause @ Some(_) = &clause.where_clause {
                sql.push_str(" WHERE ");
                sql.push_str(&expr(where_clause.as_ref())?);
            }
        }
        _ => return None,
    }
    Some(sql)
}

fn index_elem(node: Option<&ast::Node>) -> Option<String> {
    let Node::IndexElem(elem) = node? else {
        return None;
    };
    if !elem.indexcolname.is_empty()
        || !elem.collation.is_empty()
        || !elem.opclass.is_empty()
        || !elem.opclassopts.is_empty()
        || !matches!(elem.ordering, SortByDir::SortbyDefault)
        || !matches!(elem.nulls_ordering, SortByNulls::SortbyNullsDefault)
    {
        return None;
    }
    if let expression @ Some(_) = &elem.expr {
        return expr(expression.as_ref());
    }
    (!elem.name.is_empty()).then(|| quote_identifier(&elem.name))
}

fn returning_clause(clause: &ast::ReturningClause) -> Option<String> {
    if !clause.options.is_empty() {
        // `RETURNING WITH (OLD AS …, NEW AS …)` isn't generated.
        return None;
    }
    let exprs: Vec<String> = clause
        .exprs
        .iter()
        .map(|n| res_target(n.as_ref()))
        .collect::<Option<_>>()?;
    if exprs.is_empty() {
        return None;
    }
    Some(format!("RETURNING {}", exprs.join(", ")))
}

fn with_clause(with: &ast::WithClause) -> Option<String> {
    let mut sql = String::from("WITH ");
    if with.recursive {
        sql.push_str("RECURSIVE ");
    }
    let ctes: Vec<String> = with
        .ctes
        .iter()
        .map(|n| common_table_expr(n.as_ref()))
        .collect::<Option<_>>()?;
    if ctes.is_empty() {
        return None;
    }
    sql.push_str(&ctes.join(", "));
    Some(sql)
}

fn common_table_expr(node: Option<&ast::Node>) -> Option<String> {
    let Node::CommonTableExpr(cte) = node? else {
        return None;
    };
    if cte.search_clause.is_some() || cte.cycle_clause.is_some() {
        return None;
    }
    let mut sql = quote_identifier(&cte.ctename);
    if !cte.aliascolnames.is_empty() {
        let cols = strings(&cte.aliascolnames)?;
        sql.push_str(&format!(
            " ({})",
            cols.iter()
                .map(|c| quote_identifier(c))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    sql.push_str(" AS ");
    match cte.ctematerialized {
        ast::CteMaterialize::Default => {}
        ast::CteMaterialize::Always => sql.push_str("MATERIALIZED "),
        ast::CteMaterialize::Never => sql.push_str("NOT MATERIALIZED "),
    }
    sql.push('(');
    sql.push_str(&statement(cte.ctequery.as_ref())?);
    sql.push(')');
    Some(sql)
}

fn range_var(var: &ast::RangeVar) -> Option<String> {
    if !var.catalogname.is_empty() {
        return None;
    }
    let mut sql = String::new();
    if !var.inh {
        sql.push_str("ONLY ");
    }
    if !var.schemaname.is_empty() {
        sql.push_str(&quote_identifier(&var.schemaname));
        sql.push('.');
    }
    sql.push_str(&quote_identifier(&var.relname));
    if let Some(alias) = var.alias.as_ref() {
        sql.push_str(&alias_sql(alias)?);
    }
    Some(sql)
}

fn alias_sql(alias: &ast::Alias) -> Option<String> {
    if alias.aliasname.is_empty() {
        return None;
    }
    let mut sql = format!(" AS {}", quote_identifier(&alias.aliasname));
    if !alias.colnames.is_empty() {
        let cols = strings(&alias.colnames)?;
        sql.push_str(&format!(
            " ({})",
            cols.iter()
                .map(|c| quote_identifier(c))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Some(sql)
}

fn from_item(node: Option<&ast::Node>) -> Option<String> {
    match node? {
        Node::RangeVar(var) => range_var(var),
        Node::RangeSubselect(sub) => {
            let mut sql = String::new();
            if sub.lateral {
                sql.push_str("LATERAL ");
            }
            sql.push('(');
            sql.push_str(&statement(sub.subquery.as_ref())?);
            sql.push(')');
            if let Some(alias) = sub.alias.as_ref() {
                sql.push_str(&alias_sql(alias)?);
            }
            Some(sql)
        }
        Node::RangeFunction(func) => range_function(func),
        Node::JoinExpr(join) => join_expr(join),
        _ => None,
    }
}

fn range_function(func: &ast::RangeFunction) -> Option<String> {
    if func.is_rowsfrom {
        return None;
    }
    let mut sql = String::new();
    if func.lateral {
        sql.push_str("LATERAL ");
    }
    let mut calls = Vec::new();
    for function in &func.functions {
        let Node::List(list) = function.as_ref()? else {
            return None;
        };
        match list.items.as_slice() {
            [call] => calls.push(expr(call.as_ref())?),
            _ => return None,
        }
    }
    if calls.is_empty() {
        return None;
    }
    sql.push_str(&calls.join(", "));
    if func.ordinality {
        sql.push_str(" WITH ORDINALITY");
    }
    if !func.coldeflist.is_empty() {
        return None;
    }
    if let Some(alias) = func.alias.as_ref() {
        sql.push_str(&alias_sql(alias)?);
    }
    Some(sql)
}

fn join_expr(join: &ast::JoinExpr) -> Option<String> {
    if join.join_using_alias.is_some() {
        return None;
    }
    let qualifier = match join.jointype {
        JoinType::JoinInner => "",
        JoinType::JoinLeft => "LEFT ",
        JoinType::JoinRight => "RIGHT ",
        JoinType::JoinFull => "FULL ",
        _ => return None,
    };
    let natural = if join.is_natural { "NATURAL " } else { "" };
    let larg = from_item(join.larg.as_ref())?;
    let rarg = from_item(join.rarg.as_ref())?;
    let mut sql = if join.quals.is_none() && join.using_clause.is_empty() {
        // Inner joins without a condition: `CROSS JOIN` keeps the tree.
        if join.jointype != JoinType::JoinInner || join.is_natural {
            return None;
        }
        format!("{larg} CROSS JOIN {rarg}")
    } else if !join.using_clause.is_empty() {
        let cols = strings(&join.using_clause)?;
        format!(
            "{larg} {natural}{qualifier}JOIN {rarg} USING ({})",
            cols.iter()
                .map(|c| quote_identifier(c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!(
            "{larg} {natural}{qualifier}JOIN {rarg} ON {}",
            expr(join.quals.as_ref())?
        )
    };
    if let Some(alias) = join.alias.as_ref() {
        sql = format!("({sql}){}", alias_sql(alias)?);
    }
    Some(sql)
}

// ──────────────────────────────────────────────────────────────────────────
// Target lists and clause nodes.
// ──────────────────────────────────────────────────────────────────────────

fn res_target(node: Option<&ast::Node>) -> Option<String> {
    let Node::ResTarget(target) = node? else {
        return None;
    };
    if !target.indirection.is_empty() {
        // `x = …` targets with subscripts aren't produced by the fuzzer.
        return None;
    }
    if target_star(target.val.as_ref()) {
        return Some("*".to_string());
    }
    let val = expr(target.val.as_ref())?;
    if target.name.is_empty() {
        Some(val)
    } else {
        Some(format!("{val} AS {}", quote_identifier(&target.name)))
    }
}

/// An assignment target (`UPDATE … SET c = v`, `ON CONFLICT DO UPDATE`,
/// MERGE's UPDATE action): `ResTarget` with the column name and the value.
fn set_target(node: Option<&ast::Node>) -> Option<String> {
    let Node::ResTarget(target) = node? else {
        return None;
    };
    if target.name.is_empty() || !target.indirection.is_empty() {
        return None;
    }
    Some(format!(
        "{} = {}",
        quote_identifier(&target.name),
        expr(target.val.as_ref())?
    ))
}

/// Whether a target is a bare `*` (`SELECT *`, `RETURNING *`).
fn target_star(node: Option<&ast::Node>) -> bool {
    matches!(node, Some(Node::ColumnRef(cr))
        if cr.fields.len() == 1 && matches!(cr.fields[0], Some(Node::AStar(_))))
}

fn sort_by(node: Option<&ast::Node>) -> Option<String> {
    let Node::SortBy(sort) = node? else {
        return None;
    };
    if !sort.use_op.is_empty() {
        return None;
    }
    let mut sql = expr(sort.node.as_ref())?;
    match sort.sortby_dir {
        SortByDir::SortbyAsc | SortByDir::SortbyDefault => {}
        SortByDir::SortbyDesc => sql.push_str(" DESC"),
        SortByDir::SortbyUsing => return None,
    }
    match sort.sortby_nulls {
        SortByNulls::SortbyNullsDefault => {}
        SortByNulls::SortbyNullsFirst => sql.push_str(" NULLS FIRST"),
        SortByNulls::SortbyNullsLast => sql.push_str(" NULLS LAST"),
    }
    Some(sql)
}

fn grouping_element(node: Option<&ast::Node>) -> Option<String> {
    let Some(Node::GroupingSet(set)) = node else {
        return expr(node);
    };
    match set.kind {
        GroupingSetKind::GroupingSetSimple => {
            let items = set
                .content
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            Some(format!("({})", items.join(", ")))
        }
        GroupingSetKind::GroupingSetEmpty => Some("()".to_string()),
        GroupingSetKind::GroupingSetRollup | GroupingSetKind::GroupingSetCube => {
            let keyword = if set.kind == GroupingSetKind::GroupingSetRollup {
                "ROLLUP"
            } else {
                "CUBE"
            };
            let items = set
                .content
                .iter()
                .map(|n| grouping_element(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            Some(format!("{keyword} ({})", items.join(", ")))
        }
        GroupingSetKind::GroupingSetSets => {
            // A parenthesized simple set is just its expression (the parens
            // don't make a node); the empty set and nested ROLLUP/CUBE are
            // `GroupingSet`s of their own.
            let items = set
                .content
                .iter()
                .map(|n| grouping_element(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            Some(format!("GROUPING SETS ({})", items.join(", ")))
        }
    }
}

fn window_spec(window: &ast::WindowDef) -> Option<String> {
    if !window.name.is_empty() {
        return None;
    }
    if !window.refname.is_empty() {
        if !window.partition_clause.is_empty()
            || !window.order_clause.is_empty()
            || window.frame_options != 0
        {
            return None;
        }
        return Some(quote_identifier(&window.refname));
    }
    let mut parts = Vec::new();
    if !window.partition_clause.is_empty() {
        let items = window
            .partition_clause
            .iter()
            .map(|n| expr(n.as_ref()))
            .collect::<Option<Vec<_>>>()?;
        parts.push(format!("PARTITION BY {}", items.join(", ")));
    }
    if !window.order_clause.is_empty() {
        let items = window
            .order_clause
            .iter()
            .map(|n| sort_by(n.as_ref()))
            .collect::<Option<Vec<_>>>()?;
        parts.push(format!("ORDER BY {}", items.join(", ")));
    }
    // An explicit frame (`FRAMEOPTION_NONDEFAULT`) survives; the parser's
    // implicit default (`RANGE UNBOUNDED PRECEDING`) must not be written out,
    // and writing it out changes the tree.
    if window.frame_options != 0 && window.frame_options != FRAMEOPTION_DEFAULTS {
        parts.push(frame_clause(window)?);
    }
    Some(format!("({})", parts.join(" ")))
}

// The FRAMEOPTION_* bits of PostgreSQL's parsenodes.h.
const FRAMEOPTION_NONDEFAULT: i32 = 0x0001;
const FRAMEOPTION_RANGE: i32 = 0x0002;
const FRAMEOPTION_ROWS: i32 = 0x0004;
const FRAMEOPTION_GROUPS: i32 = 0x0008;
const FRAMEOPTION_BETWEEN: i32 = 0x0010;
const FRAMEOPTION_START_UNBOUNDED_PRECEDING: i32 = 0x0020;
const FRAMEOPTION_END_UNBOUNDED_PRECEDING: i32 = 0x0040;
const FRAMEOPTION_START_UNBOUNDED_FOLLOWING: i32 = 0x0080;
const FRAMEOPTION_END_UNBOUNDED_FOLLOWING: i32 = 0x0100;
const FRAMEOPTION_START_CURRENT_ROW: i32 = 0x0200;
const FRAMEOPTION_END_CURRENT_ROW: i32 = 0x0400;
const FRAMEOPTION_START_OFFSET_PRECEDING: i32 = 0x0800;
const FRAMEOPTION_END_OFFSET_PRECEDING: i32 = 0x1000;
const FRAMEOPTION_START_OFFSET_FOLLOWING: i32 = 0x2000;
const FRAMEOPTION_END_OFFSET_FOLLOWING: i32 = 0x4000;

/// PostgreSQL's `FRAMEOPTION_DEFAULTS`.
const FRAMEOPTION_DEFAULTS: i32 =
    FRAMEOPTION_RANGE | FRAMEOPTION_START_UNBOUNDED_PRECEDING | FRAMEOPTION_END_CURRENT_ROW;

fn frame_clause(window: &ast::WindowDef) -> Option<String> {
    let options = window.frame_options;
    let known = FRAMEOPTION_NONDEFAULT
        | FRAMEOPTION_RANGE
        | FRAMEOPTION_ROWS
        | FRAMEOPTION_GROUPS
        | FRAMEOPTION_BETWEEN
        | FRAMEOPTION_START_UNBOUNDED_PRECEDING
        | FRAMEOPTION_END_UNBOUNDED_PRECEDING
        | FRAMEOPTION_START_UNBOUNDED_FOLLOWING
        | FRAMEOPTION_END_UNBOUNDED_FOLLOWING
        | FRAMEOPTION_START_CURRENT_ROW
        | FRAMEOPTION_END_CURRENT_ROW
        | FRAMEOPTION_START_OFFSET_PRECEDING
        | FRAMEOPTION_END_OFFSET_PRECEDING
        | FRAMEOPTION_START_OFFSET_FOLLOWING
        | FRAMEOPTION_END_OFFSET_FOLLOWING;
    if options & !known != 0 {
        return None;
    }
    let mode = if options & FRAMEOPTION_RANGE != 0 {
        "RANGE"
    } else if options & FRAMEOPTION_ROWS != 0 {
        "ROWS"
    } else if options & FRAMEOPTION_GROUPS != 0 {
        "GROUPS"
    } else {
        return None;
    };
    let start = frame_bound(
        options,
        window.start_offset.as_ref(),
        FRAMEOPTION_START_UNBOUNDED_PRECEDING,
        FRAMEOPTION_START_UNBOUNDED_FOLLOWING,
        FRAMEOPTION_START_CURRENT_ROW,
        FRAMEOPTION_START_OFFSET_PRECEDING,
        FRAMEOPTION_START_OFFSET_FOLLOWING,
    )?;
    if options & FRAMEOPTION_BETWEEN != 0 {
        let end = frame_bound(
            options,
            window.end_offset.as_ref(),
            FRAMEOPTION_END_UNBOUNDED_PRECEDING,
            FRAMEOPTION_END_UNBOUNDED_FOLLOWING,
            FRAMEOPTION_END_CURRENT_ROW,
            FRAMEOPTION_END_OFFSET_PRECEDING,
            FRAMEOPTION_END_OFFSET_FOLLOWING,
        )?;
        Some(format!("{mode} BETWEEN {start} AND {end}"))
    } else {
        Some(format!("{mode} {start}"))
    }
}

fn frame_bound(
    options: i32,
    offset: Option<&ast::Node>,
    unbounded_preceding: i32,
    unbounded_following: i32,
    current_row: i32,
    offset_preceding: i32,
    offset_following: i32,
) -> Option<String> {
    if options & unbounded_preceding != 0 {
        return Some("UNBOUNDED PRECEDING".to_string());
    }
    if options & unbounded_following != 0 {
        return Some("UNBOUNDED FOLLOWING".to_string());
    }
    if options & current_row != 0 {
        return Some("CURRENT ROW".to_string());
    }
    if options & offset_preceding != 0 {
        return Some(format!("{} PRECEDING", expr(offset)?));
    }
    if options & offset_following != 0 {
        return Some(format!("{} FOLLOWING", expr(offset)?));
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────
// Expressions.
// ──────────────────────────────────────────────────────────────────────────

fn expr(node: Option<&ast::Node>) -> Option<String> {
    match node? {
        Node::ColumnRef(column) => column_ref(column),
        Node::AConst(c) => a_const(c),
        Node::ParamRef(param) => Some(format!("${}", param.number)),
        Node::AExpr(a) => a_expr(a),
        Node::BoolExpr(b) => bool_expr(b),
        Node::NullTest(test) => null_test(test),
        Node::BooleanTest(test) => boolean_test(test),
        Node::FuncCall(call) => func_call(call),
        Node::TypeCast(cast) => Some(format!(
            "CAST({} AS {})",
            expr(cast.arg.as_ref())?,
            type_name(cast.type_name.as_ref()?)?
        )),
        Node::CaseExpr(case) => case_expr(case),
        Node::CoalesceExpr(coalesce) => {
            let args = coalesce
                .args
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            if args.is_empty() {
                return None;
            }
            Some(format!("COALESCE({})", args.join(", ")))
        }
        Node::MinMaxExpr(minmax) => {
            let keyword = match minmax.op {
                MinMaxOp::IsGreatest => "GREATEST",
                MinMaxOp::IsLeast => "LEAST",
            };
            let args = minmax
                .args
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            if args.is_empty() {
                return None;
            }
            Some(format!("{keyword}({})", args.join(", ")))
        }
        Node::SubLink(link) => sub_link(link),
        Node::AArrayExpr(array) => {
            let elements = array
                .elements
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            if elements.is_empty() {
                return None;
            }
            Some(format!("ARRAY[{}]", elements.join(", ")))
        }
        Node::AIndirection(indirection) => indirection_sql(indirection),
        Node::CollateClause(collate) => {
            if collate.collname.is_empty() {
                return None;
            }
            let names = strings(&collate.collname)?;
            Some(format!(
                "({} COLLATE {})",
                expr(collate.arg.as_ref())?,
                names
                    .iter()
                    .map(|n| quote_identifier(n))
                    .collect::<Vec<_>>()
                    .join(".")
            ))
        }
        Node::RowExpr(row) => row_expr(row),
        Node::SqlValueFunction(func) => sql_value_function(func),
        Node::SetToDefault(_) => Some("DEFAULT".to_string()),
        _ => None,
    }
}

fn column_ref(column: &ast::ColumnRef) -> Option<String> {
    if column.fields.is_empty() {
        return None;
    }
    let fields: Vec<String> = column
        .fields
        .iter()
        .map(|field| match field.as_ref()? {
            Node::String(s) => Some(quote_identifier(&s.sval)),
            Node::AStar(_) => Some("*".to_string()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    Some(fields.join("."))
}

fn a_const(c: &ast::AConst) -> Option<String> {
    if c.isnull {
        return Some("NULL".to_string());
    }
    match c.val.as_ref()? {
        ast::AConstValue::Integer(integer) => {
            if integer.ival == i32::MIN {
                return None;
            }
            Some(integer.ival.to_string())
        }
        ast::AConstValue::Float(float) => {
            let valid = !float.fval.is_empty()
                && float
                    .fval
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
            valid.then(|| float.fval.clone())
        }
        ast::AConstValue::Boolean(boolean) => Some(if boolean.boolval {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        ast::AConstValue::String(string) => Some(quote_literal(&string.sval)),
        ast::AConstValue::BitString(bits) => {
            let valid = bits.bsval.chars().all(|c| matches!(c, '0' | '1'));
            valid.then(|| format!("B'{}'", bits.bsval))
        }
    }
}

/// A standard-conforming string constant: `'…'`, with `''` for a quote.
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn a_expr(expr_node: &ast::AExpr) -> Option<String> {
    let op = operator_name(&expr_node.name)?;
    let lexpr = match &expr_node.lexpr {
        node @ Some(_) => Some(expr(node.as_ref())?),
        None => None,
    };
    let rexpr @ Some(_) = &expr_node.rexpr else {
        return None;
    };
    match expr_node.kind {
        AExprKind::AexprOp => match lexpr {
            Some(left) => Some(format!("({left} {op} {})", expr(rexpr.as_ref())?)),
            None => {
                // Prefix operators: quoted or not, only the symbolic ones
                // PostgreSQL parses in that position.
                if !matches!(op.as_str(), "+" | "-" | "~" | "@") {
                    return None;
                }
                Some(format!("({op} {})", expr(rexpr.as_ref())?))
            }
        },
        AExprKind::AexprOpAny => Some(format!("({} {op} ANY ({}))", lexpr?, expr(rexpr.as_ref())?)),
        AExprKind::AexprOpAll => Some(format!("({} {op} ALL ({}))", lexpr?, expr(rexpr.as_ref())?)),
        AExprKind::AexprDistinct => Some(format!(
            "({} IS DISTINCT FROM {})",
            lexpr?,
            expr(rexpr.as_ref())?
        )),
        AExprKind::AexprNotDistinct => Some(format!(
            "({} IS NOT DISTINCT FROM {})",
            lexpr?,
            expr(rexpr.as_ref())?
        )),
        AExprKind::AexprNullif => Some(format!("NULLIF({}, {})", lexpr?, expr(rexpr.as_ref())?)),
        AExprKind::AexprIn => {
            let Node::List(list) = rexpr.as_ref()? else {
                return None;
            };
            let items = list
                .items
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            if items.is_empty() {
                return None;
            }
            let keyword = match &expr_node.name[..] {
                [name] if string_of(name.as_ref()) == Some("=") => "IN",
                [name] if string_of(name.as_ref()) == Some("<>") => "NOT IN",
                _ => return None,
            };
            Some(format!("({} {keyword} ({}))", lexpr?, items.join(", ")))
        }
        AExprKind::AexprLike | AExprKind::AexprIlike => {
            let [name] = &expr_node.name[..] else {
                return None;
            };
            let keyword = match (expr_node.kind, string_of(name.as_ref())) {
                (AExprKind::AexprLike, Some("~~")) => "LIKE",
                (AExprKind::AexprLike, Some("!~~")) => "NOT LIKE",
                (AExprKind::AexprIlike, Some("~~*")) => "ILIKE",
                (AExprKind::AexprIlike, Some("!~~*")) => "NOT ILIKE",
                _ => return None,
            };
            Some(format!("({} {keyword} {})", lexpr?, expr(rexpr.as_ref())?))
        }
        AExprKind::AexprSimilar => {
            let [name] = &expr_node.name[..] else {
                return None;
            };
            let keyword = match string_of(name.as_ref()) {
                Some("~") => "SIMILAR TO",
                Some("!~") => "NOT SIMILAR TO",
                _ => return None,
            };
            Some(format!("({} {keyword} {})", lexpr?, expr(rexpr.as_ref())?))
        }
        AExprKind::AexprBetween
        | AExprKind::AexprNotBetween
        | AExprKind::AexprBetweenSym
        | AExprKind::AexprNotBetweenSym => {
            let Node::List(list) = rexpr.as_ref()? else {
                return None;
            };
            let [low, high] = list.items.as_slice() else {
                return None;
            };
            let keyword = match expr_node.kind {
                AExprKind::AexprBetween => "BETWEEN",
                AExprKind::AexprNotBetween => "NOT BETWEEN",
                AExprKind::AexprBetweenSym => "BETWEEN SYMMETRIC",
                _ => "NOT BETWEEN SYMMETRIC",
            };
            Some(format!(
                "({} {keyword} {} AND {})",
                lexpr?,
                expr(low.as_ref())?,
                expr(high.as_ref())?
            ))
        }
    }
}

fn bool_expr(bool: &ast::BoolExpr) -> Option<String> {
    match bool.boolop {
        BoolExprType::AndExpr | BoolExprType::OrExpr => {
            let keyword = if bool.boolop == BoolExprType::AndExpr {
                " AND "
            } else {
                " OR "
            };
            let args = bool
                .args
                .iter()
                .map(|n| expr(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            if args.len() < 2 {
                return None;
            }
            Some(format!("({})", args.join(keyword)))
        }
        BoolExprType::NotExpr => {
            let [arg] = bool.args.as_slice() else {
                return None;
            };
            Some(format!("(NOT {})", expr(arg.as_ref())?))
        }
    }
}

fn null_test(test: &ast::NullTest) -> Option<String> {
    let keyword = match test.nulltesttype {
        NullTestType::IsNull => "IS NULL",
        NullTestType::IsNotNull => "IS NOT NULL",
    };
    Some(format!("({} {keyword})", expr(test.arg.as_ref())?))
}

fn boolean_test(test: &ast::BooleanTest) -> Option<String> {
    let keyword = match test.booltesttype {
        BoolTestType::IsTrue => "IS TRUE",
        BoolTestType::IsNotTrue => "IS NOT TRUE",
        BoolTestType::IsFalse => "IS FALSE",
        BoolTestType::IsNotFalse => "IS NOT FALSE",
        BoolTestType::IsUnknown => "IS UNKNOWN",
        BoolTestType::IsNotUnknown => "IS NOT UNKNOWN",
    };
    Some(format!("({} {keyword})", expr(test.arg.as_ref())?))
}

fn func_call(call: &ast::FuncCall) -> Option<String> {
    if call.funcformat == CoercionForm::CoerceSqlSyntax {
        return sql_syntax_func_call(call);
    }
    let name = func_name(&call.funcname)?;
    let mut sql = String::new();
    sql.push_str(&name);
    sql.push('(');
    if call.agg_star {
        sql.push('*');
    } else {
        if call.agg_distinct {
            sql.push_str("DISTINCT ");
        }
        let mut args = call
            .args
            .iter()
            .map(|n| expr(n.as_ref()))
            .collect::<Option<Vec<_>>>()?;
        if call.func_variadic {
            let last = args.last_mut()?;
            *last = format!("VARIADIC {last}");
        }
        sql.push_str(&args.join(", "));
        if !call.agg_within_group && !call.agg_order.is_empty() {
            let order = call
                .agg_order
                .iter()
                .map(|n| sort_by(n.as_ref()))
                .collect::<Option<Vec<_>>>()?;
            if !args.is_empty() {
                sql.push(' ');
            }
            sql.push_str(&format!("ORDER BY {}", order.join(", ")));
        }
    }
    sql.push(')');
    if call.agg_within_group {
        let order = call
            .agg_order
            .iter()
            .map(|n| sort_by(n.as_ref()))
            .collect::<Option<Vec<_>>>()?;
        if order.is_empty() {
            return None;
        }
        sql.push_str(&format!(" WITHIN GROUP (ORDER BY {})", order.join(", ")));
    }
    if let filter @ Some(_) = &call.agg_filter {
        sql.push_str(&format!(" FILTER (WHERE {})", expr(filter.as_ref())?));
    }
    if let Some(over) = call.over.as_deref() {
        sql.push_str(" OVER ");
        sql.push_str(&window_spec(over)?);
    }
    Some(sql)
}

/// SQL-syntax function calls: `extract(year FROM …)`, `substring(…)`, `trim(…)`.
fn sql_syntax_func_call(call: &ast::FuncCall) -> Option<String> {
    let names = strings(&call.funcname)?;
    // Only the exact name the grammar itself builds: a mutation can leave a
    // different name with this `funcformat`, which no SQL syntax can express.
    if names.as_slice() == ["pg_catalog", "extract"] {
        let [field, arg] = call.args.as_slice() else {
            return None;
        };
        let field = match field.as_ref()? {
            Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
                ast::AConstValue::String(s) => s.sval.clone(),
                _ => return None,
            },
            _ => return None,
        };
        // A string constant, so a field name that is a keyword (or came from a
        // mutation) still parses to the same `A_Const`.
        Some(format!(
            "extract({} FROM {})",
            quote_literal(&field),
            expr(arg.as_ref())?
        ))
    } else if names.as_slice() == ["pg_catalog", "substring"] {
        // `substring(a FROM b [FOR c])`: a missing FOR is a `None` node.
        let present = |node: Option<&ast::Node>| node.is_some();
        match call.args.as_slice() {
            [a, b] if present(a.as_ref()) && present(b.as_ref()) => Some(format!(
                "substring({} FROM {})",
                expr(a.as_ref())?,
                expr(b.as_ref())?
            )),
            [a, b, c] if present(a.as_ref()) && present(b.as_ref()) && present(c.as_ref()) => {
                Some(format!(
                    "substring({} FROM {} FOR {})",
                    expr(a.as_ref())?,
                    expr(b.as_ref())?,
                    expr(c.as_ref())?
                ))
            }
            [a, b, c] if present(a.as_ref()) && !present(b.as_ref()) && present(c.as_ref()) => {
                Some(format!(
                    "substring({} FOR {})",
                    expr(a.as_ref())?,
                    expr(c.as_ref())?
                ))
            }
            _ => None,
        }
    } else if names.len() == 2
        && names[0] == "pg_catalog"
        && matches!(names[1].as_str(), "btrim" | "ltrim" | "rtrim")
    {
        // `TRIM(…)`: the grammar maps the direction to the function name
        // (BOTH → btrim, LEADING → ltrim, TRAILING → rtrim) and passes the
        // source first, then the characters to trim.
        let direction = match names[1].as_str() {
            "ltrim" => Some("LEADING"),
            "rtrim" => Some("TRAILING"),
            _ => None,
        };
        let args = call
            .args
            .iter()
            .map(|n| expr(n.as_ref()))
            .collect::<Option<Vec<_>>>()?;
        match (direction, args.as_slice()) {
            (Some(direction), [source]) => Some(format!("trim({direction} FROM {source})")),
            (Some(direction), [source, chars]) => {
                Some(format!("trim({direction} {chars} FROM {source})"))
            }
            (None, [source]) => Some(format!("trim({source})")),
            (None, [source, chars]) => Some(format!("trim({chars} FROM {source})")),
            _ => None,
        }
    } else {
        None
    }
}

fn func_name(name: &[Option<ast::Node>]) -> Option<String> {
    let parts = strings(name)?;
    if parts.is_empty() {
        return None;
    }
    Some(
        parts
            .iter()
            .map(|part| quote_identifier(part))
            .collect::<Vec<_>>()
            .join("."),
    )
}

fn operator_name(name: &[Option<ast::Node>]) -> Option<String> {
    let parts = strings(name)?;
    match parts.as_slice() {
        [op] => Some(op.clone()),
        [schema @ .., op] => Some(format!(
            "OPERATOR({}.{op})",
            schema
                .iter()
                .map(|part| quote_identifier(part))
                .collect::<Vec<_>>()
                .join(".")
        )),
        [] => None,
    }
}

fn sub_link(link: &ast::SubLink) -> Option<String> {
    let inner = statement(link.subselect.as_ref())?;
    match link.sub_link_type {
        SubLinkType::ExistsSublink => Some(format!("EXISTS ({inner})")),
        SubLinkType::ExprSublink => Some(format!("({inner})")),
        SubLinkType::ArraySublink => Some(format!("ARRAY({inner})")),
        SubLinkType::AnySublink | SubLinkType::AllSublink => {
            let testexpr = expr(link.testexpr.as_ref())?;
            // `IN`/`NOT IN (subquery)`: the grammar leaves `oper_name` empty
            // for those; an explicit `= ANY (…)` / `<> ALL (…)` carries it.
            if link.oper_name.is_empty() {
                let keyword = if link.sub_link_type == SubLinkType::AnySublink {
                    "IN"
                } else {
                    "NOT IN"
                };
                return Some(format!("({testexpr} {keyword} ({inner}))"));
            }
            let keyword = if link.sub_link_type == SubLinkType::AnySublink {
                "ANY"
            } else {
                "ALL"
            };
            let op = operator_name(&link.oper_name)?;
            Some(format!("({testexpr} {op} {keyword} ({inner}))"))
        }
        _ => None,
    }
}

fn indirection_sql(indirection: &ast::AIndirection) -> Option<String> {
    let mut sql = expr(indirection.arg.as_ref())?;
    if indirection.indirection.is_empty() {
        return None;
    }
    for item in &indirection.indirection {
        match item.as_ref()? {
            Node::AIndices(indices) => {
                if indices.is_slice {
                    let low = match &indices.lidx {
                        node @ Some(_) => Some(expr(node.as_ref())?),
                        None => None,
                    };
                    let high = match &indices.uidx {
                        node @ Some(_) => Some(expr(node.as_ref())?),
                        None => None,
                    };
                    sql.push_str(&format!(
                        "[{}:{}]",
                        low.unwrap_or_default(),
                        high.unwrap_or_default()
                    ));
                } else {
                    // A single subscript is stored in `uidx` (the raw parser
                    // only fills `lidx` for a slice's lower bound).
                    let index = if indices.uidx.is_some() {
                        &indices.uidx
                    } else {
                        &indices.lidx
                    };
                    sql.push_str(&format!("[{}]", expr(index.as_ref())?));
                }
            }
            Node::String(field) => {
                sql.push('.');
                sql.push_str(&quote_identifier(&field.sval));
            }
            _ => return None,
        }
    }
    Some(format!("({sql})"))
}

fn row_expr(row: &ast::RowExpr) -> Option<String> {
    let args = row
        .args
        .iter()
        .map(|n| expr(n.as_ref()))
        .collect::<Option<Vec<_>>>()?;
    if args.is_empty() {
        return None;
    }
    match row.row_format {
        CoercionForm::CoerceExplicitCall => Some(format!("ROW({})", args.join(", "))),
        CoercionForm::CoerceImplicitCast => {
            if args.len() < 2 {
                return None;
            }
            Some(format!("({})", args.join(", ")))
        }
        _ => None,
    }
}

fn sql_value_function(func: &ast::SqlValueFunction) -> Option<String> {
    let typmod = |default: &str| -> String {
        if func.typmod < 0 {
            default.to_string()
        } else {
            format!("{default}({})", func.typmod)
        }
    };
    Some(match func.op {
        SqlValueFunctionOp::SvfopCurrentDate => "CURRENT_DATE".to_string(),
        SqlValueFunctionOp::SvfopCurrentTime => typmod("CURRENT_TIME"),
        SqlValueFunctionOp::SvfopCurrentTimeN => {
            if func.typmod < 0 {
                return None;
            }
            typmod("CURRENT_TIME")
        }
        SqlValueFunctionOp::SvfopCurrentTimestamp => typmod("CURRENT_TIMESTAMP"),
        SqlValueFunctionOp::SvfopCurrentTimestampN => {
            if func.typmod < 0 {
                return None;
            }
            typmod("CURRENT_TIMESTAMP")
        }
        SqlValueFunctionOp::SvfopLocaltime => typmod("LOCALTIME"),
        SqlValueFunctionOp::SvfopLocaltimeN => {
            if func.typmod < 0 {
                return None;
            }
            typmod("LOCALTIME")
        }
        SqlValueFunctionOp::SvfopLocaltimestamp => typmod("LOCALTIMESTAMP"),
        SqlValueFunctionOp::SvfopLocaltimestampN => {
            if func.typmod < 0 {
                return None;
            }
            typmod("LOCALTIMESTAMP")
        }
        SqlValueFunctionOp::SvfopCurrentRole => "CURRENT_ROLE".to_string(),
        SqlValueFunctionOp::SvfopCurrentUser => "CURRENT_USER".to_string(),
        SqlValueFunctionOp::SvfopUser => "USER".to_string(),
        SqlValueFunctionOp::SvfopSessionUser => "SESSION_USER".to_string(),
        SqlValueFunctionOp::SvfopCurrentCatalog => "CURRENT_CATALOG".to_string(),
        SqlValueFunctionOp::SvfopCurrentSchema => "CURRENT_SCHEMA".to_string(),
    })
}

fn type_name(type_name: &ast::TypeName) -> Option<String> {
    if type_name.setof || type_name.pct_type {
        return None;
    }
    let names = strings(&type_name.names)?;
    if names.is_empty() {
        return None;
    }
    let mut sql = names
        .iter()
        .map(|name| quote_identifier(name))
        .collect::<Vec<_>>()
        .join(".");
    if !type_name.typmods.is_empty() {
        let mods = type_name
            .typmods
            .iter()
            .map(|n| expr(n.as_ref()))
            .collect::<Option<Vec<_>>>()?;
        sql.push_str(&format!("({})", mods.join(", ")));
    }
    for bound in &type_name.array_bounds {
        match bound.as_ref()? {
            Node::Integer(integer) if integer.ival == -1 => sql.push_str("[]"),
            _ => return None,
        }
    }
    Some(sql)
}

fn case_expr(case: &ast::CaseExpr) -> Option<String> {
    let mut sql = String::from("CASE");
    if let arg @ Some(_) = &case.arg {
        sql.push(' ');
        sql.push_str(&expr(arg.as_ref())?);
    }
    for when in &case.args {
        let Node::CaseWhen(when) = when.as_ref()? else {
            return None;
        };
        sql.push_str(&format!(
            " WHEN {} THEN {}",
            expr(when.expr.as_ref())?,
            expr(when.result.as_ref())?
        ));
    }
    if let defresult @ Some(_) = &case.defresult {
        sql.push_str(&format!(" ELSE {}", expr(defresult.as_ref())?));
    }
    sql.push_str(" END");
    Some(sql)
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers.
// ──────────────────────────────────────────────────────────────────────────

/// The parts of a funcname / type name / collation name / column-name list,
/// each of which must be a plain `String` node.
fn strings(nodes: &[Option<ast::Node>]) -> Option<Vec<String>> {
    nodes
        .iter()
        .map(|node| string_of(node.as_ref()).map(str::to_owned))
        .collect()
}

fn string_of(node: Option<&ast::Node>) -> Option<&str> {
    match node? {
        Node::String(s) => Some(&s.sval),
        _ => None,
    }
}
