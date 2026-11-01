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

#[test]
fn two_constants_an_equivalence_class_equates_gate_the_join_domain() {
    // `generate_base_implied_equalities_const` derives `2 = 1` from
    // `rt.id = 1 AND rt.id = 2` and applies it over the join domain
    // (`process_implied_equality`): a single relation's restriction, which
    // proves it empty — but over several relations a gating qual above
    // their join, which proves none of them empty.
    let db = planner_setup();
    for sql in [
        "SELECT 1 FROM t, rt WHERE rt.s ~ '(' AND rt.id = 1 AND rt.id = 2",
        "SELECT 1 FROM t, rt WHERE rt.s ~ '(' AND rt.id = 1 AND t.id = 2 AND rt.id = t.id",
        "SELECT 1 FROM t, (SELECT * FROM rt WHERE id = 1 AND id = 2) q WHERE q.s ~ '('",
        "SELECT 1 FROM t JOIN rt ON rt.id = 1 AND rt.id = 2 WHERE rt.s ~ '('",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "SELECT 1 FROM rt WHERE rt.s ~ '(' AND rt.id = 1 AND rt.id = 2",
        // The nullable side of an outer join is a join domain of its own.
        "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' AND rt.id = 1 AND rt.id = 2",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn an_inner_joins_on_clause_sublinks_become_joins_before_it_is_folded() {
    // `pull_up_sublinks` turns the IN / EXISTS sublinks of an inner join's
    // ON clause reading the join's sides into semi-joins above the join,
    // as it does WHERE's; an EXISTS whose FROM joins plain relations on
    // their own columns too.
    let db = planner_setup();
    for sql in [
        "SELECT 1 FROM t JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t JOIN u ON t.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t JOIN u ON EXISTS (SELECT 1 FROM rt WHERE rt.id = u.id AND rt.s ~ '(') \
         AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt JOIN u ON u.id = rt.id \
         WHERE rt.id = t.id AND rt.s ~ '(') AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt JOIN u USING (id) \
         WHERE rt.id = t.id AND rt.s ~ '(') AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt LEFT JOIN u ON u.id = rt.id \
         WHERE rt.id = t.id AND rt.s ~ '(') AND false",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "SELECT 1 FROM t JOIN u ON (u.x IN (SELECT id FROM rt WHERE s ~ '(') OR true)",
        "SELECT 1 FROM t JOIN u ON 1 IN (SELECT id FROM rt WHERE s ~ '(') AND false",
        "SELECT 1 FROM t JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(' AND false) AND false",
        // A join's ON clause reading the query: not a join.
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt JOIN u ON u.id = t.id \
         WHERE rt.id = t.id AND rt.s ~ '(') AND false",
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt JOIN u ON u.id = rt.id \
         WHERE rt.id = t.id AND rt.s ~ '(' AND false)",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn an_outer_joins_on_clause_sublinks_become_joins_in_its_nullable_side() {
    // `pull_up_sublinks` turns the IN / EXISTS sublinks of a LEFT (RIGHT)
    // join's ON clause reading only its right (left) side into semi-joins
    // in that side, before the clause is folded: their relations are
    // estimated — the constant FALSE qual left only gates the outer join.
    // So it does an inner join's below the nullable side of one, and the
    // join whose nullable side the semi-join reads is never removed.
    let mut db = planner_setup();
    db.apply_sql("CREATE TABLE w (id int PRIMARY KEY, x int);")
        .unwrap();
    assert_regex_estimated(
        &db,
        &[
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON false AND u.x IN (SELECT id FROM rt WHERE s ~ '(')",
            "SELECT 1 FROM u RIGHT JOIN t ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON EXISTS (SELECT 1 FROM rt WHERE rt.id = u.id \
             AND rt.s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON NOT EXISTS (SELECT 1 FROM rt WHERE rt.id = u.id \
             AND s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt r WHERE r.s ~ '(' \
             AND r.id = u.id) AND false",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(' LIMIT 3) \
             AND false",
            "SELECT 1 FROM t LEFT JOIN (u JOIN w ON w.id = u.id) \
             ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN (u JOIN w ON w.x IN (SELECT id FROM rt WHERE s ~ '(') \
             AND false) ON true",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false \
             LEFT JOIN w ON true",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false \
             WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false \
             AND u.id = t.id",
            // `u.id IS NULL` proves u empty, not the semi-join's rt.
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') \
             AND u.id IS NULL",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') \
             AND u.id = t.id AND u.id IS NULL",
            // rt's key equated, but the semi-join reads rt above it.
            "SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(' \
             AND rt.id IN (SELECT id FROM u)",
            // u's key equated, but the semi-join keeps the join: its
            // nullable side is (u SEMI JOIN rt), and the constant WHERE
            // qual is a gating qual above the join of t, u and rt.
            "SELECT 1 FROM t LEFT JOIN u ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE false",
            "SELECT t.id FROM t LEFT JOIN u ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE 1 = 2",
            "SELECT 1 FROM t LEFT JOIN u v ON v.id = t.id \
             AND v.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE NULL",
            "SELECT 1 FROM u RIGHT JOIN t ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON u.id = t.id AND EXISTS (SELECT 1 FROM rt \
             WHERE rt.id = u.id AND rt.s ~ '(') WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON u.id = t.id AND NOT EXISTS (SELECT 1 FROM rt \
             WHERE rt.id = u.id AND rt.s ~ '(') WHERE false",
            "SELECT 1 FROM t LEFT JOIN (SELECT * FROM u) q ON q.id = t.id \
             AND q.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE false",
            "SELECT 1 FROM t LEFT JOIN (u JOIN w ON w.id = u.id) ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(') WHERE false",
            "SELECT 1 FROM t LEFT JOIN (u JOIN w ON w.x IN (SELECT id FROM rt WHERE s ~ '(')) \
             ON u.id = t.id WHERE false",
        ],
    );
    for sql in [
        "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT 1 / 0 FROM rt) AND false",
        "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE 1 / 0 = 1) AND false",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::DivisionByZero(_),
            "division by zero"
        );
    }
    assert_accepted(
        &db,
        &[
            // Reading the outer side, in the tested expression or the
            // query: not pulled up, folded away with the qual.
            "SELECT 1 FROM t LEFT JOIN u ON t.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(' \
             AND rt.id = t.id) AND false",
            "SELECT 1 FROM t LEFT JOIN u ON EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id \
             AND s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON 1 IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            // Nor a FULL join's, nor under an OR, nor NOT IN.
            "SELECT 1 FROM t FULL JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            "SELECT 1 FROM t LEFT JOIN u ON (u.x IN (SELECT id FROM rt WHERE s ~ '(') OR true) \
             AND false",
            "SELECT 1 FROM t LEFT JOIN u ON u.x NOT IN (SELECT id FROM rt WHERE s ~ '(') AND false",
            // The semi-join's own quals prove its relation empty.
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(' AND false) \
             AND false",
            "SELECT 1 FROM t LEFT JOIN u ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(' AND false) WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON u.id = t.id \
             AND u.x IN (SELECT id FROM rt WHERE s ~ '(' AND id IS NULL) WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON u.x IN (SELECT id FROM rt WHERE s ~ '(' \
             AND id IS NULL) AND false",
            // No sublink: the AND folds whole.
            "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' AND false",
            // A SubPlan reading both sides, in a join removed.
            "SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(' \
             AND rt.id IN (SELECT id FROM u WHERE u.x = rt2.id)",
        ],
    );
}

#[test]
fn an_outer_joins_constant_qual_proves_no_relation_empty() {
    // A constant ON qual of an outer join (or of a join below its
    // nullable side) is attached to that join (`distribute_qual_to_rels`,
    // `ojscope`): the relations of the level are still estimated.
    let db = planner_setup();
    assert_regex_estimated(
        &db,
        &[
            "SELECT 1 FROM t LEFT JOIN u ON false WHERE t.s ~ '('",
            "SELECT 1 FROM t LEFT JOIN u ON 1 = 2 WHERE t.s ~ '('",
            "SELECT 1 FROM t LEFT JOIN u ON NULL WHERE t.s ~ '('",
            "SELECT 1 FROM t FULL JOIN u ON false WHERE t.s ~ '('",
            "SELECT 1 FROM t LEFT JOIN (u JOIN rt ON false) ON true WHERE t.s ~ '('",
            "SELECT 1 FROM t LEFT JOIN (SELECT * FROM u WHERE false) q ON true WHERE t.s ~ '('",
            // An inner join's gates the join of its two relations.
            "SELECT 1 FROM t JOIN u ON false WHERE t.s ~ '('",
        ],
    );
    // A nullable side that is an inner join of two tables is never
    // removed (`join_is_removable` needs a single relation there): the
    // constant WHERE qual gates the join of the level's three relations.
    let mut db = db;
    db.apply_sql("CREATE TABLE w (id int PRIMARY KEY, x int, s text);")
        .unwrap();
    assert_regex_estimated(
        &db,
        &[
            "SELECT 1 FROM t LEFT JOIN (u JOIN w ON w.id = u.id AND w.s ~ '(') \
             ON u.id = t.id WHERE false",
            "SELECT 1 FROM t LEFT JOIN (u CROSS JOIN w) ON u.id = t.id AND w.s ~ '(' \
             WHERE false",
            "SELECT 1 FROM t FULL JOIN (u JOIN w ON w.id = u.id AND w.s ~ '(') \
             ON u.id = t.id WHERE false",
        ],
    );
    assert_accepted(
        &db,
        &[
            // A single table, removed: t alone is proven empty.
            "SELECT 1 FROM t LEFT JOIN rt ON rt.id = t.id AND rt.s ~ '(' WHERE false",
            "SELECT 1 FROM t LEFT JOIN u ON false WHERE t.s ~ '(' AND false",
            "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' AND rt.id = t.id AND false",
        ],
    );
}

#[test]
fn an_inlined_cte_is_estimated_as_the_from_item_it_is_pulled_up_as() {
    // A CTE referenced once is inlined and pulled up where its reference
    // is: its relations' restrictions are estimated unless that level
    // proves them empty or removes them.
    let db = planner_setup();
    for sql in [
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM c",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM rt2 JOIN c ON c.id = rt2.id",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM t, c WHERE t.id IS NULL",
        "WITH c AS (SELECT s FROM rt) SELECT 1 FROM c WHERE c.s ~ '('",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') \
         UPDATE rt2 SET s = 'x' FROM c WHERE c.id = rt2.id",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') DELETE FROM rt2 USING c WHERE c.id = rt2.id",
        // A CTE named as the target is no reference to it.
        "WITH rt2 AS (SELECT 1 AS k WHERE false) UPDATE rt2 SET s = 'x' WHERE rt2.s ~ '('",
        // Referenced twice: scanned, planned whole.
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM c, c c2",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM c WHERE false",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM c WHERE c.id IS NULL",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') \
         SELECT 1 FROM rt2 JOIN c ON c.id = rt2.id WHERE c.id IS NULL",
        "WITH c AS (SELECT s FROM rt) SELECT 1 FROM c WHERE c.s ~ '(' AND false",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') \
         UPDATE rt2 SET s = 'x' FROM c WHERE c.id = rt2.id AND c.id IS NULL",
        // Pulled up, its constant FALSE qual proves the single relation
        // beside it empty.
        "WITH c AS (SELECT 1 AS k WHERE false) SELECT 1 FROM rt2, c WHERE rt2.s ~ '('",
        "WITH c AS (SELECT 1 AS k WHERE false) UPDATE rt2 SET s = 'x' FROM c WHERE rt2.s ~ '('",
        "WITH c AS (SELECT 1 AS k WHERE false) DELETE FROM rt2 USING c WHERE rt2.s ~ '('",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_nullable_side_read_above_the_join_is_not_removed() {
    // `join_is_removable` keeps a join whose nullable side has a column
    // needed above it — read in the select list, ORDER BY, GROUP BY, …:
    // that side is planned, its restrictions estimated. A read in another
    // outer join's ON clause doesn't keep it when that join is removed
    // first.
    let db = planner_setup();
    for sql in [
        "SELECT x.id FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id",
        "SELECT x.* FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id \
         GROUP BY x.id",
        "SELECT rt.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('",
        "SELECT * FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('",
        "SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(' ORDER BY rt.id",
        "SELECT count(rt.id) FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') \
         SELECT c.id FROM rt2 LEFT JOIN c ON c.id = rt2.id",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            "invalid regular expression: parentheses () not balanced"
        );
    }
    for sql in [
        "SELECT rt2.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('",
        "SELECT rt2.id FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x ON x.id = rt2.id",
        "SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(' \
         LEFT JOIN u ON u.id = rt.id",
        "SELECT 1 FROM rt2 LEFT JOIN (rt LEFT JOIN u ON u.id = rt.id) \
         ON rt.id = rt2.id AND rt.s ~ '('",
        "WITH c AS (SELECT id FROM rt WHERE s ~ '(') SELECT 1 FROM rt2 LEFT JOIN c ON c.id = rt2.id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Assert each of `sql` is rejected with the invalid-regex error the
/// planner raises estimating `rt.s ~ '('`.
fn assert_regex_estimated(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        match db.analyze(sql) {
            Err(AnalyzeError::Invalid(m))
                if m.starts_with("invalid regular expression: parentheses () not balanced") => {}
            other => panic!("{sql}: expected the invalid-regex error, got {other:?}"),
        }
    }
}

/// Assert each of `sql` is accepted.
fn assert_accepted(db: &PgCatalog, sqls: &[&str]) {
    for sql in sqls {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn a_subquerys_select_list_keeps_a_join_only_where_the_query_reading_it_does() {
    // Pulled up, a subquery's output nothing reads is referenced by
    // nothing; planned on its own, `remove_unused_subquery_outputs` drops
    // it. An EXISTS's select list is thrown away (`simplify_EXISTS_query`).
    // Either way, it doesn't keep the join whose nullable side it reads.
    let mut db = planner_setup();
    db.apply_sql("CREATE TABLE s9 (a int2, b int);").unwrap();
    let sub = "SELECT rt.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('";
    let accepted = [
        format!("SELECT 1 FROM ({sub}) q"),
        format!("WITH c AS ({sub}) SELECT 1 FROM c"),
        format!("SELECT count(*) FROM ({sub}) q"),
        format!("SELECT 1 WHERE EXISTS ({sub})"),
        "SELECT q.id FROM (SELECT rt2.id, rt.id AS rid FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(') q"
            .to_owned(),
        format!("SELECT 1 FROM ({sub} OFFSET 0) q"),
        "SELECT 1 FROM t WHERE EXISTS (SELECT rt.id FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(' WHERE rt2.id = t.id)"
            .to_owned(),
        format!("INSERT INTO s9 (b) SELECT 1 FROM ({sub}) q"),
        format!("SELECT 1 FROM t, LATERAL ({sub}) q"),
        format!("SELECT 1 FROM t LEFT JOIN ({sub}) q ON true"),
        "SELECT t.id FROM t JOIN (SELECT rt2.id AS a, rt.id AS b FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(') q ON q.a = t.id"
            .to_owned(),
        "SELECT q.z FROM (SELECT rt2.id, rt.id FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(') q(z, a)"
            .to_owned(),
        "WITH c(z, a) AS (SELECT rt2.id, rt.id FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(') SELECT c.z FROM c"
            .to_owned(),
        format!("SELECT 1 FROM (SELECT q.id FROM ({sub}) q) q2"),
        format!("SELECT 1 FROM ({sub} UNION ALL SELECT 1) q"),
        "SELECT 1 FROM (SELECT rt2.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(' \
         WINDOW w AS (ORDER BY rt.id)) q"
            .to_owned(),
        "SELECT 1 WHERE EXISTS (SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id \
         AND rt.s ~ '(' ORDER BY rt.id)"
            .to_owned(),
        "SELECT 1 WHERE EXISTS (SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id \
         AND rt.s ~ '(' GROUP BY rt.id)"
            .to_owned(),
        // Planned on its own under a row mark, a subquery's whole row is
        // needed — its outputs, not what they don't read.
        "MERGE INTO s9 USING (SELECT t.id FROM t LEFT JOIN rt ON rt.id = t.id \
         AND rt.s ~ '(' OFFSET 0) q ON s9.b = q.id WHEN MATCHED THEN DELETE"
            .to_owned(),
    ];
    assert_accepted(
        &db,
        &accepted.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let rejected = [
        format!("SELECT q.id FROM ({sub}) q"),
        format!("SELECT * FROM ({sub}) q"),
        format!("SELECT q.* FROM ({sub}) q"),
        format!("SELECT 1 FROM ({sub}) q WHERE q.id = 1"),
        format!("SELECT 1 FROM t JOIN ({sub}) q ON q.id = t.id"),
        "SELECT q.a FROM (SELECT rt2.id, rt.id FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(') q(z, a)"
            .to_owned(),
        "WITH c(z, a) AS (SELECT rt2.id, rt.id FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(') SELECT c.a FROM c"
            .to_owned(),
        format!("WITH c AS ({sub}) SELECT c.id FROM c"),
        format!("WITH c AS ({sub}) SELECT 1 FROM c z WHERE z.id = 1"),
        // Scanned, a CTE's every output is planned.
        format!("WITH c AS MATERIALIZED ({sub}) SELECT 1 FROM c"),
        format!("WITH c AS ({sub}) SELECT 1 FROM c, c c2"),
        // A scalar or IN sublink's select list is its value.
        format!("SELECT ({sub} LIMIT 1)"),
        format!("SELECT 1 WHERE 1 IN ({sub})"),
        // ORDER BY, GROUP BY, DISTINCT keep the subquery from being pulled
        // up, and their columns from being dropped.
        "SELECT 1 FROM (SELECT rt2.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id \
         AND rt.s ~ '(' ORDER BY rt.id) q"
            .to_owned(),
        "SELECT 1 FROM (SELECT rt2.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id \
         AND rt.s ~ '(' GROUP BY rt2.id, rt.id) q"
            .to_owned(),
        "SELECT 1 FROM (SELECT DISTINCT rt.id FROM rt2 LEFT JOIN rt ON rt.id = rt2.id \
         AND rt.s ~ '(') q"
            .to_owned(),
        // An EXISTS whose select list `simplify_EXISTS_query` keeps.
        "SELECT 1 WHERE EXISTS (SELECT row_number() OVER (ORDER BY rt.id) FROM rt2 \
         LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '(')"
            .to_owned(),
        format!("SELECT 1 WHERE EXISTS ({sub} OFFSET 1)"),
        // The statement's own select list.
        format!("INSERT INTO s9 (b) {sub}"),
        format!("{sub} UNION ALL SELECT 1"),
        // Row marks keep a pulled-up subquery's relations.
        format!("UPDATE s9 SET b = 1 FROM ({sub}) q"),
        format!("MERGE INTO s9 USING ({sub}) q ON true WHEN MATCHED THEN DELETE"),
        format!("WITH c AS ({sub}) UPDATE s9 SET b = 1 FROM c"),
        format!("SELECT 1 FROM t, ({sub}) q FOR UPDATE OF t"),
    ];
    assert_regex_estimated(
        &db,
        &rejected.iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

#[test]
fn a_read_constant_folding_drops_doesnt_keep_the_join() {
    // `eval_const_expressions` folds `x OR true` to TRUE, a strict operator
    // over NULL to NULL, a CASE with a constant WHEN to its branch: a
    // column read only there is no longer needed above the join.
    let db = planner_setup();
    let join = "SELECT 1 FROM rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('";
    let accepted = [
        format!("{join} JOIN u ON u.id = rt2.id AND (u.x = rt.id OR true)"),
        format!("{join} JOIN u ON u.id = rt2.id AND (u.x = rt.id AND NULL)"),
        format!("{join} WHERE (rt.id = 1 OR true)"),
        format!("{join} WHERE rt.id + NULL = 1"),
        format!("{join} WHERE CASE WHEN true THEN true ELSE rt.id = 1 END"),
        format!("{join} WHERE coalesce(1, rt.id) = 0"),
        format!("{join} WHERE EXISTS (SELECT 1 FROM u WHERE u.x = rt.id AND false)"),
        format!("{join} LEFT JOIN u ON u.id = rt.id WHERE u.x = 1 OR true"),
        // A custom plan folds `rt.id = NULL` (all-NULL parameters).
        format!("{join} WHERE rt.id = $1"),
    ];
    assert_accepted(
        &db,
        &accepted.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let rejected = [
        format!("{join} JOIN u ON u.id = rt2.id AND u.x = rt.id"),
        format!("{join} JOIN u ON u.id = rt2.id AND (u.x = rt.id OR false)"),
        format!("{join} WHERE rt2.id = 1 OR rt.id = 2"),
        format!("{join} WHERE NOT (rt.id = 2)"),
        format!("{join} WHERE coalesce(rt.id, 0) = 0"),
        format!("{join} WHERE coalesce(NULL, rt.id, 0) = 0"),
    ];
    assert_regex_estimated(
        &db,
        &rejected.iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

#[test]
fn a_nullable_side_certainly_kept_has_its_restrictions_estimated() {
    // A nullable side kept above its join — read in a sublink, kept by row
    // marks (UPDATE, DELETE, MERGE, FOR UPDATE), by another kept outer
    // join's ON clause, or a table the join can't prove unique — is one of
    // the top join domain's relations: a constant FALSE WHERE is a gating
    // qual above the join, `col IS NULL` over it (null-extended) restricts
    // nothing, and its restrictions are estimated.
    let mut db = planner_setup();
    db.apply_sql("CREATE TABLE s9 (a int2, b int);").unwrap();
    let join = "rt2 LEFT JOIN rt ON rt.id = rt2.id AND rt.s ~ '('";
    let rejected = [
        format!("SELECT rt.id FROM {join} WHERE false"),
        format!("SELECT * FROM {join} WHERE false"),
        format!("SELECT rt.id FROM {join} WHERE NULL"),
        format!("SELECT rt.id FROM {join} WHERE rt2.id = 1 AND false"),
        "SELECT x.id FROM rt2 LEFT JOIN (SELECT id FROM rt WHERE s ~ '(') x \
         ON x.id = rt2.id WHERE x.id IS NULL"
            .to_owned(),
        format!("SELECT rt2.id FROM {join} WHERE rt.id IS NULL"),
        "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' WHERE rt.id IS NULL".to_owned(),
        "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' WHERE rt.id IS NULL AND t.x = 1".to_owned(),
        // Read in a sublink.
        format!("SELECT rt2.id, (SELECT rt.id) FROM {join}"),
        format!("SELECT (SELECT rt.id FROM u LIMIT 1) FROM {join}"),
        format!("SELECT 1 FROM {join} WHERE EXISTS (SELECT 1 FROM u WHERE u.x = rt.id)"),
        format!("SELECT 1 FROM {join} WHERE rt2.id IN (SELECT x FROM u WHERE x = rt.id)"),
        format!("SELECT 1 FROM {join} WHERE rt2.id IN (SELECT rt.id FROM u)"),
        format!("SELECT 1 FROM {join} WHERE EXISTS (SELECT 1 FROM u JOIN t ON t.id = rt.id)"),
        format!(
            "SELECT 1 FROM {join} WHERE EXISTS \
             (SELECT 1 FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.id = rt.id))"
        ),
        format!(
            "SELECT 1 FROM {join} WHERE EXISTS \
             (SELECT 1 FROM generate_series(1, 2) g WHERE rt.id = 1)"
        ),
        // Read in the ON clause of an outer join that is kept.
        format!("SELECT 1 FROM {join} LEFT JOIN u ON u.id = rt.id WHERE u.x = 1"),
        format!("SELECT 1 FROM {join} LEFT JOIN u ON u.id = rt.id WHERE u.x IS NULL"),
        format!("SELECT u.x FROM {join} LEFT JOIN u ON u.id = rt.id"),
        format!("SELECT 1 FROM {join} LEFT JOIN t ON t.x = rt.id"),
        format!("SELECT 1 FROM {join} FULL JOIN u ON u.id = rt.id"),
        // Not removable, the nullable side is in the top join domain: two
        // constants equated there are a gating qual.
        "SELECT 1 FROM t LEFT JOIN rt ON true WHERE rt.s ~ '(' AND rt.id = 1 AND rt.id = 2"
            .to_owned(),
        "SELECT 1 FROM t FULL JOIN rt ON true WHERE rt.s ~ '(' AND rt.id = 1 AND rt.id = 2"
            .to_owned(),
        // Row marks.
        format!("UPDATE s9 SET b = rt.id FROM {join}"),
        format!("UPDATE s9 SET b = 1 FROM {join}"),
        format!("DELETE FROM s9 USING {join} WHERE s9.b = rt.id"),
        format!("DELETE FROM s9 USING {join}"),
        format!("MERGE INTO s9 USING {join} ON true WHEN MATCHED THEN DELETE"),
        format!("SELECT rt2.id FROM {join} FOR UPDATE OF rt2"),
        format!("WITH m AS (DELETE FROM s9 USING {join} RETURNING 1) SELECT 1"),
        // A semi-join's relation proven empty doesn't keep another of its
        // relations from being estimated.
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt JOIN u ON u.id = rt.id AND u.x IS NULL \
         WHERE rt.id = t.id AND rt.s ~ '(') AND false"
            .to_owned(),
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt JOIN u ON u.id = rt.id \
         WHERE u.id IS NULL AND rt.id = t.id AND rt.s ~ '(') AND false"
            .to_owned(),
        "SELECT 1 FROM t WHERE t.x IN (SELECT rt.id FROM rt JOIN u ON u.id = rt.id \
         WHERE rt.s ~ '(' AND u.id IS NULL)"
            .to_owned(),
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND rt.s ~ '(') \
         AND t.id IS NULL"
            .to_owned(),
    ];
    assert_regex_estimated(
        &db,
        &rejected.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let accepted = [
        // Removed (nothing reads it), the join leaves a single relation
        // the FALSE qual proves empty.
        format!("SELECT rt2.id FROM {join} WHERE false"),
        "SELECT 1 FROM (SELECT * FROM t WHERE s ~ '(') q LEFT JOIN rt ON rt.id = q.id WHERE false"
            .to_owned(),
        // A strict qual reduces the outer join: `rt.id IS NULL` restricts
        // `rt` again.
        format!("SELECT rt2.id FROM {join} WHERE rt.id IS NULL AND rt.s = 'x'"),
        "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '(' WHERE rt.id IS NULL AND rt.s = 'x'".to_owned(),
        "SELECT 1 FROM (SELECT rt.id FROM t LEFT JOIN rt ON rt.s ~ '(' WHERE rt.id IS NULL) q \
         WHERE q.id = 5"
            .to_owned(),
        // In the semi-join, the qual restricts the very relation.
        "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = t.id AND rt.s ~ '(' \
         AND rt.id IS NULL)"
            .to_owned(),
        "SELECT 1 FROM t WHERE t.x IN (SELECT rt.id FROM rt WHERE rt.s ~ '(' AND rt.id IS NULL)"
            .to_owned(),
        // A sublink naming another `rt`, or keeping no reference.
        format!("SELECT 1 FROM {join} WHERE EXISTS (SELECT 1 FROM rt WHERE rt.id = 1)"),
        format!("SELECT 1 FROM {join} WHERE rt2.id IN (SELECT rt.id FROM rt)"),
        format!("SELECT 1 FROM {join} WHERE EXISTS (SELECT rt.id FROM u)"),
        format!("SELECT 1 FROM {join} WHERE EXISTS (SELECT 1 FROM u, LATERAL (SELECT rt.id) l)"),
        // The other outer join is removed first.
        format!("SELECT 1 FROM {join} LEFT JOIN t ON t.id = rt.id"),
        // A subquery planned on its own is still read through its row.
        "UPDATE s9 SET b = (SELECT t.id FROM t LEFT JOIN rt ON rt.id = t.id AND rt.s ~ '(' \
         LIMIT 1)"
            .to_owned(),
        "UPDATE s9 SET b = 1 FROM t LEFT JOIN rt ON rt.id = t.id WHERE t.s ~ '(' AND false"
            .to_owned(),
    ];
    assert_accepted(
        &db,
        &accepted.iter().map(String::as_str).collect::<Vec<_>>(),
    );
}

#[test]
fn a_join_proves_its_nullable_side_unique_only_by_equating_a_unique_key() {
    // `join_is_removable` needs the nullable side proven unique for the
    // join (`rel_is_distinct_for`): a table has every key column of a
    // unique index equated by the join (or restricted to a constant), a
    // subquery planned on its own makes its rows distinct. Otherwise the
    // join is kept and the side estimated.
    let db = planner_setup();
    let q = "(SELECT * FROM t WHERE s ~ '(') q";
    assert_regex_estimated(
        &db,
        &[
            "SELECT 1 FROM t LEFT JOIN rt ON rt.s ~ '('",
            &format!("SELECT 1 FROM {q} LEFT JOIN rt ON rt.s = q.s WHERE false"),
            &format!("SELECT 1 FROM {q} LEFT JOIN rt USING (s) WHERE false"),
            &format!("SELECT 1 FROM {q} LEFT JOIN u USING (x) WHERE false"),
            &format!(
                "SELECT 1 FROM {q} LEFT JOIN (SELECT * FROM rt OFFSET 0) r ON r.id = q.id \
                 WHERE false"
            ),
            "SELECT 1 FROM t LEFT JOIN (SELECT rt.id FROM rt WHERE rt.s ~ '(' OFFSET 0) r \
             ON r.id = t.id WHERE false",
            "SELECT 1 FROM t LEFT JOIN (SELECT rt.id FROM rt WHERE rt.s ~ '(') r ON true",
        ],
    );
    assert_accepted(
        &db,
        &[
            "SELECT 1 FROM t LEFT JOIN rt ON rt.id = 1 AND rt.s ~ '('",
            &format!("SELECT 1 FROM {q} LEFT JOIN rt ON rt.id = q.x WHERE false"),
            &format!("SELECT 1 FROM {q} LEFT JOIN rt ON rt.id = 3 WHERE false"),
            &format!("SELECT 1 FROM {q} LEFT JOIN rt USING (id) WHERE false"),
            &format!("SELECT 1 FROM {q} NATURAL LEFT JOIN u WHERE false"),
            &format!(
                "SELECT 1 FROM {q} LEFT JOIN (SELECT DISTINCT id FROM rt) r ON r.id = q.id \
                 WHERE false"
            ),
            &format!(
                "SELECT 1 FROM {q} LEFT JOIN (SELECT id FROM rt GROUP BY id) r ON r.id = q.id \
                 WHERE false"
            ),
            // A subquery's own equality (a LATERAL one's, a constant one)
            // proves its relation unique once it is pulled up.
            "SELECT 1 FROM t LEFT JOIN LATERAL (SELECT rt.id FROM rt \
             WHERE rt.id = t.id AND rt.s ~ '(') r ON true",
            "SELECT 1 FROM t LEFT JOIN (SELECT rt.id FROM rt WHERE rt.id = 1 AND rt.s ~ '(') r \
             ON true",
        ],
    );
}
