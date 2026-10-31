//! Aggregates and GROUP BY / HAVING: COUNT, SUM, MIN, MAX, AVG,
//! string_agg, array_agg. Nullability rules for empty sets and grouped
//! columns. Strict vs non-strict builtin functions.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            age  INT
         );
         CREATE TABLE posts (
            id           BIGINT PRIMARY KEY,
            user_id      BIGINT NOT NULL,
            title        TEXT NOT NULL,
            body         TEXT,
            published_at TIMESTAMPTZ
         );
         CREATE TABLE comments (
            id          BIGINT PRIMARY KEY,
            post_id     BIGINT NOT NULL,
            author_name TEXT NOT NULL,
            content     TEXT NOT NULL,
            rating      INT
         );",
    )
    .unwrap();
    db
}

// ── COUNT / GROUP BY basic shapes ────────────────────────────────────────────

#[test]
fn types_match_count_star() {
    let db = setup();
    let s = db.analyze("SELECT count(*) AS total FROM users").unwrap();
    assert_cols(&s, vec![c("total", int8())]);
}

#[test]
fn types_match_group_by_count() {
    let db = setup();
    let s = db
        .analyze("SELECT user_id, count(*) AS post_count FROM posts GROUP BY user_id")
        .unwrap();
    assert_cols(&s, vec![c("user_id", int8()), c("post_count", int8())]);
}

#[test]
fn types_match_group_by_multiple_aggregates() {
    let db = setup();
    // max() is nullable — returns NULL for empty groups.
    let s = db
        .analyze(
            "SELECT user_id, count(*) AS cnt, max(published_at) AS latest \
             FROM posts GROUP BY user_id",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("user_id", int8()),
            c("cnt", int8()),
            cn("latest", timestamptz()),
        ],
    );
}

#[test]
fn count_not_null() {
    let db = setup();
    let sql = "SELECT COUNT(*) as cnt FROM users";
    let info = db.analyze(sql).unwrap();
    assert!(!col(&info, "cnt").nullable);
}

// ── GROUP BY + aggregates: SUM/MIN/MAX/AVG/string_agg ────────────────────────

#[test]
fn agg_sum_with_group_by_not_null_input() {
    let db = setup();
    // user_id is NOT NULL + GROUP BY → SUM guaranteed non-null.
    // SUM(int8) → numeric.
    let sql = "SELECT user_id, SUM(user_id) as total FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("user_id", int8()), c("total", numeric())]);
}

#[test]
fn agg_sum_with_group_by_nullable_input() {
    let db = setup();
    // rating is nullable + GROUP BY → SUM still nullable (all rows in group could be NULL).
    // SUM(int4) → int8.
    let sql = "SELECT post_id, SUM(rating) as total FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("post_id", int8()), cn("total", int8())]);
}

#[test]
fn agg_min_max_with_group_by_not_null() {
    let db = setup();
    // title is NOT NULL + GROUP BY → MIN/MAX are NOT NULL.
    // MIN/MAX(text) → text.
    let sql = "SELECT user_id, MIN(title) as first_title, MAX(title) as last_title \
               FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("user_id", int8()),
            c("first_title", text()),
            c("last_title", text()),
        ],
    );
}

#[test]
fn agg_avg_with_group_by_not_null() {
    let db = setup();
    // id is NOT NULL + GROUP BY → AVG is NOT NULL.
    // AVG(int8) → numeric.
    let sql = "SELECT user_id, AVG(id) as avg_id FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("user_id", int8()), c("avg_id", numeric())]);
}

#[test]
fn agg_count_with_group_by() {
    let db = setup();
    // COUNT is always NOT NULL, with or without GROUP BY.
    // COUNT(*) → int8.
    let sql = "SELECT user_id, COUNT(*) as cnt FROM posts GROUP BY user_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("user_id", int8()), c("cnt", int8())]);
}

#[test]
fn agg_count_without_group_by() {
    let db = setup();
    // COUNT without GROUP BY: still NOT NULL (returns 0).
    // COUNT(*) → int8.
    let sql = "SELECT COUNT(*) as cnt FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("cnt", int8())]);
}

// ── aggfinalfn-driven return types ──────────────────────────────────────────
//
// These exercise the codepath that walks pg_aggregate.aggfinalfn → pg_proc
// → prorettype. Without a properly populated `aggfinalfn` in the seed,
// the returned type would silently fall back to the aggregate's transition
// type (e.g., AVG(int) would return its internal accumulator instead of
// numeric). Each variant of AVG we test below has its own finalfn.

#[test]
fn avg_int4_returns_numeric_via_finalfn() {
    let db = setup();
    // PG: avg(int4) → numeric. Driven by `int8_avg` finalfn — `aggfinalfn`
    // points at it; the analyzer walks that to recover the type.
    let sql = "SELECT AVG(rating) AS avg_rating FROM comments";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("avg_rating", numeric())]);
}

#[test]
fn avg_int8_returns_numeric_via_finalfn() {
    let db = setup();
    let sql = "SELECT AVG(id) AS avg_id FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("avg_id", numeric())]);
}

#[test]
fn avg_numeric_returns_numeric_via_finalfn() {
    // Cover NUMERIC explicitly — its accumulator is `_numeric` (an array)
    // but the finalfn collapses to numeric. If `aggfinalfn` weren't
    // resolved, we'd surface the array intermediate.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (price NUMERIC NOT NULL);")
        .unwrap();
    let info = db.analyze("SELECT AVG(price) AS avg_price FROM t").unwrap();
    assert_cols(&info, vec![cn("avg_price", numeric())]);
}

#[test]
fn variance_int4_returns_numeric_via_finalfn() {
    // VARIANCE / STDDEV also have finalfns. Different finalfn than AVG —
    // makes sure we're not just lucky on one binding.
    let db = setup();
    let info = db
        .analyze("SELECT VARIANCE(rating) AS v FROM comments")
        .unwrap();
    assert_cols(&info, vec![cn("v", numeric())]);
}

#[test]
fn agg_sum_without_group_by_always_nullable() {
    let db = setup();
    // SUM without GROUP BY: table could be empty → NULL.
    // Even with NOT NULL input.
    // SUM(int8) → numeric.
    let sql = "SELECT SUM(id) as total FROM posts";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![cn("total", numeric())]);
}

#[test]
fn agg_mixed_nullability_with_group_by() {
    let db = setup();
    // Mix of NOT NULL and nullable aggregates in same GROUP BY query.
    // COUNT(*) → int8, SUM(int4) → int8, MIN(text) → text, MAX(int4) → int4.
    let sql = "SELECT post_id, \
                      COUNT(*) as cnt, \
                      SUM(rating) as sum_rating, \
                      MIN(author_name) as first_author, \
                      MAX(rating) as max_rating \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("post_id", int8()),
            c("cnt", int8()),
            cn("sum_rating", int8()),
            c("first_author", text()),
            cn("max_rating", int4()),
        ],
    );
}

#[test]
fn agg_with_group_by_and_left_join() {
    let db = setup();
    // LEFT JOIN + GROUP BY: right-side columns are nullable from JOIN,
    // so aggregate on them is nullable even with GROUP BY.
    // COUNT(x) → int8, MAX(text) → text.
    let sql = "SELECT u.id, COUNT(p.id) as post_count, MAX(p.title) as last_title \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.id";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("id", int8()),
            c("post_count", int8()),
            cn("last_title", text()),
        ],
    );
}

#[test]
fn agg_string_agg_with_group_by_not_null() {
    let db = setup();
    // string_agg(NOT NULL, delimiter) with GROUP BY → NOT NULL.
    // The literal ', ' has type UNKNOWN — resolved via UNKNOWN-compatible matching.
    // string_agg(text, text) → text.
    let sql = "SELECT post_id, string_agg(author_name, ', ') as authors \
               FROM comments GROUP BY post_id";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("post_id", int8()), c("authors", text())]);
}

// ── Stress / torture ─────────────────────────────────────────────────────────

#[test]
fn stress_aggregates_no_group_by() {
    let db = setup();
    // COUNT(*) → int8, SUM(int4) → int8, MAX(text) → text.
    // SUM and MAX are nullable (empty table → NULL).
    let sql = "SELECT COUNT(*) as cnt, SUM(age) as total_age, MAX(name) as last_name FROM users";
    let info = db.analyze(sql).unwrap();
    assert_cols(
        &info,
        vec![
            c("cnt", int8()),
            cn("total_age", int8()),
            cn("last_name", text()),
        ],
    );
}

#[test]
fn torture_count_with_group_by() {
    let db = setup();
    // COUNT in GROUP BY context is still NOT NULL.
    // COUNT(x) → int8.
    let sql = "SELECT u.name, COUNT(p.id) as post_count \
               FROM users u \
               LEFT JOIN posts p ON p.user_id = u.id \
               GROUP BY u.name";
    let info = db.analyze(sql).unwrap();
    assert_cols(&info, vec![c("name", text()), c("post_count", int8())]);
}

// ── Placement rules ──────────────────────────────────────────────────────────

#[test]
fn aggregate_in_where_rejected() {
    let db = setup();
    // PG: `aggregate functions are not allowed in WHERE`.
    assert_analyze_err!(
        db.analyze("SELECT id FROM users WHERE COUNT(*) > 0"),
        AnalyzeError::GroupingError(_),
        concat!(
            "aggregate functions are not allowed in WHERE\n",
            "  ╭────\n",
            "1 │ SELECT id FROM users WHERE COUNT(*) > 0\n",
            "  ·                            ──┬──\n",
            "  ·                              ╰─ aggregate not allowed here\n",
            "  ╰────\n",
            "  help: to filter on an aggregate, use a HAVING clause instead of WHERE\n",
        ),
    );
}

#[test]
fn call_no_aggregate_matches_is_a_missing_function_not_a_misplaced_aggregate() {
    let db = setup();
    // `max()` names aggregates, but none takes no argument: PG resolves the
    // call first and fails it as a missing function, before any placement
    // rule could apply.
    for sql in [
        "INSERT INTO users (id, name) VALUES (max(), 'n')",
        "SELECT id FROM users WHERE max() > 0",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::UndefinedFunction(_),
            "function max() does not exist"
        );
    }
    // count(*) and an ordered-set aggregate still count as aggregates.
    for sql in [
        "SELECT id FROM users WHERE count(*) > 0",
        "SELECT id FROM users WHERE percentile_cont(0.5) WITHIN GROUP (ORDER BY age) > 0",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "aggregate functions are not allowed in WHERE"
        );
    }
}

#[test]
fn aggregate_in_group_by_rejected() {
    let db = setup();
    // PG: `aggregate functions are not allowed in GROUP BY`.
    assert_analyze_err!(
        db.analyze("SELECT age FROM users GROUP BY COUNT(*)"),
        AnalyzeError::GroupingError(_),
        concat!(
            "aggregate functions are not allowed in GROUP BY\n",
            "  ╭────\n",
            "1 │ SELECT age FROM users GROUP BY COUNT(*)\n",
            "  ·                                ──┬──\n",
            "  ·                                  ╰─ aggregate not allowed here\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn nested_aggregate_rejected() {
    let db = setup();
    // PG: `aggregate function calls cannot be nested`.
    assert_analyze_err!(
        db.analyze("SELECT SUM(COUNT(*)) FROM posts GROUP BY user_id"),
        AnalyzeError::GroupingError(_),
        concat!(
            "aggregate function calls cannot be nested\n",
            "  ╭────\n",
            "1 │ SELECT SUM(COUNT(*)) FROM posts GROUP BY user_id\n",
            "  ·            ─────\n",
            "  ╰────\n",
            "  help: compute the inner aggregate in a subquery, e.g. `SELECT max(n) FROM (SELECT count(*) AS n FROM t GROUP BY g) s`\n",
        ),
    );
}

#[test]
fn aggregate_placement_errors_point_at_the_call() {
    let db = setup();
    // A GROUP BY ordinal naming an aggregate output: PG points at the call.
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM posts GROUP BY 1"),
        AnalyzeError::GroupingError(_),
        "\
aggregate functions are not allowed in GROUP BY
  ╭────
1 │ SELECT count(*) FROM posts GROUP BY 1
  ·        ─────
  ╰────
",
    );
    assert_analyze_err!(
        db.analyze("SELECT json_agg(json_agg(id)) FROM posts"),
        AnalyzeError::GroupingError(_),
        "\
aggregate function calls cannot be nested
  ╭────
1 │ SELECT json_agg(json_agg(id)) FROM posts
  ·                 ────────
  ╰────
  help: compute the inner aggregate in a subquery, e.g. `SELECT max(n) FROM (SELECT count(*) AS n FROM t GROUP BY g) s`
",
    );
    assert_analyze_err!(
        db.analyze("SELECT JSON_ARRAYAGG(count(*)) FROM posts"),
        AnalyzeError::GroupingError(_),
        "\
aggregate function calls cannot be nested
  ╭────
1 │ SELECT JSON_ARRAYAGG(count(*)) FROM posts
  ·                      ─────
  ╰────
  help: compute the inner aggregate in a subquery, e.g. `SELECT max(n) FROM (SELECT count(*) AS n FROM t GROUP BY g) s`
",
    );
}

#[test]
fn window_function_in_aggregate_argument_rejected() {
    let db = setup();
    // PG (SQLSTATE 42803): `aggregate function calls cannot contain window
    // function calls`. Mirror PG's wording verbatim so the sanity check
    // passes.
    assert_analyze_err!(
        db.analyze("SELECT SUM(ROW_NUMBER() OVER ()) FROM posts"),
        AnalyzeError::GroupingError(_),
        concat!(
            "aggregate function calls cannot contain window function calls\n",
            "  ╭────\n",
            "1 │ SELECT SUM(ROW_NUMBER() OVER ()) FROM posts\n",
            "  ·            ──────────\n",
            "  ╰────\n",
        ),
    );
}

// ── GROUP BY column validation + select-alias fallback ───────────────────────

#[test]
fn group_by_unknown_column_rejected() {
    // A typo in GROUP BY used to be silently accepted — the walker
    // discarded `infer_expr`'s error to preserve "param coverage". Now
    // we propagate the error, but with a fallback for select aliases.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users GROUP BY ghost"),
        AnalyzeError::UndefinedColumn(_),
        concat!(
            "column \"ghost\" does not exist\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users GROUP BY ghost\n",
            "  ·                                     ──┬──\n",
            "  ·                                       ╰─ column does not exist\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn group_by_resolves_select_alias() {
    // PG accepts `GROUP BY <select_alias>` even though the alias isn't
    // in the FROM scope. The fallback in walk_group_clause_node makes
    // this still work after the propagation fix.
    let db = setup();
    let s = db
        .analyze("SELECT name AS author, count(*) AS posts FROM users GROUP BY author")
        .unwrap();
    assert_cols(&s, vec![c("author", text()), c("posts", int8())]);
}

// ── Ungrouped column in a grouped query (PG SQLSTATE 42803) ──────────────────

#[test]
fn ungrouped_column_with_aggregate_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*), age FROM users"),
        AnalyzeError::GroupingError(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*), age FROM users\n",
            "  ·                  ─┬─\n",
            "  ·                   ╰─ not in GROUP BY\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_inside_expression_rejected() {
    let db = setup();
    // The offending column can be nested in an expression, not just bare.
    assert_analyze_err!(
        db.analyze("SELECT count(*), age + 1 FROM users"),
        AnalyzeError::GroupingError(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*), age + 1 FROM users\n",
            "  ·                  ─┬─\n",
            "  ·                   ╰─ not in GROUP BY\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_with_non_pk_group_by_rejected() {
    let db = setup();
    // Grouping by `name` doesn't cover `age`.
    assert_analyze_err!(
        db.analyze("SELECT name, age FROM users GROUP BY name"),
        AnalyzeError::GroupingError(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT name, age FROM users GROUP BY name\n",
            "  ·              ─┬─\n",
            "  ·               ╰─ not in GROUP BY\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_in_having_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users HAVING age > 0"),
        AnalyzeError::GroupingError(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users HAVING age > 0\n",
            "  ·                                   ─┬─\n",
            "  ·                                    ╰─ not in GROUP BY\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn ungrouped_column_in_order_by_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users ORDER BY age"),
        AnalyzeError::GroupingError(_),
        concat!(
            "column \"users.age\" must appear in the GROUP BY clause or be used in an aggregate function\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users ORDER BY age\n",
            "  ·                                     ─┬─\n",
            "  ·                                      ╰─ not in GROUP BY\n",
            "  ╰────\n",
            "  help: add `users.age` to the GROUP BY clause, or wrap it in an aggregate like max(age)\n",
        ),
    );
}

#[test]
fn grouped_column_accepted() {
    let db = setup();
    let s = db
        .analyze("SELECT age, count(*) AS n FROM users GROUP BY age")
        .unwrap();
    assert_cols(&s, vec![cn("age", int4()), c("n", int8())]);
}

#[test]
fn non_grouped_columns_ok_when_primary_key_is_grouped() {
    // Functional dependency: grouping by the PK (`id`) functionally determines
    // every other column, so `name` and `age` need not be listed. PG accepts.
    let db = setup();
    let s = db
        .analyze("SELECT id, name, age FROM users GROUP BY id")
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), cn("age", int4())],
    );
}

#[test]
fn window_function_does_not_require_grouping() {
    // A window function is not an aggregate — the query isn't grouped, so a
    // plain column alongside it is fine.
    let db = setup();
    let s = db
        .analyze("SELECT age, row_number() OVER () AS rn FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("age", int4()), c("rn", int8())]);
}

#[test]
fn non_boolean_having_rejected() {
    // PG: a non-boolean HAVING is `argument of HAVING must be type boolean…`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT count(*) FROM users HAVING age"),
        AnalyzeError::DatatypeMismatch(_),
        concat!(
            "argument of HAVING must be type boolean, not type integer\n",
            "  ╭────\n",
            "1 │ SELECT count(*) FROM users HAVING age\n",
            "  ·                                   ─┬─\n",
            "  ·                                    ╰─ this is integer, expected boolean\n",
            "  ╰────\n",
        ),
    );
}

// ── Aggregate overload resolution must check arg types and arity ─────────────

#[test]
fn single_aggregate_overload_rejects_wrong_arg_type() {
    // `bool_or(boolean)` is the lone `bool_or` overload, but a single
    // aggregate candidate must still type-check: `integer` is not coercible to
    // `boolean`, so the call does not resolve.
    // PG: `function bool_or(integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT bool_or(age) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function bool_or(integer) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT bool_or(age) FROM users\n",
            "  ·        ───┬───\n",
            "  ·           ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: the only candidate is:\n",
            "          bool_or(boolean)\n",
        ),
    );
}

#[test]
fn single_aggregate_overload_rejects_wrong_arity() {
    // A lone aggregate overload must also reject the wrong argument count.
    // PG: `function bool_or(text, integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT bool_or(name, age) FROM users"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function bool_or(text, integer) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT bool_or(name, age) FROM users\n",
            "  ·        ───┬───\n",
            "  ·           ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: the only candidate is:\n",
            "          bool_or(boolean)\n",
        ),
    );
}

// ── max/min over the `record` pseudo-type only accepts composite actuals ─────

#[test]
fn max_over_scalar_without_overload_rejected() {
    // `max(record)` exists, but its `record` parameter accepts only an actual
    // composite/record value — not an arbitrary scalar. `max(boolean)` must be
    // rejected (it used to resolve to `record` via the pseudo-type fallback).
    // PG: `function max(boolean) does not exist`.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (active BOOLEAN, prefs JSONB);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT max(active) FROM t"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function max(boolean) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT max(active) FROM t\n",
            "  ·        ─┬─\n",
            "  ·         ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: candidates are:\n",
            "          max(anyarray)\n",
            "          max(anyenum)\n",
            "          max(bigint)\n",
            "          max(bytea)\n",
            "          max(character)\n",
            "          max(date)\n",
            "          max(double precision)\n",
            "          max(inet)\n",
            "          max(integer)\n",
            "          max(interval)\n",
            "          … and 14 more\n",
        ),
    );
    assert_analyze_err!(
        db.analyze("SELECT max(prefs) FROM t"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function max(jsonb) does not exist\n",
            "  ╭────\n",
            "1 │ SELECT max(prefs) FROM t\n",
            "  ·        ─┬─\n",
            "  ·         ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: No function matches the given name and argument types. You might need to add explicit type casts.\n",
            "  note: candidates are:\n",
            "          max(anyarray)\n",
            "          max(anyenum)\n",
            "          max(bigint)\n",
            "          max(bytea)\n",
            "          max(character)\n",
            "          max(date)\n",
            "          max(double precision)\n",
            "          max(inet)\n",
            "          max(integer)\n",
            "          max(interval)\n",
            "          … and 14 more\n",
        ),
    );
}

#[test]
fn max_over_composite_column_accepted() {
    // The `max(record)` overload still resolves for an actual composite value —
    // guards against over-rejecting after the scalar tightening.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE pt AS (x FLOAT8, y FLOAT8);
         CREATE TABLE shapes (id BIGINT PRIMARY KEY, p pt NOT NULL);",
    )
    .unwrap();
    let r = db.analyze("SELECT max(p) FROM shapes");
    assert!(r.is_ok(), "expected max(composite) to resolve, got: {r:?}");
}

// ── parseCheckAggregates over expressions, sublinks and levels ───────────────

fn grouping_rules_db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL, w text, n int);
         CREATE TABLE s (id int PRIMARY KEY, v int NOT NULL, x text);
         CREATE TABLE a (id int PRIMARY KEY, x int);
         CREATE TABLE b (id int PRIMARY KEY, y int);",
    )
    .unwrap();
    db
}

#[track_caller]
fn assert_ungrouped(db: &PgCatalog, sql: &str, col: &str) {
    assert_err_prefix!(
        db.analyze(sql),
        AnalyzeError::GroupingError(_),
        &format!(
            "column \"{col}\" must appear in the GROUP BY clause or be used in an aggregate \
             function"
        )
    );
}

/// A GROUP BY expression groups the expressions equal to it — nothing
/// else: the columns inside it stay ungrouped (PG compares transformed
/// expressions with `equal()`, not the columns they mention).
#[test]
fn expression_grouping_checks_every_other_column() {
    let db = grouping_rules_db();
    for (sql, col) in [
        ("SELECT id FROM t GROUP BY lower(w)", "t.id"),
        ("SELECT id FROM t GROUP BY 1 + 1", "t.id"),
        ("SELECT id FROM t GROUP BY now()", "t.id"),
        ("SELECT id FROM t GROUP BY n + 1", "t.id"),
        ("SELECT id FROM t GROUP BY $a", "t.id"),
        ("SELECT w FROM t GROUP BY id + 0", "t.w"),
        ("SELECT v * 2 FROM t GROUP BY v + 1", "t.v"),
        ("SELECT id FROM t GROUP BY 1 + 0", "t.id"),
        ("SELECT w FROM t GROUP BY v, lower(w)", "t.w"),
        ("SELECT w || 'x' FROM t GROUP BY w || 'y'", "t.w"),
        ("SELECT v FROM t GROUP BY ROLLUP (v + 1)", "t.v"),
        (
            "SELECT t.w FROM t GROUP BY GROUPING SETS ((v), (v + 1))",
            "t.w",
        ),
    ] {
        assert_ungrouped(&db, sql, col);
    }
    for sql in [
        "SELECT t.v * 2 FROM t GROUP BY v * 2",
        "SELECT lower(w), count(*) FROM t GROUP BY lower(t.w)",
        "SELECT v + 1 AS q FROM t GROUP BY q",
        "SELECT v + 1 FROM t GROUP BY 1",
        "SELECT w FROM t GROUP BY 1",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Every expression kind is walked for ungrouped columns — GREATEST, ROW,
/// COLLATE, an IN's left side, SQL/JSON and XML constructors — as are
/// DISTINCT ON and the direct arguments of an ordered-set aggregate.
#[test]
fn ungrouped_columns_found_in_every_expression_kind() {
    let db = grouping_rules_db();
    for (sql, col) in [
        ("SELECT GREATEST(w, 'a') FROM t GROUP BY v", "t.w"),
        ("SELECT ROW(w) FROM t GROUP BY v", "t.w"),
        ("SELECT w COLLATE \"C\" FROM t GROUP BY v", "t.w"),
        ("SELECT id IN (SELECT id FROM s) FROM t GROUP BY v", "t.id"),
        ("SELECT JSON_OBJECT('a': w) FROM t GROUP BY v", "t.w"),
        ("SELECT xmlelement(name a, w) FROM t GROUP BY v", "t.w"),
        ("SELECT DISTINCT ON (w) v FROM t GROUP BY v", "t.w"),
        (
            "SELECT percentile_cont(n::float8) WITHIN GROUP (ORDER BY id) FROM t GROUP BY w",
            "t.n",
        ),
    ] {
        assert_ungrouped(&db, sql, col);
    }
}

/// An aggregate anywhere in the level — ORDER BY, DISTINCT ON, a window's
/// PARTITION BY / ORDER BY — makes the query grouped.
#[test]
fn aggregate_outside_the_select_list_groups_the_query() {
    let db = grouping_rules_db();
    for (sql, col) in [
        ("SELECT id FROM t ORDER BY sum(v)", "t.id"),
        ("SELECT 1 FROM t ORDER BY v, count(*)", "t.v"),
        ("SELECT id FROM t WINDOW w AS (ORDER BY count(*))", "t.id"),
        ("SELECT DISTINCT ON (id) count(*) FROM t", "t.id"),
        ("SELECT sum(id) OVER (ORDER BY sum(id)) FROM t", "t.id"),
    ] {
        assert_ungrouped(&db, sql, col);
    }
}

/// An aggregate whose arguments only reference an outer query belongs to
/// that query: it makes it grouped, obeys its clause's placement rule, and
/// is allowed where the subquery's own clause would forbid it.
#[test]
fn outer_level_aggregate_belongs_to_the_outer_query() {
    let db = grouping_rules_db();
    assert_ungrouped(&db, "SELECT id, (SELECT sum(t.id) FROM s) FROM t", "t.id");
    assert_ungrouped(
        &db,
        "SELECT (SELECT count(t.v) FROM s LIMIT 1), id FROM t",
        "t.id",
    );
    for (sql, msg) in [
        (
            "SELECT id FROM t WHERE v IN (SELECT count(t.v) FROM s)",
            "aggregate functions are not allowed in WHERE",
        ),
        (
            "SELECT * FROM a JOIN b ON (SELECT sum(a.x)) > 0",
            "aggregate functions are not allowed in JOIN conditions",
        ),
        (
            "SELECT * FROM a, LATERAL (SELECT sum(a.x)) s",
            "aggregate functions are not allowed in FROM clause of their own query level",
        ),
        (
            "SELECT * FROM a, generate_series(1, (SELECT sum(a.x))) s",
            "aggregate functions are not allowed in functions in FROM",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::GroupingError(_), msg);
    }
    // An UPDATE's or RETURNING's sublink can't hold the statement's
    // aggregate either.
    for (sql, msg) in [
        (
            "UPDATE t SET v = (SELECT count(t.v) FROM s)",
            "aggregate functions are not allowed in UPDATE",
        ),
        (
            "UPDATE t SET (v, w) = (SELECT count(t.v), 'x' FROM s)",
            "aggregate functions are not allowed in UPDATE",
        ),
        (
            "DELETE FROM t RETURNING (SELECT count(t.v) FROM s)",
            "aggregate functions are not allowed in RETURNING",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::GroupingError(_), msg);
    }
    // In the subquery's WHERE, the outer aggregate is just a value.
    db.analyze("SELECT (SELECT 1 FROM b WHERE count(a.x) > 0) FROM a")
        .unwrap();
    db.analyze("SELECT (SELECT sum(t.v)) FROM t GROUP BY w")
        .unwrap();
}

/// An aggregate belongs to the outermost level its arguments reference,
/// so one inside another's argument — even through sublinks and
/// another level's aggregate — is still nested in it.
#[test]
fn aggregates_nested_through_an_outer_reference_are_rejected() {
    let db = grouping_rules_db();
    for sql in [
        // `avg(a2.x)` is the outermost query's, inside its own `avg(...)`.
        "SELECT avg((SELECT avg(a1.id ORDER BY (SELECT avg(a2.x) FROM b a3)) FROM b a1))
         FROM a a2",
        "SELECT sum((SELECT count(a.x) FROM b)) FROM a",
        "SELECT sum((SELECT max(b.id) FILTER (WHERE count(a.x) > 0) FROM b)) FROM a",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "aggregate function calls cannot be nested"
        );
    }
    // The inner aggregate of the subquery's own level isn't nested.
    for sql in [
        "SELECT avg((SELECT avg(a1.id ORDER BY (SELECT avg(a3.id) FROM b a3)) FROM b a1))
         FROM a a2",
        "SELECT sum((SELECT count(b.id) FROM b WHERE b.id = a.x)) FROM a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// An aggregate is nested only in an aggregate of its own level (or a
/// lower one): in `(SELECT max(sum(a.x) + b.y) FROM b)`, `max` reads `b`
/// and is the subquery's, while `sum` is the outer query's — a value
/// there, not a nesting.
#[test]
fn outer_aggregate_in_an_inner_aggregate_is_not_nested() {
    let db = grouping_rules_db();
    for sql in [
        "SELECT (SELECT max(sum(a.x) + b.y) FROM b) FROM a",
        "SELECT (SELECT max(b.y + sum(a.x)) FROM b) FROM a GROUP BY a.id",
        "SELECT (SELECT json_agg(sum(a.x) + b.y) FROM b) FROM a",
        "SELECT (SELECT json_arrayagg(sum(a.x) + b.y) FROM b) FROM a",
        "SELECT (SELECT json_objectagg(b.y : sum(a.x)) FROM b) FROM a",
        "SELECT (SELECT array_agg(b.y ORDER BY sum(a.x)) FROM b) FROM a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // With no column of its own, the inner call takes the level of the
    // aggregate it holds: a nesting.
    for sql in [
        "SELECT (SELECT max(sum(a.x)) FROM b) FROM a",
        "SELECT (SELECT json_arrayagg(sum(a.x)) FROM b) FROM a",
        "SELECT sum(count(*)) FROM a",
        "SELECT json_arrayagg(sum(x)) FROM a",
        "SELECT json_objectagg(id : count(*)) FROM a",
        "SELECT json_agg(sum(x) + x) FROM a",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "aggregate function calls cannot be nested"
        );
    }
}

/// A VALUES list's rows can't hold an aggregate, `GROUPING(…)` or window
/// call of its own level (EXPR_KIND_VALUES), even nested in another or
/// reached through a sublink; an outer level's makes that level grouped.
#[test]
fn values_rows_hold_no_calls_of_their_level() {
    let db = grouping_rules_db();
    assert_analyze_err!(
        db.analyze("SELECT 1 FROM (VALUES (1), (sum(2))) v"),
        AnalyzeError::GroupingError(_),
        "\
aggregate functions are not allowed in VALUES
  ╭────
1 │ SELECT 1 FROM (VALUES (1), (sum(2))) v
  ·                             ─┬─
  ·                              ╰─ aggregate not allowed here
  ╰────
",
    );
    for sql in [
        "VALUES (sum(1))",
        // The inner call is transformed, and rejected, first.
        "VALUES (sum(sum(1)))",
        "VALUES (sum(1)), (generate_series(1, 2))",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "aggregate functions are not allowed in VALUES"
        );
    }
    assert_err_prefix!(
        db.analyze("VALUES (row_number() OVER ())"),
        AnalyzeError::WindowingError(_),
        "window functions are not allowed in VALUES"
    );
    assert_err_prefix!(
        db.analyze("SELECT a.x, (VALUES (sum(a.x))) FROM a"),
        AnalyzeError::GroupingError(_),
        "column \"a.x\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    for sql in [
        "SELECT (VALUES (sum(a.x))) FROM a",
        "SELECT (SELECT 1 FROM (VALUES (sum(a.x))) v) FROM a",
        "SELECT (VALUES (grouping(a.x))) FROM a GROUP BY a.x",
        "VALUES ((SELECT sum(1)))",
        "SELECT (VALUES ((SELECT sum(a.x)))) FROM a",
        // `column1` is `o`'s: the rows don't see their own columns.
        "SELECT (SELECT 1 FROM (VALUES ((SELECT sum(column1)))) v) FROM (VALUES (5)) o",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A call of a VALUES list's own level in its ORDER BY passes parse
/// analysis as a sort key, but the plan has no Agg / WindowAgg node to run
/// it: every execution fails initializing it (`ExecInitExprRec`, XX000) —
/// where the VALUES is certain to be planned.
#[test]
fn values_sort_keys_hold_no_calls_of_their_level() {
    let db = grouping_rules_db();
    for (sql, message) in [
        (
            "VALUES (1) ORDER BY sum(1)",
            "Aggref found in non-Agg plan node",
        ),
        (
            "VALUES (1) ORDER BY 1 + count(*)",
            "Aggref found in non-Agg plan node",
        ),
        (
            "VALUES (1) ORDER BY sum(column1)",
            "Aggref found in non-Agg plan node",
        ),
        (
            "VALUES (1) ORDER BY (SELECT sum(column1))",
            "Aggref found in non-Agg plan node",
        ),
        (
            "SELECT (VALUES (1) ORDER BY sum(column1)) FROM (VALUES (5)) o",
            "Aggref found in non-Agg plan node",
        ),
        (
            "VALUES (1) ORDER BY grouping(column1)",
            "GroupingFunc found in non-Agg plan node",
        ),
        (
            "VALUES (1) ORDER BY row_number() OVER ()",
            "WindowFunc found in non-WindowAgg plan node",
        ),
        // The sort keys are initialized in order.
        (
            "VALUES (1) ORDER BY row_number() OVER (), sum(1)",
            "WindowFunc found in non-WindowAgg plan node",
        ),
        (
            "VALUES (1) ORDER BY sum(1), row_number() OVER ()",
            "Aggref found in non-Agg plan node",
        ),
        (
            "EXPLAIN VALUES (1) ORDER BY sum(1)",
            "Aggref found in non-Agg plan node",
        ),
        (
            "SELECT (VALUES (1) ORDER BY sum(1))",
            "Aggref found in non-Agg plan node",
        ),
        // A FROM subquery, an IN sublink or a CTE the plan holds — unless
        // it is a dummy one (see below).
        (
            "SELECT * FROM (VALUES (1) ORDER BY sum(1)) s",
            "Aggref found in non-Agg plan node",
        ),
        (
            "SELECT * FROM (VALUES (1) ORDER BY count(*) OVER ()) s",
            "WindowFunc found in non-WindowAgg plan node",
        ),
        (
            "SELECT 1 FROM (SELECT * FROM (VALUES (1) ORDER BY sum(1)) v) s",
            "Aggref found in non-Agg plan node",
        ),
        (
            "SELECT * FROM (VALUES (1) ORDER BY sum(1)) s LIMIT 0",
            "Aggref found in non-Agg plan node",
        ),
        (
            "SELECT * FROM a WHERE x IN (VALUES (1) ORDER BY sum(1))",
            "Aggref found in non-Agg plan node",
        ),
        (
            "WITH s AS MATERIALIZED (VALUES (1) ORDER BY sum(1)) SELECT * FROM s",
            "Aggref found in non-Agg plan node",
        ),
        (
            "WITH s AS (VALUES (1) ORDER BY sum(1)) SELECT * FROM s",
            "Aggref found in non-Agg plan node",
        ),
        // A SubPlan is started even in a dummy plan, a CTE scanned on its
        // own too.
        (
            "SELECT * FROM a WHERE x = (VALUES (1) ORDER BY sum(1)) AND x = 1 AND x = 2",
            "Aggref found in non-Agg plan node",
        ),
        (
            "WITH s AS MATERIALIZED (VALUES (1) ORDER BY sum(1)) SELECT * FROM s WHERE false",
            "Aggref found in non-Agg plan node",
        ),
        (
            "WITH s AS (VALUES (1) ORDER BY sum(1)) SELECT * FROM s, s s2 WHERE false",
            "Aggref found in non-Agg plan node",
        ),
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::PgInternalError(_), message);
    }
    assert_err_prefix!(
        db.analyze("SELECT * FROM (VALUES (1) ORDER BY generate_series(1, 2)) s"),
        AnalyzeError::FeatureNotSupported(_),
        "set-valued function called in context that cannot accept a set"
    );
    // An outer level's aggregate is a value there; a VALUES the planner
    // drops, or never runs, raises nothing.
    for sql in [
        "SELECT (VALUES (1) ORDER BY sum(a.x)) FROM a",
        "SELECT (VALUES (1) ORDER BY (SELECT sum(a.x))) FROM a",
        "SELECT * FROM (VALUES (1) ORDER BY sum(1)) v WHERE false",
        "SELECT EXISTS (VALUES (1) ORDER BY sum(1))",
        // The planner proves the plan empty: a dummy one, never started.
        "SELECT * FROM a, (VALUES (1) ORDER BY sum(1)) v WHERE false",
        "SELECT * FROM a JOIN (VALUES (1) ORDER BY sum(1)) v(c) ON false",
        "SELECT count(*) FROM (VALUES (1) ORDER BY sum(1)) v HAVING false",
        "SELECT * FROM a WHERE x IN (VALUES (1) ORDER BY sum(1)) AND x = 1 AND x = 2",
        "SELECT * FROM a WHERE EXISTS (VALUES (1) ORDER BY sum(1))",
        "WITH s AS (VALUES (1) ORDER BY sum(1)) SELECT * FROM s WHERE false",
        "WITH s AS (VALUES (1) ORDER BY sum(1)) SELECT 1",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_err_prefix!(
        db.analyze("SELECT a.x, (VALUES (1) ORDER BY sum(a.x)) FROM a"),
        AnalyzeError::GroupingError(_),
        "column \"a.x\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
}

/// An ordered-set aggregate that belongs to an outer level (its ordering
/// columns are outer) can't take a subquery's columns as direct
/// arguments.
#[test]
fn ordered_set_direct_arguments_must_not_hold_lower_level_variables() {
    let db = grouping_rules_db();
    for sql in [
        "SELECT array(SELECT percentile_disc(a) WITHIN GROUP (ORDER BY x)
                      FROM (VALUES (0.3), (0.7)) v(a) GROUP BY a)
         FROM generate_series(1,5) g(x)",
        "SELECT (SELECT percentile_cont(b.id::float8) WITHIN GROUP (ORDER BY a.x) FROM b LIMIT 1)
         FROM a",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "outer-level aggregate cannot contain a lower-level variable in its direct arguments"
        );
    }
    // Direct arguments of the aggregate's own level, or constants, are fine.
    for sql in [
        "SELECT array(SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY x)
                      FROM (VALUES (0.3), (0.7)) v(a))
         FROM generate_series(1,5) g(x)",
        "SELECT percentile_disc(a) WITHIN GROUP (ORDER BY x)
         FROM (VALUES (0.3, 1), (0.7, 2)) v(a, x) GROUP BY a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A grouped query's sublinks may only use its grouped columns
/// (`subquery uses ungrouped column … from outer query`).
#[test]
fn sublink_in_grouped_query_uses_only_grouped_outer_columns() {
    let db = grouping_rules_db();
    for (sql, col) in [
        ("SELECT (SELECT t.w) FROM t GROUP BY v", "t.w"),
        (
            "SELECT EXISTS (SELECT 1 FROM s WHERE s.id = t.id) FROM t GROUP BY v",
            "t.id",
        ),
        (
            "SELECT v FROM t GROUP BY v HAVING (SELECT t.w) IS NULL",
            "t.w",
        ),
        ("SELECT count(*) FROM t HAVING (SELECT t.v) > 0", "t.v"),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            &format!("subquery uses ungrouped column \"{col}\" from outer query")
        );
    }
    for sql in [
        "SELECT (SELECT t.w) FROM t GROUP BY w",
        "SELECT (SELECT count(*) FROM a WHERE a.x = t.v) FROM t GROUP BY t.v",
        "SELECT (SELECT max(a.x) FROM a GROUP BY a.id HAVING a.id = t.v LIMIT 1) FROM t \
         GROUP BY t.v",
        "SELECT (SELECT t.w) FROM t GROUP BY id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// GROUPING() arguments must equal grouping expressions of the level the
/// call belongs to — a sublink's GROUPING over outer columns is the outer
/// query's — and there may be at most 31.
#[test]
fn grouping_function_arguments_and_level() {
    let db = grouping_rules_db();
    assert_err_prefix!(
        db.analyze("SELECT GROUPING(v + 1) FROM t GROUP BY v"),
        AnalyzeError::GroupingError(_),
        "arguments to GROUPING must be grouping expressions of the associated query level"
    );
    let s = db
        .analyze("SELECT (SELECT GROUPING(t.v) FROM s LIMIT 1) AS g FROM t GROUP BY t.v")
        .unwrap();
    assert_cols(&s, vec![cn("g", int4())]);
    db.analyze("SELECT GROUPING(v + 1) FROM t GROUP BY v + 1")
        .unwrap();
    let many = ["v"; 32].join(", ");
    assert_err_prefix!(
        db.analyze(&format!("SELECT GROUPING({many}) FROM t GROUP BY v")),
        AnalyzeError::TooManyArguments(_),
        "GROUPING must have fewer than 32 arguments"
    );
}

/// PG's limits on grouping sets: 12 CUBE elements, 4096 sets.
#[test]
fn grouping_set_limits() {
    let db = grouping_rules_db();
    let v13 = ["v"; 13].join(", ");
    assert_err_prefix!(
        db.analyze(&format!("SELECT 1 FROM t GROUP BY CUBE({v13})")),
        AnalyzeError::TooManyColumns(_),
        "CUBE is limited to 12 elements"
    );
    let v12 = ["v"; 12].join(", ");
    assert_err_prefix!(
        db.analyze(&format!("SELECT 1 FROM t GROUP BY CUBE({v12}), CUBE(v)")),
        AnalyzeError::StatementTooComplex(_),
        "too many grouping sets present (maximum 4096)"
    );
    db.analyze(&format!("SELECT 1 FROM t GROUP BY CUBE({v12})"))
        .unwrap();
}

/// A JOIN USING column stands for the left input's column (the right's
/// for RIGHT JOIN, COALESCE of both for FULL JOIN) when grouping, as PG's
/// flatten_join_alias_vars makes it.
#[test]
fn join_using_column_groups_as_the_column_it_stands_for() {
    let db = grouping_rules_db();
    for sql in [
        "SELECT a.id FROM a JOIN b USING (id) GROUP BY id",
        "SELECT id FROM a JOIN b USING (id) GROUP BY a.id",
        "SELECT id, a.x FROM a JOIN b USING (id) GROUP BY id",
        "SELECT id FROM a FULL JOIN b USING (id) GROUP BY id",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, col) in [
        ("SELECT id, x FROM a JOIN b USING (id) GROUP BY x", "a.id"),
        ("SELECT b.id FROM a JOIN b USING (id) GROUP BY id", "b.id"),
        (
            "SELECT id FROM a FULL JOIN b USING (id) GROUP BY a.id",
            "b.id",
        ),
    ] {
        assert_ungrouped(&db, sql, col);
    }
    // An aliased join's columns stand for its inputs' too — a grouped
    // primary key reached through it determines its table's columns.
    for sql in [
        "SELECT j.w FROM (t JOIN s USING (id)) j GROUP BY j.id",
        "SELECT j.w FROM (t LEFT JOIN s USING (id)) j GROUP BY id",
        "SELECT j.c FROM (t JOIN s ON t.id = s.id) j(a, b, c, d, e, f) GROUP BY j.a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, col) in [
        (
            "SELECT j.x FROM (t JOIN s USING (id)) j GROUP BY j.id",
            "s.x",
        ),
        (
            "SELECT j.w FROM (t FULL JOIN s USING (id)) j GROUP BY id",
            "t.w",
        ),
    ] {
        assert_ungrouped(&db, sql, col);
    }
    // An INNER join's USING column is the side that needs no coercion.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE a (id int PRIMARY KEY, x int);
         CREATE TABLE b (id bigint PRIMARY KEY, y int);",
    )
    .unwrap();
    db.analyze("SELECT b.y FROM a JOIN b USING (id) GROUP BY id")
        .unwrap();
    assert_ungrouped(
        &db,
        "SELECT id FROM a JOIN b USING (id) GROUP BY a.id",
        "b.id",
    );
    assert_ungrouped(
        &db,
        "SELECT a.x FROM a LEFT JOIN b USING (id) GROUP BY id",
        "a.x",
    );
}

/// An aggregate is no function to call in FROM.
#[test]
fn aggregate_rejected_as_from_function() {
    let db = grouping_rules_db();
    assert_err_prefix!(
        db.analyze("SELECT * FROM count(*)"),
        AnalyzeError::GroupingError(_),
        "aggregate functions are not allowed in functions in FROM"
    );
}

/// An aggregate over a whole-row reference works in a FROM subquery or a
/// CTE as it does at the top level.
#[test]
fn whole_row_aggregates_in_a_from_subquery_or_cte() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (a int NOT NULL, b int); CREATE TABLE u (x int);")
        .unwrap();
    for (sql, col) in [
        (
            "SELECT j FROM (SELECT json_agg(x) j FROM t x) s",
            cn("j", json_ty()),
        ),
        (
            "WITH c AS (SELECT json_agg(t) j FROM t) SELECT j FROM c",
            cn("j", json_ty()),
        ),
        (
            "SELECT * FROM (SELECT count(t) FROM t) s",
            c("count", int8()),
        ),
        (
            "SELECT * FROM (SELECT count(x.*) FROM t x) s",
            c("count", int8()),
        ),
        (
            "SELECT j FROM (SELECT json_agg(t) j FROM t) s",
            cn("j", json_ty()),
        ),
    ] {
        let s = db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_cols(&s, vec![col]);
    }
    // A LATERAL subquery sees its parent's entries: a name its own entries
    // don't hold as a column is the parent's column, or its row.
    for sql in [
        "SELECT * FROM t, LATERAL (SELECT json_agg(t) FROM u) s",
        "SELECT * FROM u, LATERAL (SELECT json_agg(x) FROM t x) s",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "aggregate functions are not allowed in FROM clause of their own query level"
        );
    }
    // `count(t)` is the outer query's: over an empty `u` the subquery has
    // no row, so NULL.
    let s = db
        .analyze("SELECT (SELECT count(t) FROM u) AS n FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("n", int8())]);
}

/// A name reaching a level whose FROM clause is still being analyzed
/// belongs to it only if an entry of it the inner query can see has the
/// name; else, like PG's level search, it goes on outward. A plain FROM
/// subquery or a CTE sees none of that level's entries, a LATERAL item
/// those to its left, a JOIN condition's or FROM function's sublink the
/// inputs.
#[test]
fn outer_names_skip_a_level_whose_from_clause_is_being_analyzed() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE a (x int); CREATE TABLE b (y int); CREATE TABLE c (z int);")
        .unwrap();
    // `x` and `a` are the outermost level's: their aggregates are values
    // inside the middle level's FROM clause.
    for sql in [
        "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT sum(x)) s) FROM a",
        "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT sum(x) FROM c) s) FROM a",
        "SELECT (SELECT 1 FROM b, (SELECT sum(x) FROM c) s) FROM a",
        "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT count(a)) s) FROM a",
        "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT json_agg(a)) s) FROM a",
        "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT sum(a.x)) s) FROM a",
        "SELECT (SELECT 1 FROM b, (SELECT sum(a.x) FROM c) s) FROM a",
        "SELECT (SELECT 1 FROM b JOIN c ON (SELECT sum(x)) > 0) FROM a",
        "SELECT (SELECT 1 FROM b, generate_series(1, (SELECT sum(x))::int)) FROM a",
        "SELECT (WITH w AS (SELECT sum(x) FROM c) SELECT 1 FROM w) FROM a",
        "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL \
         (SELECT 1 FROM c CROSS JOIN LATERAL (SELECT sum(x)) t) s) FROM a",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // They make the outermost level grouped.
    for sql in [
        "SELECT x, (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT sum(x)) s) FROM a",
        "SELECT x, (SELECT 1 FROM b, (SELECT count(a) FROM c) s) FROM a",
        "SELECT x, (SELECT 1 FROM b JOIN c ON (SELECT sum(x)) > 0) FROM a",
        "SELECT x, (WITH w AS (SELECT sum(x)) SELECT 1 FROM w) FROM a",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            "column \"a.x\" must appear in the GROUP BY clause or be used in an aggregate function"
        );
    }
    let s = db
        .analyze("SELECT (SELECT s.v FROM b CROSS JOIN LATERAL (SELECT sum(x) AS v) s) AS n FROM a")
        .unwrap();
    assert_cols(&s, vec![cn("n", int8())]);
    // A name an entry it sees has is the middle level's, whose FROM clause
    // can't hold its aggregate.
    for (sql, clause) in [
        (
            "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT sum(y)) s) FROM a",
            "FROM clause of their own query level",
        ),
        (
            "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT count(b)) s) FROM a",
            "FROM clause of their own query level",
        ),
        (
            "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL \
             (SELECT 1 FROM c CROSS JOIN LATERAL (SELECT sum(y)) t) s) FROM a",
            "FROM clause of their own query level",
        ),
        (
            "SELECT (SELECT 1 FROM b CROSS JOIN LATERAL (SELECT 1 FROM c, (SELECT sum(y)) t) s) \
             FROM a",
            "FROM clause of their own query level",
        ),
        (
            "SELECT (SELECT 1 FROM b JOIN c ON (SELECT sum(y)) > 0) FROM a",
            "JOIN conditions",
        ),
        (
            "SELECT (SELECT 1 FROM b, generate_series(1, (SELECT sum(y))::int)) FROM a",
            "functions in FROM",
        ),
        (
            "UPDATE b SET y = (SELECT 1 FROM c CROSS JOIN LATERAL (SELECT sum(y)) s)",
            "UPDATE",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::GroupingError(_),
            &format!("aggregate functions are not allowed in {clause}")
        );
    }
}
