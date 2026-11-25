//! The `non_sargable_predicate` rule: reports a `WHERE` or `JOIN … ON`
//! comparison (`=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (…)`, `= ANY`)
//! that wraps an indexed column so no index on it can serve the
//! comparison: PostgreSQL matches an index only to the bare column (a
//! binary-compatible relabeling aside), never to an expression of it.
//!
//! - A cast that converts the value (`created_at::date = $1`,
//!   `id::text = $1`); a cast to the type it has, or to a binary-compatible
//!   one the index's operator family compares (`varchar` to `text` under
//!   `text_ops`), keeps the column — `citext` relabeled to `text` under
//!   `citext_ops` doesn't.
//! - A function call with the column as an argument (`lower(email) = $1`,
//!   `date_trunc('day', created_at) = $1`).
//! - Arithmetic or concatenation with the column as an operand
//!   (`id + 1 = $1`).
//! - An implicit conversion the other operand's type forces on the column
//!   (an `int4` column compared with a `numeric` value is converted to
//!   `numeric`): the column's value is converted for every row. A
//!   polymorphic operator (an enum's `anyenum` `=`) converts nothing (see
//!   [`typedpg_analyzer::QueryView::value_conversion`]).
//!
//! Only reported when the comparison would otherwise have an index: the
//! column (resolved to a plain or partitioned table of the same query
//! level) leads a non-partial btree index, and the table has no expression
//! index at all (one may match the expression). The other side must be a
//! value an index scan could use: no column of the same table, no subquery,
//! no volatile call. A plain indexed column of another relation is not
//! reported: the planner computes the wrapped side's value and looks that
//! relation up with an index scan on its own index (a nested loop with
//! `b."level" = a."level" + 1` as the index condition), so the comparison is
//! sargable through that side. Only the top-level `AND` parts of a condition
//! are read.
//! The help names the sargable rewrite when it is a range (`::date`,
//! `date_trunc`): one no other rule reports (a `timestamptz` column's
//! bounds convert the day explicitly, as the cast reads it in the session's
//! `TimeZone`), and says when it selects other rows (around a
//! daylight-saving change at midnight, a `date_trunc` value not truncated).
//! No range is named where none is equivalent. No fix is offered.

use typedpg_analyzer::{CastMethod, PgClassOid, PgOpclassOid, PgTypeOid, RelKind, RelationRef};
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, AExpr, AExprKind, ColumnRef, Node};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding};

const DATE: PgTypeOid = PgTypeOid::from_raw(1082);
const TIMESTAMP: PgTypeOid = PgTypeOid::from_raw(1114);
const TIMESTAMPTZ: PgTypeOid = PgTypeOid::from_raw(1184);
const UNKNOWN: PgTypeOid = PgTypeOid::from_raw(705);

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

/// A binary comparison read with the wrapped column's side on the left
/// (`$1 < f(col)` is `f(col) > $1`).
struct Compared {
    /// `=`, `<`, `<=`, `>`, `>=`, or `BETWEEN` (its two bounds the other
    /// side).
    op: &'static str,
    /// The other side's type as the operator takes it: `unknown` for a
    /// parameter or an untyped literal it typed. `None` when unresolved.
    other_type: Option<PgTypeOid>,
}

fn comparison(ctx: &Ctx<'_>, level: &Level, expr: &AExpr) -> Option<RawFinding> {
    let operator = operator_name(expr)?;
    let left = expr.lexpr.as_ref()?;
    let right = expr.rexpr.as_ref()?;
    type Candidate<'n> = (&'n Node, Vec<&'n Node>, Option<Compared>);
    let candidates: Vec<Candidate<'_>> = match expr.kind {
        AExprKind::AexprOp if matches!(operator, "=" | "<" | "<=" | ">" | ">=") => {
            let types = ctx.view.operator_types(expr.location);
            let (op, flipped): (&'static str, &'static str) = match operator {
                "=" => ("=", "="),
                "<" => ("<", ">"),
                "<=" => ("<=", ">="),
                ">" => (">", "<"),
                _ => (">=", "<="),
            };
            vec![
                (
                    left,
                    vec![right],
                    Some(Compared {
                        op,
                        other_type: types.map(|t| t.right),
                    }),
                ),
                (
                    right,
                    vec![left],
                    Some(Compared {
                        op: flipped,
                        other_type: types.and_then(|t| t.left),
                    }),
                ),
            ]
        }
        AExprKind::AexprBetween => {
            let Node::List(list) = right else {
                return None;
            };
            // Its two comparisons share its location: one type when the
            // bounds agree.
            let types = ctx.view.operator_types(expr.location);
            vec![(
                left,
                list.items.iter().flatten().collect(),
                Some(Compared {
                    op: "BETWEEN",
                    other_type: types.map(|t| t.right),
                }),
            )]
        }
        AExprKind::AexprIn if operator == "=" => {
            let Node::List(list) = right else {
                return None;
            };
            vec![(left, list.items.iter().flatten().collect(), None)]
        }
        AExprKind::AexprOpAny if operator == "=" => vec![(left, vec![right], None)],
        _ => return None,
    };
    for (side, others, compared) in candidates {
        let Some(wrapped) = wrapped_column(ctx, level, side) else {
            continue;
        };
        if !others
            .iter()
            .all(|other| index_usable_value(ctx, other, &wrapped.source))
        {
            continue;
        }
        // A nested loop looks the other relation up by the value this side
        // computes (`b."level" = a."level" + 1`, an index scan on `b`): the
        // comparison is sargable through that side, so this column's index
        // is not what the planner reaches for.
        if compared.is_some()
            && matches!(others.as_slice(), [other]
                if indexed_other_column(ctx, level, other, &wrapped.source))
        {
            continue;
        }
        return Some(finding(
            ctx,
            expr,
            side,
            &others,
            &wrapped,
            compared.as_ref(),
        ));
    }
    None
}

fn finding(
    ctx: &Ctx<'_>,
    expr: &AExpr,
    side: &Node,
    others: &[&Node],
    wrapped: &Wrapped<'_>,
    compared: Option<&Compared>,
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
    // The other side, single, and how the comparison reads it.
    let other = match (others, compared) {
        ([other], Some(compared)) if compared.op != "BETWEEN" => Some((*other, compared)),
        _ => None,
    };
    let (why, help) = match &wrapped.wrap {
        Wrap::Cast { to } => {
            let why = format!("the column is cast to {}", type_name(*to));
            let day_cast = *to == DATE && matches!(column_type, Some(TIMESTAMP | TIMESTAMPTZ));
            let range = match (column_type, compared) {
                (Some(ty), Some(compared)) if day_cast => {
                    date_range(ctx, &column, ty, others, compared)
                }
                _ => None,
            };
            let help = match (range, column_type) {
                (Some(help), _) => help,
                // Converting the other side would compare instants with
                // each day's midnight: a range is what matches the day.
                (None, Some(_)) if day_cast => "compare a range on the column itself: a day \
                                                is from its midnight up to the next day's"
                    .to_owned(),
                (None, Some(ty)) => format!(
                    "compare the column itself, converting the other side to {} instead (the \
                     same rows only where that conversion is exact: a value it rounds, or a \
                     text it reads other than as written, compares differently)",
                    type_name(ty)
                ),
                (None, None) => "compare the column itself".to_owned(),
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
            let range = match (unit, column_type, other) {
                (Some(unit), Some(ty @ (TIMESTAMP | TIMESTAMPTZ)), Some((other, compared)))
                    if call.args.len() == 2
                        && matches!(call.args.get(1), Some(Some(Node::ColumnRef(_)))) =>
                {
                    trunc_range(ctx, &column, ty, &unit, other, compared)
                }
                _ => None,
            };
            let help = range.unwrap_or_else(|| {
                format!(
                    "compare the column itself, or create an expression index on ({})",
                    text(side)
                )
            });
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
                "cast the other side to {} so the column is compared as stored (the same rows \
                 only where that cast is exact: a value it rounds or truncates compares \
                 differently)",
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

/// The other side of a comparison as an expression of type `date`, written:
/// as is when it is one (in parentheses unless it is a single term), with
/// `::date` when it is a parameter or an untyped literal the comparison
/// typed. `None` for any other type: no range is equivalent then.
fn date_operand(ctx: &Ctx<'_>, other: &Node, compared: &Compared) -> Option<String> {
    let written = ctx.written(node_span(ctx, other.to_ref())?).to_owned();
    match compared.other_type? {
        DATE => Some(match other {
            Node::ParamRef(_)
            | Node::ColumnRef(_)
            | Node::FuncCall(_)
            | Node::TypeCast(_)
            | Node::SqlValueFunction(_)
            | Node::AConst(_) => written,
            _ => format!("({written})"),
        }),
        UNKNOWN => match other {
            Node::ParamRef(_) => Some(format!("{written}::date")),
            Node::AConst(c) if matches!(c.val, Some(AConstValue::String(_))) => {
                Some(format!("{written}::date"))
            }
            _ => None,
        },
        _ => None,
    }
}

/// The range on the bare column `column` (of type `timestamp` or
/// `timestamptz`) that selects what `column::date <op> other` does: the
/// day starts at its midnight — for `timestamptz`, in the session's
/// `TimeZone`, as the `::date` cast reads it, so the bounds convert the
/// day explicitly (`::timestamptz`) rather than leave the comparison to
/// mix `date` and `timestamptz` (see `timestamp_timezone_mix`).
fn date_range(
    ctx: &Ctx<'_>,
    column: &str,
    column_type: PgTypeOid,
    others: &[&Node],
    compared: &Compared,
) -> Option<String> {
    // A day's first instant, and the next day's.
    let bounds = |other: &Node| {
        let day = date_operand(ctx, other, compared)?;
        Some(if column_type == TIMESTAMPTZ {
            (
                format!("{day}::timestamptz"),
                format!("({day} + 1)::timestamptz"),
            )
        } else {
            (day.clone(), format!("{day} + 1"))
        })
    };
    let range = match (compared.op, others) {
        ("BETWEEN", [low, high]) => {
            let ((start, _), (_, next)) = (bounds(low)?, bounds(high)?);
            format!("{column} >= {start} AND {column} < {next}")
        }
        (op, [other]) => {
            let (start, next) = bounds(other)?;
            match op {
                "=" => format!("{column} >= {start} AND {column} < {next}"),
                "<" => format!("{column} < {start}"),
                "<=" => format!("{column} < {next}"),
                ">" => format!("{column} >= {next}"),
                ">=" => format!("{column} >= {start}"),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(if column_type == TIMESTAMPTZ {
        format!(
            "compare a range on the column itself: `{range}` reads days in the session's \
             TimeZone as the cast does, and selects the same rows but around a daylight-saving \
             change at midnight; for days of a fixed zone instead (other rows, unless the \
             session is in that zone), convert each bound with `::timestamp AT TIME ZONE 'UTC'`"
        )
    } else {
        format!(
            "compare a range on the column itself: `{range}` selects the same rows (a date \
             compares with a timestamp as its midnight)"
        )
    })
}

/// The range on the bare column `column` (of type `timestamp` or
/// `timestamptz`) that selects what `date_trunc('<unit>', column) = other`
/// does, for a parameter or literal `other` or one of the column's type.
/// Adding the interval to a `timestamptz` steps through the session's
/// `TimeZone` as `date_trunc` truncates in it.
fn trunc_range(
    ctx: &Ctx<'_>,
    column: &str,
    column_type: PgTypeOid,
    unit: &str,
    other: &Node,
    compared: &Compared,
) -> Option<String> {
    if compared.op != "=" {
        return None;
    }
    let step = match unit.to_ascii_lowercase().as_str() {
        unit @ ("microseconds" | "milliseconds" | "second" | "minute" | "hour" | "day" | "week"
        | "month" | "year" | "decade" | "century" | "millennium") => {
            format!("1 {unit}")
        }
        "quarter" => "3 months".to_owned(),
        _ => return None,
    };
    let written = ctx.written(node_span(ctx, other.to_ref())?).to_owned();
    let type_name = if column_type == TIMESTAMPTZ {
        "timestamptz"
    } else {
        "timestamp"
    };
    let value = match (compared.other_type?, other) {
        (UNKNOWN, Node::ParamRef(_)) => written,
        (UNKNOWN, Node::AConst(c)) if matches!(c.val, Some(AConstValue::String(_))) => {
            format!("{written}::{type_name}")
        }
        (ty, Node::ParamRef(_) | Node::ColumnRef(_) | Node::FuncCall(_) | Node::TypeCast(_))
            if ty == column_type =>
        {
            written
        }
        (ty, _) if ty == column_type => format!("({written})"),
        _ => return None,
    };
    let dst = if column_type == TIMESTAMPTZ {
        ", but around a daylight-saving change at midnight"
    } else {
        ""
    };
    Some(format!(
        "compare a range on the column itself: `{column} >= {value} AND {column} < {value} + \
         interval '{step}'` selects the same rows when {value} is itself truncated to the \
         {unit} (date_trunc never equals any other value){dst}"
    ))
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
    // The type a binary-compatible conversion relabels the column to:
    // PostgreSQL strips the relabeling when it matches an index, so the
    // index serves the comparison when its operator family compares that
    // type (`varchar` relabeled to `text` under `text_ops`), and not
    // otherwise (`citext` relabeled to `text` under `citext_ops`).
    let mut relabeled = None;
    let (column, wrap) = match side {
        Node::ColumnRef(column) => {
            let conversion = ctx.view.value_conversion(NodeRef::ColumnRef(column))?;
            if conversion.explicit {
                return None;
            }
            if binary_coercible(ctx, conversion.from, conversion.to) {
                relabeled = Some(conversion.to);
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
                relabeled = Some(conversion.to);
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
    let (index, opclass) = leading_btree_index(ctx, table, attnum)?;
    if relabeled.is_some_and(|to| family_compares(ctx, opclass, to)) {
        return None;
    }
    Some(Wrapped {
        column,
        source,
        table,
        attnum,
        index,
        wrap,
    })
}

/// Whether `node` is a plain column of another FROM entry, with the index
/// that serves the comparison the other way round: a nested loop looks
/// that relation up by the value this side computes, so the wrapped
/// column's own index is irrelevant to the comparison.
fn indexed_other_column(ctx: &Ctx<'_>, level: &Level, node: &Node, wrapped_source: &str) -> bool {
    let Node::ColumnRef(column) = node else {
        return false;
    };
    let Some(binding) = ctx.view.column_ref_binding(column.location) else {
        return false;
    };
    if binding.outer {
        return false;
    }
    let Some(source) = binding
        .source
        .as_deref()
        .filter(|source| *source != wrapped_source)
    else {
        return false;
    };
    let Some(table) = level.table(source) else {
        return false;
    };
    let name = match column.fields.last() {
        Some(Some(Node::String(s))) => s.sval.as_str(),
        _ => return false,
    };
    let Some(attnum) = ctx
        .catalog
        .attributes_of(table)
        .iter()
        .find(|attribute| attribute.attnum > 0 && attribute.attname == name)
        .map(|attribute| attribute.attnum)
    else {
        return false;
    };
    let Some((_, opclass)) = leading_btree_index(ctx, table, attnum) else {
        return false;
    };
    // As for the wrapped column: the comparison reads this one as stored,
    // or relabeled to a type its index's operator family compares. A
    // conversion that changes the value leaves the index unable to serve.
    match ctx.view.value_conversion(NodeRef::ColumnRef(column)) {
        None => true,
        Some(conversion) => {
            !conversion.explicit
                && binary_coercible(ctx, conversion.from, conversion.to)
                && family_compares(ctx, opclass, conversion.to)
        }
    }
}

/// The name of the first non-partial btree index of `table` led by column
/// `attnum`, and its leading column's operator class, when the table has no
/// expression index (which might match the wrapped expression).
fn leading_btree_index(
    ctx: &Ctx<'_>,
    table: PgClassOid,
    attnum: i16,
) -> Option<(String, Option<PgOpclassOid>)> {
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
    // The first such index created (the catalog's iteration order is
    // arbitrary): the same one every run.
    let index = indexes
        .iter()
        .filter(|index| {
            index.indpred.is_none()
                && index.indkey.first() == Some(&attnum)
                && ctx.catalog.index_access_method(index.indexrelid) == "btree"
        })
        .min_by_key(|index| index.indexrelid)?;
    ctx.catalog.class_row(index.indexrelid).map(|class| {
        (
            class.relname.clone(),
            index.indclass.first().copied().flatten(),
        )
    })
}

/// Whether the btree operator family of `opclass` holds an operator whose
/// left operand is of type `ty`: one an index scan can match a column
/// relabeled to `ty` with. An unresolved operator class counts as one that
/// does (no finding without the proof).
fn family_compares(ctx: &Ctx<'_>, opclass: Option<PgOpclassOid>, ty: PgTypeOid) -> bool {
    opclass
        .and_then(|opclass| ctx.catalog.opclass_family_has_left_type(opclass, ty))
        .unwrap_or(true)
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

    use crate::rules::test_support::lint_rule;
    use crate::{LintOptions, Rule, Severity, lint_query};

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
                 CREATE INDEX slugs_lower ON slugs (lower(slug));
                 CREATE TABLE leagues (id bigint PRIMARY KEY, level int NOT NULL, name text);
                 CREATE INDEX leagues_level ON leagues (level);
                 CREATE TABLE rungs (id int PRIMARY KEY, level int NOT NULL, label text);",
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
                "`created_at >= $1::date::timestamptz AND created_at < ($1::date + \
                 1)::timestamptz`",
            ),
            // Quoted as written: named parameters keep their names.
            (
                "SELECT id FROM users WHERE created_at::date = $day",
                "`created_at >= $day::date::timestamptz AND created_at < ($day::date + \
                 1)::timestamptz` reads days in the session's TimeZone as the cast does",
            ),
            (
                "SELECT id FROM posts WHERE posted_on::date = $day",
                "`posted_on >= $day::date AND posted_on < $day::date + 1` selects the same rows",
            ),
            (
                "SELECT id FROM posts WHERE $day < posted_on::date",
                "`posted_on >= $day::date + 1`",
            ),
            (
                "SELECT id FROM users WHERE created_at::date <= current_date - 7",
                "`created_at < ((current_date - 7) + 1)::timestamptz`",
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
                "UPDATE users SET name = 'x' WHERE lower(email) = $1",
                "expression index",
            ),
            ("DELETE FROM users WHERE id::text = $1", "converting"),
            (
                "SELECT id FROM users WHERE created_at::date BETWEEN $1 AND $2",
                "`created_at >= $1::date::timestamptz AND created_at < ($2::date + \
                 1)::timestamptz`",
            ),
            (
                "SELECT id FROM users WHERE created_at::date IN ($1, $2)",
                "a day is from its midnight up to the next day's",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let h = findings[0].help.as_deref().unwrap();
            assert!(h.contains(help), "{sql}: {h}");
            assert!(findings[0].fix.is_empty());
        }
    }

    /// A column reaching the operator unconverted — a polymorphic
    /// parameter (`anyenum`, `anyarray`) takes it as it is, a domain is its
    /// base type, a cross-type operator (`int4 = int8`) is in the index's
    /// family — or relabeled to a type its index's operator family compares
    /// (`varchar` as `text`) is served by the index: EXPLAIN shows an Index
    /// Scan for every one of these on PostgreSQL. A relabeling the family
    /// doesn't compare (`citext` as `text`: `citext_ops` has no `text`
    /// operator, EXPLAIN shows a Filter) is reported.
    #[test]
    fn a_column_the_index_sees_through_its_conversion_is_not_reported() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            "CREATE EXTENSION citext;
             CREATE TYPE arch AS ENUM ('x86_64', 'aarch64');
             CREATE TABLE fa (arch arch PRIMARY KEY, n int);
             CREATE TABLE v (c varchar(10) PRIMARY KEY);
             CREATE DOMAIN dt AS text;
             CREATE TABLE d (c dt PRIMARY KEY);
             CREATE TABLE i (c int4 PRIMARY KEY);
             CREATE TABLE ar (c int[] PRIMARY KEY);
             CREATE TABLE ci (c citext PRIMARY KEY);",
        )
        .unwrap();
        for sql in [
            "SELECT n FROM fa WHERE arch = 'x86_64'",
            "SELECT n FROM fa WHERE arch = 'x86_64'::text::arch",
            "SELECT n FROM fa WHERE arch IN ('x86_64', 'aarch64')",
            "SELECT n FROM fa WHERE arch = ANY('{x86_64}'::arch[])",
            "SELECT n FROM fa WHERE arch < $1",
            "SELECT c FROM v WHERE c = 'a'::text",
            "SELECT c FROM v WHERE c = $1",
            "SELECT c FROM d WHERE c = 'a'::text",
            "SELECT c FROM i WHERE c = 5::int8",
            "SELECT c FROM ar WHERE c = '{1}'::int[]",
            "SELECT c FROM ci WHERE c = 'a'",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        for (sql, message) in [
            (
                "SELECT c FROM ci WHERE c = 'a'::text",
                "the column is converted from citext to text",
            ),
            (
                "SELECT c FROM ci WHERE c::text = $1",
                "the column is cast to text",
            ),
            (
                "SELECT c FROM i WHERE c = 5::numeric",
                "the column is converted from int4 to numeric",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].message.contains(message), "{sql}: {findings:?}");
        }
    }

    /// Every concrete rewrite the help suggests — the text between its
    /// first pair of backticks, put in place of the comparison — compiles,
    /// and no rule reports it (every rule enabled).
    #[test]
    fn every_suggested_rewrite_passes_every_rule() {
        let db = catalog();
        let mut all = LintOptions::default();
        for rule in Rule::ALL {
            all.levels.insert(rule, Some(Severity::Info));
        }
        let mut checked = 0;
        for sql in [
            "SELECT id FROM users WHERE created_at::date = $day",
            "SELECT id FROM users WHERE created_at::date = '2024-05-01'",
            "SELECT id FROM users WHERE created_at::date = current_date",
            "SELECT id FROM users WHERE created_at::date = current_date - 7",
            "SELECT id FROM users WHERE created_at::date < $day",
            "SELECT id FROM users WHERE created_at::date <= $day",
            "SELECT id FROM users WHERE created_at::date > $day",
            "SELECT id FROM users WHERE created_at::date >= $day",
            "SELECT id FROM users WHERE $day >= created_at::date",
            "SELECT id FROM users WHERE created_at::date BETWEEN $from AND $to",
            "SELECT id FROM posts WHERE posted_on::date BETWEEN '2024-05-01' AND $to",
            "SELECT id FROM posts WHERE posted_on::date = $day",
            "SELECT id FROM posts WHERE posted_on::date = '2024-05-01'",
            "SELECT id FROM posts WHERE posted_on::date = current_date",
            "SELECT id FROM posts WHERE posted_on::date > $day",
            "SELECT id FROM posts WHERE $day <= posted_on::date",
            "SELECT id FROM posts WHERE date_trunc('day', posted_on) = $day",
            "SELECT id FROM posts WHERE date_trunc('month', posted_on) = '2024-05-01'",
            "SELECT id FROM posts WHERE date_trunc('quarter', posted_on) = $q",
            "SELECT id FROM posts WHERE date_trunc('hour', posted_on) = localtimestamp(0)",
            "SELECT id FROM users WHERE date_trunc('day', created_at) = $day",
            "SELECT id FROM users WHERE date_trunc('week', created_at) = '2024-05-06'",
            "SELECT id FROM users WHERE $day = date_trunc('day', created_at)",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let help = findings[0].help.as_deref().unwrap();
            let rewrite = help
                .split('`')
                .nth(1)
                .unwrap_or_else(|| panic!("{sql}: no rewrite in {help}"));
            let span = findings[0].span.clone();
            let rewritten = format!("{}{rewrite}{}", &sql[..span.start], &sql[span.end..]);
            let found = lint_query(&db, &rewritten, &all)
                .unwrap_or_else(|e| panic!("{rewritten}: {}", e.error));
            assert!(found.is_empty(), "{sql} → {rewritten}: {found:?}");
            checked += 1;
        }
        assert_eq!(checked, 23);
        // The fixed-zone alternative the timestamptz help names passes too.
        let fixed = "SELECT id FROM users WHERE created_at >= $day::date::timestamp AT TIME ZONE \
                     'UTC' AND created_at < ($day::date + 1)::timestamp AT TIME ZONE 'UTC'";
        let found = lint_query(&db, fixed, &all).unwrap();
        assert!(found.is_empty(), "{found:?}");
        // The check would catch a rewrite another rule reports: the range
        // with a bare `date` bound on a timestamptz column mixes time zones.
        let found = lint_query(
            &db,
            "SELECT id FROM users WHERE created_at >= $day::date AND created_at < $day::date + 1",
            &all,
        )
        .unwrap();
        assert!(
            found
                .iter()
                .any(|finding| finding.rule == Rule::TimestampTimezoneMix),
            "{found:?}"
        );
    }

    /// The other sides no range is equivalent for get the general help.
    #[test]
    fn no_range_is_suggested_where_none_is_equivalent() {
        let db = catalog();
        for sql in [
            // A timestamp: `date = timestamp` compares the date's midnight.
            "SELECT id FROM posts WHERE posted_on::date = localtimestamp",
            // A list.
            "SELECT id FROM posts WHERE posted_on::date = ANY ($1::date[])",
            // A truncation in an explicit zone, or of an unknown unit.
            "SELECT id FROM users WHERE date_trunc('day', created_at, 'UTC') = $1",
            "SELECT id FROM posts WHERE date_trunc('dow', posted_on) = $1",
            // Only equality has a range for date_trunc.
            "SELECT id FROM posts WHERE date_trunc('day', posted_on) < $1",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            let help = findings[0].help.as_deref().unwrap();
            assert!(!help.contains('`'), "{sql}: {help}");
        }
    }

    /// The other side of a join comparison is a plain indexed column of
    /// another relation: the planner looks that relation up by the value
    /// this side computes (a nested loop with an index scan on it), so the
    /// comparison is sargable through that side and the wrapped column's
    /// own index is irrelevant.
    #[test]
    fn a_join_lookup_through_the_other_sides_index_is_not_reported() {
        let db = catalog();
        for sql in [
            // EXPLAIN: Nested Loop, `Index Cond: (level = (c.level + 1))` on
            // leagues_level.
            "SELECT c.id FROM leagues c JOIN leagues n ON n.level = c.level + 1",
            // The same comparison the other way round.
            "SELECT c.id FROM leagues c JOIN leagues n ON c.level + 1 = n.level",
            "SELECT c.id FROM leagues c, rungs r WHERE r.id = c.level + 1",
            // The other side's primary key serves the lookup.
            "SELECT c.id FROM leagues c JOIN rungs r ON r.id = c.level + 1",
            // A cast on this side: posts_pkey serves the lookup
            // (`Index Cond: (id = (u.id)::bigint)`).
            "SELECT u.name FROM users u JOIN posts p ON u.id::bigint = p.id",
            "SELECT u.name FROM users u JOIN posts p ON p.id = u.id::bigint",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
        // Not a single plain column on the other side: an unindexed column
        // or an expression.
        for (sql, index) in [
            (
                "SELECT c.id FROM leagues c JOIN rungs r ON r.level = c.level + 1",
                "leagues_level",
            ),
            (
                "SELECT c.id FROM leagues c JOIN rungs r ON r.level + 0 = c.level + 1",
                "leagues_level",
            ),
            (
                r#"SELECT c.id FROM leagues c JOIN rungs r ON r.level = c.level + 1 AND r.id = $1"#,
                "leagues_level",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(
                findings[0].message.contains(index),
                "{sql}: {:?}",
                findings[0].message
            );
        }
        for sql in [
            "SELECT id FROM users WHERE lower(email) = $1",
            "SELECT id FROM users WHERE lower(email) = 'a'",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
        }
    }

    /// The reported shape: a self-join on the next level, whose lookup the
    /// index on the other alias's column serves (`League_level_key` is the
    /// index condition; the wrapped expression's own index is not used).
    #[test]
    fn the_league_self_join_is_not_reported() {
        let mut db = PgCatalog::new().unwrap();
        db.apply_sql(
            r#"CREATE SCHEMA "olitef";
               CREATE TABLE "olitef"."League" ("id" bigint PRIMARY KEY, "level" int NOT NULL);
               CREATE INDEX "League_level_key" ON "olitef"."League" ("level");"#,
        )
        .unwrap();
        let sql = r#"SELECT current_league."id" FROM "olitef"."League" current_league
                     JOIN "olitef"."League" next_league
                       ON next_league."level" = current_league."level" + 1"#;
        let findings = lint_rule(&db, sql, RULE);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn the_index_named_is_the_first_one_created() {
        // Several btree indexes lead with the column: the message names the
        // same one every run, whatever order the catalog keeps them in.
        for _ in 0..20 {
            let mut db = PgCatalog::new().unwrap();
            db.apply_sql(
                "CREATE TABLE codes (id bigint, code text NOT NULL, extra int);
                 CREATE INDEX codes_a ON codes (code);
                 CREATE INDEX codes_b ON codes (code, extra);
                 CREATE INDEX codes_c ON codes (code, id);",
            )
            .unwrap();
            let findings = lint_rule(
                &db,
                "SELECT id FROM codes WHERE lower(code) = $1",
                Rule::NonSargablePredicate,
            );
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert!(
                findings[0].message.contains("index codes_a "),
                "{findings:?}"
            );
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
