//! The `non_sargable_predicate` rule: reports a `WHERE` or `JOIN … ON`
//! comparison (`=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (…)`, `= ANY`)
//! that wraps an indexed column so no index on it can serve the
//! comparison: PostgreSQL matches an index only to the bare column (a
//! binary-compatible relabeling aside), never to an expression of it.
//!
//! - A cast that converts the value (`created_at::date = $1`,
//!   `id::text = $1`); a cast to the type it has, or to a binary-compatible
//!   one (`varchar` to `text`), keeps the column.
//! - A function call with the column as an argument (`lower(email) = $1`,
//!   `date_trunc('day', created_at) = $1`).
//! - Arithmetic or concatenation with the column as an operand
//!   (`id + 1 = $1`).
//! - An implicit conversion the other operand's type forces on the column
//!   (an `int4` column compared with a `numeric` value is converted to
//!   `numeric`): the column's value is converted for every row.
//!
//! Only reported when the comparison would otherwise have an index: the
//! column (resolved to a plain or partitioned table of the same query
//! level) leads a non-partial btree index, and the table has no expression
//! index at all (one may match the expression). The other side must be a
//! value an index scan could use: no column of the same table, no subquery,
//! no volatile call. Only the top-level `AND` parts of a condition are read.
//! The help names the sargable rewrite when it is a range (`::date`,
//! `date_trunc`). No fix is offered.

use typedpg_analyzer::{CastMethod, PgClassOid, PgTypeOid, RelKind, RelationRef};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, AExpr, AExprKind, ColumnRef, Node};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding};

const DATE: PgTypeOid = PgTypeOid::from_raw(1082);
const TIMESTAMP: PgTypeOid = PgTypeOid::from_raw(1114);
const TIMESTAMPTZ: PgTypeOid = PgTypeOid::from_raw(1184);

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let mut level = Level::default();
        let mut quals: Vec<&Node> = Vec::new();
        match node {
            NodeRef::SelectStmt(select) => {
                for item in select.from_clause.iter().flatten() {
                    level.add(ctx, item, &mut quals);
                }
                quals.extend(select.where_clause.as_ref());
            }
            NodeRef::UpdateStmt(update) => {
                if let Some(relation) = update.relation.as_deref() {
                    level.add_relation(ctx, relation);
                }
                for item in update.from_clause.iter().flatten() {
                    level.add(ctx, item, &mut quals);
                }
                quals.extend(update.where_clause.as_ref());
            }
            NodeRef::DeleteStmt(delete) => {
                if let Some(relation) = delete.relation.as_deref() {
                    level.add_relation(ctx, relation);
                }
                for item in delete.using_clause.iter().flatten() {
                    level.add(ctx, item, &mut quals);
                }
                quals.extend(delete.where_clause.as_ref());
            }
            _ => continue,
        }
        for qual in quals {
            for conjunct in conjuncts(qual) {
                if let Node::AExpr(expr) = conjunct
                    && let Some(finding) = comparison(ctx, &level, expr)
                {
                    out.push(finding);
                }
            }
        }
    }
    out
}

/// The FROM entries of one query level, by the name a column reference
/// binds to: a table's oid, or `None` for an entry that isn't a table (a
/// subquery, a function, a CTE, a view).
#[derive(Default)]
struct Level {
    entries: Vec<(String, Option<PgClassOid>)>,
}

impl Level {
    fn add<'a>(&mut self, ctx: &Ctx<'_>, item: &'a Node, quals: &mut Vec<&'a Node>) {
        match item {
            Node::RangeVar(relation) => self.add_relation(ctx, relation),
            Node::JoinExpr(join) => {
                for side in [join.larg.as_ref(), join.rarg.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    self.add(ctx, side, quals);
                }
                quals.extend(join.quals.as_ref());
                if let Some(alias) = &join.alias {
                    self.entries.push((alias.aliasname.clone(), None));
                }
            }
            Node::RangeSubselect(sub) => {
                if let Some(alias) = &sub.alias {
                    self.entries.push((alias.aliasname.clone(), None));
                }
            }
            Node::RangeFunction(function) => {
                if let Some(alias) = &function.alias {
                    self.entries.push((alias.aliasname.clone(), None));
                }
            }
            _ => {}
        }
    }

    fn add_relation(&mut self, ctx: &Ctx<'_>, relation: &typedpg_pg_query::ast::RangeVar) {
        let name = relation
            .alias
            .as_ref()
            .map_or_else(|| relation.relname.clone(), |alias| alias.aliasname.clone());
        // What the analyzer resolved the reference to: a `WITH` query of
        // the same name is no table.
        let oid = match ctx.view.relation_of(relation) {
            Some(RelationRef::Relation(oid)) => ctx
                .catalog
                .class_row(oid)
                .filter(|class| matches!(class.relkind, RelKind::Table | RelKind::Partitioned))
                .map(|class| class.oid),
            _ => None,
        };
        self.entries.push((name, oid));
    }

    /// The table a reference binding to entry `name` reads, when the name
    /// is one table's.
    fn table(&self, name: &str) -> Option<PgClassOid> {
        let mut found = self.entries.iter().filter(|(entry, _)| entry == name);
        match (found.next(), found.next()) {
            (Some((_, oid)), None) => *oid,
            _ => None,
        }
    }
}

/// The top-level `AND` parts of `node`.
fn conjuncts(node: &Node) -> Vec<&Node> {
    match node {
        Node::BoolExpr(b) if b.boolop == typedpg_pg_query::ast::BoolExprType::AndExpr => {
            b.args.iter().flatten().flat_map(conjuncts).collect()
        }
        other => vec![other],
    }
}

/// How the column is wrapped.
enum Wrap<'a> {
    Cast {
        to: PgTypeOid,
    },
    Function {
        name: String,
        call: &'a typedpg_pg_query::ast::FuncCall,
    },
    Arithmetic {
        operator: String,
    },
    Implicit {
        from: PgTypeOid,
        to: PgTypeOid,
    },
}

/// A resolved, indexed column a comparison side wraps.
struct Wrapped<'a> {
    column: &'a ColumnRef,
    source: String,
    table: PgClassOid,
    attnum: i16,
    index: String,
    wrap: Wrap<'a>,
}

fn comparison(ctx: &Ctx<'_>, level: &Level, expr: &AExpr) -> Option<RawFinding> {
    let operator = operator_name(expr)?;
    let left = expr.lexpr.as_ref()?;
    let right = expr.rexpr.as_ref()?;
    let (candidates, range_op): (Vec<(&Node, Vec<&Node>)>, bool) = match expr.kind {
        AExprKind::AexprOp if matches!(operator, "=" | "<" | "<=" | ">" | ">=") => (
            vec![(left, vec![right]), (right, vec![left])],
            operator == "=",
        ),
        AExprKind::AexprBetween => {
            let Node::List(list) = right else {
                return None;
            };
            (vec![(left, list.items.iter().flatten().collect())], false)
        }
        AExprKind::AexprIn if operator == "=" => {
            let Node::List(list) = right else {
                return None;
            };
            (vec![(left, list.items.iter().flatten().collect())], false)
        }
        AExprKind::AexprOpAny if operator == "=" => (vec![(left, vec![right])], false),
        _ => return None,
    };
    for (side, others) in candidates {
        let Some(wrapped) = wrapped_column(ctx, level, side) else {
            continue;
        };
        if !others
            .iter()
            .all(|other| index_usable_value(ctx, other, &wrapped.source))
        {
            continue;
        }
        return Some(finding(ctx, expr, side, &others, &wrapped, range_op));
    }
    None
}

fn finding(
    ctx: &Ctx<'_>,
    expr: &AExpr,
    side: &Node,
    others: &[&Node],
    wrapped: &Wrapped<'_>,
    equality: bool,
) -> RawFinding {
    // Quoted as written: a `$day` parameter stays `$day`.
    let text = |node: &Node| {
        node_span(ctx, node.to_ref())
            .map(|span| ctx.written(span).to_owned())
            .unwrap_or_default()
    };
    let column = node_span(ctx, NodeRef::ColumnRef(wrapped.column))
        .map(|span| ctx.written(span).to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| wrapped_column_name(wrapped.column));
    let type_name = |oid: PgTypeOid| {
        ctx.catalog
            .type_row(oid)
            .map_or_else(|| "?".to_owned(), |ty| ty.typname.clone())
    };
    let column_type = ctx
        .catalog
        .attributes_of(wrapped.table)
        .iter()
        .find(|a| a.attnum == wrapped.attnum)
        .map(|a| a.atttypid);
    let rhs = (others.len() == 1).then(|| text(others[0]));
    let (why, help) = match &wrapped.wrap {
        Wrap::Cast { to } => {
            let why = format!("the column is cast to {}", type_name(*to));
            let help = match (column_type, rhs.as_deref()) {
                (Some(TIMESTAMP | TIMESTAMPTZ), Some(rhs)) if *to == DATE && equality => format!(
                    "compare a range on the column itself: {column} >= {rhs} AND {column} < {rhs} + 1"
                ),
                (Some(ty), _) => format!(
                    "compare the column itself, converting the other side to {} instead",
                    type_name(ty)
                ),
                (None, _) => "compare the column itself".to_owned(),
            };
            (why, help)
        }
        Wrap::Function { name, call } => {
            let why = format!("the column is an argument of {name}()");
            let unit = (name.eq_ignore_ascii_case("date_trunc"))
                .then(|| match call.args.first() {
                    Some(Some(Node::AConst(c))) => match &c.val {
                        Some(AConstValue::String(s)) => Some(s.sval.clone()),
                        _ => None,
                    },
                    _ => None,
                })
                .flatten();
            let help = match (unit, rhs.as_deref()) {
                (Some(unit), Some(rhs)) if equality => format!(
                    "compare a range on the column itself: {column} >= {rhs} AND {column} < {rhs} \
                     + interval '1 {unit}'"
                ),
                _ => format!(
                    "compare the column itself, or create an expression index on ({})",
                    text(side)
                ),
            };
            (why, help)
        }
        Wrap::Arithmetic { operator } => (
            format!("the column is an operand of {operator}"),
            "move the computation to the other side so the column stands alone".to_owned(),
        ),
        Wrap::Implicit { from, to } => (
            format!(
                "the column is converted from {} to {} to match the other side's type",
                type_name(*from),
                type_name(*to)
            ),
            format!(
                "cast the other side to {} so the column is compared as stored",
                type_name(*from)
            ),
        ),
    };
    let span = node_span(ctx, NodeRef::AExpr(expr))
        .or_else(|| node_span(ctx, side.to_ref()))
        .unwrap_or(0..0);
    RawFinding {
        span,
        message: format!(
            "this comparison can't use the index {} on {column}: {why}",
            typedpg_core::quote_identifier(&wrapped.index)
        ),
        help: Some(help),
        fix: Vec::new(),
    }
}

fn wrapped_column_name(column: &ColumnRef) -> String {
    column
        .fields
        .iter()
        .filter_map(|field| match field {
            Some(Node::String(s)) => Some(typedpg_core::quote_identifier(&s.sval)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// The indexed column `side` wraps, and how.
fn wrapped_column<'a>(ctx: &Ctx<'_>, level: &Level, side: &'a Node) -> Option<Wrapped<'a>> {
    let (column, wrap) = match side {
        Node::ColumnRef(column) => {
            let conversion = ctx.view.value_conversion(NodeRef::ColumnRef(column))?;
            if conversion.explicit || binary_coercible(ctx, conversion.from, conversion.to) {
                return None;
            }
            (
                &**column,
                Wrap::Implicit {
                    from: conversion.from,
                    to: conversion.to,
                },
            )
        }
        Node::TypeCast(cast) => {
            let Some(Node::ColumnRef(column)) = cast.arg.as_ref() else {
                return None;
            };
            let conversion = ctx.view.value_conversion(NodeRef::ColumnRef(column))?;
            if binary_coercible(ctx, conversion.from, conversion.to) {
                return None;
            }
            (&**column, Wrap::Cast { to: conversion.to })
        }
        Node::FuncCall(call) => {
            if call.over.is_some()
                || call.agg_star
                || call.agg_distinct
                || call.agg_filter.is_some()
                || !call.agg_order.is_empty()
            {
                return None;
            }
            let mut found = None;
            for arg in call.args.iter().flatten() {
                match arg {
                    Node::ColumnRef(column) if found.is_none() => found = Some(&**column),
                    Node::TypeCast(cast)
                        if found.is_none()
                            && matches!(cast.arg.as_ref(), Some(Node::ColumnRef(_))) =>
                    {
                        let Some(Node::ColumnRef(column)) = cast.arg.as_ref() else {
                            return None;
                        };
                        found = Some(&**column);
                    }
                    other if has_column_ref(other) => return None,
                    _ => {}
                }
            }
            let name = call.funcname.iter().rev().find_map(|part| match part {
                Some(Node::String(s)) => Some(s.sval.clone()),
                _ => None,
            })?;
            (found?, Wrap::Function { name, call })
        }
        Node::AExpr(e) if e.kind == AExprKind::AexprOp => {
            let operator = operator_name(e)?;
            if !matches!(operator, "+" | "-" | "*" | "/" | "%" | "||") {
                return None;
            }
            let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
            let column = match (l, r) {
                (Node::ColumnRef(column), other) | (other, Node::ColumnRef(column))
                    if !has_column_ref(other) =>
                {
                    &**column
                }
                _ => return None,
            };
            (
                column,
                Wrap::Arithmetic {
                    operator: operator.to_owned(),
                },
            )
        }
        _ => return None,
    };
    let binding = ctx.view.column_ref_binding(column.location)?;
    if binding.outer {
        return None;
    }
    let source = binding.source.clone()?;
    let table = level.table(&source)?;
    let name = match column.fields.last()? {
        Some(Node::String(s)) => s.sval.as_str(),
        _ => return None,
    };
    let attnum = ctx
        .catalog
        .attributes_of(table)
        .iter()
        .find(|a| a.attnum > 0 && a.attname == name)?
        .attnum;
    let index = leading_btree_index(ctx, table, attnum)?;
    Some(Wrapped {
        column,
        source,
        table,
        attnum,
        index,
        wrap,
    })
}

/// The name of a non-partial btree index of `table` led by column
/// `attnum`, when the table has no expression index (which might match the
/// wrapped expression).
fn leading_btree_index(ctx: &Ctx<'_>, table: PgClassOid, attnum: i16) -> Option<String> {
    let indexes: Vec<_> = ctx
        .catalog
        .iter_indexes()
        .filter(|index| index.indrelid == table)
        .collect();
    if indexes
        .iter()
        .any(|index| index.indkey.contains(&0) || !index.indexprs.is_empty())
    {
        return None;
    }
    let index = indexes.iter().find(|index| {
        index.indpred.is_none()
            && index.indkey.first() == Some(&attnum)
            && ctx.catalog.index_access_method(index.indexrelid) == "btree"
    })?;
    ctx.catalog
        .class_row(index.indexrelid)
        .map(|class| class.relname.clone())
}

/// Whether converting `from` to `to` keeps the value's bits, so an index on
/// the column still matches (PostgreSQL strips such a relabeling).
fn binary_coercible(ctx: &Ctx<'_>, from: PgTypeOid, to: PgTypeOid) -> bool {
    from == to
        || ctx.catalog.iter_casts().any(|cast| {
            cast.castsource == from
                && cast.casttarget == to
                && cast.castmethod == CastMethod::Binary
        })
}

/// Whether `node` is a value an index scan on a column of entry `source`
/// could compare with: it reads no column of `source` (nor one this can't
/// place), runs no subquery and calls nothing volatile.
fn index_usable_value(ctx: &Ctx<'_>, node: &Node, source: &str) -> bool {
    node.nodes().all(|(inner, _)| match inner {
        NodeRef::SubLink(_) => false,
        NodeRef::ColumnRef(column) => ctx
            .view
            .column_ref_binding(column.location)
            .is_some_and(|binding| binding.source.as_deref().is_some_and(|s| s != source)),
        NodeRef::FuncCall(call) => !ctx.catalog.call_is_volatile(call),
        _ => true,
    })
}

fn has_column_ref(node: &Node) -> bool {
    node.nodes()
        .any(|(inner, _)| matches!(inner, NodeRef::ColumnRef(_)))
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::NonSargablePredicate;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE users (
                    id int PRIMARY KEY,
                    email varchar(100) NOT NULL,
                    code char(3),
                    name text,
                    created_at timestamptz NOT NULL
                 );
                 CREATE INDEX users_email ON users (email);
                 CREATE INDEX users_created_at ON users (created_at);
                 CREATE INDEX users_name_partial ON users (name) WHERE name IS NOT NULL;
                 CREATE TABLE posts (
                    id bigint PRIMARY KEY,
                    user_id int NOT NULL REFERENCES users,
                    slug text NOT NULL,
                    posted_on timestamp NOT NULL
                 );
                 CREATE INDEX posts_posted_on ON posts (posted_on);
                 CREATE TABLE slugs (id int PRIMARY KEY, slug text NOT NULL);
                 CREATE INDEX slugs_slug ON slugs (slug);
                 CREATE INDEX slugs_lower ON slugs (lower(slug));",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn a_wrapped_indexed_column_is_reported() {
        let db = catalog();
        for (sql, help) in [
            (
                "SELECT id FROM users WHERE created_at::date = $1",
                "created_at >= $1 AND created_at < $1 + 1",
            ),
            // Quoted as written: named parameters keep their names.
            (
                "SELECT id FROM users WHERE created_at::date = $day",
                "created_at >= $day AND created_at < $day + 1",
            ),
            (
                "SELECT id FROM posts WHERE date_trunc('day', posted_on) = $day",
                "posted_on >= $day AND posted_on < $day + interval '1 day'",
            ),
            (
                "SELECT id FROM users WHERE lower(email) = $email AND name = $name",
                "expression index on (lower(email))",
            ),
            (
                "SELECT id FROM posts WHERE date_trunc('day', posted_on) = $1",
                "posted_on >= $1 AND posted_on < $1 + interval '1 day'",
            ),
            (
                "SELECT id FROM users WHERE lower(email) = $1",
                "expression index on (lower(email))",
            ),
            (
                "SELECT id FROM users WHERE id::text = $1",
                "converting the other side to int4",
            ),
            (
                "SELECT id FROM users WHERE id + 1 = $1",
                "move the computation",
            ),
            (
                "SELECT name FROM users WHERE id = 1.5",
                "cast the other side to int4",
            ),
            (
                "SELECT name FROM users WHERE id IN (1, 2.5)",
                "cast the other side to int4",
            ),
            (
                "SELECT u.name FROM users u JOIN posts p ON u.id::bigint = p.id",
                "converting the other side",
            ),
            (
                "UPDATE users SET name = 'x' WHERE lower(email) = $1",
                "expression index",
            ),
            ("DELETE FROM users WHERE id::text = $1", "converting"),
            (
                "SELECT id FROM users WHERE created_at::date BETWEEN $1 AND $2",
                "compare the column itself",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let h = findings[0].help.as_deref().unwrap();
            assert!(h.contains(help), "{sql}: {h}");
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn sargable_or_unindexed_comparisons_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM users WHERE created_at >= $1",
            "SELECT id FROM users WHERE email = $1",
            // Binary-compatible: varchar is compared as text.
            "SELECT id FROM users WHERE email::text = $1",
            "SELECT id FROM users WHERE email = 'a'::text",
            // A cast to its own type changes nothing.
            "SELECT id FROM users WHERE id::int = $1",
            // Cross-type operators of the index's family.
            "SELECT id FROM users WHERE id = 5::bigint",
            "SELECT id FROM posts WHERE posted_on >= now()",
            // No index on name (only a partial one), code or slug.
            "SELECT id FROM users WHERE lower(name) = $1",
            "SELECT id FROM users WHERE upper(code) = 'ABC'",
            "SELECT id FROM posts WHERE lower(slug) = $1",
            // An expression index may match.
            "SELECT id FROM slugs WHERE lower(slug) = $1",
            "SELECT id FROM slugs WHERE upper(slug) = $1",
            // The other side reads the same table.
            "SELECT id FROM users WHERE lower(email) = name",
            // Not a comparison an index serves, or not top-level.
            "SELECT id FROM users WHERE lower(email) <> $1",
            "SELECT id FROM users WHERE lower(email) = $1 OR id = 2",
            "SELECT id FROM users WHERE lower(email) LIKE 'a%'",
            // In the select list.
            "SELECT lower(email) = 'a' FROM users",
            // A subquery's output isn't a table.
            "SELECT id FROM (SELECT id, email FROM users) s WHERE lower(email) = $1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
