//! The values a `DO` block's variables hold, as far as the analyzer can
//! know them without running the block, and what a statement it can't run
//! may change (see [`super::do_block`] and [`super::uncertain`]).
//!
//! A value is a set of possible constants — one when it is known — or a
//! string with unknown parts ([`Val`]): the literal pieces of a `format()`
//! or a concatenation around the parts the analyzer doesn't know. That is
//! enough to tell what an `EXECUTE format('GRANT %s ON %s TO %I', ...)`
//! can do — grant to the role the last argument names — without knowing
//! the privilege or the object. Nothing here evaluates a query: a condition
//! reading the catalog is decided only for the few catalog views whose rows
//! the analyzer's catalog holds directly (roles, publications).

use typedpg_pg_query::ast::{self, Node};

use super::uncertain::{RoleScope, Uncertain};
use crate::pg_catalog::PgCatalog;

/// How many alternatives a value or an `EXECUTE`d string is followed with
/// before the analyzer treats it as unknown.
const MAX_ALTERNATIVES: usize = 64;

/// A value: one of a few constants (`None` is NULL), as their type's
/// output text — a single one when it is known — or a string with unknown
/// parts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    OneOf(Vec<Option<String>>),
    Text(Vec<Piece>),
}

/// A part of a string [`Val`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Piece {
    Lit(String),
    Hole(Hole),
}

/// An unknown part of a string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Hole {
    /// Anything.
    Any,
    /// One of these texts.
    OneOf(Vec<String>),
    /// A routine's definition (`pg_get_functiondef`), possibly with some of
    /// its text replaced: a `CREATE OR REPLACE` of an existing routine.
    FunctionDef,
}

impl Val {
    pub(crate) fn any() -> Val {
        Val::Text(vec![Piece::Hole(Hole::Any)])
    }

    pub(crate) fn known(value: Option<String>) -> Val {
        Val::OneOf(vec![value])
    }

    /// The value, when it is known.
    pub(crate) fn as_known(&self) -> Option<&Option<String>> {
        match self {
            Val::OneOf(values) if values.len() == 1 => values.first(),
            _ => None,
        }
    }

    /// A value that is `self` or `other`.
    pub(crate) fn join(&self, other: &Val) -> Val {
        if self == other {
            return self.clone();
        }
        match (self, other) {
            (Val::OneOf(a), Val::OneOf(b)) => {
                let mut all = a.clone();
                for v in b {
                    if !all.contains(v) {
                        all.push(v.clone());
                    }
                }
                if all.len() > MAX_ALTERNATIVES {
                    Val::any()
                } else {
                    Val::OneOf(all)
                }
            }
            _ => Val::any(),
        }
    }

    /// The value as string pieces, NULL as the empty string (`format`'s
    /// `%s`, `concat`).
    fn pieces(&self) -> Vec<Piece> {
        match self {
            Val::OneOf(values) => {
                let texts: Vec<String> = values
                    .iter()
                    .map(|v| v.clone().unwrap_or_default())
                    .collect();
                one_of_piece(texts)
            }
            Val::Text(pieces) => pieces.clone(),
        }
    }

    /// The value with `f` applied to each possible text (an unknown part
    /// stays unknown): `quote_ident`, `quote_literal`.
    fn map_text(&self, f: impl Fn(Option<&str>) -> Option<String>) -> Val {
        match self {
            Val::OneOf(values) => Val::OneOf(values.iter().map(|v| f(v.as_deref())).collect()),
            Val::Text(pieces) => match pieces.as_slice() {
                [Piece::Lit(text)] => Val::known(f(Some(text))),
                [Piece::Hole(Hole::OneOf(texts))] => {
                    Val::OneOf(texts.iter().map(|t| f(Some(t))).collect())
                }
                _ => Val::any(),
            },
        }
    }

    fn from_pieces(pieces: Vec<Piece>) -> Val {
        let mut out: Vec<Piece> = Vec::new();
        for piece in pieces {
            match (out.last_mut(), piece) {
                (_, Piece::Lit(text)) if text.is_empty() => {}
                (Some(Piece::Lit(last)), Piece::Lit(text)) => last.push_str(&text),
                (_, piece) => out.push(piece),
            }
        }
        match out.as_slice() {
            [] => Val::known(Some(String::new())),
            [Piece::Lit(text)] => Val::known(Some(text.clone())),
            _ => Val::Text(out),
        }
    }
}

fn one_of_piece(mut texts: Vec<String>) -> Vec<Piece> {
    texts.dedup();
    match texts.len() {
        1 => vec![Piece::Lit(texts.remove(0))],
        n if n > MAX_ALTERNATIVES => vec![Piece::Hole(Hole::Any)],
        _ => vec![Piece::Hole(Hole::OneOf(texts))],
    }
}

/// What an expression can read: the block's variables and records' fields.
pub(crate) trait Scope {
    /// The value of variable `name`; `None` when no variable has that name.
    fn variable(&self, name: &str) -> Option<Val>;
    /// The value of field `field` of record `record`; `None` when no
    /// record has that name.
    fn field(&self, record: &str, field: &str) -> Option<Val>;
    /// The catalog, when what it holds now is what the expression reads
    /// (a statement that certainly runs, not one that only may).
    fn catalog(&self) -> Option<&PgCatalog> {
        None
    }
}

/// `quote_literal` (quote.c): `'...'` with quotes doubled, `E'...'` with
/// backslashes doubled when there are any.
fn quote_literal(s: &str) -> String {
    let mut out = String::new();
    if s.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Parse `expr` as a select-list expression.
fn parse_expression(expr: &str) -> Option<Node> {
    let parsed = typedpg_pg_query::parse(&format!("SELECT {expr}")).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let Some(Node::SelectStmt(select)) = raw.stmt.as_ref() else {
        return None;
    };
    if !select.from_clause.is_empty() || select.where_clause.is_some() {
        return None;
    }
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(Node::ResTarget(rt)) = target.as_ref() else {
        return None;
    };
    rt.val.clone()
}

/// The value of PL/pgSQL expression `expr` (its SQL text).
pub(crate) fn eval(scope: &dyn Scope, expr: &str) -> Val {
    match parse_expression(expr) {
        Some(node) => eval_node(scope, &node),
        None => Val::any(),
    }
}

fn string_of(node: Option<&Node>) -> Option<&str> {
    match node? {
        Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// A function's name, unqualified, when it is unqualified or in
/// `pg_catalog`.
fn function_name(names: &[Option<Node>]) -> Option<&str> {
    match names {
        [name] => string_of(name.as_ref()),
        [schema, name] if string_of(schema.as_ref()) == Some("pg_catalog") => {
            string_of(name.as_ref())
        }
        _ => None,
    }
}

/// The text types whose values the analyzer follows through a cast.
fn is_text_type(type_name: &ast::TypeName) -> bool {
    let names: Vec<&str> = type_name
        .names
        .iter()
        .filter_map(|n| string_of(n.as_ref()))
        .collect();
    let last = names.last().copied().unwrap_or_default();
    type_name.array_bounds.is_empty()
        && matches!(
            last,
            "text" | "varchar" | "name" | "bpchar" | "char" | "character varying"
        )
        && type_name.typmods.is_empty()
}

pub(crate) fn eval_node(scope: &dyn Scope, node: &Node) -> Val {
    match node {
        Node::AConst(c) => {
            if c.isnull {
                return Val::known(None);
            }
            Val::known(Some(match c.val.as_ref() {
                Some(ast::AConstValue::Integer(i)) => i.ival.to_string(),
                Some(ast::AConstValue::Float(f)) => f.fval.clone(),
                Some(ast::AConstValue::String(s)) => s.sval.clone(),
                Some(ast::AConstValue::Boolean(b)) => if b.boolval { "t" } else { "f" }.to_owned(),
                Some(ast::AConstValue::BitString(b)) => b.bsval.clone(),
                None => return Val::any(),
            }))
        }
        Node::ColumnRef(c) => {
            let names: Vec<Option<&str>> = c.fields.iter().map(|f| string_of(f.as_ref())).collect();
            match names.as_slice() {
                [Some(name)] => scope.variable(name).unwrap_or_else(Val::any),
                [Some(record), Some(field)] => scope.field(record, field).unwrap_or_else(Val::any),
                _ => Val::any(),
            }
        }
        Node::TypeCast(cast) => {
            let Some(arg) = cast.arg.as_ref() else {
                return Val::any();
            };
            match cast.type_name.as_deref() {
                Some(t) if is_text_type(t) => eval_node(scope, arg),
                _ => Val::any(),
            }
        }
        Node::AExpr(e)
            if e.kind == ast::AExprKind::AexprOp
                && function_name(&e.name) == Some("||")
                && e.lexpr.is_some() =>
        {
            let left = e
                .lexpr
                .as_ref()
                .map_or_else(Val::any, |l| eval_node(scope, l));
            let right = e
                .rexpr
                .as_ref()
                .map_or_else(Val::any, |r| eval_node(scope, r));
            // `NULL || x` is NULL.
            if left.as_known() == Some(&None) || right.as_known() == Some(&None) {
                return Val::known(None);
            }
            let mut pieces = left.pieces();
            pieces.extend(right.pieces());
            Val::from_pieces(pieces)
        }
        Node::CaseExpr(case) => {
            // One of the results, whichever branch is taken.
            let mut result: Option<Val> = None;
            let mut add = |v: Val| {
                result = Some(match result.take() {
                    Some(r) => r.join(&v),
                    None => v,
                });
            };
            for when in &case.args {
                let Some(Node::CaseWhen(w)) = when.as_ref() else {
                    return Val::any();
                };
                add(w
                    .result
                    .as_ref()
                    .map_or_else(Val::any, |r| eval_node(scope, r)));
            }
            add(case
                .defresult
                .as_ref()
                .map_or_else(|| Val::known(None), |d| eval_node(scope, d)));
            result.unwrap_or_else(Val::any)
        }
        Node::FuncCall(call) => eval_call(scope, call),
        _ => Val::any(),
    }
}

fn eval_call(scope: &dyn Scope, call: &ast::FuncCall) -> Val {
    if call.agg_star || call.agg_distinct || call.over.is_some() || call.func_variadic {
        return Val::any();
    }
    let args: Vec<Val> = call
        .args
        .iter()
        .map(|a| a.as_ref().map_or_else(Val::any, |a| eval_node(scope, a)))
        .collect();
    match (function_name(&call.funcname), args.as_slice()) {
        (Some("pg_get_functiondef"), [_]) => Val::Text(vec![Piece::Hole(Hole::FunctionDef)]),
        (Some("current_setting"), [name])
            if name.as_known() == Some(&Some("search_path".into())) =>
        {
            match scope.catalog() {
                Some(interp) => Val::known(Some(interp.search_path_guc.effective_setting())),
                None => Val::any(),
            }
        }
        (Some("current_schema"), []) => match scope.catalog() {
            Some(interp) => Val::known(super::util::creation_schema(interp).ok()),
            None => Val::any(),
        },
        (Some("quote_ident"), [arg]) => arg.map_text(|v| v.map(typedpg_core::quote_identifier)),
        (Some("quote_literal"), [arg]) => arg.map_text(|v| v.map(quote_literal)),
        (Some("quote_nullable"), [arg]) => {
            arg.map_text(|v| Some(v.map_or_else(|| "NULL".to_owned(), quote_literal)))
        }
        (Some("replace"), [text, from, to]) => {
            match (text.as_known(), from.as_known(), to.as_known()) {
                (Some(t), Some(f), Some(r)) => match (t, f, r) {
                    (Some(t), Some(f), Some(r)) if !f.is_empty() => {
                        Val::known(Some(t.replace(f.as_str(), r)))
                    }
                    (Some(t), Some(f), Some(_)) if f.is_empty() => Val::known(Some(t.clone())),
                    _ => Val::known(None),
                },
                // Replacing text in a routine's definition still defines
                // that routine.
                _ if *text == Val::Text(vec![Piece::Hole(Hole::FunctionDef)]) => text.clone(),
                _ => Val::any(),
            }
        }
        (Some("format"), [format, rest @ ..]) => match format.as_known() {
            Some(Some(format)) => eval_format(format, rest),
            Some(None) => Val::known(None),
            None => Val::any(),
        },
        _ => Val::any(),
    }
}

/// `format(format, args...)` (varlena.c `text_format`): `%s`, `%I`, `%L`
/// and `%%`, with an argument position (`%2$I`). Widths and flags are not
/// followed.
fn eval_format(format: &str, args: &[Val]) -> Val {
    let mut pieces = Vec::new();
    let mut next = 0;
    let mut chars = format.chars().peekable();
    let mut literal = String::new();
    while let Some(c) = chars.next() {
        if c != '%' {
            literal.push(c);
            continue;
        }
        if chars.next_if_eq(&'%').is_some() {
            literal.push('%');
            continue;
        }
        // `n$`: the argument at position n; the next one follows it.
        let mut digits = String::new();
        while let Some(d) = chars.next_if(char::is_ascii_digit) {
            digits.push(d);
        }
        if !digits.is_empty() {
            if chars.next_if_eq(&'$').is_none() {
                return Val::any();
            }
            match digits.parse::<usize>() {
                Ok(n) if n >= 1 => next = n - 1,
                _ => return Val::any(),
            }
        }
        let Some(spec) = chars.next() else {
            return Val::any();
        };
        let Some(arg) = args.get(next) else {
            return Val::any();
        };
        next += 1;
        pieces.push(Piece::Lit(std::mem::take(&mut literal)));
        let value = match spec {
            's' => arg.clone(),
            // A NULL `%I` fails; one that runs had a value.
            'I' => arg.map_text(|v| v.map(typedpg_core::quote_identifier)),
            'L' => arg.map_text(|v| Some(v.map_or_else(|| "NULL".to_owned(), quote_literal))),
            _ => return Val::any(),
        };
        pieces.extend(value.pieces());
    }
    pieces.push(Piece::Lit(literal));
    Val::from_pieces(pieces)
}

/// The elements of a constant array expression (`ARRAY['a', 'b']`), one
/// dimension.
pub(crate) fn array_elements(scope: &dyn Scope, expr: &str) -> Option<Vec<Val>> {
    let node = parse_expression(expr)?;
    let node = match &node {
        Node::TypeCast(cast) => cast.arg.clone()?,
        _ => node,
    };
    let Node::AArrayExpr(array) = &node else {
        return None;
    };
    array
        .elements
        .iter()
        .map(|e| {
            let e = e.as_ref()?;
            if matches!(e, Node::AArrayExpr(_)) {
                return None;
            }
            Some(eval_node(scope, e))
        })
        .collect()
}

/// The rows of a `SELECT * FROM (VALUES (...), ...) AS alias (columns)`
/// query of constants: each row's columns by name.
pub(crate) fn constant_values_rows(
    scope: &dyn Scope,
    query: &str,
) -> Option<Vec<Vec<(String, Val)>>> {
    let parsed = typedpg_pg_query::parse(query).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let Some(Node::SelectStmt(select)) = raw.stmt.as_ref() else {
        return None;
    };
    let plain = |s: &ast::SelectStmt| {
        s.where_clause.is_none()
            && s.group_clause.is_empty()
            && s.having_clause.is_none()
            && s.sort_clause.is_empty()
            && s.limit_count.is_none()
            && s.limit_offset.is_none()
            && s.with_clause.is_none()
            && s.distinct_clause.is_empty()
            && s.larg.is_none()
            && s.into_clause.is_none()
            && s.locking_clause.is_empty()
    };
    if !plain(select) {
        return None;
    }
    // `SELECT *`.
    let [target] = select.target_list.as_slice() else {
        return None;
    };
    let Some(Node::ResTarget(rt)) = target.as_ref() else {
        return None;
    };
    let Some(Node::ColumnRef(star)) = rt.val.as_ref() else {
        return None;
    };
    if !matches!(star.fields.as_slice(), [Some(Node::AStar(_))]) {
        return None;
    }
    let [from] = select.from_clause.as_slice() else {
        return None;
    };
    let Some(Node::RangeSubselect(sub)) = from.as_ref() else {
        return None;
    };
    let Some(Node::SelectStmt(values)) = sub.subquery.as_ref() else {
        return None;
    };
    if values.values_lists.is_empty() || !plain(values) {
        return None;
    }
    let columns: Vec<String> = sub
        .alias
        .as_ref()?
        .colnames
        .iter()
        .map(|c| string_of(c.as_ref()).map(str::to_owned))
        .collect::<Option<_>>()?;
    let mut rows = Vec::new();
    for list in &values.values_lists {
        let Some(Node::List(list)) = list.as_ref() else {
            return None;
        };
        if list.items.len() != columns.len() {
            return None;
        }
        let mut row = Vec::new();
        for (column, item) in columns.iter().zip(&list.items) {
            let value = eval_node(scope, item.as_ref()?);
            value.as_known()?;
            row.push((column.clone(), value));
        }
        rows.push(row);
    }
    Some(rows)
}

/// The record a `FOR rec IN query` loop's iterations see, field by field,
/// whatever row they read: each output column's value as far as its
/// expression alone tells (a constant, a `CASE` of constants, a routine's
/// definition), unknown otherwise. `None` when the query isn't a plain
/// `SELECT` whose output columns are named.
pub(crate) fn query_record(scope: &dyn Scope, query: &str) -> Option<Vec<(String, Val)>> {
    let parsed = typedpg_pg_query::parse(query).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let Some(Node::SelectStmt(select)) = raw.stmt.as_ref() else {
        return None;
    };
    let mut fields = Vec::new();
    for target in &select.target_list {
        let Some(Node::ResTarget(rt)) = target.as_ref() else {
            return None;
        };
        let value = rt.val.as_ref()?;
        let name = if rt.name.is_empty() {
            match value {
                Node::ColumnRef(c) => string_of(c.fields.last()?.as_ref())?.to_owned(),
                Node::FuncCall(f) => string_of(f.funcname.last()?.as_ref())?.to_owned(),
                _ => return None,
            }
        } else {
            rt.name.clone()
        };
        // Column references read the query's rows, not the block's
        // variables: only expressions without any are followed.
        let reads_rows = value
            .nodes()
            .any(|(n, _)| matches!(n, typedpg_pg_query::NodeRef::ColumnRef(_)));
        let val = match value {
            Node::FuncCall(f) if function_name(&f.funcname) == Some("pg_get_functiondef") => {
                Val::Text(vec![Piece::Hole(Hole::FunctionDef)])
            }
            Node::CaseExpr(case) => {
                // The results alone, when they are constants.
                let mut result: Option<Val> = None;
                let mut ok = true;
                for when in &case.args {
                    match when.as_ref() {
                        Some(Node::CaseWhen(w)) => match w.result.as_ref() {
                            Some(r @ Node::AConst(_)) => {
                                let v = eval_node(scope, r);
                                result = Some(result.map_or(v.clone(), |acc| acc.join(&v)));
                            }
                            _ => ok = false,
                        },
                        _ => ok = false,
                    }
                }
                let default = match case.defresult.as_ref() {
                    None => Some(Val::known(None)),
                    Some(d @ Node::AConst(_)) => Some(eval_node(scope, d)),
                    Some(_) => None,
                };
                match (ok, default, result) {
                    (true, Some(d), Some(r)) => r.join(&d),
                    _ => Val::any(),
                }
            }
            _ if reads_rows => Val::any(),
            _ => eval_node(scope, value),
        };
        fields.push((name, val));
    }
    Some(fields)
}

// ─── Conditions ─────────────────────────────────────────────────────────────

/// The truth of a condition, when the analyzer knows it.
pub(crate) fn truth(scope: &dyn Scope, expr: &str) -> Option<bool> {
    let node = parse_expression(expr)?;
    truth_node(scope, &node)
}

fn truth_node(scope: &dyn Scope, node: &Node) -> Option<bool> {
    match node {
        Node::BoolExpr(b) => {
            let args: Vec<Option<bool>> = b
                .args
                .iter()
                .map(|a| a.as_ref().and_then(|a| truth_node(scope, a)))
                .collect();
            match b.boolop {
                ast::BoolExprType::NotExpr => match args.as_slice() {
                    [Some(v)] => Some(!v),
                    _ => None,
                },
                ast::BoolExprType::AndExpr => {
                    if args.contains(&Some(false)) {
                        Some(false)
                    } else if args.iter().all(|a| *a == Some(true)) {
                        Some(true)
                    } else {
                        None
                    }
                }
                ast::BoolExprType::OrExpr => {
                    if args.contains(&Some(true)) {
                        Some(true)
                    } else if args.iter().all(|a| *a == Some(false)) {
                        Some(false)
                    } else {
                        None
                    }
                }
            }
        }
        Node::SubLink(link) if link.sub_link_type == ast::SubLinkType::ExistsSublink => {
            let Some(Node::SelectStmt(select)) = link.subselect.as_ref() else {
                return None;
            };
            catalog_exists(scope, scope.catalog()?, select)
        }
        Node::NullTest(test) => {
            let value = eval_node(scope, test.arg.as_ref()?);
            let is_null = value.as_known()?.is_none();
            Some(match test.nulltesttype {
                ast::NullTestType::IsNull => is_null,
                ast::NullTestType::IsNotNull => !is_null,
            })
        }
        Node::AExpr(e) => {
            let op = function_name(&e.name)?;
            let left = eval_node(scope, e.lexpr.as_ref()?);
            let right = eval_node(scope, e.rexpr.as_ref()?);
            let (Some(left), Some(right)) = (left.as_known(), right.as_known()) else {
                return None;
            };
            match (e.kind, op) {
                (ast::AExprKind::AexprOp, "=") => Some(left.as_ref()? == right.as_ref()?),
                (ast::AExprKind::AexprOp, "<>" | "!=") => Some(left.as_ref()? != right.as_ref()?),
                (ast::AExprKind::AexprDistinct, _) => Some(left != right),
                (ast::AExprKind::AexprNotDistinct, _) => Some(left == right),
                _ => None,
            }
        }
        _ => match eval_node(scope, node).as_known()?.as_deref()? {
            "t" | "true" => Some(true),
            "f" | "false" => Some(false),
            _ => None,
        },
    }
}

/// `EXISTS (SELECT ... FROM view WHERE column = value AND ...)` over a
/// catalog view whose rows the analyzer's catalog holds: `pg_roles` /
/// `pg_authid` (`rolname`), `pg_user` (`usename`), `pg_publication`
/// (`pubname`, `puballtables`) and `pg_publication_tables` of a publication
/// that doesn't exist. `None` for anything else, or when what it reads may
/// differ from the catalog (see [`super::uncertain`]).
fn catalog_exists(scope: &dyn Scope, interp: &PgCatalog, select: &ast::SelectStmt) -> Option<bool> {
    let uncertain = &interp.roles.uncertain;
    if uncertain.catalog
        || !select.group_clause.is_empty()
        || select.having_clause.is_some()
        || select.with_clause.is_some()
        || select.limit_count.is_some()
        || select.limit_offset.is_some()
        || select.larg.is_some()
        || !select.values_lists.is_empty()
    {
        return None;
    }
    let [from] = select.from_clause.as_slice() else {
        return None;
    };
    let Some(Node::RangeVar(rv)) = from.as_ref() else {
        return None;
    };
    if !rv.catalogname.is_empty() || !matches!(rv.schemaname.as_str(), "" | "pg_catalog") {
        return None;
    }
    let alias = rv.alias.as_ref().map(|a| a.aliasname.as_str());
    // The view's columns the WHERE pins: `column = value`, a bare boolean
    // column, `NOT column`.
    let mut equal: Vec<(String, String)> = Vec::new();
    let mut flags: Vec<(String, bool)> = Vec::new();
    let column = |node: &Node| -> Option<String> {
        let Node::ColumnRef(c) = node else {
            return None;
        };
        let names: Vec<&str> = c
            .fields
            .iter()
            .map(|f| string_of(f.as_ref()))
            .collect::<Option<_>>()?;
        match names.as_slice() {
            [name] => Some((*name).to_owned()),
            [qualifier, name] if Some(*qualifier) == alias || *qualifier == rv.relname => {
                Some((*name).to_owned())
            }
            _ => None,
        }
    };
    let mut conjuncts = Vec::new();
    if let Some(w) = select.where_clause.as_ref() {
        flatten_and(w, &mut conjuncts);
    }
    for conjunct in conjuncts {
        match conjunct {
            Node::AExpr(e)
                if e.kind == ast::AExprKind::AexprOp && function_name(&e.name) == Some("=") =>
            {
                let (l, r) = (e.lexpr.as_ref()?, e.rexpr.as_ref()?);
                let (col, value) = match (column(l), column(r)) {
                    (Some(c), None) => (c, r),
                    (None, Some(c)) => (c, l),
                    _ => return None,
                };
                let Some(Some(value)) = eval_node(scope, value).as_known().cloned() else {
                    return None;
                };
                equal.push((col, value));
            }
            Node::BoolExpr(b) if b.boolop == ast::BoolExprType::NotExpr => {
                let [Some(arg)] = b.args.as_slice() else {
                    return None;
                };
                flags.push((column(arg)?, false));
            }
            other => flags.push((column(other)?, true)),
        }
    }
    let only = |allowed: &[&str]| {
        equal.iter().all(|(c, _)| allowed.contains(&c.as_str()))
            && flags.iter().all(|(c, _)| allowed.contains(&c.as_str()))
    };
    let value_of = |name: &str| -> Option<&str> {
        let mut values = equal
            .iter()
            .filter(|(c, _)| c == name)
            .map(|(_, v)| v.as_str());
        let first = values.next()?;
        // Two different values for one column match nothing; the caller
        // can't tell that from a missing one, so it isn't decided.
        values.all(|v| v == first).then_some(first)
    };
    match rv.relname.as_str() {
        "pg_roles" | "pg_authid" | "pg_user" => {
            let column = if rv.relname == "pg_user" {
                "usename"
            } else {
                "rolname"
            };
            if !flags.is_empty() || !only(&[column]) {
                return None;
            }
            let name = value_of(column)?;
            if uncertain.roles.contains(name) {
                return None;
            }
            Some(if rv.relname == "pg_user" {
                interp.roles.exists_with_login(name)
            } else {
                interp.roles.exists(name)
            })
        }
        "pg_publication" => {
            if !only(&["pubname", "puballtables"]) {
                return None;
            }
            let name = value_of("pubname")?;
            let Some(all_tables) = super::publications::publication_all_tables(interp, name) else {
                return Some(false);
            };
            Some(
                flags
                    .iter()
                    .all(|(c, want)| c != "puballtables" || *want == all_tables),
            )
        }
        "pg_publication_tables" => {
            if !flags.is_empty() || !only(&["pubname", "schemaname", "tablename"]) {
                return None;
            }
            let name = value_of("pubname")?;
            if super::publications::publication_exists(interp, name) {
                return None;
            }
            Some(false)
        }
        _ => None,
    }
}

fn flatten_and<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    match node {
        Node::BoolExpr(b) if b.boolop == ast::BoolExprType::AndExpr => {
            for arg in b.args.iter().flatten() {
                flatten_and(arg, out);
            }
        }
        other => out.push(other),
    }
}

// ─── What a statement may change ────────────────────────────────────────────

/// The prefix of the identifiers standing for a string's unknown parts.
const HOLE: &str = "typedpg_unknown_";

/// What `EXECUTE` of string `val` may change: the string's statements, its
/// unknown parts standing for one name each (every alternative of a part
/// that is one of a few texts), classified by [`classify_sql`]. A string
/// whose statements can't be told — a fully unknown one, one that doesn't
/// parse with its parts filled in — may change anything; a routine's
/// definition redefines that routine.
pub(crate) fn classify_dynamic(val: &Val, uncertain: &mut Uncertain) {
    let pieces = match val {
        Val::OneOf(values) => values
            .iter()
            .flatten()
            .map(|v| vec![Piece::Lit(v.clone())])
            .collect::<Vec<_>>(),
        Val::Text(pieces) => vec![pieces.clone()],
    };
    for pieces in pieces {
        if pieces == [Piece::Hole(Hole::FunctionDef)] {
            uncertain.routines = true;
            continue;
        }
        // Expand the parts that are one of a few texts.
        let mut alternatives: Vec<String> = vec![String::new()];
        let mut holes = 0;
        for piece in &pieces {
            match piece {
                Piece::Lit(text) => alternatives.iter_mut().for_each(|a| a.push_str(text)),
                Piece::Hole(Hole::OneOf(texts))
                    if alternatives.len() * texts.len() <= MAX_ALTERNATIVES =>
                {
                    alternatives = alternatives
                        .iter()
                        .flat_map(|a| texts.iter().map(move |t| format!("{a}{t}")))
                        .collect();
                }
                Piece::Hole(Hole::FunctionDef) => {
                    uncertain.catalog = true;
                    return;
                }
                Piece::Hole(_) => {
                    holes += 1;
                    alternatives
                        .iter_mut()
                        .for_each(|a| a.push_str(&format!(" {HOLE}{holes} ")));
                }
            }
        }
        for sql in alternatives {
            classify_sql(&sql, uncertain);
        }
    }
}

/// The roles a role list names; a name standing for an unknown part, or
/// `PUBLIC`, `CURRENT_ROLE` and the like, is any role.
fn role_scope(specs: &[Option<Node>], scope: &mut RoleScope) {
    for spec in specs {
        match spec.as_ref() {
            Some(Node::RoleSpec(r)) => role_spec(r, scope),
            _ => scope.add_all(),
        }
    }
}

fn role_spec(spec: &ast::RoleSpec, scope: &mut RoleScope) {
    if spec.roletype == ast::RoleSpecType::RolespecCstring && !spec.rolename.starts_with(HOLE) {
        scope.add(&spec.rolename);
    } else {
        scope.add_all();
    }
}

fn named(name: &str) -> RoleScope {
    let mut scope = RoleScope::default();
    if name.starts_with(HOLE) {
        scope.add_all();
    } else {
        scope.add(name);
    }
    scope
}

/// What SQL `sql`, run by code the analyzer couldn't follow, may change.
pub(crate) fn classify_sql(sql: &str, uncertain: &mut Uncertain) {
    let Ok(parsed) = typedpg_pg_query::parse(sql) else {
        uncertain.catalog = true;
        return;
    };
    for raw in &parsed.tree.stmts {
        if let Some(stmt) = raw.stmt.as_ref() {
            classify_statement(stmt, uncertain);
        }
    }
}

pub(crate) fn classify_statement(stmt: &Node, uncertain: &mut Uncertain) {
    match stmt {
        // Rows, not the catalog.
        // A query changes rows, not the catalog — but for the settings a
        // `set_config` call changes (see `session::select_side_effects`).
        Node::SelectStmt(s) if s.into_clause.is_none() => {
            if stmt.nodes().any(|(node, _)| match node {
                typedpg_pg_query::NodeRef::FuncCall(f) => {
                    function_name(&f.funcname) == Some("set_config")
                }
                _ => false,
            }) {
                uncertain.catalog = true;
            }
        }
        Node::InsertStmt(_)
        | Node::UpdateStmt(_)
        | Node::DeleteStmt(_)
        | Node::MergeStmt(_)
        | Node::NotifyStmt(_)
        | Node::ListenStmt(_)
        | Node::UnlistenStmt(_)
        | Node::LockStmt(_)
        | Node::VacuumStmt(_)
        | Node::CheckPointStmt(_)
        | Node::TransactionStmt(_)
        | Node::ExplainStmt(_)
        | Node::CommentStmt(_) => {}
        Node::VariableSetStmt(s)
            if !matches!(
                s.name.as_str(),
                "search_path" | "role" | "session_authorization"
            ) && !s.name.is_empty() => {}
        Node::GrantStmt(g) => {
            let mut grantees = RoleScope::default();
            role_scope(&g.grantees, &mut grantees);
            if g.is_grant {
                uncertain.gained_privileges.merge(&grantees);
            } else if g.behavior == ast::DropBehavior::DropCascade {
                // A cascade takes what the grantees granted on, from anyone.
                uncertain.lost_privileges.add_all();
            } else {
                uncertain.lost_privileges.merge(&grantees);
            }
        }
        Node::GrantRoleStmt(g) => {
            let mut granted = RoleScope::default();
            for role in &g.granted_roles {
                match role.as_ref() {
                    Some(Node::AccessPriv(p)) => granted.merge(&named(&p.priv_name)),
                    _ => granted.add_all(),
                }
            }
            let mut members = RoleScope::default();
            role_scope(&g.grantee_roles, &mut members);
            if g.is_grant {
                uncertain.gained_memberships.push((granted, members));
            } else if g.behavior == ast::DropBehavior::DropCascade {
                uncertain
                    .lost_memberships
                    .push((granted, RoleScope::every()));
            } else {
                uncertain.lost_memberships.push((granted, members));
            }
        }
        Node::CreateRoleStmt(c) => {
            let role = named(&c.role);
            uncertain.roles.merge(&role);
            for option in &c.options {
                let Some(Node::DefElem(d)) = option.as_ref() else {
                    continue;
                };
                let mut others = RoleScope::default();
                if let Some(Node::List(list)) = d.arg.as_ref() {
                    role_scope(&list.items, &mut others);
                } else {
                    others.add_all();
                }
                match d.defname.as_str() {
                    "addroleto" => uncertain.gained_memberships.push((others, role.clone())),
                    "rolemembers" | "adminmembers" => {
                        uncertain.gained_memberships.push((role.clone(), others));
                    }
                    _ => {}
                }
            }
        }
        Node::AlterRoleStmt(a) => {
            // An attribute may make the role a superuser, or take that.
            let mut role = RoleScope::default();
            match a.role.as_deref() {
                Some(spec) => role_spec(spec, &mut role),
                None => role.add_all(),
            }
            uncertain.roles.merge(&role);
            uncertain.gained_privileges.merge(&role);
            uncertain.lost_privileges.merge(&role);
        }
        Node::AlterRoleSetStmt(a) => match a.role.as_deref() {
            Some(spec) => role_spec(spec, &mut uncertain.roles),
            None => uncertain.roles.add_all(),
        },
        Node::DropRoleStmt(d) => {
            let mut roles = RoleScope::default();
            role_scope(&d.roles, &mut roles);
            uncertain.roles.merge(&roles);
            uncertain.lost_privileges.merge(&roles);
            uncertain
                .lost_memberships
                .push((roles.clone(), RoleScope::every()));
            uncertain.lost_memberships.push((RoleScope::every(), roles));
        }
        Node::RenameStmt(r) if r.rename_type == ast::ObjectType::ObjectRole => {
            // The role's grants follow it to its new name.
            let mut roles = named(&r.subname);
            roles.merge(&named(&r.newname));
            uncertain.roles.merge(&roles);
            uncertain.gained_privileges.merge(&roles);
            uncertain.lost_privileges.merge(&roles);
            uncertain
                .gained_memberships
                .push((roles.clone(), RoleScope::every()));
            uncertain
                .gained_memberships
                .push((RoleScope::every(), roles.clone()));
            uncertain
                .lost_memberships
                .push((roles.clone(), RoleScope::every()));
            uncertain.lost_memberships.push((RoleScope::every(), roles));
        }
        Node::AlterOwnerStmt(a) => owner_change(a.newowner.as_deref(), uncertain),
        // Which triggers fire (not the foreign keys' own: `ALL` turns those
        // off, which nullability inference reads).
        Node::AlterTableStmt(a)
            if !a.cmds.is_empty()
                && a.cmds.iter().all(|c| {
                    matches!(c, Some(Node::AlterTableCmd(c)) if matches!(
                        c.subtype,
                        ast::AlterTableType::AtEnableTrig
                            | ast::AlterTableType::AtEnableAlwaysTrig
                            | ast::AlterTableType::AtEnableReplicaTrig
                            | ast::AlterTableType::AtDisableTrig
                            | ast::AlterTableType::AtEnableTrigUser
                            | ast::AlterTableType::AtDisableTrigUser
                    ))
                }) =>
        {
            uncertain.triggers = true;
        }
        Node::AlterTableStmt(a)
            if !a.cmds.is_empty()
                && a.cmds.iter().all(|c| {
                    matches!(c, Some(Node::AlterTableCmd(c))
                        if c.subtype == ast::AlterTableType::AtChangeOwner)
                }) =>
        {
            for cmd in &a.cmds {
                if let Some(Node::AlterTableCmd(c)) = cmd {
                    owner_change(c.newowner.as_deref(), uncertain);
                }
            }
        }
        Node::AlterDefaultPrivilegesStmt(a) => {
            uncertain.default_acls = true;
            // The objects created later get these privileges, or lose them.
            let mut grantees = RoleScope::default();
            match a.action.as_deref() {
                Some(g) => role_scope(&g.grantees, &mut grantees),
                None => grantees.add_all(),
            }
            uncertain.gained_privileges.merge(&grantees);
            uncertain.lost_privileges.merge(&grantees);
        }
        _ => uncertain.catalog = true,
    }
}

/// `ALTER ... OWNER TO new_owner`: the new owner gains the object's
/// privileges, its old owner — unknown here — loses them.
fn owner_change(new_owner: Option<&ast::RoleSpec>, uncertain: &mut Uncertain) {
    let mut owner = RoleScope::default();
    match new_owner {
        Some(spec) => role_spec(spec, &mut owner),
        None => owner.add_all(),
    }
    uncertain.gained_privileges.merge(&owner);
    uncertain.lost_privileges.add_all();
    uncertain.owners.add_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope with the variables `vars`, no record and no catalog.
    struct Vars(Vec<(&'static str, Val)>);

    impl Scope for Vars {
        fn variable(&self, name: &str) -> Option<Val> {
            self.0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        }

        fn field(&self, _: &str, _: &str) -> Option<Val> {
            None
        }
    }

    fn text(s: &str) -> Val {
        Val::known(Some(s.to_owned()))
    }

    #[test]
    fn format_follows_positions_and_quoting() {
        let scope = Vars(vec![("t", text("My Table")), ("n", text("1"))]);
        assert_eq!(
            eval(
                &scope,
                "format('UPDATE %1$I SET a = %2$L WHERE %1$I.b = %s', t, n)"
            ),
            text("UPDATE \"My Table\" SET a = '1' WHERE \"My Table\".b = 1")
        );
        assert_eq!(eval(&scope, "format('%L', NULL)"), text("NULL"));
        assert_eq!(
            eval(&scope, "quote_literal('it''s \\ here')"),
            text("E'it''s \\\\ here'")
        );
    }

    #[test]
    fn unknown_parts_stay_holes_between_known_text() {
        let scope = Vars(vec![]);
        let val = eval(&scope, "format('GRANT %s ON %s TO %I', p, o, 'app')");
        assert_eq!(
            val,
            Val::Text(vec![
                Piece::Lit("GRANT ".into()),
                Piece::Hole(Hole::Any),
                Piece::Lit(" ON ".into()),
                Piece::Hole(Hole::Any),
                Piece::Lit(" TO app".into()),
            ])
        );
        let mut found = Uncertain::default();
        classify_dynamic(&val, &mut found);
        assert!(found.gained_privileges.contains("app"));
        assert!(!found.gained_privileges.contains("other"));
        assert!(!found.catalog);
    }

    #[test]
    fn a_string_of_unknown_kind_may_change_anything() {
        let mut found = Uncertain::default();
        classify_dynamic(&Val::any(), &mut found);
        assert!(found.catalog);
    }

    #[test]
    fn a_case_of_constants_is_one_of_its_results() {
        let scope = Vars(vec![]);
        let val = eval(
            &scope,
            "format('GRANT %I TO %I', CASE x WHEN 1 THEN 'a' ELSE 'b' END, m)",
        );
        let mut found = Uncertain::default();
        classify_dynamic(&val, &mut found);
        assert!(found.membership_may_be_gained("a", "anyone"));
        assert!(found.membership_may_be_gained("b", "anyone"));
        assert!(!found.membership_may_be_gained("c", "anyone"));
    }
}
