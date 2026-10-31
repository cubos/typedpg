//! WHERE clause: AND/OR, IN, LIKE, IS NULL, NOT, comparison operators.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id         BIGINT PRIMARY KEY,
            name       TEXT NOT NULL,
            email      TEXT NOT NULL,
            age        INT,
            created_at TIMESTAMPTZ NOT NULL
        );",
    )
    .unwrap();
    db
}

// ── Non-boolean expression in WHERE is a type mismatch ───────────────────────

#[test]
fn where_int4_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE 42"),
        AnalyzeError::DatatypeMismatch(_),
        "argument of WHERE must be type boolean, not type integer",
    );
}

#[test]
fn where_text_column_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type text\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name\n",
            "  ·                            ──┬─\n",
            "  ·                              ╰─ this is text, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_int8_column_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT name FROM users WHERE id"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type bigint\n",
            "  ╭────\n",
            "1 │ SELECT name FROM users WHERE id\n",
            "  ·                              ─┬\n",
            "  ·                               ╰─ this is bigint, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_timestamptz_column_not_boolean() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE created_at"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of WHERE must be type boolean, not type timestamp with time zone\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE created_at\n",
            "  ·                            ─────┬────\n",
            "  ·                                 ╰─ this is timestamp with time zone, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

// ── IS NULL / IS NOT NULL ────────────────────────────────────────────────────

#[test]
fn where_is_not_null() {
    let db = setup();
    // The rows past the WHERE have a non-NULL age.
    let s = db
        .analyze("SELECT id, age FROM users WHERE age IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("age", int4())]);
}

#[test]
fn where_is_null() {
    let db = setup();
    let s = db
        .analyze("SELECT id, name FROM users WHERE age IS NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

#[test]
fn where_is_null_unknown_column_rejected() {
    // `IS NULL` / `IS NOT NULL` / `IS [NOT] TRUE|FALSE` must still resolve
    // their argument against the scope — historically the analyzer treated
    // these nodes as opaque booleans and silently accepted typo'd columns.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE ghost IS NULL"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE ghost IS NULL\n",
            "  ·                            ──┬──\n",
            "  ·                              ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_is_not_false_unknown_column_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE ghost IS NOT FALSE"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE ghost IS NOT FALSE\n",
            "  ·                            ──┬──\n",
            "  ·                              ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn where_is_not_false_unknown_column_in_subquery_rejected() {
    // The original report: a typo'd column inside a scalar subquery's
    // `IS NOT FALSE` was silently accepted because the BooleanTest arm
    // never descended into its arg.
    let db = setup();
    assert_analyze_err!(
        db.analyze(
            "SELECT id FROM users u \
             WHERE EXISTS (SELECT 1 FROM users x WHERE x.ghost IS NOT FALSE)",
        ),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column x.ghost does not exist\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM users x WHERE x.ghost IS NOT FALSE)\n",
            "  ·                                                                  ───┬───\n",
            "  ·                                                                     ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

// ── AND / OR / NOT ───────────────────────────────────────────────────────────

#[test]
fn where_and() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 AND email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn where_or() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name = $p1 OR email = $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text()), p(text())]);
}

#[test]
fn where_not() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE NOT (age > $p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

// ── IN / LIKE / comparison ───────────────────────────────────────────────────

#[test]
fn where_in_list() {
    let db = setup();
    // Literal form: PG promotes list elements to the column's type (int4), but
    // since they are constants nothing surfaces in the param list.
    let s = db
        .analyze("SELECT id FROM users WHERE age IN (1, 2, 3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn where_in_list_with_params() {
    let db = setup();
    // Param form: each `$pN` inside the IN list must be inferred with the
    // left-hand column's type as the goal, so all three params surface as int4.
    let s = db
        .analyze("SELECT id FROM users WHERE age IN ($p1, $p2, $p3)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(int4())]);
}

#[test]
fn where_like() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name LIKE $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn where_comparison_operators() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age >= $p1 AND age <= $p2 AND name <> $p3")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4()), p(text())]);
}

// ── BETWEEN ──────────────────────────────────────────────────────────────────

#[test]
fn where_between_literals() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN 18 AND 65")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn where_between_params() {
    let db = setup();
    // Both bounds are walked with the lhs column type as the inference goal,
    // so `$p1`/`$p2` resolve to `int4` (the type of `age`).
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN $p1 AND $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn where_not_between_params() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE age NOT BETWEEN $p1 AND $p2")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn where_between_symmetric_with_params() {
    let db = setup();
    // BETWEEN SYMMETRIC should route through the same walker as plain BETWEEN.
    let s = db
        .analyze("SELECT id FROM users WHERE age BETWEEN SYMMETRIC $p1 AND $p2")
        .unwrap();
    assert_params(&s, vec![p(int4()), p(int4())]);
}

// ── IS [NOT] DISTINCT FROM ──────────────────────────────────────────────────

#[test]
fn where_is_distinct_from_literal() {
    let db = setup();
    // PG defines `IS DISTINCT FROM` to be NULL-aware and to always produce
    // a definite boolean, so `d` is NOT NULL even though `age` is nullable.
    let s = db
        .analyze("SELECT id, age IS DISTINCT FROM 10 AS d FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("d", bool_ty())]);
}

#[test]
fn where_is_not_distinct_from_null() {
    let db = setup();
    // `col IS NOT DISTINCT FROM NULL` is effectively `col IS NULL` — still
    // always-definite, so the result column is NOT NULL.
    let s = db
        .analyze("SELECT id, age IS NOT DISTINCT FROM NULL AS d FROM users")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("d", bool_ty())]);
}

#[test]
fn where_is_distinct_from_with_param() {
    let db = setup();
    // The rhs param is walked and inferred from the lhs column type.
    // Matches the convention used by other WHERE-clause param tests: params
    // default to NOT NULL unless the caller annotates them with `$p1?`.
    let s = db
        .analyze("SELECT id FROM users WHERE age IS DISTINCT FROM $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(int4())]);
}

// ── POSIX regex + SIMILAR TO ─────────────────────────────────────────────────

#[test]
fn where_regex_match() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name ~ '^foo'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn where_regex_imatch_with_param() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name ~* $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

#[test]
fn where_regex_not_match() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name !~ 'bar'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn where_similar_to() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE name SIMILAR TO 'ab%'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

// ── Cross-type operators rejected ───────────────────────────────────────────

#[test]
fn text_eq_int_rejected() {
    let db = setup();
    // PG: `operator does not exist: text = bigint`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = id"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = bigint\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name = id\n",
            "  ·                                 ┬\n",
            "  ·                                 ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

#[test]
fn any_operator_mismatch_points_at_the_operator() {
    // PG positions it at the operator, like a plain comparison.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = ANY(ARRAY[1, 2])"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name = ANY(ARRAY[1, 2])\n",
            "  ·                                 ┬\n",
            "  ·                                 ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

#[test]
fn join_using_operator_mismatch_gets_the_hint() {
    // PG gives this one no position; the hint and the cast still help.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT 1 FROM (SELECT id FROM users) a JOIN (SELECT name AS id FROM users) b USING (id)"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: bigint = text\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `bigint = bigint` exists: cast the right operand to bigint (`expr::bigint`)\n",
        ),
    );
}

#[test]
fn text_eq_int_literal_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE name = 42"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE name = 42\n",
            "  ·                                 ┬\n",
            "  ·                                 ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

#[test]
fn timestamptz_lt_int_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE created_at < 42"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: timestamp with time zone < integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE created_at < 42\n",
            "  ·                                       ┬\n",
            "  ·                                       ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
        ),
    );
}

#[test]
fn int_like_text_rejected() {
    let db = setup();
    // PG: `operator does not exist: bigint ~~ text`. Catches typos where a
    // developer writes `id LIKE 'foo'` instead of `id::text LIKE 'foo'`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE id LIKE '%1%'"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: bigint ~~ unknown\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE id LIKE '%1%'\n",
            "  ·                               ─┬\n",
            "  ·                                ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
        ),
    );
}

#[test]
fn same_param_with_conflicting_types_rejected() {
    let db = setup();
    // First use (`age = $p1`) pins `$p1` to int4. Second use (`name = $p1`)
    // would then be `text = int4`, which has no operator in PG — the
    // cross-type check above catches this uniformly.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE age = $p1 AND name = $p1"),
        AnalyzeError::UndefinedOperator(_),
        concat!(
            "operator does not exist: text = integer\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE age = $p1 AND name = $p1\n",
            "  ·                                               ┬\n",
            "  ·                                               ╰─ operator does not exist\n",
            "  ╰────\n",
            "  help: No operator matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: `text = text` exists: cast the right operand to text (`expr::text`)\n",
        ),
    );
}

// ── Stress ───────────────────────────────────────────────────────────────────

#[test]
fn stress_complex_where_params() {
    let db = setup();
    let sql = "SELECT id FROM users \
               WHERE (name = $p1 OR email = $p2) AND age > $p3";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("id", int8())]);
    assert_params(&info, vec![p(text()), p(text()), p(int4())]);
}

// ── Error ordering: resolution before placement before boolean coercion ─────

#[test]
fn where_unresolvable_aggregate_reports_resolution_error() {
    // PG transforms bottom-up: `min(text, text)` has no overload, so the
    // `function … does not exist` error fires before the aggregate-placement
    // rule gets a chance.
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users WHERE min(name, name)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("function min(text, text) does not exist"),
        "got: {err}"
    );
}

#[test]
fn where_valid_aggregate_reports_placement_error() {
    // A resolvable aggregate in WHERE keeps the placement error — and it
    // outranks the boolean-coercion complaint (`min(id)` is bigint).
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users WHERE min(id)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in WHERE"),
        "got: {err}"
    );
}

#[test]
fn join_on_non_boolean_uses_pg_wording() {
    let db = setup();
    let err = db
        .analyze("SELECT u.id FROM users u JOIN users v ON u.age + v.age")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("argument of JOIN/ON must be type boolean, not type integer"),
        "got: {err}"
    );
}

#[test]
fn aggregate_in_limit_and_offset_rejected() {
    // PG forbids aggregates/window calls in LIMIT/OFFSET — after the
    // expression resolves, before the bigint-coercion complaint.
    let db = setup();
    let err = db
        .analyze("SELECT id FROM users LIMIT count(*)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in LIMIT"),
        "got: {err}"
    );
    let err = db
        .analyze("SELECT id FROM users OFFSET sum(age)")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("aggregate functions are not allowed in OFFSET"),
        "got: {err}"
    );
    let err = db
        .analyze("SELECT id FROM users LIMIT row_number() OVER ()")
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("window functions are not allowed in LIMIT"),
        "got: {err}"
    );
}

/// PostgreSQL compiles a constant regex pattern while *planning*: a match of
/// two constants is folded (eval_const_expressions) and a WHERE / JOIN ON
/// qual `expr ~ 'pattern'` over one relation has its selectivity estimated
/// from the pattern (patternsel). An invalid pattern there fails every
/// execution, while elsewhere it only fails per evaluated row, and DDL
/// that merely stores the expression is accepted.
#[test]
fn constant_regex_patterns_compiled_by_the_planner_are_validated() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE rt (id INT PRIMARY KEY, s TEXT NOT NULL, n NAME);
         CREATE TABLE rt2 (id INT PRIMARY KEY, s TEXT);
         CREATE VIEW rv AS SELECT id FROM rt WHERE s ~ '(';",
    )
    .unwrap();
    for sql in [
        "SELECT 'a' ~ '(' AS m",
        "SELECT 'a' ~* ('(' || '') AS m",
        "SELECT 'a' SIMILAR TO '(' AS m",
        "SELECT id FROM rt WHERE s ~ '('",
        "SELECT id FROM rt WHERE NOT (s !~ '[z-a]') OR id = 1",
        "SELECT id FROM rt WHERE lower(s) ~* '('",
        "SELECT id FROM rt WHERE n ~ '('",
        "SELECT id FROM rt WHERE s SIMILAR TO '('",
        "SELECT rt.id FROM rt JOIN rt2 ON rt2.s ~ '(' AND rt2.id = rt.id",
        "DELETE FROM rt WHERE s ~ '('",
        // Reached by folding: an AND / OR argument before the deciding
        // one, a COALESCE after a NULL, a pattern folded from constants.
        "SELECT 'a' ~ '(' OR true AS m",
        "SELECT coalesce(NULL, 'a' ~ '(') AS m",
        "SELECT id FROM rt WHERE id = 1 AND s ~ '(' AND id = 1",
        "SELECT 1 AS m WHERE coalesce('a', 'b') ~ '('",
        "SELECT id FROM rt WHERE s ~ upper('(')",
        // The view's WHERE joins the reading query's once pulled up.
        "SELECT id FROM rv",
        // Folded before relation exclusion: in a level the planner may
        // prove empty, a sublink's and a pulled-up output's constants.
        "SELECT (SELECT 'a' ~ '(') AS m FROM rt WHERE false",
        "SELECT * FROM (SELECT 'a' ~ '(' AS m) s WHERE false",
        // A constant qual of a level with several relations is a gating
        // qual above their join: each is still estimated — those of a
        // pulled-up subquery or view, of a subquery planned on its own.
        "SELECT 1 AS m FROM rt2 JOIN rt ON rt.s ~ '(' WHERE false",
        "SELECT 1 AS m FROM rt2, (SELECT 1 AS q FROM rt WHERE s ~ '(') x WHERE false",
        "SELECT 1 AS m FROM rt2, (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x WHERE false",
        "SELECT 1 AS m FROM rt2, (SELECT 1 AS q FROM rt WHERE s ~ '(' GROUP BY id) x WHERE false",
        "SELECT 1 AS m FROM rt2, rv WHERE false",
        // A pulled-up subquery's column is its table's.
        "SELECT 1 AS m FROM rt2, (SELECT * FROM rt WHERE false) x WHERE x.s ~ '('",
        "SELECT 1 AS m FROM rt2, (SELECT s FROM rt) x WHERE x.s ~ '('",
        // A qual pushed down into a subquery planned on its own proves
        // nothing empty there.
        "SELECT x.id FROM (SELECT id FROM rt WHERE s ~ '(' ORDER BY 1) x WHERE x.id = 1",
        // A constant HAVING moves into WHERE: a gating qual there too.
        "SELECT rt.id FROM rt, rt2 WHERE rt.s ~ '(' GROUP BY rt.id HAVING false",
        // In a qual, NULL is FALSE only as an AND / OR argument: an OR
        // drops it, and a NOT over an AND turns into an OR.
        "SELECT 1 AS m FROM rt2, rt WHERE rt.s ~ '(' OR NULL",
        "SELECT 1 AS m FROM rt2, rt WHERE NOT (rt.s ~ '(' AND NULL)",
        "SELECT 1 AS m FROM (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x \
         WHERE (x.q = 1 AND NULL) OR x.q = 2",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression:"
        );
    }
    for sql in [
        "SELECT s ~ '(' AS m FROM rt",
        "SELECT id FROM rt WHERE '(' ~ s",
        "SELECT rt.id FROM rt JOIN rt2 ON rt.s || rt2.s ~ '('",
        "SELECT id FROM rt WHERE s ~ '(?q)('",
        "SELECT id FROM rt WHERE s SIMILAR TO '\\('",
        // What constant folding drops is never compiled, and a relation
        // the planner proves empty is never estimated.
        "SELECT true OR 'a' ~ '(' AS m",
        "SELECT CASE WHEN false THEN 'a' ~ '(' ELSE true END AS m",
        "SELECT coalesce(true, 'a' ~ '(') AS m",
        "SELECT id FROM rt WHERE false AND s ~ '('",
        "SELECT id FROM rt WHERE s ~ '(' OR true",
        "SELECT id FROM rt WHERE s ~ '(' AND NULL",
        "SELECT id FROM rt WHERE id = 1 AND id = 2 AND s ~ '('",
        "SELECT rt.id FROM rt LEFT JOIN rt2 ON rt2.id = rt.id AND rt2.s ~ '('",
        // A pulled-up view's restriction, in a level proven empty.
        "SELECT id FROM rv WHERE false",
        // A single relation: the constant qual is its restriction, which
        // proves it empty — a pulled-up subquery's, or one planned on its
        // own (never planned then).
        "SELECT 1 AS m FROM (SELECT * FROM rt WHERE s ~ '(') x WHERE false",
        "SELECT 1 AS m FROM (SELECT * FROM rt WHERE false) x WHERE x.s ~ '('",
        "SELECT 1 AS m FROM (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x WHERE false",
        "SELECT 1 AS m FROM (SELECT 1 AS q) y JOIN rt ON rt.s ~ '(' WHERE false",
        // A constant HAVING moves into WHERE, the relation's restriction.
        "SELECT id FROM rt WHERE s ~ '(' GROUP BY id HAVING false",
        "SELECT id FROM rt WHERE s ~ '(' GROUP BY id HAVING 1 = 2",
        // A pulled-up constant: `y.q = 2` is FALSE.
        "SELECT 1 AS m FROM rt2, (SELECT 1 AS q) y, rt WHERE rt.s ~ '(' AND y.q = 2",
        "SELECT 1 AS m FROM (SELECT 1 AS q) y JOIN rt ON rt.s ~ '(' WHERE y.q = 2",
        "SELECT CASE WHEN y.q = 2 THEN 'a' ~ '(' END AS m FROM (SELECT 1 AS q) y",
        // The one AND folds whole; a join removed, or merged with itself.
        "SELECT 1 AS m FROM rt2, rt WHERE rt.s ~ '(' AND rt2.id = 1 AND false",
        "SELECT 1 AS m FROM rt2 LEFT JOIN rt ON rt.s ~ '(' AND rt.id = rt2.id WHERE false",
        "SELECT 1 AS m FROM rt a JOIN rt b ON a.id = b.id AND a.s ~ '(' WHERE false",
        // `canonicalize_qual` makes an AND with a NULL argument FALSE, in
        // WHERE and in each ON clause, at any depth of the AND / OR tree.
        "SELECT 1 AS m FROM rt2, rt WHERE rt.s ~ '(' AND NULL",
        "SELECT 1 AS m FROM rt2, rt WHERE NULL AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2, rt WHERE NULL::bool AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2 JOIN rt ON rt.s ~ '(' AND NULL",
        "SELECT 1 AS m FROM rt2 JOIN rt ON NULL AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2, rt WHERE NULL AND rt.s ~ '(' AND rt2.id = 1",
        "SELECT 1 AS m FROM rt2, rt WHERE (NULL AND rt.s ~ '(')",
        "SELECT 1 AS m FROM rt2, rt WHERE (1 > 2 OR NULL) AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2, rt WHERE NULL = NULL AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2, rt WHERE NULL::int = 1 AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2, rt WHERE NULL::int > 1 AND rt.s ~ '('",
        "SELECT 1 AS m FROM rt2, rt WHERE (rt.s ~ '(' AND NULL) OR rt2.id = 1",
        "SELECT 1 AS m FROM rt2, rt WHERE NOT (rt.s ~ '(' OR NULL)",
        "SELECT 1 AS m FROM rt2, rt WHERE NOT (NOT (rt.s ~ '(' AND NULL))",
        // Such a FALSE qual proves a single relation empty.
        "SELECT 1 AS m FROM (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x \
         WHERE (x.q = 1 AND NULL) OR false",
        "SELECT 1 AS m FROM (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x \
         WHERE NOT (x.q = 1 OR NULL)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_subquery_planned_on_its_own_estimates_its_restrictions() {
    // A security_barrier view is never pulled up: the planner plans it on
    // its own, once it plans the relation it is — with the quals it may
    // push down into it, which may prove it empty.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, x int, s text);
         CREATE VIEW v4 WITH (security_barrier) AS SELECT x FROM t WHERE s ~ '(';
         CREATE VIEW v7 WITH (security_barrier) AS SELECT x FROM t WHERE x = 1 AND s ~ '(';",
    )
    .unwrap();
    for sql in [
        "SELECT 1 AS m FROM v4",
        "SELECT 1 AS m FROM v4 WHERE x = 2",
        "SELECT 1 AS m FROM v7 WHERE x = 1",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "SELECT 1 AS m FROM v4 WHERE false",
        "SELECT 1 AS m FROM v7 WHERE x = 2",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn planner_setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, x int, s text);
         CREATE TABLE u (id int PRIMARY KEY, x int);
         CREATE TABLE rt (id int PRIMARY KEY, s text);
         CREATE TABLE rt2 (id int PRIMARY KEY, s text);",
    )
    .unwrap();
    db
}

#[test]
fn in_and_exists_sublinks_become_joins_before_the_where_clause_is_folded() {
    // `pull_up_sublinks` turns an IN (`= ANY`) sublink that is a top-level
    // AND argument of WHERE, testing a column of the query, into a
    // semi-join before the qual is folded: its subquery joins the query,
    // whose constant FALSE qual is then a gating qual above the join. So
    // does it an [NOT] EXISTS whose WHERE reads the query's columns
    // (`simplify_EXISTS_query` throwing its select list away): its FROM
    // items join the query.
    let db = planner_setup();
    for sql in [
        "SELECT 1 FROM t WHERE x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t WHERE false AND x IN (SELECT id FROM rt WHERE s ~ '(')",
        "SELECT 1 FROM t WHERE t.x IN (SELECT id FROM rt WHERE s ~ '(') AND NULL",
        "SELECT 1 FROM t WHERE x = ANY (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t WHERE x IN (SELECT id FROM rt WHERE s ~ '(' LIMIT 3) AND false",
        "SELECT 1 FROM t WHERE x IN (SELECT id FROM rt WHERE s ~ '(' AND rt.id = t.id) AND false",
        "DELETE FROM t WHERE x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "UPDATE t SET x = 1 WHERE x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(') AND false",
        "SELECT 1 FROM t WHERE false AND EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(')",
        "SELECT 1 FROM t WHERE NOT EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(') \
         AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' LIMIT 1) \
         AND false",
        "DELETE FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(') AND false",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "SELECT 1 FROM t WHERE x IN (SELECT 1 / 0 FROM rt) AND false",
        "SELECT 1 FROM t WHERE x IN (SELECT id FROM rt WHERE 1 / 0 = 1) AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND 1 / 0 = 1) AND false",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
    // Not a semi-join — under an OR, NOT IN, testing no column of the
    // query (or a volatile expression), outside WHERE — the sublink is
    // folded away with the qual; a semi-join's own FALSE qual proves its
    // relation empty.
    for sql in [
        "SELECT 1 FROM t WHERE (x IN (SELECT id FROM rt WHERE s ~ '(') OR true)",
        "SELECT 1 FROM t WHERE x NOT IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t WHERE 1 IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t WHERE x + random()::int IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT x IN (SELECT id FROM rt WHERE s ~ '(') AND false FROM t",
        "SELECT (SELECT 1 FROM u WHERE t.x IN (SELECT id FROM rt WHERE s ~ '(') AND false) FROM t",
        "SELECT 1 FROM t WHERE x IN (SELECT id FROM rt WHERE s ~ '(' AND false) AND false",
        "SELECT 1 FROM t WHERE x IN (SELECT id FROM rt) AND false AND t.s ~ '('",
        // An EXISTS reading no column of the query is an InitPlan; one
        // with an aggregate or a volatile WHERE is kept; what it selects
        // is thrown away.
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt t WHERE t.id = 1 AND s ~ '(') AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT count(*) FROM rt WHERE rt.id = t.id AND s ~ '(') \
         AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' \
         AND random() > 0) AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 / 0 FROM rt WHERE rt.id = t.id) AND false",
        "SELECT 1 FROM t WHERE (EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(') OR true)",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 WHERE t.id = 1) AND false AND t.s ~ '('",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_column_proof_excludes_only_its_own_relation() {
    // `relation_excluded_by_constraints` reads a relation's own
    // restrictions: `t.id IS NULL` (t.id NOT NULL) proves t empty, and
    // every other relation is still estimated.
    let db = planner_setup();
    for sql in [
        "SELECT 1 FROM t, u, rt WHERE rt.s ~ '(' AND t.id IS NULL",
        "SELECT 1 FROM t, rt WHERE rt.s ~ '(' AND t.id IS NULL",
        "SELECT 1 FROM t, rt WHERE rt.s ~ '(' AND t.x = 1 AND t.x = 2",
        "SELECT 1 FROM t, (SELECT * FROM rt WHERE s ~ '(') q WHERE t.id IS NULL",
        "SELECT 1 FROM t, (SELECT * FROM rt WHERE s ~ '(' OFFSET 0) q WHERE t.id IS NULL",
        "SELECT 1 FROM t JOIN rt ON rt.s ~ '(' LEFT JOIN u ON t.id IS NULL",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x ON true \
         WHERE rt2.id IS NULL",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    // The relation proven empty itself — or one that can't be told apart
    // (an unqualified column, a join's alias, a LATERAL subquery's qual).
    for sql in [
        "SELECT 1 FROM t, rt WHERE rt.s ~ '(' AND rt.id IS NULL",
        "SELECT 1 FROM t, rt WHERE rt.s ~ '(' AND rt.id = 1 AND rt.id = 2",
        "SELECT 1 FROM t, (SELECT * FROM rt WHERE id IS NULL) q WHERE q.s ~ '('",
        "SELECT 1 FROM t, (SELECT * FROM rt WHERE s ~ '(' OFFSET 0) q WHERE q.id IS NULL",
        "SELECT 1 FROM t, LATERAL (SELECT * FROM rt WHERE t.id IS NULL) q WHERE t.s ~ '('",
        "SELECT 1 FROM (t JOIN u USING (id)) j, rt WHERE j.id IS NULL AND rt.s ~ '('",
        "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' AND rt.id IS NULL",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_left_join_keeps_a_subquery_it_cannot_prove_distinct() {
    // `join_is_removable` removes a nullable side planned on its own only
    // when `query_supports_distinctness` holds for it (DISTINCT, GROUP BY,
    // an aggregate, a set operation): any other is planned, its
    // restrictions estimated and its read outputs folded.
    let db = planner_setup();
    for sql in [
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x ON true",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(' LIMIT 5) x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(' ORDER BY id) x \
         ON x.id = rt2.id",
        "SELECT 1 FROM rt2 FULL JOIN (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x ON true",
        "SELECT x.q FROM rt2 LEFT JOIN (SELECT 1 AS q FROM rt WHERE s ~ '(' OFFSET 0) x ON true",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    assert_err_prefix!(
        db.analyze(
            "SELECT x.z FROM rt2 LEFT JOIN (SELECT id, 40000::int2 AS z FROM rt OFFSET 0) x \
             ON x.id = rt2.id"
        ),
        AnalyzeError::NumericValueOutOfRange(_),
        "smallint out of range"
    );
    for sql in [
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT DISTINCT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT count(*) AS c FROM rt WHERE s ~ '(') x ON true",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT max(id) AS m FROM rt WHERE s ~ '(') x ON true",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(' GROUP BY id) x \
         ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id, 40000::int2 AS z FROM rt OFFSET 0) x \
         ON x.id = rt2.id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_pulled_up_nullable_side_the_join_removes_is_not_estimated() {
    // A subquery (an inlined CTE, a view) pulled up as the whole nullable
    // side of a LEFT / RIGHT join is its one relation there:
    // `join_is_removable` drops it when the join condition equates its
    // unique key and nothing above reads it, before its restrictions are
    // estimated. An unused WINDOW clause doesn't keep it from being
    // pulled up (`is_simple_subquery` asks `hasWindowFuncs`).
    let mut db = planner_setup();
    db.apply_sql("CREATE VIEW rv AS SELECT id FROM rt WHERE s ~ '(';")
        .unwrap();
    for sql in [
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(' WINDOW w AS (ORDER BY id)) x \
         ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt r WHERE r.s ~ '(') x USING (id)",
        "SELECT 1 FROM (SELECT id FROM rt WHERE s ~ '(') x RIGHT JOIN rt2 ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN rv ON rv.id = rt2.id",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM rt2 LEFT JOIN c ON c.id = rt2.id",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(' WINDOW w AS ()) \
         SELECT 1 FROM rt2 LEFT JOIN c ON c.id = rt2.id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // Kept, joined on no equality, beside another relation, or (constant
    // folding) whatever the join: planned.
    for sql in [
        "SELECT 1 FROM rt2 JOIN (SELECT id FROM rt WHERE s ~ '(' WINDOW w AS (ORDER BY id)) x \
         ON x.id = rt2.id",
        "SELECT 1 FROM t, (SELECT id FROM rt WHERE s ~ '(' WINDOW w AS ()) x WHERE t.id IS NULL",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON true",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id > rt2.id",
        "SELECT 1 FROM rt2 FULL JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id, row_number() OVER w FROM rt WHERE s ~ '(' \
         WINDOW w AS (ORDER BY id)) x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 JOIN rv ON rv.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN rv ON true",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM rt2 LEFT JOIN c ON true",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE 1 / 0 = 1) x ON x.id = rt2.id",
        "WITH c AS (SELECT id FROM rt WHERE 1 / 0 = 1) SELECT 1 FROM rt2 LEFT JOIN c ON c.id = rt2.id",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
}

#[test]
fn an_exists_sublink_drops_its_group_by_window_and_null_limit() {
    // `simplify_EXISTS_query` throws GROUP BY, WINDOW, DISTINCT and ORDER
    // BY away and drops a NULL (ALL) LIMIT: the EXISTS still becomes a
    // semi-join before the FALSE qual is folded. Grouping sets keep it.
    let db = planner_setup();
    for sql in [
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' \
         WINDOW w AS ()) AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' \
         GROUP BY rt.id) AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' \
         LIMIT NULL) AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' \
         LIMIT ALL) AND false",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    db.analyze(
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND s ~ '(' \
         GROUP BY ROLLUP (rt.id)) AND false",
    )
    .unwrap();
}
