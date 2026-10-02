//! Template generators: expressions, statements (SELECT / set-op / CTE /
//! MERGE / DML / VALUES), literal probes, and the `$pN` ⇄ `$N` parameter
//! form conversions used by the mutation pipeline.

use super::*;

// ──────────────────────────────────────────────────────────────────────────
// Strategy 1 — schema-driven template generation.
// ──────────────────────────────────────────────────────────────────────────

pub(crate) fn literal_for(ty: Ty, rng: &mut StdRng) -> String {
    match ty {
        Ty::Int => ["0", "42", "-7", "2147483647"][rng.random_range(0..4)].to_string(),
        Ty::BigInt => ["0", "9999999999", "-1"][rng.random_range(0..3)].to_string(),
        Ty::Numeric => ["3.14", "0.0", "100.5"][rng.random_range(0..3)].to_string(),
        Ty::Float => ["2.5", "1e3", "0.0"][rng.random_range(0..3)].to_string(),
        Ty::Text => ["'hello'", "'x'", "''"][rng.random_range(0..3)].to_string(),
        Ty::Bool => ["true", "false"][rng.random_range(0..2)].to_string(),
        Ty::Timestamptz => "now()".to_string(),
        Ty::Date => "current_date".to_string(),
        Ty::Uuid => "'00000000-0000-0000-0000-000000000000'::uuid".to_string(),
        Ty::Jsonb => "'{}'::jsonb".to_string(),
        Ty::Enum => ["'draft'", "'published'", "'archived'"][rng.random_range(0..3)].to_string(),
        Ty::IntArr => "ARRAY[1, 2, 3]".to_string(),
        Ty::TextArr => "ARRAY['a', 'b']".to_string(),
    }
}

/// A column reference from `table`, optionally a deliberately-wrong type.
pub(crate) fn random_col<'a>(table: &'a Table, rng: &mut StdRng) -> &'a Col {
    &table.cols[rng.random_range(0..table.cols.len())]
}

/// Generate an expression. `depth` bounds recursion. The generator is only
/// loosely type-aware: it freely mixes types so the oracle sees both
/// well-typed queries (type-inference bugs) and ill-typed ones (error-message
/// bugs). `np` is the running parameter counter (see [`next_param`]).
pub(crate) fn gen_expr(table: &Table, depth: u32, rng: &mut StdRng, np: &mut u32) -> String {
    if depth == 0 || rng.random_bool(0.35) {
        // Leaf: column ref or literal.
        return if rng.random_bool(0.6) {
            random_col(table, rng).name.to_string()
        } else {
            let ty = [
                Ty::Int,
                Ty::Text,
                Ty::Bool,
                Ty::Numeric,
                Ty::Float,
                Ty::Timestamptz,
            ][rng.random_range(0..6)];
            // ~15% of leaves are a query parameter rather than a column /
            // literal, so the oracle's input-parameter-type comparison gets
            // exercised. A third are pinned with an explicit cast (checks
            // the trivially-known type round-trips); the rest are *bare* so
            // the enclosing context — function argument, operator operand,
            // CASE branch — must infer them exactly like PG does.
            if rng.random_bool(0.05) {
                let t = BASE_TYPE_NAMES[rng.random_range(0..BASE_TYPE_NAMES.len())];
                return format!("(${}::{t})", next_param(np));
            }
            if rng.random_bool(0.105) {
                return format!("${}", next_param(np));
            }
            literal_for(ty, rng)
        };
    }
    match rng.random_range(0..13) {
        0 => {
            // Binary op.
            let op = OPERATORS[rng.random_range(0..OPERATORS.len())];
            format!(
                "({} {} {})",
                gen_expr(table, depth - 1, rng, np),
                op,
                gen_expr(table, depth - 1, rng, np)
            )
        }
        8 => {
            // NULL test / IS DISTINCT FROM — always-boolean predicates that
            // accept any operand type. The DISTINCT rhs may be a bare
            // param (typed from the lhs, like `=`).
            let col = random_col(table, rng);
            match rng.random_range(0..3) {
                0 => format!("({} IS NULL)", col.name),
                1 => format!("({} IS NOT NULL)", gen_expr(table, depth - 1, rng, np)),
                _ => format!(
                    "({} IS DISTINCT FROM {})",
                    col.name,
                    lit_or_param(col.ty, rng, np)
                ),
            }
        }
        9 => {
            // BETWEEN / IN-list / = ANY(array) over a column, with literals
            // of the column's own type or bare params (typed per-bound by
            // PG); the mutator mistypes the literals later, exercising
            // per-bound coercion errors.
            let col = random_col(table, rng);
            match rng.random_range(0..3) {
                0 => format!(
                    "({} BETWEEN {} AND {})",
                    col.name,
                    lit_or_param(col.ty, rng, np),
                    lit_or_param(col.ty, rng, np)
                ),
                1 => format!(
                    "({} IN ({}, {}))",
                    col.name,
                    lit_or_param(col.ty, rng, np),
                    lit_or_param(col.ty, rng, np)
                ),
                _ => format!(
                    "({} = ANY(ARRAY[{}, {}]))",
                    col.name,
                    lit_or_param(col.ty, rng, np),
                    lit_or_param(col.ty, rng, np)
                ),
            }
        }
        10 => {
            // Array subscript / slice over one of the array columns.
            let arr = if rng.random_bool(0.5) { "tags" } else { "nums" };
            if table.cols.iter().any(|c| c.name == arr) {
                if rng.random_bool(0.3) {
                    format!("({arr}[1:2])")
                } else {
                    format!("({}[{}])", arr, gen_expr(table, depth - 1, rng, np))
                }
            } else {
                gen_expr(table, depth - 1, rng, np)
            }
        }
        11 => {
            // Literal-content probe in expression position: stresses the
            // analyzer's parse-time input validation (`literal_input`).
            let lit = LITERAL_PROBES[rng.random_range(0..LITERAL_PROBES.len())];
            let ty = PROBE_TYPE_NAMES[rng.random_range(0..PROBE_TYPE_NAMES.len())];
            format!("('{}'::{})", lit.replace('\'', "''"), ty)
        }
        12 => {
            // COLLATE decoration over a text-ish operand — sometimes on a
            // non-string type or with a bogus collation name, both PG error
            // paths the analyzer mirrors.
            let col = random_col(table, rng);
            let coll = ["\"C\"", "\"POSIX\"", "\"C\"", "\"nope\""][rng.random_range(0..4)];
            format!("({} COLLATE {coll})", col.name)
        }
        7 => {
            // Type-aware comparison: a column against a literal — or a bare
            // parameter — of its own type. The parameter form stresses param
            // type *inference* from operator context (PG should report the
            // param as the column's type), the highest-value param case.
            let col = random_col(table, rng);
            let op = ["=", "<>", "<", ">", "<=", ">="][rng.random_range(0..6)];
            let rhs = if rng.random_bool(0.4) {
                format!("${}", next_param(np))
            } else {
                literal_for(col.ty, rng)
            };
            format!("({} {} {})", col.name, op, rhs)
        }
        1 => {
            // Function call with 0..3 args.
            let f = FUNCTIONS[rng.random_range(0..FUNCTIONS.len())];
            let nargs = rng.random_range(0..3);
            let args: Vec<String> = (0..nargs)
                .map(|_| gen_expr(table, depth - 1, rng, np))
                .collect();
            format!("{}({})", f, args.join(", "))
        }
        2 => {
            // Cast.
            let t = BASE_TYPE_NAMES[rng.random_range(0..BASE_TYPE_NAMES.len())];
            format!("({})::{}", gen_expr(table, depth - 1, rng, np), t)
        }
        3 => format!(
            "COALESCE({}, {})",
            gen_expr(table, depth - 1, rng, np),
            gen_expr(table, depth - 1, rng, np)
        ),
        4 => format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            gen_expr(table, depth - 1, rng, np),
            gen_expr(table, depth - 1, rng, np),
            gen_expr(table, depth - 1, rng, np)
        ),
        5 => {
            // Aggregate (valid only with/without GROUP BY; the oracle
            // judges), occasionally with a FILTER clause — placement and
            // FILTER-must-be-boolean rules get exercised for free.
            let agg = ["count", "sum", "avg", "min", "max"][rng.random_range(0..5)];
            let call = format!("{}({})", agg, gen_expr(table, depth - 1, rng, np));
            if rng.random_bool(0.2) {
                format!(
                    "({call} FILTER (WHERE {}))",
                    gen_expr(table, depth - 1, rng, np)
                )
            } else {
                call
            }
        }
        _ => format!("(NOT {})", gen_expr(table, depth - 1, rng, np)),
    }
}

/// Allocate the next positional parameter name (`p0`, `p1`, …). Names are
/// handed out left-to-right as the query string is built, so first-occurrence
/// order matches the `$1, $2, …` numbering PG assigns — keeping the analyzer's
/// and PG's parameter lists index-aligned for the oracle's comparison.
pub(crate) fn next_param(np: &mut u32) -> String {
    let name = format!("p{}", *np);
    *np += 1;
    name
}

/// A literal of `ty` — or, some of the time, a bare `$pN` parameter in its
/// place. Bare params in rich positions (BETWEEN bounds, IN items, function
/// args, CASE branches, …) are what exercise PG's parameter-type inference;
/// this is exactly the surface where the analyzer's typing has to match
/// PG's Describe.
pub(crate) fn lit_or_param(ty: Ty, rng: &mut StdRng, np: &mut u32) -> String {
    if rng.random_bool(0.3) {
        format!("${}", next_param(np))
    } else {
        literal_for(ty, rng)
    }
}

/// Convert the fuzzer's named placeholders (`$pN`, the form the analyzer
/// accepts) into PG-native positional ones (`$N`) so `typedpg_pg_query` can parse
/// the statement — the mutation/minimization pipeline operates on the
/// positional form. Quote-aware enough for fuzzer-generated SQL.
pub(crate) fn named_to_positional(sql: &str) -> String {
    rewrite_params(sql, |digits, out| {
        out.push('$');
        out.push_str(digits);
    })
}

/// Inverse of [`named_to_positional`]: deparsed/mutated SQL carries `$N`;
/// the analyzer wants `$pN`.
pub(crate) fn positional_to_named(sql: &str) -> String {
    rewrite_params(sql, |digits, out| {
        out.push_str("$p");
        out.push_str(digits);
    })
}

/// Shared scanner: find `$p?<digits>` outside single-quoted strings and let
/// `emit` rewrite each occurrence.
pub(crate) fn rewrite_params(sql: &str, emit: impl Fn(&str, &mut String)) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            in_string = !in_string;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_string && c == '$' {
            let mut j = i + 1;
            if j < chars.len() && chars[j] == 'p' {
                j += 1;
            }
            let digits_start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_start {
                let digits: String = chars[digits_start..j].iter().collect();
                emit(&digits, &mut out);
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

pub(crate) fn pick_table(rng: &mut StdRng) -> &'static Table {
    &TABLES[rng.random_range(0..TABLES.len())]
}

/// A standalone scalar literal of a random type (no column references) — used
/// in contexts without a FROM scope (INSERT … VALUES).
pub(crate) fn scalar_literal(rng: &mut StdRng) -> String {
    let ty = [
        Ty::Int,
        Ty::BigInt,
        Ty::Numeric,
        Ty::Float,
        Ty::Text,
        Ty::Bool,
        Ty::Timestamptz,
        Ty::Date,
        Ty::Uuid,
        Ty::Jsonb,
        Ty::Enum,
        Ty::IntArr,
        Ty::TextArr,
    ][rng.random_range(0..13)];
    literal_for(ty, rng)
}

/// Pick `k` distinct columns from `cols` (partial Fisher-Yates).
pub(crate) fn pick_cols<'a>(cols: &[&'a Col], k: usize, rng: &mut StdRng) -> Vec<&'a Col> {
    let mut idxs: Vec<usize> = (0..cols.len()).collect();
    let k = k.min(idxs.len());
    for i in 0..k {
        let j = rng.random_range(i..idxs.len());
        idxs.swap(i, j);
    }
    idxs[..k].iter().map(|&i| cols[i]).collect()
}

/// Top-level statement dispatcher. Each invocation owns its parameter counter.
pub(crate) fn gen_statement(rng: &mut StdRng) -> String {
    let np = &mut 0u32;
    match rng.random_range(0..100) {
        0..=36 => gen_select(rng, np),
        37..=42 => gen_narrowing_select(rng),
        43..=46 => gen_constraint_select(rng),
        47..=57 => gen_set_op(rng, np),
        58..=66 => gen_cte(rng, np),
        67..=72 => gen_values_select(rng, np),
        73..=78 => gen_merge(rng, np),
        _ => gen_dml(rng, np),
    }
}

/// Full-featured SELECT: DISTINCT [ON], varied joins, subquery predicates,
/// scalar subqueries, GROUP BY / ORDER BY / LIMIT.
pub(crate) fn gen_select(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let mut sql = String::from("SELECT ");

    match rng.random_range(0..10) {
        0 => sql.push_str("DISTINCT "),
        1 => sql.push_str(&format!("DISTINCT ON ({}) ", gen_expr(table, 2, rng, np))),
        _ => {}
    }

    // Projection: 1..4 expressions, occasionally a scalar subquery or a
    // window function call (placement + frame rules judged by the oracle).
    let n = rng.random_range(1..4);
    let projs: Vec<String> = (0..n)
        .map(|i| {
            let e = if rng.random_bool(0.15) {
                gen_scalar_subquery(rng, np)
            } else if rng.random_bool(0.12) {
                gen_window_call(table, rng, np)
            } else {
                gen_expr(table, 3, rng, np)
            };
            if rng.random_bool(0.4) {
                format!("{e} AS c{i}")
            } else {
                e
            }
        })
        .collect();
    sql.push_str(&projs.join(", "));
    sql.push_str(&format!(" FROM {}", table.name));

    // Optional join — varied type. CROSS JOIN takes no ON clause; LATERAL
    // subqueries may reference the left table's columns.
    if rng.random_bool(0.25) {
        let other = pick_table(rng);
        match rng.random_range(0..6) {
            0 => sql.push_str(&format!(" CROSS JOIN {} AS j", other.name)),
            1 => sql.push_str(&format!(
                ", LATERAL (SELECT {} AS lx FROM {} WHERE {}) AS l",
                gen_expr(other, 2, rng, np),
                other.name,
                gen_expr(table, 2, rng, np),
            )),
            r => {
                let jt = ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"][r - 2];
                sql.push_str(&format!(
                    " {jt} {} AS j ON {}",
                    other.name,
                    gen_expr(table, 2, rng, np)
                ));
            }
        }
    }

    if rng.random_bool(0.6) {
        let pred = if rng.random_bool(0.25) {
            gen_subquery_predicate(table, rng, np)
        } else {
            gen_expr(table, 3, rng, np)
        };
        sql.push_str(&format!(" WHERE {pred}"));
    }

    if rng.random_bool(0.2) {
        let c = random_col(table, rng);
        // Plain column, or the grouping-set family (ROLLUP/CUBE/GROUPING
        // SETS incl. the empty set) — exercises grouping expansion and
        // aggregate-nullability under partially-grouped rows.
        match rng.random_range(0..6) {
            0 => {
                let c2 = random_col(table, rng);
                sql.push_str(&format!(" GROUP BY ROLLUP ({}, {})", c.name, c2.name));
            }
            1 => {
                let c2 = random_col(table, rng);
                sql.push_str(&format!(" GROUP BY CUBE ({}, {})", c.name, c2.name));
            }
            2 => {
                let c2 = random_col(table, rng);
                sql.push_str(&format!(
                    " GROUP BY GROUPING SETS (({}), ({}), ())",
                    c.name, c2.name
                ));
            }
            _ => sql.push_str(&format!(" GROUP BY {}", c.name)),
        }
        // HAVING — sometimes an aggregate predicate (valid), sometimes an
        // arbitrary expression (exercises HAVING placement / boolean rules).
        if rng.random_bool(0.4) {
            let pred = if rng.random_bool(0.5) {
                format!("count(*) > {}", rng.random_range(0..5))
            } else {
                gen_expr(table, 2, rng, np)
            };
            sql.push_str(&format!(" HAVING {pred}"));
        }
    }

    if rng.random_bool(0.2) {
        sql.push_str(&format!(" ORDER BY {}", gen_expr(table, 2, rng, np)));
        match rng.random_range(0..4) {
            0 => sql.push_str(" DESC"),
            1 => sql.push_str(" NULLS FIRST"),
            2 => sql.push_str(" DESC NULLS LAST"),
            _ => {}
        }
    }

    if rng.random_bool(0.2) {
        let lim = match rng.random_range(0..3) {
            0 => rng.random_range(0..100).to_string(),
            1 => format!("${}", next_param(np)),
            _ => gen_expr(table, 1, rng, np),
        };
        if rng.random_bool(0.2) {
            // SQL-standard form; WITH TIES requires an ORDER BY, which this
            // SELECT only sometimes has — the invalid combination is itself
            // a useful probe (PG rejects it with a dedicated message).
            let ties = if rng.random_bool(0.5) {
                "WITH TIES"
            } else {
                "ONLY"
            };
            sql.push_str(&format!(" FETCH FIRST {lim} ROWS {ties}"));
        } else {
            sql.push_str(&format!(" LIMIT {lim}"));
        }
        if rng.random_bool(0.3) {
            sql.push_str(&format!(" OFFSET {}", rng.random_range(0..10)));
        }
    }

    sql
}

/// A window-function call for a projection slot: ranking functions,
/// aggregates with OVER, and the value-window family (`lag`/`lead`, whose
/// edge-NULL semantics make nullability interesting).
pub(crate) fn gen_window_call(table: &Table, rng: &mut StdRng, np: &mut u32) -> String {
    let over = {
        let mut parts = Vec::new();
        if rng.random_bool(0.5) {
            parts.push(format!("PARTITION BY {}", random_col(table, rng).name));
        }
        let has_order = rng.random_bool(0.7);
        if has_order {
            parts.push(format!("ORDER BY {}", random_col(table, rng).name));
            // Frame clauses (need an ORDER BY to be meaningful; RANGE with
            // an offset additionally needs a sortable single key — the
            // oracle judges). A $pN offset exercises frame-bound param
            // typing (int8 for ROWS).
            if rng.random_bool(0.3) {
                parts.push(match rng.random_range(0..4) {
                    0 => "ROWS BETWEEN 1 PRECEDING AND CURRENT ROW".to_string(),
                    1 => "RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING".to_string(),
                    2 => format!("ROWS BETWEEN ${} PRECEDING AND CURRENT ROW", next_param(np)),
                    _ => "ROWS 2 PRECEDING".to_string(),
                });
            }
        }
        parts.join(" ")
    };
    match rng.random_range(0..4) {
        0 => format!(
            "{}() OVER ({over})",
            ["row_number", "rank", "dense_rank"][rng.random_range(0..3)]
        ),
        1 => format!(
            "{}({}) OVER ({over})",
            ["sum", "avg", "min", "max", "count"][rng.random_range(0..5)],
            random_col(table, rng).name
        ),
        2 if rng.random_bool(0.35) => format!(
            // Two-arg lag/lead: the offset is int4 in PG's signature — a
            // bare param here is typed through function-argument inference.
            "{}({}, ${}) OVER ({over})",
            ["lag", "lead"][rng.random_range(0..2)],
            random_col(table, rng).name,
            next_param(np)
        ),
        2 => format!(
            "{}({}) OVER ({over})",
            ["lag", "lead", "first_value", "last_value"][rng.random_range(0..4)],
            random_col(table, rng).name
        ),
        _ => format!("ntile({}) OVER ({over})", gen_expr(table, 1, rng, np)),
    }
}

/// `SELECT <projs> FROM <table> [WHERE <expr>]` — no clauses that would be
/// syntactically awkward inside a set-operation branch / CTE body / subquery.
pub(crate) fn gen_simple_select(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let n = rng.random_range(1..3);
    let projs: Vec<String> = (0..n).map(|_| gen_expr(table, 2, rng, np)).collect();
    let mut sql = format!("SELECT {} FROM {}", projs.join(", "), table.name);
    if rng.random_bool(0.5) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
    }
    sql
}

/// `(SELECT <agg>(<col>) FROM <table>)` — a scalar subquery for a projection
/// slot. Mostly single-column so PG accepts it as a scalar.
pub(crate) fn gen_scalar_subquery(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let agg = ["count", "sum", "avg", "min", "max"][rng.random_range(0..5)];
    let arg = gen_expr(table, 1, rng, np);
    format!("(SELECT {agg}({arg}) FROM {})", table.name)
}

/// `col IN (SELECT …)` / `[NOT] EXISTS (SELECT …)` for a WHERE clause.
pub(crate) fn gen_subquery_predicate(
    table: &'static Table,
    rng: &mut StdRng,
    np: &mut u32,
) -> String {
    let other = pick_table(rng);
    match rng.random_range(0..3) {
        0 => {
            let col = random_col(table, rng);
            let inner = random_col(other, rng);
            format!(
                "{} IN (SELECT {} FROM {})",
                col.name, inner.name, other.name
            )
        }
        1 => format!(
            "EXISTS (SELECT 1 FROM {} WHERE {})",
            other.name,
            gen_expr(other, 2, rng, np)
        ),
        _ => format!(
            "NOT EXISTS (SELECT 1 FROM {} WHERE {})",
            other.name,
            gen_expr(other, 2, rng, np)
        ),
    }
}

/// Two simple selects combined with a set operation — exercises column-count
/// and common-type reconciliation across the branches.
pub(crate) fn gen_set_op(rng: &mut StdRng, np: &mut u32) -> String {
    let op = ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"][rng.random_range(0..4)];
    format!(
        "{} {op} {}",
        gen_simple_select(rng, np),
        gen_simple_select(rng, np)
    )
}

/// `WITH …` statements: pass-through CTEs, RECURSIVE ones, data-modifying
/// CTEs (`WITH x AS (DELETE … RETURNING …) SELECT …`), and a CTE feeding an
/// INSERT.
pub(crate) fn gen_cte(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..10) {
        // Recursive CTE: numeric or text accumulation, sometimes with a
        // param in the recursion bound.
        0..=2 => {
            let bound = if rng.random_bool(0.25) {
                format!("${}", next_param(np))
            } else {
                rng.random_range(2..20).to_string()
            };
            if rng.random_bool(0.5) {
                format!(
                    "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL \
                     SELECT n + 1 FROM r WHERE n < {bound}) SELECT * FROM r"
                )
            } else {
                format!(
                    "WITH RECURSIVE r AS (SELECT 'a'::text AS s, 1 AS n UNION ALL \
                     SELECT s || 'x', n + 1 FROM r WHERE n < {bound}) SELECT s FROM r"
                )
            }
        }
        // Data-modifying CTE: the statement's rows come from a DML's
        // RETURNING list.
        3..=4 => {
            let table = pick_table(rng);
            match rng.random_range(0..2) {
                0 => format!(
                    "WITH moved AS (DELETE FROM {} WHERE {} RETURNING id) \
                     SELECT count(*) FROM moved",
                    table.name,
                    gen_expr(table, 2, rng, np),
                ),
                _ => {
                    let col = random_col(table, rng);
                    format!(
                        "WITH up AS (UPDATE {} SET {} = {} RETURNING id, {}) \
                         SELECT * FROM up",
                        table.name,
                        col.name,
                        lit_or_param(col.ty, rng, np),
                        col.name,
                    )
                }
            }
        }
        // WITH feeding a DML.
        5 => format!(
            "WITH src AS (SELECT id, name FROM users WHERE {}) \
             INSERT INTO posts (user_id, title) SELECT id, name FROM src",
            gen_expr(&TABLES[0], 2, rng, np),
        ),
        _ => format!(
            "WITH cte AS ({}) SELECT * FROM cte",
            gen_simple_select(rng, np)
        ),
    }
}

/// `MERGE INTO … USING … ON … WHEN [NOT] MATCHED …` — exercises the merge
/// resolver: join-condition typing, per-action assignment coercion, the
/// source relation's scope inside UPDATE SET / INSERT VALUES, and action
/// conditions.
pub(crate) fn gen_merge(rng: &mut StdRng, np: &mut u32) -> String {
    // Fixed direction (posts ← users) so the ON join makes sense; the
    // expressions inside perturb freely.
    let mut sql = String::from("MERGE INTO posts p USING users u ON p.user_id = u.id");

    let matched_cond = if rng.random_bool(0.3) {
        format!(" AND {}", gen_expr(&TABLES[1], 1, rng, np))
    } else {
        String::new()
    };
    match rng.random_range(0..3) {
        0 => {
            // Skip col 0 (`id`, the identity PK).
            let col = &TABLES[1].cols[rng.random_range(1..TABLES[1].cols.len())];
            sql.push_str(&format!(
                " WHEN MATCHED{matched_cond} THEN UPDATE SET {} = {}",
                col.name,
                lit_or_param(col.ty, rng, np),
            ));
        }
        1 => sql.push_str(&format!(" WHEN MATCHED{matched_cond} THEN DELETE")),
        _ => sql.push_str(&format!(" WHEN MATCHED{matched_cond} THEN DO NOTHING")),
    }
    if rng.random_bool(0.7) {
        match rng.random_range(0..2) {
            0 => sql.push_str(&format!(
                " WHEN NOT MATCHED THEN INSERT (user_id, title) VALUES (u.id, {})",
                if rng.random_bool(0.4) {
                    format!("${}", next_param(np))
                } else {
                    "u.name".to_string()
                },
            )),
            _ => sql.push_str(" WHEN NOT MATCHED THEN DO NOTHING"),
        }
    }
    sql
}

// ── DML ──────────────────────────────────────────────────────────────────────

pub(crate) fn gen_dml(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..3) {
        0 => gen_insert(rng, np),
        1 => gen_update(rng, np),
        _ => gen_delete(rng, np),
    }
}

/// `RETURNING *` or a small projection over the affected table.
pub(crate) fn gen_returning(table: &'static Table, rng: &mut StdRng, np: &mut u32) -> String {
    if rng.random_bool(0.3) {
        return " RETURNING *".to_string();
    }
    let n = rng.random_range(1..3);
    let projs: Vec<String> = (0..n).map(|_| gen_expr(table, 2, rng, np)).collect();
    format!(" RETURNING {}", projs.join(", "))
}

/// A value for `INSERT … VALUES` — no FROM scope, so column-typed literals,
/// parameters (inferred by assignment context), NULL, DEFAULT, or a
/// deliberately mistyped literal.
pub(crate) fn gen_insert_value(col: &Col, rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..10) {
        0..=4 => literal_for(col.ty, rng),
        5..=6 => format!("${}", next_param(np)),
        7 => "NULL".to_string(),
        8 => "DEFAULT".to_string(),
        _ => scalar_literal(rng),
    }
}

pub(crate) fn gen_insert(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    // Skip the identity `id` so most rows are insertable.
    let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
    let k = rng.random_range(1..=cols.len().min(4));
    let chosen = pick_cols(&cols, k, rng);
    let collist = chosen.iter().map(|c| c.name).collect::<Vec<_>>().join(", ");
    let mut sql = if rng.random_bool(0.2) {
        // INSERT … SELECT — arity and per-column assignment coercion across
        // a query source instead of a VALUES list.
        let src = pick_table(rng);
        let exprs: Vec<String> = (0..k).map(|_| gen_expr(src, 2, rng, np)).collect();
        format!(
            "INSERT INTO {} ({collist}) SELECT {} FROM {}",
            table.name,
            exprs.join(", "),
            src.name
        )
    } else {
        let vals = chosen
            .iter()
            .map(|c| gen_insert_value(c, rng, np))
            .collect::<Vec<_>>()
            .join(", ");
        format!("INSERT INTO {} ({collist}) VALUES ({vals})", table.name)
    };
    // ON CONFLICT over the PK — DO NOTHING or DO UPDATE with a (sometimes
    // mistyped) SET, plus the EXCLUDED pseudo-relation occasionally.
    if rng.random_bool(0.15) {
        match rng.random_range(0..3) {
            0 => sql.push_str(" ON CONFLICT (id) DO NOTHING"),
            1 => {
                let c = chosen[rng.random_range(0..chosen.len())];
                sql.push_str(&format!(
                    " ON CONFLICT (id) DO UPDATE SET {} = {}",
                    c.name,
                    literal_for(c.ty, rng)
                ));
            }
            _ => {
                let c = chosen[rng.random_range(0..chosen.len())];
                sql.push_str(&format!(
                    " ON CONFLICT (id) DO UPDATE SET {} = EXCLUDED.{}",
                    c.name, c.name
                ));
            }
        }
    }
    if rng.random_bool(0.4) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

/// `SELECT … FROM (VALUES …) AS v(a, b)` — a derived VALUES table: column
/// aliasing, cross-row common-type resolution, and the unknown-literal
/// column case all live here.
pub(crate) fn gen_values_select(rng: &mut StdRng, np: &mut u32) -> String {
    let n_rows = rng.random_range(1..=3);
    let n_cols = rng.random_range(1..=3);
    let rows: Vec<String> = (0..n_rows)
        .map(|_| {
            let vals: Vec<String> = (0..n_cols)
                .map(|_| {
                    if rng.random_bool(0.15) {
                        format!("${}", next_param(np))
                    } else {
                        scalar_literal(rng)
                    }
                })
                .collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    let aliases: Vec<String> = (0..n_cols).map(|i| format!("a{i}")).collect();
    let proj = if rng.random_bool(0.5) {
        "*".to_string()
    } else {
        aliases[rng.random_range(0..aliases.len())].clone()
    };
    format!(
        "SELECT {proj} FROM (VALUES {}) AS v({})",
        rows.join(", "),
        aliases.join(", ")
    )
}

/// A standalone literal-content probe (Strategy 5): `'<content>'::<type>`
/// in one of several coercion contexts — explicit cast, comparison against
/// a typed column, COALESCE branch, INSERT assignment. Directly stresses
/// the analyzer's parse-time input validation (`literal_input`) in both
/// directions: rejections must match PG's wording, acceptances must agree
/// on the result type.
pub(crate) fn gen_literal_probe(rng: &mut StdRng) -> String {
    let lit = LITERAL_PROBES[rng.random_range(0..LITERAL_PROBES.len())].replace('\'', "''");
    let ty = PROBE_TYPE_NAMES[rng.random_range(0..PROBE_TYPE_NAMES.len())];
    match rng.random_range(0..5) {
        0 => format!("SELECT '{lit}'::{ty} AS c0"),
        1 => format!("SELECT '{lit}'::{ty} AS c0 FROM users"),
        2 => {
            // Comparison against a typed column — the literal is coerced by
            // operator resolution, not an explicit cast.
            let table = pick_table(rng);
            let col = random_col(table, rng);
            format!("SELECT id FROM {} WHERE {} = '{lit}'", table.name, col.name)
        }
        3 => {
            let table = pick_table(rng);
            let col = random_col(table, rng);
            format!("SELECT COALESCE({}, '{lit}') FROM {}", col.name, table.name)
        }
        _ => {
            // INSERT assignment context.
            let table = pick_table(rng);
            let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
            let col = cols[rng.random_range(0..cols.len())];
            format!("INSERT INTO {} ({}) VALUES ('{lit}')", table.name, col.name)
        }
    }
}

pub(crate) fn gen_update(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let cols: Vec<&Col> = table.cols.iter().filter(|c| c.name != "id").collect();
    let k = rng.random_range(1..=cols.len().min(3));
    let chosen = pick_cols(&cols, k, rng);
    // In UPDATE, SET expressions can reference the table's columns.
    let sets = chosen
        .iter()
        .map(|c| {
            let v = match rng.random_range(0..10) {
                0..=4 => literal_for(c.ty, rng),
                5..=6 => format!("${}", next_param(np)),
                7 => "NULL".to_string(),
                8 => "DEFAULT".to_string(),
                _ => gen_expr(table, 1, rng, np),
            };
            format!("{} = {v}", c.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("UPDATE {} SET {sets}", table.name);
    // `FROM other` — extra joinable source visible to WHERE/RETURNING.
    let from = rng.random_bool(0.2).then(|| pick_table(rng));
    if let Some(f) = from {
        sql.push_str(&format!(" FROM {} AS f_src", f.name));
    }
    if rng.random_bool(0.6) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
        if let Some(f) = from {
            let fc = random_col(f, rng);
            sql.push_str(&format!(" AND f_src.{} IS NOT NULL", fc.name));
        }
    }
    if rng.random_bool(0.3) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}

pub(crate) fn gen_delete(rng: &mut StdRng, np: &mut u32) -> String {
    let table = pick_table(rng);
    let mut sql = format!("DELETE FROM {}", table.name);
    // `USING other` — extra joinable source visible to WHERE/RETURNING.
    let using = rng.random_bool(0.2).then(|| pick_table(rng));
    if let Some(u) = using {
        sql.push_str(&format!(" USING {} AS u_src", u.name));
    }
    if rng.random_bool(0.7) {
        sql.push_str(&format!(" WHERE {}", gen_expr(table, 2, rng, np)));
        if let Some(u) = using {
            let uc = random_col(u, rng);
            let tc = random_col(table, rng);
            sql.push_str(&format!(
                " AND u_src.{} IS NOT NULL AND {}.{} IS NOT NULL",
                uc.name, table.name, tc.name
            ));
        }
    }
    if rng.random_bool(0.3) {
        sql.push_str(&gen_returning(table, rng, np));
    }
    sql
}
