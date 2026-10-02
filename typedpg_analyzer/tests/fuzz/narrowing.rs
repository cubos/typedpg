//! Queries shaped to exercise nullability narrowing: outer joins whose
//! WHERE may or may not reduce them, CASE branches over NULL tests, and
//! aggregates with a FILTER. Valid by construction (the columns are
//! compared with literals of their own type), so what the oracle mostly
//! judges here is the soundness of every NOT NULL the analyzer infers.

use super::*;

/// A qualified column of `table` under `alias`.
struct QCol {
    alias: &'static str,
    col: &'static Col,
}

impl QCol {
    fn sql(&self) -> String {
        format!("{}.{}", self.alias, self.col.name)
    }
}

fn pick(alias: &'static str, table: &'static Table, rng: &mut StdRng) -> QCol {
    QCol {
        alias,
        col: random_col(table, rng),
    }
}

/// Whether `ty` has the ordering / equality operators the atoms use with
/// a literal of the same type.
fn comparable(ty: Ty) -> bool {
    matches!(
        ty,
        Ty::Int | Ty::BigInt | Ty::Numeric | Ty::Float | Ty::Text | Ty::Timestamptz | Ty::Date
    )
}

/// One predicate over `c`: strict (proves `c`), non-strict, or a NULL test.
fn atom(c: &QCol, rng: &mut StdRng) -> String {
    let col = c.sql();
    let ty = c.col.ty;
    if !comparable(c.col.ty) {
        return match rng.random_range(0..3) {
            0 => format!("{col} IS NOT NULL"),
            1 => format!("{col} IS NULL"),
            _ => format!("NOT ({col} IS NULL)"),
        };
    }
    match rng.random_range(0..12) {
        0 => format!("{col} IS NOT NULL"),
        1 => format!("{col} IS NULL"),
        2 => format!("{col} = {}", literal_for(ty, rng)),
        3 => format!("{col} > {}", literal_for(ty, rng)),
        4 => format!(
            "{col} IN ({}, {})",
            literal_for(ty, rng),
            literal_for(ty, rng)
        ),
        5 => format!(
            "{col} BETWEEN {} AND {}",
            literal_for(ty, rng),
            literal_for(ty, rng)
        ),
        6 => format!("({col} = {}) IS TRUE", literal_for(ty, rng)),
        7 => format!("({col} = {}) IS NOT TRUE", literal_for(ty, rng)),
        8 => format!(
            "coalesce({col}, {}) = {}",
            literal_for(ty, rng),
            literal_for(ty, rng)
        ),
        9 => format!("{col} IS DISTINCT FROM {}", literal_for(ty, rng)),
        10 => format!("NOT ({col} IS NULL)"),
        _ => format!("{col}::text <> ''"),
    }
}

/// A predicate of one to three atoms joined by AND / OR, maybe negated.
fn predicate(cols: &[QCol], rng: &mut StdRng) -> String {
    let n = rng.random_range(1..=3);
    let atoms: Vec<String> = (0..n)
        .map(|_| atom(&cols[rng.random_range(0..cols.len())], rng))
        .collect();
    let joined = atoms.join(if rng.random_bool(0.6) {
        " AND "
    } else {
        " OR "
    });
    if rng.random_bool(0.15) {
        format!("NOT ({joined})")
    } else {
        joined
    }
}

/// A CASE whose branches read columns its WHEN may or may not prove.
fn case_expr(cols: &[QCol], rng: &mut StdRng) -> String {
    let c = &cols[rng.random_range(0..cols.len())];
    let other = &cols[rng.random_range(0..cols.len())];
    let fallback = literal_for(c.col.ty, rng);
    match rng.random_range(0..4) {
        0 => format!(
            "CASE WHEN {} IS NULL THEN {fallback} ELSE {} END",
            other.sql(),
            c.sql()
        ),
        1 => format!(
            "CASE WHEN {} THEN {} ELSE {fallback} END",
            predicate(cols, rng),
            c.sql()
        ),
        2 => format!(
            "CASE WHEN {} THEN {fallback} ELSE {} END",
            predicate(cols, rng),
            c.sql()
        ),
        _ => format!(
            "CASE WHEN {} IS NULL THEN {fallback} WHEN {} THEN {} ELSE {} END",
            other.sql(),
            predicate(cols, rng),
            c.sql(),
            c.sql()
        ),
    }
}

/// `users u <join> posts p` (sometimes a third entry joined to `p`), a WHERE
/// that may reduce the joins, and projections reading both sides — plain,
/// in CASE branches, or aggregated under a FILTER.
pub(crate) fn gen_narrowing_select(rng: &mut StdRng) -> String {
    let users = &TABLES[0];
    let posts = &TABLES[1];
    let jt = ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"][rng.random_range(0..4)];
    let mut from = format!("users u {jt} posts p ON p.user_id = u.id");
    let mut cols: Vec<QCol> = (0..3)
        .flat_map(|_| [pick("u", users, rng), pick("p", posts, rng)])
        .collect();
    if rng.random_bool(0.3) {
        let on = predicate(&cols, rng);
        from.push_str(&format!(" AND ({on})"));
    }
    if rng.random_bool(0.3) {
        let jt2 = ["JOIN", "LEFT JOIN"][rng.random_range(0..2)];
        from.push_str(&format!(" {jt2} users u2 ON u2.id = p.user_id"));
        cols.push(pick("u2", users, rng));
        cols.push(pick("u2", users, rng));
    }

    let grouped = rng.random_bool(0.3);
    let mut projs: Vec<String> = Vec::new();
    if grouped {
        projs.push("u.id AS gid".into());
        for i in 0..rng.random_range(1..3) {
            let c = &cols[rng.random_range(0..cols.len())];
            let filter = if rng.random_bool(0.7) {
                format!(" FILTER (WHERE {})", predicate(&cols, rng))
            } else {
                String::new()
            };
            let agg = ["array_agg", "max", "min", "count"][rng.random_range(0..4)];
            let agg = if !comparable(c.col.ty) && agg != "count" {
                "array_agg"
            } else {
                agg
            };
            projs.push(format!("{agg}({}){filter} AS a{i}", c.sql()));
        }
    } else {
        for (i, c) in cols.iter().enumerate().take(rng.random_range(2..5)) {
            projs.push(format!("{} AS c{i}", c.sql()));
        }
        for i in 0..rng.random_range(0..3) {
            projs.push(format!("{} AS k{i}", case_expr(&cols, rng)));
        }
    }

    let mut sql = format!("SELECT {} FROM {from}", projs.join(", "));
    if rng.random_bool(0.8) {
        sql.push_str(&format!(" WHERE {}", predicate(&cols, rng)));
    }
    if grouped {
        sql.push_str(" GROUP BY u.id");
    }
    sql
}
