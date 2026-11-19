//! A small SQL printer for the differential fuzzer's mutated parse trees.
//!
//! The fuzzer mutates a parsed seed's protobuf tree and needs SQL back to
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
use typedpg_pg_query::NodeEnum;
use typedpg_pg_query::protobuf::{
    self, AExprKind, BoolExprType, BoolTestType, CmdType, CoercionForm, CteMaterialize,
    GroupingSetKind, JoinType, LimitOption, MergeMatchKind, MinMaxOp, NullTestType,
    OnConflictAction, SetOperation, SortByDir, SortByNulls, SqlValueFunctionOp, SubLinkType,
    a_const,
};

/// Render `tree` as SQL: its statements, joined by `"; "`.
///
/// Returns `None` if any part of the tree isn't supported: the caller then
/// skips the tree rather than feeding the oracle a wrong query.
pub(crate) fn print(tree: &protobuf::ParseResult) -> Option<String> {
    let mut stmts = Vec::with_capacity(tree.stmts.len());
    for raw in &tree.stmts {
        stmts.push(statement(raw.stmt.as_deref()?)?);
    }
    Some(stmts.join("; "))
}

/// One statement (a CTE body included): the node kinds the fuzzer produces.
fn statement(node: &protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        NodeEnum::SelectStmt(sel) => select_stmt(sel),
        NodeEnum::InsertStmt(ins) => insert_stmt(ins),
        NodeEnum::UpdateStmt(upd) => update_stmt(upd),
        NodeEnum::DeleteStmt(del) => delete_stmt(del),
        NodeEnum::MergeStmt(mrg) => merge_stmt(mrg),
        _ => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Statements.
// ──────────────────────────────────────────────────────────────────────────

fn select_stmt(sel: &protobuf::SelectStmt) -> Option<String> {
    // A set operation: both arms are selects of their own, parenthesized so
    // their grouping survives.
    if enumeration::<SetOperation>(sel.op)? != SetOperation::SetopNone {
        let op = match enumeration::<SetOperation>(sel.op)? {
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
                let NodeEnum::List(list) = row.node.as_ref()? else {
                    return None;
                };
                let exprs: Vec<String> = list.items.iter().map(expr).collect::<Option<_>>()?;
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
        if sel.distinct_clause.len() == 1 && sel.distinct_clause[0].node.is_none() {
            sql.push_str("DISTINCT ");
        } else {
            let items: Vec<String> = sel
                .distinct_clause
                .iter()
                .map(expr)
                .collect::<Option<_>>()?;
            sql.push_str(&format!("DISTINCT ON ({}) ", items.join(", ")));
        }
    }

    let targets: Vec<String> = sel
        .target_list
        .iter()
        .map(res_target)
        .collect::<Option<_>>()?;
    if targets.is_empty() {
        return None;
    }
    sql.push_str(&targets.join(", "));

    if !sel.from_clause.is_empty() {
        let from: Vec<String> = sel
            .from_clause
            .iter()
            .map(from_item)
            .collect::<Option<_>>()?;
        sql.push_str(" FROM ");
        sql.push_str(&from.join(", "));
    }
    if let Some(where_clause) = sel.where_clause.as_deref() {
        sql.push_str(" WHERE ");
        sql.push_str(&expr(where_clause)?);
    }
    if !sel.group_clause.is_empty() {
        let groups: Vec<String> = sel
            .group_clause
            .iter()
            .map(grouping_element)
            .collect::<Option<_>>()?;
        sql.push_str(" GROUP BY ");
        if sel.group_distinct {
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&groups.join(", "));
    }
    if let Some(having) = sel.having_clause.as_deref() {
        sql.push_str(" HAVING ");
        sql.push_str(&expr(having)?);
    }
    if !sel.window_clause.is_empty() {
        let windows: Vec<String> = sel
            .window_clause
            .iter()
            .map(|w| {
                let NodeEnum::WindowDef(w) = w.node.as_ref()? else {
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
fn sort_and_limit(sel: &protobuf::SelectStmt) -> Option<String> {
    let mut sql = String::new();
    if !sel.sort_clause.is_empty() {
        let sorts: Vec<String> = sel.sort_clause.iter().map(sort_by).collect::<Option<_>>()?;
        sql.push_str(" ORDER BY ");
        sql.push_str(&sorts.join(", "));
    }
    if let Some(count) = sel.limit_count.as_deref() {
        if enumeration::<LimitOption>(sel.limit_option)? == LimitOption::WithTies {
            sql.push_str(&format!(" FETCH FIRST {} ROWS WITH TIES", expr(count)?));
        } else {
            sql.push_str(&format!(" LIMIT {}", expr(count)?));
        }
    }
    // `LIMIT ALL` needs no case of its own: the grammar stores it as a NULL
    // constant count (`LIMIT NULL` parses the same), and a select without a
    // LIMIT has no count and `LimitOption::Count`, PostgreSQL's default.
    if let Some(offset) = sel.limit_offset.as_deref() {
        sql.push_str(&format!(" OFFSET {}", expr(offset)?));
    }
    Some(sql)
}

fn insert_stmt(ins: &protobuf::InsertStmt) -> Option<String> {
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
                let NodeEnum::ResTarget(target) = c.node.as_ref()? else {
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
    sql.push_str(&statement(ins.select_stmt.as_deref()?)?);
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

fn update_stmt(upd: &protobuf::UpdateStmt) -> Option<String> {
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
        .map(set_target)
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
            .map(from_item)
            .collect::<Option<_>>()?;
        sql.push_str(" FROM ");
        sql.push_str(&from.join(", "));
    }
    if let Some(where_clause) = upd.where_clause.as_deref() {
        sql.push_str(" WHERE ");
        sql.push_str(&expr(where_clause)?);
    }
    if let Some(returning) = upd.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn delete_stmt(del: &protobuf::DeleteStmt) -> Option<String> {
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
            .map(from_item)
            .collect::<Option<_>>()?;
        sql.push_str(" USING ");
        sql.push_str(&using.join(", "));
    }
    if let Some(where_clause) = del.where_clause.as_deref() {
        sql.push_str(" WHERE ");
        sql.push_str(&expr(where_clause)?);
    }
    if let Some(returning) = del.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn merge_stmt(mrg: &protobuf::MergeStmt) -> Option<String> {
    let mut sql = String::new();
    if let Some(with) = mrg.with_clause.as_ref() {
        sql.push_str(&with_clause(with)?);
        sql.push(' ');
    }
    sql.push_str("MERGE INTO ");
    sql.push_str(&range_var(mrg.relation.as_ref()?)?);
    sql.push_str(" USING ");
    sql.push_str(&from_item(mrg.source_relation.as_deref()?)?);
    sql.push_str(" ON ");
    sql.push_str(&expr(mrg.join_condition.as_deref()?)?);
    for when in &mrg.merge_when_clauses {
        sql.push(' ');
        sql.push_str(&merge_when_clause(when)?);
    }
    if let Some(returning) = mrg.returning_clause.as_ref() {
        sql.push(' ');
        sql.push_str(&returning_clause(returning)?);
    }
    Some(sql)
}

fn merge_when_clause(node: &protobuf::Node) -> Option<String> {
    let NodeEnum::MergeWhenClause(when) = node.node.as_ref()? else {
        return None;
    };
    let mut sql = String::from("WHEN ");
    sql.push_str(match enumeration::<MergeMatchKind>(when.match_kind)? {
        MergeMatchKind::MergeWhenMatched => "MATCHED",
        MergeMatchKind::MergeWhenNotMatchedBySource => "NOT MATCHED BY SOURCE",
        MergeMatchKind::MergeWhenNotMatchedByTarget => "NOT MATCHED",
        _ => return None,
    });
    if let Some(condition) = when.condition.as_deref() {
        sql.push_str(" AND ");
        sql.push_str(&expr(condition)?);
    }
    sql.push_str(" THEN ");
    match enumeration::<CmdType>(when.command_type)? {
        protobuf::CmdType::CmdUpdate => {
            let sets: Vec<String> = when
                .target_list
                .iter()
                .map(set_target)
                .collect::<Option<_>>()?;
            if sets.is_empty() {
                return None;
            }
            sql.push_str("UPDATE SET ");
            sql.push_str(&sets.join(", "));
        }
        protobuf::CmdType::CmdDelete => sql.push_str("DELETE"),
        protobuf::CmdType::CmdNothing => sql.push_str("DO NOTHING"),
        protobuf::CmdType::CmdInsert => {
            sql.push_str("INSERT");
            if !when.target_list.is_empty() {
                let cols: Vec<String> = when
                    .target_list
                    .iter()
                    .map(|c| {
                        let NodeEnum::ResTarget(target) = c.node.as_ref()? else {
                            return None;
                        };
                        (!target.name.is_empty()).then(|| quote_identifier(&target.name))
                    })
                    .collect::<Option<_>>()?;
                sql.push_str(&format!(" ({})", cols.join(", ")));
            }
            let values: Vec<String> = when.values.iter().map(expr).collect::<Option<_>>()?;
            sql.push_str(&format!(" VALUES ({})", values.join(", ")));
        }
        _ => return None,
    }
    Some(sql)
}

fn on_conflict_clause(clause: &protobuf::OnConflictClause) -> Option<String> {
    let mut sql = String::from("ON CONFLICT");
    if let Some(infer) = clause.infer.as_deref() {
        let elems: Vec<String> = infer
            .index_elems
            .iter()
            .map(index_elem)
            .collect::<Option<_>>()?;
        sql.push_str(&format!(" ({})", elems.join(", ")));
        if let Some(where_clause) = infer.where_clause.as_deref() {
            sql.push_str(" WHERE ");
            sql.push_str(&expr(where_clause)?);
        }
    }
    match enumeration::<OnConflictAction>(clause.action)? {
        protobuf::OnConflictAction::OnconflictNothing => sql.push_str(" DO NOTHING"),
        protobuf::OnConflictAction::OnconflictUpdate => {
            let sets: Vec<String> = clause
                .target_list
                .iter()
                .map(set_target)
                .collect::<Option<_>>()?;
            if sets.is_empty() {
                return None;
            }
            sql.push_str(" DO UPDATE SET ");
            sql.push_str(&sets.join(", "));
            if let Some(where_clause) = clause.where_clause.as_deref() {
                sql.push_str(" WHERE ");
                sql.push_str(&expr(where_clause)?);
            }
        }
        _ => return None,
    }
    Some(sql)
}

fn index_elem(node: &protobuf::Node) -> Option<String> {
    let NodeEnum::IndexElem(elem) = node.node.as_ref()? else {
        return None;
    };
    if !elem.indexcolname.is_empty()
        || !elem.collation.is_empty()
        || !elem.opclass.is_empty()
        || !elem.opclassopts.is_empty()
        || !matches!(
            enumeration::<SortByDir>(elem.ordering)?,
            SortByDir::Undefined | SortByDir::SortbyDefault
        )
        || !matches!(
            enumeration::<SortByNulls>(elem.nulls_ordering)?,
            SortByNulls::Undefined | SortByNulls::SortbyNullsDefault
        )
    {
        return None;
    }
    if let Some(expression) = elem.expr.as_deref() {
        return expr(expression);
    }
    (!elem.name.is_empty()).then(|| quote_identifier(&elem.name))
}

fn returning_clause(clause: &protobuf::ReturningClause) -> Option<String> {
    if !clause.options.is_empty() {
        // `RETURNING WITH (OLD AS …, NEW AS …)` isn't generated.
        return None;
    }
    let exprs: Vec<String> = clause.exprs.iter().map(res_target).collect::<Option<_>>()?;
    if exprs.is_empty() {
        return None;
    }
    Some(format!("RETURNING {}", exprs.join(", ")))
}

fn with_clause(with: &protobuf::WithClause) -> Option<String> {
    let mut sql = String::from("WITH ");
    if with.recursive {
        sql.push_str("RECURSIVE ");
    }
    let ctes: Vec<String> = with
        .ctes
        .iter()
        .map(common_table_expr)
        .collect::<Option<_>>()?;
    if ctes.is_empty() {
        return None;
    }
    sql.push_str(&ctes.join(", "));
    Some(sql)
}

fn common_table_expr(node: &protobuf::Node) -> Option<String> {
    let NodeEnum::CommonTableExpr(cte) = node.node.as_ref()? else {
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
    match enumeration::<CteMaterialize>(cte.ctematerialized)? {
        protobuf::CteMaterialize::Default => {}
        protobuf::CteMaterialize::Always => sql.push_str("MATERIALIZED "),
        protobuf::CteMaterialize::Never => sql.push_str("NOT MATERIALIZED "),
        _ => return None,
    }
    sql.push('(');
    sql.push_str(&statement(cte.ctequery.as_deref()?)?);
    sql.push(')');
    Some(sql)
}

fn range_var(var: &protobuf::RangeVar) -> Option<String> {
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

fn alias_sql(alias: &protobuf::Alias) -> Option<String> {
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

fn from_item(node: &protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        NodeEnum::RangeVar(var) => range_var(var),
        NodeEnum::RangeSubselect(sub) => {
            let mut sql = String::new();
            if sub.lateral {
                sql.push_str("LATERAL ");
            }
            sql.push('(');
            sql.push_str(&statement(sub.subquery.as_deref()?)?);
            sql.push(')');
            if let Some(alias) = sub.alias.as_ref() {
                sql.push_str(&alias_sql(alias)?);
            }
            Some(sql)
        }
        NodeEnum::RangeFunction(func) => range_function(func),
        NodeEnum::JoinExpr(join) => join_expr(join),
        _ => None,
    }
}

fn range_function(func: &protobuf::RangeFunction) -> Option<String> {
    if func.is_rowsfrom {
        return None;
    }
    let mut sql = String::new();
    if func.lateral {
        sql.push_str("LATERAL ");
    }
    let mut calls = Vec::new();
    for function in &func.functions {
        let NodeEnum::List(list) = function.node.as_ref()? else {
            return None;
        };
        match list.items.as_slice() {
            [call] => calls.push(expr(call)?),
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

fn join_expr(join: &protobuf::JoinExpr) -> Option<String> {
    if join.join_using_alias.is_some() {
        return None;
    }
    let qualifier = match enumeration::<JoinType>(join.jointype)? {
        JoinType::JoinInner => "",
        JoinType::JoinLeft => "LEFT ",
        JoinType::JoinRight => "RIGHT ",
        JoinType::JoinFull => "FULL ",
        _ => return None,
    };
    let natural = if join.is_natural { "NATURAL " } else { "" };
    let larg = from_item(join.larg.as_deref()?)?;
    let rarg = from_item(join.rarg.as_deref()?)?;
    let mut sql = if join.quals.is_none() && join.using_clause.is_empty() {
        // Inner joins without a condition: `CROSS JOIN` keeps the tree.
        if enumeration::<JoinType>(join.jointype)? != JoinType::JoinInner || join.is_natural {
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
            expr(join.quals.as_deref()?)?
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

fn res_target(node: &protobuf::Node) -> Option<String> {
    let NodeEnum::ResTarget(target) = node.node.as_ref()? else {
        return None;
    };
    if !target.indirection.is_empty() {
        // `x = …` targets with subscripts aren't produced by the fuzzer.
        return None;
    }
    if target_star(target.val.as_deref()?) {
        return Some("*".to_string());
    }
    let val = expr(target.val.as_deref()?)?;
    if target.name.is_empty() {
        Some(val)
    } else {
        Some(format!("{val} AS {}", quote_identifier(&target.name)))
    }
}

/// An assignment target (`UPDATE … SET c = v`, `ON CONFLICT DO UPDATE`,
/// MERGE's UPDATE action): `ResTarget` with the column name and the value.
fn set_target(node: &protobuf::Node) -> Option<String> {
    let NodeEnum::ResTarget(target) = node.node.as_ref()? else {
        return None;
    };
    if target.name.is_empty() || !target.indirection.is_empty() {
        return None;
    }
    Some(format!(
        "{} = {}",
        quote_identifier(&target.name),
        expr(target.val.as_deref()?)?
    ))
}

/// Whether a target is a bare `*` (`SELECT *`, `RETURNING *`).
fn target_star(node: &protobuf::Node) -> bool {
    matches!(&node.node, Some(NodeEnum::ColumnRef(cr))
        if cr.fields.len() == 1 && matches!(cr.fields[0].node, Some(NodeEnum::AStar(_))))
}

fn sort_by(node: &protobuf::Node) -> Option<String> {
    let NodeEnum::SortBy(sort) = node.node.as_ref()? else {
        return None;
    };
    if !sort.use_op.is_empty() {
        return None;
    }
    let mut sql = expr(sort.node.as_deref()?)?;
    match enumeration::<SortByDir>(sort.sortby_dir)? {
        SortByDir::SortbyAsc | SortByDir::SortbyDefault | SortByDir::Undefined => {}
        SortByDir::SortbyDesc => sql.push_str(" DESC"),
        SortByDir::SortbyUsing => return None,
    }
    match enumeration::<SortByNulls>(sort.sortby_nulls)? {
        SortByNulls::SortbyNullsDefault | SortByNulls::Undefined => {}
        SortByNulls::SortbyNullsFirst => sql.push_str(" NULLS FIRST"),
        SortByNulls::SortbyNullsLast => sql.push_str(" NULLS LAST"),
    }
    Some(sql)
}

fn grouping_element(node: &protobuf::Node) -> Option<String> {
    let Some(NodeEnum::GroupingSet(set)) = node.node.as_ref() else {
        return expr(node);
    };
    match enumeration::<GroupingSetKind>(set.kind)? {
        GroupingSetKind::GroupingSetSimple => {
            let items = set.content.iter().map(expr).collect::<Option<Vec<_>>>()?;
            Some(format!("({})", items.join(", ")))
        }
        GroupingSetKind::GroupingSetEmpty => Some("()".to_string()),
        GroupingSetKind::GroupingSetRollup | GroupingSetKind::GroupingSetCube => {
            let keyword = if enumeration::<GroupingSetKind>(set.kind)?
                == GroupingSetKind::GroupingSetRollup
            {
                "ROLLUP"
            } else {
                "CUBE"
            };
            let items = set
                .content
                .iter()
                .map(grouping_element)
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
                .map(grouping_element)
                .collect::<Option<Vec<_>>>()?;
            Some(format!("GROUPING SETS ({})", items.join(", ")))
        }
        _ => None,
    }
}

fn window_spec(window: &protobuf::WindowDef) -> Option<String> {
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
            .map(expr)
            .collect::<Option<Vec<_>>>()?;
        parts.push(format!("PARTITION BY {}", items.join(", ")));
    }
    if !window.order_clause.is_empty() {
        let items = window
            .order_clause
            .iter()
            .map(sort_by)
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

fn frame_clause(window: &protobuf::WindowDef) -> Option<String> {
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
        window.start_offset.as_deref(),
        FRAMEOPTION_START_UNBOUNDED_PRECEDING,
        FRAMEOPTION_START_UNBOUNDED_FOLLOWING,
        FRAMEOPTION_START_CURRENT_ROW,
        FRAMEOPTION_START_OFFSET_PRECEDING,
        FRAMEOPTION_START_OFFSET_FOLLOWING,
    )?;
    if options & FRAMEOPTION_BETWEEN != 0 {
        let end = frame_bound(
            options,
            window.end_offset.as_deref(),
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
    offset: Option<&protobuf::Node>,
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
        return Some(format!("{} PRECEDING", expr(offset?)?));
    }
    if options & offset_following != 0 {
        return Some(format!("{} FOLLOWING", expr(offset?)?));
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────
// Expressions.
// ──────────────────────────────────────────────────────────────────────────

fn expr(node: &protobuf::Node) -> Option<String> {
    match node.node.as_ref()? {
        NodeEnum::ColumnRef(column) => column_ref(column),
        NodeEnum::AConst(c) => a_const(c),
        NodeEnum::ParamRef(param) => Some(format!("${}", param.number)),
        NodeEnum::AExpr(a) => a_expr(a),
        NodeEnum::BoolExpr(b) => bool_expr(b),
        NodeEnum::NullTest(test) => null_test(test),
        NodeEnum::BooleanTest(test) => boolean_test(test),
        NodeEnum::FuncCall(call) => func_call(call),
        NodeEnum::TypeCast(cast) => Some(format!(
            "CAST({} AS {})",
            expr(cast.arg.as_deref()?)?,
            type_name(cast.type_name.as_ref()?)?
        )),
        NodeEnum::CaseExpr(case) => case_expr(case),
        NodeEnum::CoalesceExpr(coalesce) => {
            let args = coalesce.args.iter().map(expr).collect::<Option<Vec<_>>>()?;
            if args.is_empty() {
                return None;
            }
            Some(format!("COALESCE({})", args.join(", ")))
        }
        NodeEnum::MinMaxExpr(minmax) => {
            let keyword = match enumeration::<MinMaxOp>(minmax.op)? {
                MinMaxOp::IsGreatest => "GREATEST",
                MinMaxOp::IsLeast => "LEAST",
                _ => return None,
            };
            let args = minmax.args.iter().map(expr).collect::<Option<Vec<_>>>()?;
            if args.is_empty() {
                return None;
            }
            Some(format!("{keyword}({})", args.join(", ")))
        }
        NodeEnum::SubLink(link) => sub_link(link),
        NodeEnum::AArrayExpr(array) => {
            let elements = array
                .elements
                .iter()
                .map(expr)
                .collect::<Option<Vec<_>>>()?;
            if elements.is_empty() {
                return None;
            }
            Some(format!("ARRAY[{}]", elements.join(", ")))
        }
        NodeEnum::AIndirection(indirection) => indirection_sql(indirection),
        NodeEnum::CollateClause(collate) => {
            if collate.collname.is_empty() {
                return None;
            }
            let names = strings(&collate.collname)?;
            Some(format!(
                "({} COLLATE {})",
                expr(collate.arg.as_deref()?)?,
                names
                    .iter()
                    .map(|n| quote_identifier(n))
                    .collect::<Vec<_>>()
                    .join(".")
            ))
        }
        NodeEnum::RowExpr(row) => row_expr(row),
        NodeEnum::SqlvalueFunction(func) => sql_value_function(func),
        NodeEnum::SetToDefault(_) => Some("DEFAULT".to_string()),
        _ => None,
    }
}

fn column_ref(column: &protobuf::ColumnRef) -> Option<String> {
    if column.fields.is_empty() {
        return None;
    }
    let fields: Vec<String> = column
        .fields
        .iter()
        .map(|field| match field.node.as_ref()? {
            NodeEnum::String(s) => Some(quote_identifier(&s.sval)),
            NodeEnum::AStar(_) => Some("*".to_string()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    Some(fields.join("."))
}

fn a_const(c: &protobuf::AConst) -> Option<String> {
    if c.isnull {
        return Some("NULL".to_string());
    }
    match c.val.as_ref()? {
        a_const::Val::Ival(integer) => {
            if integer.ival == i32::MIN {
                return None;
            }
            Some(integer.ival.to_string())
        }
        a_const::Val::Fval(float) => {
            let valid = !float.fval.is_empty()
                && float
                    .fval
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
            valid.then(|| float.fval.clone())
        }
        a_const::Val::Boolval(boolean) => Some(if boolean.boolval {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        a_const::Val::Sval(string) => Some(quote_literal(&string.sval)),
        a_const::Val::Bsval(bits) => {
            let valid = bits.bsval.chars().all(|c| matches!(c, '0' | '1'));
            valid.then(|| format!("B'{}'", bits.bsval))
        }
    }
}

/// A standard-conforming string constant: `'…'`, with `''` for a quote.
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn a_expr(expr_node: &protobuf::AExpr) -> Option<String> {
    let op = operator_name(&expr_node.name)?;
    let lexpr = match expr_node.lexpr.as_deref() {
        Some(node) => Some(expr(node)?),
        None => None,
    };
    let rexpr = expr_node.rexpr.as_deref()?;
    match enumeration::<AExprKind>(expr_node.kind)? {
        AExprKind::AexprOp => match lexpr {
            Some(left) => Some(format!("({left} {op} {})", expr(rexpr)?)),
            None => {
                // Prefix operators: quoted or not, only the symbolic ones
                // PostgreSQL parses in that position.
                if !matches!(op.as_str(), "+" | "-" | "~" | "@") {
                    return None;
                }
                Some(format!("({op} {})", expr(rexpr)?))
            }
        },
        AExprKind::AexprOpAny => Some(format!("({} {op} ANY ({}))", lexpr?, expr(rexpr)?)),
        AExprKind::AexprOpAll => Some(format!("({} {op} ALL ({}))", lexpr?, expr(rexpr)?)),
        AExprKind::AexprDistinct => Some(format!("({} IS DISTINCT FROM {})", lexpr?, expr(rexpr)?)),
        AExprKind::AexprNotDistinct => Some(format!(
            "({} IS NOT DISTINCT FROM {})",
            lexpr?,
            expr(rexpr)?
        )),
        AExprKind::AexprNullif => Some(format!("NULLIF({}, {})", lexpr?, expr(rexpr)?)),
        AExprKind::AexprIn => {
            let NodeEnum::List(list) = rexpr.node.as_ref()? else {
                return None;
            };
            let items = list.items.iter().map(expr).collect::<Option<Vec<_>>>()?;
            if items.is_empty() {
                return None;
            }
            let keyword = match &expr_node.name[..] {
                [name] if string_of(name) == Some("=") => "IN",
                [name] if string_of(name) == Some("<>") => "NOT IN",
                _ => return None,
            };
            Some(format!("({} {keyword} ({}))", lexpr?, items.join(", ")))
        }
        AExprKind::AexprLike | AExprKind::AexprIlike => {
            let [name] = &expr_node.name[..] else {
                return None;
            };
            let keyword = match (enumeration::<AExprKind>(expr_node.kind)?, string_of(name)) {
                (AExprKind::AexprLike, Some("~~")) => "LIKE",
                (AExprKind::AexprLike, Some("!~~")) => "NOT LIKE",
                (AExprKind::AexprIlike, Some("~~*")) => "ILIKE",
                (AExprKind::AexprIlike, Some("!~~*")) => "NOT ILIKE",
                _ => return None,
            };
            Some(format!("({} {keyword} {})", lexpr?, expr(rexpr)?))
        }
        AExprKind::AexprSimilar => {
            let [name] = &expr_node.name[..] else {
                return None;
            };
            let keyword = match string_of(name) {
                Some("~") => "SIMILAR TO",
                Some("!~") => "NOT SIMILAR TO",
                _ => return None,
            };
            Some(format!("({} {keyword} {})", lexpr?, expr(rexpr)?))
        }
        AExprKind::AexprBetween
        | AExprKind::AexprNotBetween
        | AExprKind::AexprBetweenSym
        | AExprKind::AexprNotBetweenSym => {
            let NodeEnum::List(list) = rexpr.node.as_ref()? else {
                return None;
            };
            let [low, high] = list.items.as_slice() else {
                return None;
            };
            let keyword = match enumeration::<AExprKind>(expr_node.kind)? {
                AExprKind::AexprBetween => "BETWEEN",
                AExprKind::AexprNotBetween => "NOT BETWEEN",
                AExprKind::AexprBetweenSym => "BETWEEN SYMMETRIC",
                _ => "NOT BETWEEN SYMMETRIC",
            };
            Some(format!(
                "({} {keyword} {} AND {})",
                lexpr?,
                expr(low)?,
                expr(high)?
            ))
        }
        _ => None,
    }
}

fn bool_expr(bool: &protobuf::BoolExpr) -> Option<String> {
    match enumeration::<BoolExprType>(bool.boolop)? {
        BoolExprType::AndExpr | BoolExprType::OrExpr => {
            let keyword = if enumeration::<BoolExprType>(bool.boolop)? == BoolExprType::AndExpr {
                " AND "
            } else {
                " OR "
            };
            let args = bool.args.iter().map(expr).collect::<Option<Vec<_>>>()?;
            if args.len() < 2 {
                return None;
            }
            Some(format!("({})", args.join(keyword)))
        }
        BoolExprType::NotExpr => {
            let [arg] = bool.args.as_slice() else {
                return None;
            };
            Some(format!("(NOT {})", expr(arg)?))
        }
        _ => None,
    }
}

fn null_test(test: &protobuf::NullTest) -> Option<String> {
    let keyword = match enumeration::<NullTestType>(test.nulltesttype)? {
        NullTestType::IsNull => "IS NULL",
        NullTestType::IsNotNull => "IS NOT NULL",
        _ => return None,
    };
    Some(format!("({} {keyword})", expr(test.arg.as_deref()?)?))
}

fn boolean_test(test: &protobuf::BooleanTest) -> Option<String> {
    let keyword = match enumeration::<BoolTestType>(test.booltesttype)? {
        BoolTestType::IsTrue => "IS TRUE",
        BoolTestType::IsNotTrue => "IS NOT TRUE",
        BoolTestType::IsFalse => "IS FALSE",
        BoolTestType::IsNotFalse => "IS NOT FALSE",
        BoolTestType::IsUnknown => "IS UNKNOWN",
        BoolTestType::IsNotUnknown => "IS NOT UNKNOWN",
        _ => return None,
    };
    Some(format!("({} {keyword})", expr(test.arg.as_deref()?)?))
}

fn func_call(call: &protobuf::FuncCall) -> Option<String> {
    if enumeration::<CoercionForm>(call.funcformat)? == CoercionForm::CoerceSqlSyntax {
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
        let mut args = call.args.iter().map(expr).collect::<Option<Vec<_>>>()?;
        if call.func_variadic {
            let last = args.last_mut()?;
            *last = format!("VARIADIC {last}");
        }
        sql.push_str(&args.join(", "));
        if !call.agg_within_group && !call.agg_order.is_empty() {
            let order = call
                .agg_order
                .iter()
                .map(sort_by)
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
            .map(sort_by)
            .collect::<Option<Vec<_>>>()?;
        if order.is_empty() {
            return None;
        }
        sql.push_str(&format!(" WITHIN GROUP (ORDER BY {})", order.join(", ")));
    }
    if let Some(filter) = call.agg_filter.as_deref() {
        sql.push_str(&format!(" FILTER (WHERE {})", expr(filter)?));
    }
    if let Some(over) = call.over.as_deref() {
        sql.push_str(" OVER ");
        sql.push_str(&window_spec(over)?);
    }
    Some(sql)
}

/// SQL-syntax function calls: `extract(year FROM …)`, `substring(…)`, `trim(…)`.
fn sql_syntax_func_call(call: &protobuf::FuncCall) -> Option<String> {
    let names = strings(&call.funcname)?;
    // Only the exact name the grammar itself builds: a mutation can leave a
    // different name with this `funcformat`, which no SQL syntax can express.
    if names.as_slice() == ["pg_catalog", "extract"] {
        let [field, arg] = call.args.as_slice() else {
            return None;
        };
        let field = match field.node.as_ref()? {
            NodeEnum::AConst(c) if !c.isnull => match c.val.as_ref()? {
                a_const::Val::Sval(s) => s.sval.clone(),
                _ => return None,
            },
            _ => return None,
        };
        // A string constant, so a field name that is a keyword (or came from a
        // mutation) still parses to the same `A_Const`.
        Some(format!(
            "extract({} FROM {})",
            quote_literal(&field),
            expr(arg)?
        ))
    } else if names.as_slice() == ["pg_catalog", "substring"] {
        // `substring(a FROM b [FOR c])`: a missing FOR is a `None` node.
        let present = |node: &protobuf::Node| node.node.is_some();
        match call.args.as_slice() {
            [a, b] if present(a) && present(b) => {
                Some(format!("substring({} FROM {})", expr(a)?, expr(b)?))
            }
            [a, b, c] if present(a) && present(b) && present(c) => Some(format!(
                "substring({} FROM {} FOR {})",
                expr(a)?,
                expr(b)?,
                expr(c)?
            )),
            [a, b, c] if present(a) && !present(b) && present(c) => {
                Some(format!("substring({} FOR {})", expr(a)?, expr(c)?))
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
        let args = call.args.iter().map(expr).collect::<Option<Vec<_>>>()?;
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

fn func_name(name: &[protobuf::Node]) -> Option<String> {
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

fn operator_name(name: &[protobuf::Node]) -> Option<String> {
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

fn sub_link(link: &protobuf::SubLink) -> Option<String> {
    let subselect = link.subselect.as_deref()?;
    let inner = statement(subselect)?;
    match enumeration::<SubLinkType>(link.sub_link_type)? {
        SubLinkType::ExistsSublink => Some(format!("EXISTS ({inner})")),
        SubLinkType::ExprSublink => Some(format!("({inner})")),
        SubLinkType::ArraySublink => Some(format!("ARRAY({inner})")),
        SubLinkType::AnySublink | SubLinkType::AllSublink => {
            let testexpr = expr(link.testexpr.as_deref()?)?;
            // `IN`/`NOT IN (subquery)`: the grammar leaves `oper_name` empty
            // for those; an explicit `= ANY (…)` / `<> ALL (…)` carries it.
            if link.oper_name.is_empty() {
                let keyword =
                    if enumeration::<SubLinkType>(link.sub_link_type)? == SubLinkType::AnySublink {
                        "IN"
                    } else {
                        "NOT IN"
                    };
                return Some(format!("({testexpr} {keyword} ({inner}))"));
            }
            let keyword =
                if enumeration::<SubLinkType>(link.sub_link_type)? == SubLinkType::AnySublink {
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

fn indirection_sql(indirection: &protobuf::AIndirection) -> Option<String> {
    let mut sql = expr(indirection.arg.as_deref()?)?;
    if indirection.indirection.is_empty() {
        return None;
    }
    for item in &indirection.indirection {
        match item.node.as_ref()? {
            NodeEnum::AIndices(indices) => {
                if indices.is_slice {
                    let low = match indices.lidx.as_deref() {
                        Some(node) => Some(expr(node)?),
                        None => None,
                    };
                    let high = match indices.uidx.as_deref() {
                        Some(node) => Some(expr(node)?),
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
                    let index = indices.uidx.as_deref().or(indices.lidx.as_deref())?;
                    sql.push_str(&format!("[{}]", expr(index)?));
                }
            }
            NodeEnum::String(field) => {
                sql.push('.');
                sql.push_str(&quote_identifier(&field.sval));
            }
            _ => return None,
        }
    }
    Some(format!("({sql})"))
}

fn row_expr(row: &protobuf::RowExpr) -> Option<String> {
    let args = row.args.iter().map(expr).collect::<Option<Vec<_>>>()?;
    if args.is_empty() {
        return None;
    }
    match enumeration::<CoercionForm>(row.row_format)? {
        CoercionForm::CoerceExplicitCall => Some(format!("ROW({})", args.join(", "))),
        CoercionForm::CoerceImplicitCast | CoercionForm::Undefined => {
            if args.len() < 2 {
                return None;
            }
            Some(format!("({})", args.join(", ")))
        }
        _ => None,
    }
}

fn sql_value_function(func: &protobuf::SqlValueFunction) -> Option<String> {
    let typmod = |default: &str| -> String {
        if func.typmod < 0 {
            default.to_string()
        } else {
            format!("{default}({})", func.typmod)
        }
    };
    Some(match enumeration::<SqlValueFunctionOp>(func.op)? {
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
        _ => return None,
    })
}

fn type_name(type_name: &protobuf::TypeName) -> Option<String> {
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
            .map(expr)
            .collect::<Option<Vec<_>>>()?;
        sql.push_str(&format!("({})", mods.join(", ")));
    }
    for bound in &type_name.array_bounds {
        match bound.node.as_ref()? {
            NodeEnum::Integer(integer) if integer.ival == -1 => sql.push_str("[]"),
            _ => return None,
        }
    }
    Some(sql)
}

fn case_expr(case: &protobuf::CaseExpr) -> Option<String> {
    let mut sql = String::from("CASE");
    if let Some(arg) = case.arg.as_deref() {
        sql.push(' ');
        sql.push_str(&expr(arg)?);
    }
    for when in &case.args {
        let NodeEnum::CaseWhen(when) = when.node.as_ref()? else {
            return None;
        };
        sql.push_str(&format!(
            " WHEN {} THEN {}",
            expr(when.expr.as_deref()?)?,
            expr(when.result.as_deref()?)?
        ));
    }
    if let Some(defresult) = case.defresult.as_deref() {
        sql.push_str(&format!(" ELSE {}", expr(defresult)?));
    }
    sql.push_str(" END");
    Some(sql)
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers.
// ──────────────────────────────────────────────────────────────────────────

/// A protobuf enum field (stored as `i32`) as its enum: `None` for a value
/// the schema doesn't know.
fn enumeration<T: TryFrom<i32>>(value: i32) -> Option<T> {
    T::try_from(value).ok()
}

/// The parts of a funcname / type name / collation name / column-name list,
/// each of which must be a plain `String` node.
fn strings(nodes: &[protobuf::Node]) -> Option<Vec<String>> {
    nodes
        .iter()
        .map(|node| string_of(node).map(str::to_owned))
        .collect()
}

fn string_of(node: &protobuf::Node) -> Option<&str> {
    match node.node.as_ref()? {
        NodeEnum::String(s) => Some(&s.sval),
        _ => None,
    }
}
