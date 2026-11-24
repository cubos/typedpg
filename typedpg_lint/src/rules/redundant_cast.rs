//! The `redundant_cast` rule: reports an explicit cast whose operand
//! already has the target type and type modifier, so PostgreSQL adds no
//! node for it (`coerce_type` hands the operand back). The analyzer records
//! which casts those are (see
//! [`typedpg_analyzer::QueryView::cast_is_redundant`]). A cast of a
//! parameter types the parameter, a cast of an untyped literal gives it its
//! type, and `x::varchar` of a `varchar(10)` drops the length: none of
//! those is redundant.
//!
//! The fix deletes the `::type` of a postfix cast. It is withheld for a
//! `CAST(x AS type)`, and where the cast names an output column the
//! operand wouldn't name the same: `SELECT 1::int` is the column `int4`,
//! `SELECT 1` is `?column?` (a column or function operand gives its own
//! name either way).

use std::collections::HashSet;

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{Node as N, TypeCast};

use super::{leftmost_start, location, node_span};
use crate::{Ctx, RawFinding, SqlEdit};

/// The words that continue a multi-word type name: `double precision`,
/// `character varying`, `timestamp with time zone`.
const CONTINUATIONS: &[&str] = &["precision", "varying", "with", "without", "time", "zone"];

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    // The casts that name an output column (see `naming_expressions`).
    let naming: HashSet<i32> = super::naming_expressions(ctx)
        .into_iter()
        .filter_map(|node| match node {
            N::TypeCast(cast) => Some(cast.location),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        if let NodeRef::TypeCast(cast) = node
            && ctx.view.cast_is_redundant(cast.location) == Some(true)
        {
            redundant(ctx, cast, naming.contains(&cast.location), &mut out);
        }
    }
    out
}

fn redundant(ctx: &Ctx<'_>, cast: &TypeCast, names_column: bool, out: &mut Vec<RawFinding>) {
    let (Some(at), Some(type_name)) = (location(cast.location), cast.type_name.as_ref()) else {
        return;
    };
    let Some(type_start) = location(type_name.location) else {
        return;
    };
    let postfix = ctx.sql.get(at..).is_some_and(|rest| rest.starts_with("::"));
    let type_end = type_name_end(ctx, type_start);
    // From the operand's first token, a parenthesis opening it included
    // (`(a)::int`).
    let start = cast
        .arg
        .as_ref()
        .and_then(leftmost_start)
        .into_iter()
        .chain(node_span(ctx, NodeRef::TypeCast(cast)).map(|span| span.start))
        .min()
        .filter(|start| *start <= at)
        .unwrap_or(at);
    let written = &ctx.sql[type_start..type_end.max(type_start)];
    // The operand names the column as the cast does only when it is a
    // column or a function call (FigureColnameInternal's strength 2).
    let keeps_name = !names_column
        || matches!(
            cast.arg.as_ref(),
            Some(N::ColumnRef(_) | N::FuncCall(_) | N::CoalesceExpr(_) | N::MinMaxExpr(_))
        );
    let fix = if postfix && type_end > type_start && keeps_name {
        vec![SqlEdit {
            range: at..type_end,
            text: String::new(),
        }]
    } else {
        Vec::new()
    };
    out.push(RawFinding {
        span: start..type_end.max(at + 2),
        message: format!("cast to {written} has no effect: the value already is one"),
        help: Some("remove the cast".to_owned()),
        fix,
    });
}

/// The end of the type name starting at byte `start`: its (qualified)
/// name, the words of a multi-word name, then type modifiers in
/// parentheses and array brackets.
fn type_name_end(ctx: &Ctx<'_>, start: usize) -> usize {
    let mut end = start;
    let mut tokens = ctx
        .tokens
        .iter()
        .filter(|token| token.start >= start)
        .peekable();
    let text = |token: &crate::Tok| &ctx.sql[token.start..token.end];
    let is_word =
        |s: &str| s.starts_with('"') || s.starts_with(|c: char| c.is_alphabetic() || c == '_');
    // The name: words joined by dots, and the continuation words.
    let mut expect_word = true;
    while let Some(token) = tokens.peek() {
        let s = text(token);
        if expect_word && is_word(s) {
            expect_word = false;
        } else if !expect_word && s == "." {
            expect_word = true;
        } else if !expect_word && CONTINUATIONS.iter().any(|w| s.eq_ignore_ascii_case(w)) {
        } else {
            break;
        }
        end = token.end;
        tokens.next();
    }
    // Modifiers and array bounds, balanced.
    while let Some(token) = tokens.peek() {
        let open = text(token);
        let close = match open {
            "(" => ")",
            "[" => "]",
            _ => break,
        };
        let mut depth = 0usize;
        for token in tokens.by_ref() {
            let s = text(token);
            if s == open {
                depth += 1;
            } else if s == close {
                depth -= 1;
            }
            end = token.end;
            if depth == 0 {
                break;
            }
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use crate::Rule;
    use crate::rules::test_support::{catalog, fixed, lint_rule};

    const RULE: Rule = Rule::RedundantCast;

    #[test]
    fn casts_to_the_type_a_value_has_are_removed() {
        let db = catalog();
        for (sql, expected) in [
            ("SELECT id::bigint FROM users", "SELECT id FROM users"),
            (
                "SELECT id::int8 AS x FROM users",
                "SELECT id AS x FROM users",
            ),
            (
                "SELECT id FROM users WHERE name::text = 'x'",
                "SELECT id FROM users WHERE name = 'x'",
            ),
            (
                "SELECT email::varchar(100) FROM users",
                "SELECT email FROM users",
            ),
            (
                "SELECT id FROM users WHERE name::pg_catalog.text = 'x'",
                "SELECT id FROM users WHERE name = 'x'",
            ),
            (
                "SELECT lower(name)::text FROM users",
                "SELECT lower(name) FROM users",
            ),
            (
                "SELECT ARRAY[id]::bigint[] AS ids FROM users",
                "SELECT ARRAY[id] AS ids FROM users",
            ),
            (
                "SELECT now()::timestamp with time zone AS t",
                "SELECT now() AS t",
            ),
            (
                "SELECT code::char(3) AS c FROM users",
                "SELECT code AS c FROM users",
            ),
            (
                "SELECT (id::bigint + 1)::bigint AS n FROM users",
                "SELECT (id + 1) AS n FROM users",
            ),
            // The WHERE pins the output column to the constant with the
            // cast as without it: the fix keeps that narrowing.
            (
                "SELECT id FROM users WHERE id = 5::int",
                "SELECT id FROM users WHERE id = 5",
            ),
            (
                "SELECT id FROM users WHERE id IN (5::int, 6)",
                "SELECT id FROM users WHERE id IN (5, 6)",
            ),
            (
                "SELECT name FROM users WHERE name::text = 'x'",
                "SELECT name FROM users WHERE name = 'x'",
            ),
            (
                "SELECT email FROM users WHERE email::varchar(100) = 'a'",
                "SELECT email FROM users WHERE email = 'a'",
            ),
            (
                "SELECT id FROM users WHERE id::bigint BETWEEN 2 AND 4",
                "SELECT id FROM users WHERE id BETWEEN 2 AND 4",
            ),
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(!findings.is_empty(), "{sql}: no finding");
            assert!(
                findings.iter().all(|f| !f.fix.is_empty()),
                "{sql}: {findings:?}"
            );
            assert_eq!(fixed(sql, &findings), expected, "{sql}");
        }
    }

    #[test]
    fn the_span_starts_at_a_parenthesis_opening_the_operand() {
        let db = catalog();
        let sql = "SELECT (id)::bigint AS i FROM users";
        let findings = lint_rule(&db, sql, RULE);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(&sql[findings[0].span.clone()], "(id)::bigint");
        assert_eq!(fixed(sql, &findings), "SELECT (id) AS i FROM users");
    }

    #[test]
    fn what_has_no_fix() {
        let db = catalog();
        for sql in [
            // The cast names the column `int4`.
            "SELECT 1::int",
            // The CASE is named after its ELSE's cast, `int4`.
            "SELECT CASE WHEN id > 1 THEN 2 ELSE 1::int END FROM users",
            "SELECT CAST(id AS bigint) FROM users",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].fix.is_empty(), "{sql}");
        }
    }

    #[test]
    fn casts_that_change_something_are_not_reported() {
        let db = catalog();
        for sql in [
            // Another type.
            "SELECT id::int FROM users",
            "SELECT name::varchar FROM users",
            // The length goes, or changes.
            "SELECT email::varchar FROM users",
            "SELECT email::varchar(10) FROM users",
            "SELECT id FROM users WHERE code::bpchar = 'a'",
            // A parameter or an untyped literal takes the type.
            "SELECT $1::bigint",
            "SELECT 'x'::text",
            "SELECT NULL::text",
            "SELECT date '2020-01-01'",
            // An operand the cast types: a `record` or an `integer[]`
            // without it.
            "SELECT ARRAY[ROW(id, name, email, code)::users] FROM users",
            "SELECT ARRAY[id::int]::bigint[] FROM users",
            // A whole-row reference to a subquery is a `record`: the cast
            // rebuilds it as the composite.
            "SELECT s::users AS u FROM (SELECT id, name, email, code FROM users) s",
            "SELECT (s.*)::users AS u FROM (SELECT id, name, email, code FROM users) s",
            // A cast to `record` forgets the row's fields.
            "SELECT s::record AS r FROM (SELECT 1 AS a, 'x' AS b) s",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }
}
