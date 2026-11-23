//! The `cartesian_join` rule: reports FROM items of one `SELECT` that no
//! condition connects — comma-separated items, or a `JOIN … ON` whose
//! condition doesn't relate its sides — so every row of one pairs with
//! every row of the other. Items are connected by a `WHERE`, `JOIN … ON` or
//! `HAVING` condition (an `AND` argument of one) that references both, or
//! transitively through other items; by two `AND` arguments equating a
//! column of each to the same parameter or outer column (`a.t = $t AND
//! b.t = $t` is `a.t = b.t`); by `USING` / `NATURAL`; and by an explicit
//! `CROSS JOIN`, which says the product is meant.
//!
//! What each column reference names comes from the analyzer (see
//! [`typedpg_analyzer::QueryView::column_ref_binding`]), so an unqualified
//! column counts for the item that has it. A condition the rule can't
//! place — a reference to a join's merged `USING` column or to an entry
//! with no name — connects every item, so nothing is reported for it.
//!
//! Not reported, as an item whose product is cheap or meant:
//! - an item proven to return at most one row: a subquery the analyzer's
//!   cardinality proof accepts (`(SELECT count(*) FROM t)`, a one-row
//!   `VALUES`) or with a `LIMIT 1`, a reference to such a `WITH` query (an
//!   `INSERT` / `UPDATE` / `DELETE … RETURNING` the proof bounds to one row
//!   too), or a table whose unique key the conditions that filter only it
//!   equate to constants or parameters: the `WHERE` conjuncts, and the `ON`
//!   conjuncts of an inner join or of an outer join it is the nullable side
//!   of (`users u, posts p WHERE u.id = $u`);
//! - a function in FROM (`generate_series`, `unnest`, `to_tsquery(…)`):
//!   whether it returns a set is not resolved here, and a set-returning
//!   one is the usual way to multiply rows on purpose. Like a `LATERAL`
//!   subquery, it is connected to the items its arguments reference.
//!
//! Only a `SELECT`'s FROM clause is read: an `UPDATE … FROM` or
//! `DELETE … USING` is not.

use std::collections::HashMap;

use typedpg_analyzer::{select_at_most_one_row, statement_node_at_most_one_row};
use typedpg_pg_query::ast::{
    AConstValue, AExprKind, BoolExpr, BoolExprType, JoinType, LimitOption, Node, Node as N,
    RangeVar, SelectStmt,
};
use typedpg_pg_query::{NodeBox, NodeRef};

use super::{location, operator_name, scan_until, starts_clause};
use crate::{Ctx, RawFinding};

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let one_row_ctes = one_row_ctes(ctx);
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::SelectStmt(select) = node
            && !select.from_clause.is_empty()
        {
            level(ctx, select, &one_row_ctes, &mut out);
        }
    }
    out
}

/// The names of the `WITH` queries proven to return at most one row; a
/// name some other `WITH` query of the tree also takes is left out.
fn one_row_ctes(ctx: &Ctx<'_>) -> HashMap<String, bool> {
    let mut ctes: HashMap<String, bool> = HashMap::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::CommonTableExpr(cte) = node {
            let one_row = match cte.ctequery.as_ref() {
                Some(N::SelectStmt(body)) => at_most_one_row(ctx, body),
                // A data-modifying `WITH` query's `RETURNING` rows.
                Some(body) => statement_node_at_most_one_row(ctx.catalog, ctx.view, body.to_ref()),
                None => false,
            };
            *ctes.entry(cte.ctename.clone()).or_insert(true) &= one_row;
        }
    }
    ctes
}

/// Whether `select` returns at most one row: the analyzer's proof, or a
/// `LIMIT` / `FETCH FIRST` of 0 or 1 without `WITH TIES`.
fn at_most_one_row(ctx: &Ctx<'_>, select: &SelectStmt) -> bool {
    let limited = select.limit_option != LimitOption::WithTies
        && matches!(select.limit_count.as_ref(), Some(N::AConst(count))
            if !count.isnull
                && matches!(&count.val, Some(AConstValue::Integer(n)) if n.ival <= 1));
    limited || select_at_most_one_row(ctx.catalog, ctx.view, select)
}

/// One FROM item that isn't a join.
struct Leaf {
    /// The name the item answers to, for the message.
    name: Option<String>,
    /// Its first byte.
    start: usize,
    /// Whether a product with it is reported (see the module docs).
    reportable: bool,
}

/// The FROM items of one level and what connects them.
struct Level<'a> {
    leaves: Vec<Leaf>,
    /// The leaves each name reaches: an item's alias or relation name, a
    /// join alias (every leaf inside).
    names: HashMap<String, Vec<usize>>,
    /// Union-find parents.
    parent: Vec<usize>,
    /// The conditions to place: `ON` conditions and `WHERE` / `HAVING`.
    conditions: Vec<&'a Node>,
    /// The subtrees of `LATERAL` items and FROM functions, whose references
    /// to the items on their left connect them.
    lateral: Vec<(usize, &'a Node)>,
    /// The leaves that are a table (not a `WITH` query), which a key
    /// lookup may bound to one row.
    tables: Vec<(usize, &'a RangeVar)>,
    /// Each `ON` condition with the leaves whose rows it filters: both
    /// sides of an inner join, the nullable side of an outer one.
    filters: Vec<(&'a Node, std::ops::Range<usize>)>,
}

impl Level<'_> {
    fn find(&mut self, mut leaf: usize) -> usize {
        while self.parent[leaf] != leaf {
            self.parent[leaf] = self.parent[self.parent[leaf]];
            leaf = self.parent[leaf];
        }
        leaf
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[b] = a;
        }
    }

    fn union_all(&mut self, leaves: &[usize]) {
        for pair in leaves.windows(2) {
            self.union(pair[0], pair[1]);
        }
    }
}

fn level(
    ctx: &Ctx<'_>,
    select: &SelectStmt,
    one_row_ctes: &HashMap<String, bool>,
    out: &mut Vec<RawFinding>,
) {
    let mut level = Level {
        leaves: Vec::new(),
        names: HashMap::new(),
        parent: Vec::new(),
        conditions: Vec::new(),
        lateral: Vec::new(),
        tables: Vec::new(),
        filters: Vec::new(),
    };
    for item in select.from_clause.iter().flatten() {
        if from_item(ctx, item, one_row_ctes, &mut level).is_none() {
            // An item the rule doesn't model: say nothing about this level.
            return;
        }
    }
    if level.leaves.len() < 2 {
        return;
    }
    level.conditions.extend(select.where_clause.as_ref());
    level.conditions.extend(select.having_clause.as_ref());
    let mut conjuncts = Vec::new();
    for condition in std::mem::take(&mut level.conditions) {
        split_and(condition, &mut conjuncts);
    }
    // The leaves each parameter or outer column is equated to.
    let mut equated: HashMap<Value, Vec<usize>> = HashMap::new();
    for conjunct in conjuncts {
        let Some(leaves) = referenced(ctx, conjunct, &level, false) else {
            let all: Vec<usize> = (0..level.leaves.len()).collect();
            level.union_all(&all);
            continue;
        };
        level.union_all(&leaves);
        if let Some((leaf, value)) = value_equality(ctx, conjunct, &level) {
            equated.entry(value).or_default().push(leaf);
        }
    }
    for leaves in equated.values() {
        level.union_all(leaves);
    }
    one_row_tables(ctx, select, &mut level);
    for (leaf, subtree) in std::mem::take(&mut level.lateral) {
        // A FROM function's arguments resolve in the level itself, a
        // LATERAL subquery's references out of it in the level around it:
        // every reference naming a leaf counts (one naming the subquery's
        // own relation of the same name too, which only connects more).
        let mut leaves = referenced(ctx, subtree, &level, true).unwrap_or_default();
        leaves.push(leaf);
        level.union_all(&leaves);
    }
    // The components, in the order of their first leaf, keeping those with
    // a reportable leaf.
    let mut components: Vec<(usize, usize)> = Vec::new();
    let mut seen = Vec::new();
    for leaf in 0..level.leaves.len() {
        let root = level.find(leaf);
        if seen.contains(&root) {
            continue;
        }
        let first_reportable = (leaf..level.leaves.len())
            .find(|&other| level.leaves[other].reportable && level.find(other) == root);
        seen.push(root);
        if let Some(first) = first_reportable {
            components.push((root, first));
        }
    }
    components.sort_by_key(|&(_, first)| level.leaves[first].start);
    let Some(&(_, anchor)) = components.first() else {
        return;
    };
    for &(_, first) in &components[1..] {
        let leaf = &level.leaves[first];
        let (end, _) = scan_until(ctx, leaf.start, |text| text == "," || starts_clause(text));
        out.push(RawFinding {
            span: leaf.start..end.max(leaf.start + 1),
            message: format!(
                "{} is joined to {} with no condition connecting them: every row of one pairs \
                 with every row of the other",
                display(leaf),
                display(&level.leaves[anchor]),
            ),
            help: Some(
                "add the join condition, or write CROSS JOIN if every combination is meant"
                    .to_owned(),
            ),
            fix: Vec::new(),
        });
    }
}

/// A value one evaluation of the level sees once: a parameter, or a column
/// of an enclosing query level (by its entry's name and its own).
#[derive(PartialEq, Eq, Hash)]
enum Value {
    Param(i32),
    Outer(String, String),
}

/// The leaf and value of a `column = value` conjunct (either way round)
/// equating a column of one leaf of the level to a [`Value`].
fn value_equality(ctx: &Ctx<'_>, conjunct: &Node, level: &Level<'_>) -> Option<(usize, Value)> {
    let N::AExpr(expr) = conjunct else {
        return None;
    };
    if expr.kind != AExprKind::AexprOp || operator_name(expr) != Some("=") {
        return None;
    }
    let (left, right) = (expr.lexpr.as_ref()?, expr.rexpr.as_ref()?);
    let leaf = |node: &Node| {
        let N::ColumnRef(column) = node else {
            return None;
        };
        let binding = ctx.view.column_ref_binding(column.location)?;
        if binding.outer {
            return None;
        }
        match level.names.get(binding.source.as_deref()?)?.as_slice() {
            [leaf] => Some(*leaf),
            _ => None,
        }
    };
    let value = |node: &Node| match node {
        N::ParamRef(param) => Some(Value::Param(param.number)),
        N::ColumnRef(column) => {
            let binding = ctx.view.column_ref_binding(column.location)?;
            let Some(N::String(name)) = column.fields.last()? else {
                return None;
            };
            if !binding.outer {
                return None;
            }
            Some(Value::Outer(binding.source.clone()?, name.sval.clone()))
        }
        _ => None,
    };
    match (leaf(left), leaf(right)) {
        (Some(leaf), None) => Some((leaf, value(right)?)),
        (None, Some(leaf)) => Some((leaf, value(left)?)),
        _ => None,
    }
}

/// Mark as not reportable each table leaf the conditions filtering only it
/// bound to at most one row: the `WHERE` conjuncts and the `ON` conjuncts
/// of the joins filtering its rows that reference no other leaf, judged
/// by the analyzer's cardinality proof over the table alone.
fn one_row_tables(ctx: &Ctx<'_>, select: &SelectStmt, level: &mut Level<'_>) {
    let all = 0..level.leaves.len();
    let mut filters: Vec<(&Node, std::ops::Range<usize>)> = Vec::new();
    filters.extend(select.where_clause.as_ref().map(|w| (w, all.clone())));
    filters.extend(level.filters.iter().cloned());
    for &(leaf, range) in &level.tables {
        if !level.leaves[leaf].reportable {
            continue;
        }
        let mut own = Vec::new();
        for (condition, leaves) in &filters {
            if !leaves.contains(&leaf) {
                continue;
            }
            let mut conjuncts = Vec::new();
            split_and(condition, &mut conjuncts);
            own.extend(conjuncts.into_iter().filter(|conjunct| {
                referenced(ctx, conjunct, level, false)
                    .is_some_and(|refs| !refs.is_empty() && refs.iter().all(|&r| r == leaf))
            }));
        }
        if own.is_empty() {
            continue;
        }
        let where_clause = if let [conjunct] = own.as_slice() {
            (*conjunct).clone()
        } else {
            N::BoolExpr(NodeBox::new(BoolExpr {
                boolop: BoolExprType::AndExpr,
                args: own
                    .into_iter()
                    .map(|conjunct| Some(conjunct.clone()))
                    .collect(),
                location: -1,
            }))
        };
        let lookup = SelectStmt {
            from_clause: vec![Some(N::RangeVar(NodeBox::new(range.clone())))],
            where_clause: Some(where_clause),
            ..SelectStmt::default()
        };
        if select_at_most_one_row(ctx.catalog, ctx.view, &lookup) {
            level.leaves[leaf].reportable = false;
        }
    }
}

fn display(leaf: &Leaf) -> String {
    match &leaf.name {
        Some(name) => typedpg_core::quote_identifier(name),
        None => "a subquery".to_owned(),
    }
}

/// Add the leaves of FROM item `item` to `level`, returning their index
/// range, or `None` for an item the rule doesn't model.
fn from_item<'a>(
    ctx: &Ctx<'_>,
    item: &'a Node,
    one_row_ctes: &HashMap<String, bool>,
    level: &mut Level<'a>,
) -> Option<std::ops::Range<usize>> {
    let first = level.leaves.len();
    let add = |level: &mut Level<'a>, name: Option<String>, start: usize, reportable| {
        let index = level.leaves.len();
        level.leaves.push(Leaf {
            name: name.clone(),
            start,
            reportable,
        });
        level.parent.push(index);
        if let Some(name) = name {
            level.names.entry(name).or_default().push(index);
        }
        index
    };
    match item {
        N::RangeVar(range) => {
            let name = range
                .alias
                .as_ref()
                .map_or(range.relname.clone(), |alias| alias.aliasname.clone());
            let cte = range.schemaname.is_empty() && one_row_ctes.contains_key(&range.relname);
            let one_row = cte && one_row_ctes.get(&range.relname).copied() == Some(true);
            let leaf = add(level, Some(name), location(range.location)?, !one_row);
            if !cte {
                level.tables.push((leaf, range));
            }
        }
        N::RangeTableSample(sample) => {
            let N::RangeVar(range) = sample.relation.as_ref()? else {
                return None;
            };
            let name = range
                .alias
                .as_ref()
                .map_or(range.relname.clone(), |alias| alias.aliasname.clone());
            let leaf = add(level, Some(name), location(range.location)?, true);
            level.lateral.push((leaf, item));
        }
        N::RangeSubselect(sub) => {
            let N::SelectStmt(body) = sub.subquery.as_ref()? else {
                return None;
            };
            let start = subquery_start(ctx, sub.subquery.as_ref()?)?;
            let one_row = at_most_one_row(ctx, body);
            let name = sub.alias.as_ref().map(|alias| alias.aliasname.clone());
            let leaf = add(level, name, start, !one_row);
            level.lateral.push((leaf, item));
        }
        N::RangeFunction(_) | N::RangeTableFunc(_) | N::JsonTable(_) => {
            let start = item
                .nodes()
                .filter_map(|(node, _)| node.location())
                .filter_map(location)
                .min()?;
            let leaf = add(level, None, start, false);
            // Its alias, when it has one, reaches it too.
            let alias = match item {
                N::RangeFunction(f) => f.alias.as_ref().map(|a| a.aliasname.clone()),
                N::RangeTableFunc(f) => f.alias.as_ref().map(|a| a.aliasname.clone()),
                N::JsonTable(f) => f.alias.as_ref().map(|a| a.aliasname.clone()),
                _ => None,
            };
            if let Some(alias) = alias {
                level.names.entry(alias).or_default().push(leaf);
            }
            level.lateral.push((leaf, item));
        }
        N::JoinExpr(join) => {
            let left = from_item(ctx, join.larg.as_ref()?, one_row_ctes, level)?;
            let right = from_item(ctx, join.rarg.as_ref()?, one_row_ctes, level)?;
            if join.is_natural || !join.using_clause.is_empty() || join.quals.is_none() {
                // USING / NATURAL relate the sides; no condition at all is
                // an explicit CROSS JOIN.
                level.union(left.start, right.start);
            }
            if let Some(quals) = join.quals.as_ref() {
                level.conditions.push(quals);
                let filtered = match join.jointype {
                    JoinType::JoinInner => Some(left.start..right.end),
                    JoinType::JoinLeft => Some(right.clone()),
                    JoinType::JoinRight => Some(left.clone()),
                    _ => None,
                };
                if let Some(filtered) = filtered {
                    level.filters.push((quals, filtered));
                }
            }
            let inside: Vec<usize> = (left.start..right.end).collect();
            for alias in [join.alias.as_ref(), join.join_using_alias.as_ref()]
                .into_iter()
                .flatten()
            {
                level
                    .names
                    .entry(alias.aliasname.clone())
                    .or_default()
                    .extend(inside.iter().copied());
            }
        }
        _ => return None,
    }
    Some(first..level.leaves.len())
}

/// The first byte of a parenthesized subquery: the `(` before its
/// `SELECT`, `VALUES`, `WITH` or `TABLE`.
fn subquery_start(ctx: &Ctx<'_>, body: &Node) -> Option<usize> {
    let first = body
        .nodes()
        .filter_map(|(node, _)| node.location())
        .filter_map(location)
        .min()?;
    let index = ctx.tokens.iter().rposition(|token| {
        token.start < first && {
            let text = &ctx.sql[token.start..token.end];
            ["SELECT", "VALUES", "WITH", "TABLE"]
                .iter()
                .any(|keyword| text.eq_ignore_ascii_case(keyword))
        }
    })?;
    let paren = ctx.tokens.get(index.checked_sub(1)?)?;
    (&ctx.sql[paren.start..paren.end] == "(").then_some(paren.start)
}

/// The arguments of the top-level `AND`s of `node`.
fn split_and<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    match node {
        N::BoolExpr(and) if and.boolop == BoolExprType::AndExpr => {
            for arg in and.args.iter().flatten() {
                split_and(arg, out);
            }
        }
        _ => out.push(node),
    }
}

/// The leaves of `level` that `node` references. A reference of this
/// level (outside any nested `SELECT` of `node`) that can't be placed makes
/// it `None`: it may connect anything. References out of a nested `SELECT`
/// count when they name a leaf. With `any`, every reference naming a leaf
/// counts and nothing makes it `None`.
fn referenced(ctx: &Ctx<'_>, node: &Node, level: &Level<'_>, any: bool) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    let mut stack = vec![(node.to_ref(), false)];
    while let Some((node, nested)) = stack.pop() {
        if let NodeRef::ColumnRef(column) = node {
            let binding = ctx.view.column_ref_binding(column.location);
            let qualifier = match column.fields.as_slice() {
                [.., Some(N::String(q)), _] => Some(q.sval.as_str()),
                _ => None,
            };
            let (outer, name) = match binding {
                Some(binding) => (binding.outer, binding.source.as_deref()),
                // A system column (`t.ctid`) is not recorded: its
                // qualifier names the entry.
                None => (nested, qualifier),
            };
            let leaves = name.and_then(|name| level.names.get(name));
            match leaves {
                Some(leaves) if any || !nested || outer => out.extend(leaves.iter().copied()),
                _ if !any && !nested && !outer && binding.is_some() => return None,
                _ => {}
            }
            continue;
        }
        let nested = nested || matches!(node, NodeRef::SelectStmt(_));
        stack.extend(node.children().map(|child| (child, nested)));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, lint_rule};

    const RULE: Rule = Rule::CartesianJoin;

    #[test]
    fn items_no_condition_connects_are_reported() {
        let db = catalog();
        for (sql, reported, message) in [
            (
                "SELECT 1 FROM users, posts",
                "posts",
                "posts is joined to users with no condition connecting them: every row of one \
                 pairs with every row of the other",
            ),
            (
                "SELECT 1 FROM users u, posts p WHERE u.name = 'x' AND p.title = 'y'",
                "posts p",
                "p is joined to u with no condition connecting them: every row of one pairs with \
                 every row of the other",
            ),
            (
                "SELECT 1 FROM users u JOIN posts p ON true",
                "posts p",
                "p is joined to u with no condition connecting them: every row of one pairs with \
                 every row of the other",
            ),
            (
                "SELECT 1 FROM users u LEFT JOIN posts p ON p.title = 'x'",
                "posts p",
                "p is joined to u with no condition connecting them: every row of one pairs with \
                 every row of the other",
            ),
            (
                // Connected to one item, not to the other.
                "SELECT 1 FROM users u, posts p, posts q WHERE u.id = p.user_id",
                "posts q",
                "q is joined to u with no condition connecting them: every row of one pairs with \
                 every row of the other",
            ),
            (
                "SELECT 1 FROM users u, (SELECT user_id FROM posts) s",
                "(SELECT user_id FROM posts) s",
                "s is joined to u with no condition connecting them: every row of one pairs with \
                 every row of the other",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert_eq!(&sql[findings[0].span.clone()], reported, "{sql}");
            assert_eq!(findings[0].message, message, "{sql}");
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn one_row_tables_and_shared_values_are_not_reported() {
        let db = catalog();
        for sql in [
            // Key lookups on both sides.
            "SELECT 1 FROM posts p, users u WHERE p.id = $p AND u.email = $u",
            "SELECT 1 FROM posts p JOIN users u ON u.email = $u",
            "SELECT 1 FROM posts p LEFT JOIN users u ON u.email = $u",
            "SELECT 1 FROM posts p, users u WHERE u.id = 1",
            // A one-row data-modifying WITH query.
            "WITH w AS (UPDATE users SET name = 'x' WHERE id = $w RETURNING id)              SELECT 1 FROM posts p, w",
            "WITH w AS (INSERT INTO users (id, email) VALUES (1, 'a') RETURNING id)              SELECT 1 FROM posts p JOIN w ON true",
            // The same parameter or outer column on both sides.
            "SELECT 1 FROM users u, posts p WHERE u.id = $t AND p.user_id = $t",
            "SELECT 1 FROM users u JOIN posts p ON p.user_id = $t WHERE $t = u.id",
            "SELECT (SELECT count(*) FROM users u, posts p              WHERE u.id = o.user_id AND p.user_id = o.user_id) FROM posts o",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for sql in [
            // Not a key: many users may have that name.
            "SELECT 1 FROM posts p, users u WHERE u.name = $n",
            // The preserved side of an outer join isn't filtered by its ON.
            "SELECT 1 FROM users u LEFT JOIN posts p ON u.email = $u",
            // A key lookup OR something else.
            "SELECT 1 FROM posts p, users u WHERE u.id = $1 OR u.name = 'x'",
            // Different parameters.
            "SELECT 1 FROM users u, posts p WHERE u.name = $a AND p.title = $b",
            // A data-modifying WITH query that may return many rows.
            "WITH w AS (UPDATE users SET name = 'x' WHERE name = $n RETURNING id)              SELECT 1 FROM posts p, w",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        }
    }

    #[test]
    fn connected_or_intended_products_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT 1 FROM users u, posts p WHERE u.id = p.user_id",
            "SELECT 1 FROM users u JOIN posts p ON p.user_id = u.id",
            // Unqualified columns count for the item that has them.
            "SELECT 1 FROM users u, posts p WHERE email = title",
            // Transitively.
            "SELECT 1 FROM users u, posts p, posts q WHERE u.id = p.user_id AND q.id = p.id",
            // Through a subquery's correlated reference.
            "SELECT 1 FROM users u, posts p WHERE EXISTS \
             (SELECT 1 FROM posts x WHERE x.user_id = u.id AND x.id = p.id)",
            // Explicit.
            "SELECT 1 FROM users CROSS JOIN posts",
            "SELECT 1 FROM users JOIN posts USING (id)",
            "SELECT 1 FROM users NATURAL JOIN posts",
            // At most one row.
            "SELECT 1 FROM users, (SELECT count(*) FROM posts) c",
            "SELECT 1 FROM users, (VALUES (1)) v (x)",
            "SELECT 1 FROM users, (SELECT title FROM posts ORDER BY id LIMIT 1) p",
            "WITH c AS (SELECT count(*) AS n FROM posts) SELECT 1 FROM users, c",
            // Functions in FROM.
            "SELECT 1 FROM users, generate_series(1, 3) g",
            "SELECT 1 FROM users u, unnest(ARRAY[u.name]) n",
            // LATERAL referencing the left side.
            "SELECT 1 FROM users u, LATERAL (SELECT title FROM posts p WHERE p.user_id = u.id) s",
            "SELECT 1 FROM users u LEFT JOIN LATERAL \
             (SELECT title FROM posts p WHERE p.user_id = u.id LIMIT 1) s ON true",
            // One item.
            "SELECT 1 FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
