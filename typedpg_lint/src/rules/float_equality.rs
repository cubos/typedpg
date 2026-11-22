//! The `float_equality` rule: reports `=`, `<>` / `!=`, `IN` and `NOT IN`
//! comparisons PostgreSQL makes between floating-point (`real` / `double
//! precision`) values — after the operands' implicit conversions, so
//! `price = 0.1` over a `float8` column counts, and `price::numeric = 0.1`
//! doesn't. A value computed in binary floating point rarely equals the
//! decimal it is written as (`0.1 + 0.2 = 0.3` is false), so an exact
//! comparison silently misses rows.
//!
//! Comparisons against a value that is exact in binary floating point are
//! not reported, as testing for one is well defined and common: an integer
//! literal the type holds exactly (`score = 0`, `rating = 5`; every integer
//! up to 2^24 in magnitude for `real`, 2^53 for `double precision`, but not
//! `16777217` for `real`), a decimal literal whose value the type holds
//! exactly (`0.5`, `2.25`, `1e3`, but not `0.1`), and the
//! special values `'NaN'`, `'Infinity'` and `'-Infinity'`. An `IN` list is
//! reported unless every item is such a value. A column, a parameter or a
//! computed value on both sides — a join `a.f = b.f` included — is
//! reported. No fix is offered: a tolerance, a range or a `numeric` column
//! is the author's choice.

use typedpg_analyzer::PgTypeOid;
use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{AConstValue, AExpr, AExprKind, Node};

use super::{node_span, operator_name};
use crate::{Ctx, RawFinding};

const FLOAT4: PgTypeOid = PgTypeOid::from_raw(700);
const FLOAT8: PgTypeOid = PgTypeOid::from_raw(701);
const BOOL: PgTypeOid = PgTypeOid::from_raw(16);

pub(super) fn findings(ctx: &Ctx<'_>) -> Vec<RawFinding> {
    let mut out = Vec::new();
    for (node, _) in ctx.tree.nodes() {
        let NodeRef::AExpr(expr) = node else {
            continue;
        };
        if let Some(finding) = comparison(ctx, expr) {
            out.push(finding);
        }
    }
    out
}

fn comparison(ctx: &Ctx<'_>, expr: &AExpr) -> Option<RawFinding> {
    let operator = operator_name(expr)?;
    // `OPERATOR(myschema.=)` may be anything.
    if expr.name.len() > 1
        && !matches!(expr.name.first(), Some(Some(Node::String(s))) if s.sval == "pg_catalog")
    {
        return None;
    }
    let right = expr.rexpr.as_ref()?;
    let left = expr.lexpr.as_ref()?;
    let others: Vec<&Node> = match expr.kind {
        AExprKind::AexprOp if matches!(operator, "=" | "<>" | "!=") => vec![left, right],
        AExprKind::AexprIn => {
            let Node::List(list) = right else {
                return None;
            };
            list.items.iter().flatten().collect()
        }
        _ => return None,
    };
    let types = ctx.view.operator_types(expr.location)?;
    if types.result != BOOL {
        return None;
    }
    let left_type = types.left?;
    let is_float = |t: PgTypeOid| t == FLOAT4 || t == FLOAT8;
    if !is_float(left_type) && !is_float(types.right) {
        return None;
    }
    let bits = if left_type == FLOAT4 || types.right == FLOAT4 {
        24
    } else {
        53
    };
    let exempt = match expr.kind {
        AExprKind::AexprIn => others.iter().all(|item| exact_value(item, bits)),
        _ => others.iter().any(|operand| exact_value(operand, bits)),
    };
    if exempt {
        return None;
    }
    let span = node_span(ctx, NodeRef::AExpr(expr))?;
    let what = match (expr.kind, operator) {
        (AExprKind::AexprIn, "<>") => "NOT IN",
        (AExprKind::AexprIn, _) => "IN",
        (_, op) => op,
    };
    let type_name = if bits == 24 {
        "real"
    } else {
        "double precision"
    };
    Some(RawFinding {
        span,
        message: format!(
            "{what} compares {type_name} values exactly: a floating-point value rarely equals \
             the decimal it is written as (0.1 + 0.2 = 0.3 is false), so rows may be missed"
        ),
        help: Some(
            "compare within a tolerance (abs(a - b) < 1e-9) or a range, or store exact values \
             as numeric"
                .to_owned(),
        ),
        fix: Vec::new(),
    })
}

/// Whether `node` is a literal whose value a float of `bits` mantissa bits
/// holds exactly: an integer or a decimal such as `0.5` or `1e3` that fits
/// (possibly negated or cast), or `'NaN'` / `'Infinity'` / `'-Infinity'`.
fn exact_value(node: &Node, bits: u32) -> bool {
    match node {
        Node::AConst(value) => match &value.val {
            // Every `int4` literal fits a `double precision` mantissa, but
            // one above 2^24 in magnitude doesn't fit a `real` one.
            Some(AConstValue::Integer(i)) => exactly_representable(&i.ival.to_string(), bits),
            Some(AConstValue::Float(f)) => exactly_representable(&f.fval, bits),
            Some(AConstValue::String(s)) => {
                let text = s.sval.trim();
                let special = text.trim_start_matches(['+', '-']).to_ascii_lowercase();
                matches!(special.as_str(), "nan" | "infinity" | "inf")
                    || exactly_representable(text, bits)
            }
            _ => false,
        },
        Node::TypeCast(cast) => cast.arg.as_ref().is_some_and(|arg| exact_value(arg, bits)),
        // A negation `-0.5`.
        Node::AExpr(e)
            if e.kind == AExprKind::AexprOp
                && e.lexpr.is_none()
                && operator_name(e) == Some("-") =>
        {
            e.rexpr.as_ref().is_some_and(|arg| exact_value(arg, bits))
        }
        _ => false,
    }
}

/// Whether the decimal `text` (`[+-]digits[.digits][e[+-]digits]`) is a
/// binary fraction whose odd part fits in `bits` bits: a float with that
/// many mantissa bits holds it exactly. Anything this can't parse, or too
/// large to decide, is not.
fn exactly_representable(text: &str, bits: u32) -> bool {
    let text = text.trim_start_matches(['+', '-']);
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(at) => match text[at + 1..].parse::<i32>() {
            Ok(exponent) => (&text[..at], exponent),
            Err(_) => return false,
        },
        None => (text, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return true;
    }
    let Ok(mut value) = digits.parse::<u128>() else {
        return false;
    };
    let Ok(fraction_len) = i32::try_from(fraction.len()) else {
        return false;
    };
    // value * 10^power, with power = exponent - fraction digits.
    let power = exponent - fraction_len;
    if power >= 0 {
        for _ in 0..power {
            let Some(next) = value.checked_mul(5) else {
                return false;
            };
            value = next;
        }
    } else {
        // 10^-k = 2^-k * 5^-k: 5^k must divide the digits.
        for _ in 0..-power {
            if value % 5 != 0 {
                return false;
            }
            value /= 5;
        }
    }
    let odd = value >> value.trailing_zeros();
    odd < (1u128 << bits)
}

#[cfg(test)]
mod tests {
    use typedpg_analyzer::PgCatalog;

    use super::exactly_representable;
    use crate::Rule;
    use crate::rules::test_support::lint_rule;

    const RULE: Rule = Rule::FloatEquality;

    fn catalog() -> PgCatalog {
        let mut catalog = PgCatalog::new().unwrap();
        catalog
            .apply_sql(
                "CREATE TABLE items (
                    id bigint PRIMARY KEY,
                    price float8 NOT NULL,
                    weight real,
                    amount numeric,
                    qty int NOT NULL
                 );
                 CREATE TABLE prices (id bigint PRIMARY KEY, price float8);",
            )
            .unwrap();
        catalog
    }

    #[test]
    fn exact_comparisons_of_floats_are_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM items WHERE price = 0.1",
            "SELECT id FROM items WHERE price <> 19.99",
            "SELECT id FROM items WHERE price != $1",
            "SELECT id FROM items WHERE 0.3 = price",
            "SELECT id FROM items WHERE weight = 0.5 + 0.25",
            "SELECT id FROM items WHERE price = amount",
            "SELECT id FROM items WHERE price IN (0.1, 0.2)",
            "SELECT id FROM items WHERE price NOT IN (1, 0.1)",
            "SELECT i.id FROM items i JOIN prices p ON p.price = i.price",
            "SELECT price = qty / 3.0 FROM items",
            "SELECT id FROM items WHERE weight = 0.1",
            // Integers a real (24 mantissa bits) or a double precision (53)
            // can't hold.
            "SELECT id FROM items WHERE weight = 16777217",
            "SELECT id FROM items WHERE weight = -16777217",
            "SELECT id FROM items WHERE weight = 2147483647",
            "SELECT id FROM items WHERE weight IN (0, 16777217)",
            "SELECT id FROM items WHERE price = 9007199254740993",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert_eq!(findings.len(), 1, "{sql}: {findings:?}");
            assert!(findings[0].fix.is_empty());
        }
    }

    #[test]
    fn comparisons_with_exact_values_or_other_types_are_not_reported() {
        let db = catalog();
        for sql in [
            "SELECT id FROM items WHERE price = 0",
            "SELECT id FROM items WHERE price = -1",
            "SELECT id FROM items WHERE price = 0.5",
            "SELECT id FROM items WHERE price = 1e3",
            "SELECT id FROM items WHERE price = 2.25::float8",
            "SELECT id FROM items WHERE price = 'NaN'",
            "SELECT id FROM items WHERE price <> '-Infinity'::float8",
            "SELECT id FROM items WHERE price IN (0, 1, 2.5)",
            "SELECT id FROM items WHERE price < 0.1",
            "SELECT id FROM items WHERE price >= $1",
            "SELECT id FROM items WHERE price::numeric = 0.1",
            "SELECT id FROM items WHERE amount = 0.1",
            "SELECT id FROM items WHERE qty = 3",
            "SELECT id FROM items WHERE price IS DISTINCT FROM 0.1",
            // Integers the type holds exactly.
            "SELECT id FROM items WHERE weight = 16777216",
            "SELECT id FROM items WHERE weight = -16777216",
            "SELECT id FROM items WHERE weight = 33554432",
            "SELECT id FROM items WHERE price = 2147483647",
            "SELECT id FROM items WHERE price = 9007199254740992",
        ] {
            let findings = lint_rule(&db, sql, RULE);
            assert!(findings.is_empty(), "{sql}: {findings:?}");
        }
    }

    #[test]
    fn exact_decimals() {
        for (text, bits, exact) in [
            ("0.5", 53, true),
            ("0.1", 53, false),
            ("2.25", 24, true),
            ("1e3", 53, true),
            ("1e-1", 53, false),
            ("12.500", 53, true),
            ("0.0", 53, true),
            ("9007199254740993", 53, false),
            ("9007199254740992", 53, true),
            ("16777217", 24, false),
            ("abc", 53, false),
        ] {
            assert_eq!(exactly_representable(text, bits), exact, "{text}");
        }
    }
}
