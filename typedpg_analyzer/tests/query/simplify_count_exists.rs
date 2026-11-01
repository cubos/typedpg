//! A count compared with zero → `EXISTS` / `NOT EXISTS`
//! (`Rule::CountExists`): rewritten only for the plain `pg_catalog.count`
//! of a one-row aggregate query, compared with a literal `0` / `1` through
//! the built-in `int8`/`int4` operators, nothing volatile read. Under
//! `pg_sanity` every rewrite is also executed next to the original on the
//! adversarial rows.

use crate::common::*;
use typedpg_analyzer::{Rule, SimplifyOptions};

fn setup_with(extra: &str) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id bigint PRIMARY KEY, a int NOT NULL, b int);
         CREATE TABLE u (id bigint PRIMARY KEY, t_id bigint NOT NULL, c int);
         CREATE TYPE pair AS (f1 bool, f2 int);",
    )
    .unwrap();
    if !extra.is_empty() {
        db.apply_sql(extra).unwrap();
    }
    db
}

fn setup() -> PgCatalog {
    setup_with("")
}

#[track_caller]
fn assert_rewritten_in(db: &PgCatalog, sql: &str, expected: &str) {
    let a = db.analyze(sql).unwrap();
    assert_eq!(a.sql, expected, "for {sql}");
    assert!(
        !a.simplifications.is_empty()
            && a.simplifications
                .iter()
                .all(|s| s.rule == Rule::CountExists),
        "for {sql}: {:?}",
        a.simplifications
    );
}

#[track_caller]
fn assert_rewritten(sql: &str, expected: &str) {
    assert_rewritten_in(&setup(), sql, expected);
}

#[track_caller]
fn assert_kept_in(db: &PgCatalog, sql: &str) {
    let a = db.analyze(sql).unwrap();
    assert!(
        a.simplifications.is_empty(),
        "{sql} was rewritten to {}",
        a.sql
    );
}

#[track_caller]
fn assert_kept(sql: &str) {
    assert_kept_in(&setup(), sql);
}

// ── Positive shapes ─────────────────────────────────────────────────────────

#[test]
fn every_comparison_with_zero() {
    for (sql, expected) in [
        (
            "SELECT (SELECT count(*) FROM t WHERE b > 3) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT (SELECT count(*) FROM t WHERE b > 3) >= 1 AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT (SELECT count(*) FROM t WHERE b > 3) <> 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT (SELECT count(*) FROM t WHERE b > 3) != 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT (SELECT count(*) FROM t WHERE b > 3) = 0 AS x",
            "SELECT (NOT EXISTS (SELECT 1 FROM t WHERE b > 3)) AS x",
        ),
    ] {
        assert_rewritten(sql, expected);
    }
}

#[test]
fn mirrored_comparisons() {
    for (sql, expected) in [
        (
            "SELECT 0 < (SELECT count(*) FROM t WHERE b > 3) AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT 1 <= (SELECT count(*) FROM t WHERE b > 3) AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT 0 <> (SELECT count(*) FROM t WHERE b > 3) AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS x",
        ),
        (
            "SELECT 0 = (SELECT count(*) FROM t WHERE b > 3) AS x",
            "SELECT (NOT EXISTS (SELECT 1 FROM t WHERE b > 3)) AS x",
        ),
        (
            "SELECT 0<(SELECT count(*) FROM t)AS x",
            "SELECT EXISTS (SELECT 1 FROM t)AS x",
        ),
    ] {
        assert_rewritten(sql, expected);
    }
}

#[test]
fn count_of_a_not_null_expression() {
    for (sql, expected) in [
        (
            "SELECT (SELECT count(a) FROM t) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t) AS x",
        ),
        (
            "SELECT (SELECT count(t.id) FROM t) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t) AS x",
        ),
        (
            "SELECT (SELECT count(1) FROM t) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t) AS x",
        ),
        (
            "SELECT (SELECT count(a) FROM t) = 0 AS x",
            "SELECT (NOT EXISTS (SELECT 1 FROM t)) AS x",
        ),
        // Distinct values: nonzero exactly when `count(a)` is.
        (
            "SELECT (SELECT count(DISTINCT a) FROM t) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t) AS x",
        ),
        (
            "SELECT (SELECT count(x.a) FROM t x JOIN u ON u.t_id = x.id) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t x JOIN u ON u.t_id = x.id) AS x",
        ),
        (
            "SELECT (SELECT pg_catalog.count(*) FROM t) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t) AS x",
        ),
    ] {
        assert_rewritten(sql, expected);
    }
}

#[test]
fn correlated_in_where_and_dml() {
    for (sql, expected) in [
        (
            "SELECT id FROM t WHERE (SELECT count(*) FROM u WHERE u.t_id = t.id) = 0",
            "SELECT id FROM t WHERE (NOT EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id))",
        ),
        (
            "SELECT id FROM t WHERE a = 1 AND(SELECT count(*) FROM u WHERE u.t_id = t.id)>0",
            "SELECT id FROM t WHERE a = 1 AND EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
        ),
        (
            "SELECT id FROM t WHERE NOT (SELECT count(*) FROM u WHERE u.t_id = t.id) > 0",
            "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
        ),
        (
            "SELECT (SELECT count(*) FROM t WHERE a = $1) > 0 AS x",
            "SELECT EXISTS (SELECT 1 FROM t WHERE a = $1) AS x",
        ),
        (
            "UPDATE u SET c = 1 WHERE (SELECT count(*) FROM t WHERE t.id = u.t_id) > 0",
            "UPDATE u SET c = 1 WHERE EXISTS (SELECT 1 FROM t WHERE t.id = u.t_id)",
        ),
        (
            "DELETE FROM u WHERE (SELECT count(*) FROM t WHERE t.id = u.t_id) = 0 \
             RETURNING (SELECT count(*) FROM t) > 0",
            "DELETE FROM u WHERE (NOT EXISTS (SELECT 1 FROM t WHERE t.id = u.t_id)) \
             RETURNING EXISTS (SELECT 1 FROM t) AS \"?column?\"",
        ),
    ] {
        assert_rewritten(sql, expected);
    }
}

#[test]
fn unnamed_output_columns_keep_their_name() {
    for (sql, expected) in [
        (
            "SELECT (SELECT count(*) FROM t) > 0",
            "SELECT EXISTS (SELECT 1 FROM t) AS \"?column?\"",
        ),
        (
            "SELECT 0 < (SELECT count(*) FROM t), 1 AS one",
            "SELECT EXISTS (SELECT 1 FROM t) AS \"?column?\", 1 AS one",
        ),
        (
            "SELECT ((SELECT count(*) FROM t) > 0)",
            "SELECT (EXISTS (SELECT 1 FROM t)) AS \"?column?\"",
        ),
        (
            "SELECT (0 < ((SELECT count(*) FROM t))) FROM u",
            "SELECT (EXISTS ((SELECT 1 FROM t))) AS \"?column?\" FROM u",
        ),
        // In a FROM subquery the name reaches the row's field names.
        (
            "SELECT row_to_json(s) AS j FROM (SELECT (SELECT count(*) FROM t) > 0) s",
            "SELECT row_to_json(s) AS j FROM (SELECT EXISTS (SELECT 1 FROM t) AS \"?column?\") s",
        ),
    ] {
        assert_rewritten(sql, expected);
    }
}

#[test]
fn top_level_count() {
    for (sql, expected) in [
        (
            "SELECT count(*) > 0 FROM t WHERE b > 3",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS \"?column?\"",
        ),
        (
            "SELECT count(*) = 0 AS none FROM t WHERE b > 3",
            "SELECT (NOT EXISTS (SELECT 1 FROM t WHERE b > 3)) AS \"none\"",
        ),
        (
            "SELECT 1 <= count(a) AS has FROM t",
            "SELECT EXISTS (SELECT 1 FROM t) AS has",
        ),
        (
            "SELECT count(*) > 0 FROM t WHERE b > 3 -- trailing\n",
            "SELECT EXISTS (SELECT 1 FROM t WHERE b > 3) AS \"?column?\" -- trailing\n",
        ),
        (
            "SELECT count(*) <> 0 AS \"Has \"\"it\"\"\" FROM t JOIN u ON u.t_id = t.id WHERE u.c = $1",
            "SELECT EXISTS (SELECT 1 FROM t JOIN u ON u.t_id = t.id WHERE u.c = $1) \
             AS \"Has \"\"it\"\"\"",
        ),
    ] {
        assert_rewritten(sql, expected);
    }
}

/// The queries issue #172 measured.
#[test]
fn issue_queries() {
    let db = setup_with(
        "CREATE TABLE customers (id bigint PRIMARY KEY, name text NOT NULL, region text NOT NULL);
         CREATE TABLE orders (id bigint PRIMARY KEY,
                              customer_id bigint NOT NULL REFERENCES customers,
                              total numeric NOT NULL, created_at timestamptz NOT NULL);
         CREATE INDEX ON orders (customer_id);",
    );
    // `numeric > integer` casts the integer side first: not proved
    // error-free, so the issue's own spelling is kept; a numeric literal
    // compares without a cast.
    assert_kept_in(
        &db,
        "SELECT (SELECT count(*) FROM orders WHERE total > 10) > 0",
    );
    assert_rewritten_in(
        &db,
        "SELECT (SELECT count(*) FROM orders WHERE total > 10.0) > 0",
        "SELECT EXISTS (SELECT 1 FROM orders WHERE total > 10.0) AS \"?column?\"",
    );
    assert_rewritten_in(
        &db,
        "SELECT count(*) > 0 FROM orders WHERE customer_id = 42",
        "SELECT EXISTS (SELECT 1 FROM orders WHERE customer_id = 42) AS \"?column?\"",
    );
}

#[test]
fn every_count_of_a_query_is_rewritten() {
    assert_rewritten(
        "SELECT id FROM t WHERE (SELECT count(*) FROM u WHERE u.t_id = t.id) > 0 \
         AND (SELECT count(*) FROM u WHERE u.c = t.a) = 0",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id) \
         AND (NOT EXISTS (SELECT 1 FROM u WHERE u.c = t.a))",
    );
}

// ── Negative: each condition failing ────────────────────────────────────────

#[test]
fn comparisons_that_are_not_any_row() {
    for sql in [
        "SELECT (SELECT count(*) FROM t) > 1 AS x",
        "SELECT (SELECT count(*) FROM t) >= 0 AS x",
        "SELECT (SELECT count(*) FROM t) < 1 AS x",
        "SELECT (SELECT count(*) FROM t) = 1 AS x",
        "SELECT 0 > (SELECT count(*) FROM t) AS x",
        "SELECT 1 < (SELECT count(*) FROM t) AS x",
    ] {
        assert_kept(sql);
    }
}

#[test]
fn right_side_must_be_a_literal_zero_or_one() {
    for sql in [
        "SELECT (SELECT count(*) FROM t) > 0::bigint AS x",
        "SELECT (SELECT count(*) FROM t) > 00 AS x",
        "SELECT (SELECT count(*) FROM t) > 0.0 AS x",
        "SELECT (SELECT count(*) FROM t) > '0' AS x",
        "SELECT (SELECT count(*) FROM t) > $1 AS x",
        "SELECT (SELECT count(*) FROM t) > (SELECT 0) AS x",
    ] {
        assert_kept(sql);
    }
}

#[test]
fn operator_must_be_the_builtin_one() {
    // A qualified operator is left alone, even the built-in one.
    assert_kept("SELECT (SELECT count(*) FROM t) OPERATOR(pg_catalog.>) 0 AS x");
}

#[test]
fn count_must_be_the_builtin_aggregate() {
    // A user `count(int)` the call resolves to instead.
    assert_kept_in(
        &setup_with(
            "CREATE AGGREGATE public.count(int) (SFUNC = int4pl, STYPE = int, INITCOND = '5')",
        ),
        "SELECT (SELECT count(a) FROM t) > 0 AS x",
    );
    // A user `count(*)` next to the built-in one.
    assert_kept_in(
        &setup_with(
            "CREATE AGGREGATE public.count(*) (SFUNC = int8inc, STYPE = bigint, INITCOND = '5')",
        ),
        "SELECT (SELECT count(*) FROM t) > 0 AS x",
    );
    // More than the count in the target list.
    assert_kept("SELECT (SELECT count(*) + 0 FROM t) > 0 AS x");
    assert_kept("SELECT (SELECT sum(a) FROM t) > 0 AS x");
}

#[test]
fn group_by_having_window_distinct_limit_offset() {
    for sql in [
        "SELECT (SELECT count(*) FROM t GROUP BY a LIMIT 1) > 0 AS x",
        "SELECT (SELECT count(*) FROM t GROUP BY ()) > 0 AS x",
        "SELECT (SELECT count(*) FROM t HAVING count(*) > 5) > 0 AS x",
        "SELECT (SELECT count(*) OVER () FROM t LIMIT 1) > 0 AS x",
        "SELECT (SELECT count(*) FROM t WINDOW w AS ()) > 0 AS x",
        "SELECT (SELECT DISTINCT count(*) FROM t) > 0 AS x",
        "SELECT (SELECT count(*) FROM t LIMIT 0) = 0 AS x",
        "SELECT (SELECT count(*) FROM t OFFSET 1) = 0 AS x",
        "SELECT (SELECT count(*) FROM t ORDER BY 1) > 0 AS x",
        "SELECT (WITH w AS (SELECT 1) SELECT count(*) FROM t) > 0 AS x",
        "SELECT (SELECT count(*) FROM t UNION ALL SELECT 1 LIMIT 1) > 0 AS x",
    ] {
        assert_kept(sql);
    }
}

#[test]
fn set_returning_function_in_the_target_list() {
    assert_kept("SELECT (SELECT count(*) + 0 * generate_series(1, 2) FROM t LIMIT 1) > 0 AS x");
}

#[test]
fn filter_and_ordered_counts() {
    // FILTER would move its condition into the WHERE, where it may run on
    // rows the original never evaluates it on.
    assert_kept("SELECT (SELECT count(*) FILTER (WHERE b > 1) FROM t) > 0 AS x");
    assert_kept("SELECT (SELECT count(a ORDER BY b) FROM t) > 0 AS x");
    assert_kept("SELECT count(*) FILTER (WHERE b > 1) > 0 FROM t");
}

#[test]
fn counted_expression_must_be_strictly_not_null() {
    for sql in [
        "SELECT (SELECT count(b) FROM t) > 0 AS x",
        "SELECT (SELECT count(DISTINCT b) FROM t) > 0 AS x",
        "SELECT (SELECT count(NULL::int) FROM t) > 0 AS x",
        "SELECT (SELECT count(u.t_id) FROM t LEFT JOIN u ON u.t_id = t.id) > 0 AS x",
        "SELECT (SELECT count(u.c) FROM t JOIN u ON u.t_id = t.id) > 0 AS x",
        // An outer column makes the aggregate the outer query's.
        "SELECT (SELECT count(t.a) FROM u) > 0 AS x FROM t",
        "SELECT count(b) > 0 FROM t",
    ] {
        assert_kept(sql);
    }
    // A NOT NULL constraint not validated yet.
    assert_kept_in(
        &setup_with("ALTER TABLE t ADD CONSTRAINT b_nn NOT NULL b NOT VALID"),
        "SELECT (SELECT count(b) FROM t) > 0 AS x",
    );
}

/// The planner pulls `EXISTS` up into a semi- or anti-join and pushes the
/// subquery's own conditions down to its scan: `1 / s.z > 0` then runs on
/// every row of `s`, where the correlated count only ran it on the rows
/// with `s.k = o.k` — with a `(5000, 0)` row in `s`, the written query
/// returns every `o` and the rewritten one fails with `division by zero`.
#[test]
fn error_capable_subquery_pulled_up_is_kept() {
    let db = setup_with(
        "CREATE TABLE o (id int PRIMARY KEY, k int NOT NULL);
         CREATE TABLE s (k int NOT NULL, z int NOT NULL);",
    );
    assert_kept_in(
        &db,
        "SELECT id FROM o WHERE (SELECT count(*) FROM s WHERE s.k = o.k AND 1 / s.z > 0) > 0",
    );
    assert_kept_in(
        &db,
        "SELECT id FROM o WHERE (SELECT count(*) FROM s WHERE s.k = o.k AND 1 / s.z > 0) = 0",
    );
    // The same subquery with an error-free condition is rewritten.
    assert_rewritten_in(
        &db,
        "SELECT id FROM o WHERE (SELECT count(*) FROM s WHERE s.k = o.k AND s.z > 0) > 0",
        "SELECT id FROM o WHERE EXISTS (SELECT 1 FROM s WHERE s.k = o.k AND s.z > 0)",
    );
}

/// Anything in the counted query that may fail on some row keeps it as
/// written.
#[test]
fn error_capable_counted_query_is_kept() {
    for sql in [
        // Arithmetic, a cast, a function call, a pattern match.
        "SELECT id FROM t WHERE (SELECT count(*) FROM u WHERE u.t_id = t.id AND 10 / u.c > 1) > 0",
        "SELECT id FROM t WHERE (SELECT count(*) FROM u WHERE u.t_id = t.id AND u.c::smallint = 1) > 0",
        "SELECT id FROM t WHERE (SELECT count(*) FROM u WHERE u.t_id = t.id AND abs(u.c) = 1) = 0",
        "SELECT (SELECT count(*) FROM u WHERE u.c::text LIKE '1%') > 0 AS x",
        // In a join condition.
        "SELECT (SELECT count(*) FROM u JOIN t ON t.id = u.t_id AND 1 / t.a > 0) > 0 AS x",
        // The top-level form too: `EXISTS` gets a plan of its own.
        "SELECT count(*) > 0 FROM u WHERE 10 / c > 1",
        "SELECT count(*) > 0 FROM u JOIN t ON t.id = u.t_id WHERE t.a / u.c > 1",
    ] {
        assert_kept(sql);
    }
}

#[test]
fn volatile_functions() {
    assert_kept("SELECT (SELECT count(*) FROM t WHERE random() > 0.5) > 0 AS x");
    assert_kept("SELECT count(*) > 0 FROM t WHERE random() > 0.5");
    assert_kept_in(
        &setup_with("CREATE FUNCTION f() RETURNS SETOF int LANGUAGE sql VOLATILE AS 'SELECT 1'"),
        "SELECT (SELECT count(*) FROM f()) > 0 AS x",
    );
}

#[test]
fn relations_that_are_not_plain_tables() {
    // A CTE (it may hold a volatile function `EXISTS` would call less).
    assert_kept("WITH t AS (SELECT 1 AS a) SELECT (SELECT count(*) FROM t) > 0 AS x");
    // A view may hide anything.
    assert_kept_in(
        &setup_with("CREATE VIEW v AS SELECT * FROM t"),
        "SELECT (SELECT count(*) FROM v) > 0 AS x",
    );
    // Row security.
    assert_kept_in(
        &setup_with("ALTER TABLE t ENABLE ROW LEVEL SECURITY"),
        "SELECT (SELECT count(*) FROM t) > 0 AS x",
    );
}

/// A comparison another clause must match textually: rewriting one of
/// the two copies alone would make PG reject the query. The driver applies
/// a rule's candidates together, so both copies change as one — the same
/// grouping and ordering, by the same boolean.
#[test]
fn copies_another_clause_must_match_change_together() {
    const C: &str = "(SELECT count(*) FROM u WHERE u.c = t.a) > 0";
    const E: &str = "EXISTS (SELECT 1 FROM u WHERE u.c = t.a)";
    assert_rewritten(
        &format!("SELECT {C} AS x FROM t GROUP BY {C}"),
        &format!("SELECT {E} AS x FROM t GROUP BY {E}"),
    );
    assert_rewritten(
        &format!("SELECT DISTINCT {C} AS x FROM t ORDER BY {C}"),
        &format!("SELECT DISTINCT {E} AS x FROM t ORDER BY {E}"),
    );
}

/// PG names an unnamed column after its expression (FigureColname),
/// looking through casts, CASE's ELSE, subscripts and scalar subqueries: a
/// comparison anywhere in one, rewritten, would rename it (`(x > 0)::int`
/// is `int4`, `(EXISTS …)::int` is `exists`). The column keeps its name —
/// visible here in `row_to_json`'s keys, which the pg_sanity oracle
/// compares between the written and the sent query on real rows.
#[test]
fn wrappers_keep_the_column_name() {
    const C: &str = "(SELECT count(*) FROM u WHERE u.t_id = t.id)";
    const E: &str = "EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)";
    for (written, sent) in [
        // Cast: named after the type.
        (format!("({C} > 0)::int"), format!("({E})::int AS int4")),
        (
            format!("CAST({C} > 0 AS int)"),
            format!("CAST({E} AS int) AS int4"),
        ),
        // CASE: after its ELSE branch, else `case`.
        (
            format!("CASE WHEN a > 0 THEN 1 ELSE ({C} > 0)::int END"),
            format!("CASE WHEN a > 0 THEN 1 ELSE ({E})::int END AS \"case\""),
        ),
        (
            format!("CASE WHEN {C} > 0 THEN a END"),
            format!("CASE WHEN {E} THEN a END AS \"case\""),
        ),
        (
            format!("CASE WHEN a > 0 THEN false ELSE {C} = 0 END"),
            format!("CASE WHEN a > 0 THEN false ELSE (NOT {E}) END AS \"case\""),
        ),
        // COALESCE, GREATEST, LEAST, NULLIF.
        (
            format!("COALESCE({C} > 0, false)"),
            format!("COALESCE({E}, false) AS \"coalesce\""),
        ),
        (
            format!("GREATEST({C} > 0, false)"),
            format!("GREATEST({E}, false) AS \"greatest\""),
        ),
        (
            format!("LEAST({C} > 0, true)"),
            format!("LEAST({E}, true) AS \"least\""),
        ),
        (
            format!("NULLIF({C} > 0, false)"),
            format!("NULLIF({E}, false) AS \"nullif\""),
        ),
        // Row constructors.
        (
            format!("ROW({C} > 0, 1)"),
            format!("ROW({E}, 1) AS \"row\""),
        ),
        (format!("({C} > 0, 1)"), format!("({E}, 1) AS \"row\"")),
        // Subscript and field selection.
        (
            format!("(ARRAY[{C} > 0])[1]"),
            format!("(ARRAY[{E}])[1] AS \"array\""),
        ),
        (
            format!("(ROW({C} > 0, 1)::pair).f1"),
            format!("(ROW({E}, 1)::pair).f1 AS f1"),
        ),
        // IS [NOT] NULL, IS DISTINCT FROM, NOT, AND: no name.
        (
            format!("({C} > 0) IS NOT NULL"),
            format!("({E}) IS NOT NULL AS \"?column?\""),
        ),
        (
            format!("{C} > 0 IS NULL"),
            format!("{E} IS NULL AS \"?column?\""),
        ),
        (
            format!("({C} > 0) IS DISTINCT FROM a > 1"),
            format!("({E}) IS DISTINCT FROM a > 1 AS \"?column?\""),
        ),
        (format!("NOT {C} > 0"), format!("NOT {E} AS \"?column?\"")),
        (
            format!("a > 1 AND {C} > 0"),
            format!("a > 1 AND {E} AS \"?column?\""),
        ),
        // A scalar subquery: after its own column, at both levels.
        (
            format!("(SELECT ({C} > 0)::int)"),
            format!("(SELECT ({E})::int AS int4) AS int4"),
        ),
    ] {
        assert_rewritten(
            &format!("SELECT row_to_json(s) AS j FROM (SELECT {written} FROM t) s"),
            &format!("SELECT row_to_json(s) AS j FROM (SELECT {sent} FROM t) s"),
        );
    }
}

/// An output column over a `*` is named after what the `*` expands to,
/// which this level alone doesn't tell: such a column isn't rewritten.
#[test]
fn column_named_through_a_star_is_kept() {
    assert_kept(
        "SELECT row_to_json(s) AS j FROM \
         (SELECT (SELECT * FROM (SELECT ((SELECT count(*) FROM u) > 0)::int) x)) s",
    );
}

#[test]
fn nested_column_names_in_records() {
    for (written, sent) in [
        (
            "SELECT to_jsonb(s) AS j FROM (SELECT ((SELECT count(*) FROM u) > 0)::int) s",
            "SELECT to_jsonb(s) AS j FROM (SELECT (EXISTS (SELECT 1 FROM u))::int AS int4) s",
        ),
        (
            "SELECT s FROM (SELECT ((SELECT count(*) FROM u) > 0)::int, 1 AS one) s",
            "SELECT s FROM (SELECT (EXISTS (SELECT 1 FROM u))::int AS int4, 1 AS one) s",
        ),
        (
            "WITH s AS (SELECT ((SELECT count(*) FROM u) > 0)::int) SELECT row_to_json(s) AS j FROM s",
            "WITH s AS (SELECT (EXISTS (SELECT 1 FROM u))::int AS int4) SELECT row_to_json(s) AS j FROM s",
        ),
        (
            "DELETE FROM u WHERE c = 0 RETURNING ((SELECT count(*) FROM t) > 0)::int, c",
            "DELETE FROM u WHERE c = 0 RETURNING (EXISTS (SELECT 1 FROM t))::int AS int4, c",
        ),
    ] {
        assert_rewritten(written, sent);
    }
}

#[test]
fn other_sublinks() {
    assert_kept("SELECT EXISTS (SELECT count(*) FROM t) AS x");
    assert_kept("SELECT ARRAY(SELECT count(*) FROM t) AS x");
}

#[test]
fn top_level_conditions() {
    for sql in [
        // Not the only column.
        "SELECT count(*) > 0, 1 AS one FROM t",
        "SELECT count(*) > 0 FROM t GROUP BY a",
        "SELECT count(*) > 0 FROM t HAVING count(*) > 1",
        "SELECT count(*) > 0 FROM t LIMIT 0",
        "SELECT count(*) > 0 FROM t OFFSET 1",
        "SELECT count(*) > 0 FROM t ORDER BY 1",
        "SELECT DISTINCT count(*) > 0 FROM t",
        "SELECT count(*) OVER () > 0 FROM t",
        "SELECT count(*) > 1 FROM t",
        // No FROM: nothing to stop early on.
        "SELECT count(*) > 0",
        "(SELECT count(*) > 0 FROM t)",
    ] {
        assert_kept(sql);
    }
}

#[test]
fn opt_out() {
    let mut db = setup();
    db.set_simplify_options(SimplifyOptions {
        enabled: true,
        disabled: [Rule::CountExists].into(),
    });
    assert_kept_in(&db, "SELECT (SELECT count(*) FROM t) > 0 AS x");
    db.set_simplify_options(SimplifyOptions {
        enabled: false,
        disabled: Default::default(),
    });
    assert_kept_in(&db, "SELECT count(*) > 0 FROM t");
}

/// `NOT` binds looser than `IS [NOT] NULL`, `ISNULL`, `NOTNULL`, `IS [NOT]
/// UNKNOWN` and `IS [NOT] DISTINCT FROM`: the `NOT EXISTS` replacing
/// `count = 0` is parenthesized, so a test after it still applies to the
/// whole comparison.
#[test]
fn not_exists_keeps_a_trailing_test_on_the_comparison() {
    const C: &str = "(SELECT count(*) FROM u WHERE u.t_id = t.id)";
    const E: &str = "(NOT EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id))";
    for test in [
        "IS NOT NULL",
        "ISNULL",
        "NOTNULL",
        "IS NOT UNKNOWN",
        "IS NOT DISTINCT FROM NULL::bool",
    ] {
        assert_rewritten(
            &format!("SELECT id FROM t WHERE {C} = 0 {test}"),
            &format!("SELECT id FROM t WHERE {E} {test}"),
        );
        assert_rewritten(
            &format!("SELECT id FROM t WHERE 0 = {C} {test}"),
            &format!("SELECT id FROM t WHERE {E} {test}"),
        );
    }
}
